//! Frozen workloads for the pure callback engine and an in-memory byte transport.
//! No HTTP/1 parser, socket, async runtime, or application header clone participates.

use criterion::{Criterion, Throughput};
use kimojio_fsm_http2::*;
use std::{collections::VecDeque, hint::black_box, time::Duration};

#[derive(Default)]
struct Executor {
    read: Option<ReadOp>,
    write: Option<WriteOp>,
    requests: VecDeque<StreamId>,
    permits: VecDeque<SendPermit>,
    bodies: VecDeque<BodyOp>,
    buffers: Vec<Vec<u8>>,
    wakes: Vec<WakeOp>,
    cancellations: Vec<CancelOp>,
    retired: usize,
    received: usize,
}

impl Ports<Vec<u8>> for Executor {
    type Output = ();
    fn read(&mut self, op: ReadOp) -> Option<()> {
        assert!(self.read.replace(op).is_none());
        None
    }
    fn write(&mut self, op: WriteOp) -> Option<()> {
        assert!(self.write.replace(op).is_none());
        None
    }
    fn headers(&mut self, head: Head<'_>) -> Option<()> {
        black_box(head.len());
        if head.kind == HeadKind::Request {
            self.requests.push_back(head.stream);
        }
        None
    }
    fn body(&mut self, op: BodyOp) -> Option<()> {
        self.received += op.bytes().len();
        black_box(op.bytes());
        self.bodies.push_back(op);
        None
    }
    fn send_ready(&mut self, permit: SendPermit) -> Option<()> {
        self.permits.push_back(permit);
        None
    }
    fn send_stopped(&mut self, _: StreamId, reason: SendStop) -> Option<()> {
        assert_eq!(reason, SendStop::Finished);
        None
    }
    fn sent(&mut self, sent: Sent) -> Option<()> {
        assert_eq!(sent.result, Ok(()));
        assert!(sent.exact);
        self.buffers.push(sent.buffer);
        None
    }
    fn ended(&mut self, end: ReceiveEnd) -> Option<()> {
        assert_eq!(end.outcome, StreamOutcome::Complete);
        None
    }
    fn retired(&mut self, result: StreamResult) -> Option<()> {
        assert_eq!(result.outcome, StreamOutcome::Complete);
        self.retired += 1;
        None
    }
    fn cancel(&mut self, op: CancelOp) -> Option<()> {
        self.cancellations.push(op);
        None
    }
    fn wake(&mut self, op: WakeOp) -> Option<()> {
        self.wakes.push(op);
        None
    }
    fn close(&mut self, _: CloseOp) -> Option<()> {
        panic!("unexpected transport close")
    }
    fn closed(&mut self, result: ConnectionResult) -> Option<()> {
        panic!("unexpected close: {result:?}")
    }
    fn reschedule(&mut self) -> Option<()> {
        Some(())
    }
}

fn transport(
    engine: &mut Connection,
    executor: &mut Executor,
    input: &mut VecDeque<u8>,
    output: &mut VecDeque<u8>,
    fragment: usize,
) {
    engine.next(executor);
    if let Some(op) = executor.write.take() {
        let count = op.remaining().min(fragment);
        let mut remaining = count;
        for part in op.slices() {
            let length = part.len().min(remaining);
            output.extend(part[..length].iter().copied());
            remaining -= length;
        }
        assert_eq!(remaining, 0);
        engine
            .complete_write(op.complete(WriteOutcome::Written(count)))
            .unwrap();
    }
    if !input.is_empty()
        && let Some(mut op) = executor.read.take()
    {
        let count = input.len().min(fragment).min(op.buffer_mut().len());
        for target in &mut op.buffer_mut()[..count] {
            *target = input.pop_front().unwrap();
        }
        engine
            .complete_read(op.complete(ReadOutcome::Read(count)))
            .unwrap();
    }
    for cancellation in executor.cancellations.drain(..) {
        let index = executor
            .wakes
            .iter()
            .position(|wake| wake.token() == cancellation.original())
            .expect("only SETTINGS alarms need cancellation in these workloads");
        let wake = executor.wakes.remove(index);
        engine.complete_wake(wake.complete(Duration::ZERO)).unwrap();
        engine.complete_cancel(cancellation.complete()).unwrap();
    }
}

fn workload(streams: usize, payload: usize, fragment: usize, retain_first: bool) {
    let mut client = Client::new(Config::default(), Duration::ZERO).unwrap();
    let mut server = Server::new(Config::default(), Duration::ZERO).unwrap();
    let mut client_executor = Executor::default();
    let mut server_executor = Executor::default();
    let mut to_client = VecDeque::with_capacity(128 * 1024);
    let mut to_server = VecDeque::with_capacity(128 * 1024);
    let fields = [
        H2RawHeaderRef::new(b":method", b"GET"),
        H2RawHeaderRef::new(b":scheme", b"https"),
        H2RawHeaderRef::new(b":authority", b"benchmark.test"),
        H2RawHeaderRef::new(b":path", b"/"),
    ];
    for _ in 0..streams {
        client.request_ref(&fields, true).unwrap();
    }
    let mut supplied = vec![0; streams];
    let mut held: Option<(BodyOp, usize)> = None;
    let mut retained = false;
    for turn in 0..2_000_000 {
        while let Some(id) = server_executor.requests.pop_front() {
            server
                .respond_ref(id, &[H2RawHeaderRef::new(b":status", b"200")], payload == 0)
                .unwrap();
        }
        while let Some(permit) = server_executor.permits.pop_front() {
            let index = permit.stream().get() as usize / 2;
            let count = (payload - supplied[index])
                .min(permit.max_bytes())
                .min(16_384);
            let mut buffer = server_executor.buffers.pop().unwrap_or_default();
            buffer.resize(count, 42);
            supplied[index] += count;
            server
                .send(permit, buffer, supplied[index] == payload)
                .unwrap();
        }
        while let Some(body) = client_executor.bodies.pop_front() {
            if retain_first && !retained {
                held = Some((body, turn + 8));
                retained = true;
            } else {
                client.release_body(body.release()).unwrap();
            }
        }
        if held.as_ref().is_some_and(|(_, until)| turn >= *until) {
            client
                .release_body(held.take().unwrap().0.release())
                .unwrap();
        }
        transport(
            &mut client,
            &mut client_executor,
            &mut to_client,
            &mut to_server,
            fragment,
        );
        transport(
            &mut server,
            &mut server_executor,
            &mut to_server,
            &mut to_client,
            fragment,
        );
        if client_executor.retired == streams && server_executor.retired == streams {
            assert_eq!(client_executor.received, streams * payload);
            assert!(held.is_none());
            black_box((client, server));
            return;
        }
    }
    panic!("bounded benchmark executor stalled");
}

fn main() {
    let mut criterion = Criterion::default().configure_from_args();
    let mut group = criterion.benchmark_group("pure-http2-v1");
    for (name, streams, payload, fragment, retained) in [
        ("new-1x-empty", 1, 0, 32_768, false),
        ("new-1x-1KiB", 1, 1024, 32_768, false),
        ("new-16x-1KiB", 16, 1024, 32_768, false),
        ("new-4x-64KiB-held", 4, 65_536, 32_768, true),
        ("new-1x-4MiB", 1, 4 * 1024 * 1024, 32_768, false),
        ("new-1x-1KiB-fragment7", 1, 1024, 7, false),
    ] {
        group.throughput(if payload == 0 {
            Throughput::Elements(streams as u64)
        } else {
            Throughput::Bytes((streams * payload) as u64)
        });
        group.bench_function(name, |b| {
            b.iter(|| workload(streams, payload, fragment, retained))
        });
    }
    group.finish();
    criterion.final_summary();
}
