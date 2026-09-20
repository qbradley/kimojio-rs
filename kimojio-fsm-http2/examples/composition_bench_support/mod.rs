use kimojio_fsm_http1 as h1;
use kimojio_fsm_http2::{self as h2, http};
use std::time::{Duration, Instant};

mod diagnostics;
use diagnostics::Diagnostics;
#[allow(dead_code, reason = "used only by the separate allocation probe")]
pub mod allocation;
#[cfg(test)]
mod overlap;

type Buffer = &'static [u8];
static PAYLOAD: [u8; 32768] = [0x5a; 32768];
const REQUEST: [h2::H2RawHeaderRef<'static>; 4] = [
    h2::H2RawHeaderRef::new(b":method", b"POST"),
    h2::H2RawHeaderRef::new(b":scheme", b"https"),
    h2::H2RawHeaderRef::new(b":authority", b"example.test"),
    h2::H2RawHeaderRef::new(b":path", b"/"),
];
const RESPONSE: [h2::H2RawHeaderRef<'static>; 1] = [h2::H2RawHeaderRef::new(b":status", b"200")];

#[derive(Clone, Copy)]
pub struct Case {
    name: &'static str,
    request: usize,
    response: usize,
    paused: bool,
    #[cfg(test)]
    require_overlap: bool,
}

impl Case {
    pub fn named(name: &str) -> Self {
        let (name, request, response, paused) = match name {
            "empty" => ("empty", 0, 128, false),
            "duplex" => ("duplex", 4096, 4096, false),
            "32k" => ("32k", 32768, 32768, false),
            "1m" => ("1m", 1048576, 1048576, false),
            "paused" => ("paused", 0, 128, true),
            _ => panic!("unknown case: {name}"),
        };
        Self {
            name,
            request,
            response,
            paused,
            #[cfg(test)]
            require_overlap: false,
        }
    }

    #[cfg(test)]
    pub fn with_overlap_assertion(mut self) -> Self {
        assert!(self.request > 0 && self.response > 0 && !self.paused);
        self.require_overlap = true;
        self
    }
}

#[derive(Default)]
struct Slot {
    stream: Option<h2::StreamId>,
    sent: usize,
    received: usize,
    returned: usize,
    headers: usize,
    ended: bool,
    retired: bool,
}

struct Ports {
    server: bool,
    receive_bytes: usize,
    slots: Vec<Slot>,
    read: Option<h2::ReadOp>,
    write: Option<h2::WriteOp<Buffer>>,
    bodies: Vec<h2::BodyOp>,
    permits: Vec<h2::SendPermit>,
    requests: Vec<h2::StreamId>,
    alarms: Vec<h2::WakeOp>,
    cancels: Vec<h2::CancelOp>,
    close: Option<h2::CloseOp>,
    retired: usize,
    closed: bool,
    events: usize,
    wire_bytes: usize,
    paused: bool,
    diagnostics: Option<Box<Diagnostics>>,
    #[cfg(test)]
    overlap: overlap::Observer,
}

impl Ports {
    fn new(server: bool, concurrency: usize, case: Case) -> Self {
        Self {
            server,
            receive_bytes: if server { case.request } else { case.response },
            slots: (0..concurrency).map(|_| Slot::default()).collect(),
            read: None,
            write: None,
            bodies: Vec::with_capacity(concurrency * 16),
            permits: Vec::with_capacity(concurrency),
            requests: Vec::with_capacity(concurrency),
            alarms: Vec::with_capacity(16),
            cancels: Vec::with_capacity(16),
            close: None,
            retired: 0,
            closed: false,
            events: 0,
            wire_bytes: 0,
            paused: case.paused && !server,
            diagnostics: Diagnostics::from_env(server),
            #[cfg(test)]
            overlap: overlap::Observer::new(
                !server && case.require_overlap,
                case.request * concurrency,
            ),
        }
    }
    fn slot(&mut self, stream: h2::StreamId) -> &mut Slot {
        let index = (stream.get() as usize / 2) % self.slots.len();
        let slot = &mut self.slots[index];
        assert_eq!(*slot.stream.get_or_insert(stream), stream);
        slot
    }
    fn event(&mut self) -> Option<()> {
        self.events += 1;
        None
    }
    fn begin(&mut self) {
        #[cfg(test)]
        self.overlap.begin_batch();
        for slot in &mut self.slots {
            *slot = Slot::default();
        }
    }
}

impl h2::Ports<Buffer> for Ports {
    type Output = ();
    fn read(&mut self, op: h2::ReadOp) -> Option<()> {
        assert!(self.read.replace(op).is_none());
        self.event()
    }
    fn write(&mut self, op: h2::WriteOp<Buffer>) -> Option<()> {
        if let Some(diagnostics) = &mut self.diagnostics {
            diagnostics.write_issued(!op.slices()[1].is_empty());
        }
        assert!(self.write.replace(op).is_none());
        self.event()
    }
    fn headers(&mut self, head: h2::Head<'_>) -> Option<()> {
        assert_eq!(
            head.kind,
            if self.server {
                h2::HeadKind::Request
            } else {
                h2::HeadKind::Response(200)
            }
        );
        let expected = if self.server {
            &REQUEST[..]
        } else {
            &RESPONSE[..]
        };
        assert_eq!(head.len(), expected.len());
        for (field, expected) in head.fields().zip(expected) {
            assert_eq!(field.name, expected.name);
            assert_eq!(field.value, expected.value);
        }
        assert_eq!(head.end_stream, self.receive_bytes == 0);
        self.slot(head.stream).headers += 1;
        if self.server {
            self.requests.push(head.stream);
        }
        self.event()
    }
    fn body(&mut self, op: h2::BodyOp) -> Option<()> {
        // Compare all bytes, not merely the length or a sampled byte.
        assert_eq!(op.bytes(), &PAYLOAD[..op.bytes().len()]);
        #[cfg(test)]
        self.overlap.response_delivered(op.bytes().len());
        if let Some(diagnostics) = &mut self.diagnostics {
            diagnostics.body_received(op.bytes().len());
        }
        self.slot(op.stream()).received += op.bytes().len();
        self.bodies.push(op);
        self.event();
        Some(())
    }
    fn send_ready(&mut self, permit: h2::SendPermit) -> Option<()> {
        if let Some(diagnostics) = &mut self.diagnostics {
            diagnostics.permit(
                permit.stream().get(),
                permit.max_bytes(),
                permit.max_retained_capacity(),
            );
        }
        self.permits.push(permit);
        self.event()
    }
    fn send_stopped(&mut self, stream: h2::StreamId, reason: h2::SendStop) -> Option<()> {
        if reason != h2::SendStop::Finished
            && let Some(diagnostics) = &mut self.diagnostics
        {
            diagnostics.failure(format_args!(
                "send_stopped stream={} reason={reason:?}",
                stream.get()
            ));
            return self.event();
        }
        assert_eq!(reason, h2::SendStop::Finished);
        self.event()
    }
    fn sent(&mut self, result: h2::Sent<Buffer>) -> Option<()> {
        if let Some(diagnostics) = &mut self.diagnostics {
            diagnostics.sent(result.accepted, result.result.is_ok());
        }
        if result.result.is_err()
            && let Some(diagnostics) = &mut self.diagnostics
        {
            diagnostics.failure(format_args!(
                "sent stream={} result={:?} accepted={} exact={} buffer_len={}",
                result.stream.get(),
                result.result,
                result.accepted,
                result.exact,
                result.buffer.len()
            ));
            self.slot(result.stream).returned += result.accepted;
            return self.event();
        }
        assert_eq!(result.result, Ok(()));
        assert!(result.exact);
        assert_eq!(result.accepted, result.buffer.len());
        self.slot(result.stream).returned += result.accepted;
        self.event()
    }
    fn ended(&mut self, end: h2::ReceiveEnd) -> Option<()> {
        if let Some(diagnostics) = &mut self.diagnostics {
            diagnostics.receive_end(end.outcome == h2::StreamOutcome::Complete);
        }
        if end.outcome != h2::StreamOutcome::Complete
            && let Some(diagnostics) = &mut self.diagnostics
        {
            diagnostics.failure(format_args!(
                "ended stream={} outcome={:?}",
                end.stream.get(),
                end.outcome
            ));
            self.slot(end.stream).ended = true;
            return self.event();
        }
        assert_eq!(end.outcome, h2::StreamOutcome::Complete);
        assert!(!self.slot(end.stream).ended);
        self.slot(end.stream).ended = true;
        self.event()
    }
    fn retired(&mut self, result: h2::StreamResult) -> Option<()> {
        if result.outcome != h2::StreamOutcome::Complete
            && let Some(diagnostics) = &mut self.diagnostics
        {
            diagnostics.failure(format_args!(
                "retired stream={} outcome={:?}",
                result.stream.get(),
                result.outcome
            ));
            self.slot(result.stream).retired = true;
            self.retired += 1;
            return self.event();
        }
        assert_eq!(result.outcome, h2::StreamOutcome::Complete);
        let receive_bytes = self.receive_bytes;
        let slot = self.slot(result.stream);
        assert!(!slot.retired && slot.ended);
        assert_eq!(slot.headers, 1);
        assert_eq!(slot.received, receive_bytes);
        assert_eq!(slot.sent, slot.returned);
        slot.retired = true;
        self.retired += 1;
        self.event()
    }
    fn cancel(&mut self, op: h2::CancelOp) -> Option<()> {
        self.cancels.push(op);
        self.event()
    }
    fn wake(&mut self, op: h2::WakeOp) -> Option<()> {
        self.alarms.push(op);
        self.event()
    }
    fn close(&mut self, op: h2::CloseOp) -> Option<()> {
        assert!(self.close.replace(op).is_none());
        self.event()
    }
    fn closed(&mut self, result: h2::ConnectionResult) -> Option<()> {
        if let Some(diagnostics) = &mut self.diagnostics {
            diagnostics.record(format_args!("closed result={result:?}"));
            if !matches!(
                result,
                h2::ConnectionResult::Graceful | h2::ConnectionResult::PeerClosed
            ) {
                diagnostics.failure(format_args!("closed result={result:?}"));
                self.closed = true;
                return self.event();
            }
        }
        assert!(
            matches!(
                result,
                h2::ConnectionResult::Graceful | h2::ConnectionResult::PeerClosed
            ),
            "{result:?}"
        );
        assert!(!self.closed);
        self.closed = true;
        self.event()
    }
    fn reschedule(&mut self) -> Option<()> {
        self.event();
        Some(())
    }
}

// The composite requires both child capabilities. No HTTP/1 callback is valid
// in this HTTP/2-only workload.
impl h1::Ports<Vec<u8>, Buffer> for Ports {
    type Output = ();
    fn read(&mut self, _: h1::ReadOp<Vec<u8>>) -> Option<()> {
        panic!("HTTP/1")
    }
    fn write(&mut self, _: h1::WriteOp<Buffer>) -> Option<()> {
        panic!("HTTP/1")
    }
    fn readiness(&mut self, _: h1::ReadinessOp) -> Option<()> {
        panic!("HTTP/1")
    }
    fn cancel(&mut self, _: h1::CancelOp) -> Option<()> {
        panic!("HTTP/1")
    }
    fn close(&mut self, _: h1::CloseOp) -> Option<()> {
        panic!("HTTP/1")
    }
    fn body(&mut self, _: h1::BodyOp<Vec<u8>>) -> Option<()> {
        panic!("HTTP/1")
    }
    fn trailers(&mut self, _: h1::ExchangeId, _: h1::Headers<'_>) -> Option<()> {
        panic!("HTTP/1")
    }
    fn incoming_finished(&mut self, _: h1::ExchangeId) -> Option<()> {
        panic!("HTTP/1")
    }
    fn send_ready(&mut self, _: h1::ExchangeId, _: usize) -> Option<()> {
        panic!("HTTP/1")
    }
    fn source_finished(&mut self, _: h1::ExchangeId) -> Option<()> {
        panic!("HTTP/1")
    }
    fn body_sent(&mut self, _: h1::BodySent<Buffer>) -> Option<()> {
        panic!("HTTP/1")
    }
    fn exchange_finished(&mut self, _: h1::ExchangeFinished) -> Option<()> {
        panic!("HTTP/1")
    }
    fn deadline_changed(&mut self, _: Option<h1::Deadline>) -> Option<()> {
        panic!("HTTP/1")
    }
    fn upgrade_ready(&mut self, _: h1::ExchangeId) -> Option<()> {
        panic!("HTTP/1")
    }
    fn closed(&mut self, _: h1::ConnectionResult) -> Option<()> {
        panic!("HTTP/1")
    }
}
impl h1::ClientPorts<Vec<u8>, Buffer> for Ports {
    fn response(&mut self, _: h1::ExchangeId, _: h1::ResponseHead<'_>, _: bool) -> Option<()> {
        panic!("HTTP/1")
    }
}
impl h1::ServerPorts<Vec<u8>, Buffer> for Ports {
    fn request(&mut self, _: h1::ExchangeId, _: h1::RequestHead<'_>) -> Option<()> {
        panic!("HTTP/1")
    }
}
impl http::ServerPorts<Buffer> for Ports {
    fn detection_closed(&mut self, result: http::DetectionClosed) -> Option<()> {
        panic!("{result:?}")
    }
}

trait Endpoint {
    fn core(&mut self) -> &mut h2::Connection<Buffer>;
    fn drive(&mut self, ports: &mut Ports);
    fn read(&mut self, completion: h2::ReadCompletion) {
        self.core().complete_read(completion).unwrap();
    }
    fn wake(&mut self, completion: h2::WakeCompletion) {
        self.core().complete_wake(completion).unwrap();
    }
    fn cancel(&mut self, completion: h2::CancelCompletion) {
        self.core().complete_cancel(completion).unwrap();
    }
}
impl Endpoint for h2::Client<Buffer> {
    fn core(&mut self) -> &mut h2::Connection<Buffer> {
        self
    }
    #[inline(never)]
    fn drive(&mut self, ports: &mut Ports) {
        self.next(ports);
    }
}
impl Endpoint for h2::Server<Buffer> {
    fn core(&mut self) -> &mut h2::Connection<Buffer> {
        self
    }
    #[inline(never)]
    fn drive(&mut self, ports: &mut Ports) {
        self.next(ports);
    }
}
impl Endpoint for http::Client<Buffer> {
    fn core(&mut self) -> &mut h2::Connection<Buffer> {
        self.http2_mut().unwrap()
    }
    #[inline(never)]
    fn drive(&mut self, ports: &mut Ports) {
        self.next(ports);
    }
}
impl Endpoint for http::Server<Buffer> {
    fn core(&mut self) -> &mut h2::Connection<Buffer> {
        self.http2_mut().unwrap()
    }
    #[inline(never)]
    fn drive(&mut self, ports: &mut Ports) {
        self.next(ports);
    }
    fn read(&mut self, completion: h2::ReadCompletion) {
        self.complete_read(completion).unwrap();
    }
    fn wake(&mut self, completion: h2::WakeCompletion) {
        self.complete_wake(completion).unwrap();
    }
    fn cancel(&mut self, completion: h2::CancelCompletion) {
        self.complete_cancel(completion).unwrap();
    }
}
trait Client: Endpoint {
    fn request(&mut self, end: bool) -> h2::StreamId;
}
impl Client for h2::Client<Buffer> {
    fn request(&mut self, end: bool) -> h2::StreamId {
        self.request_ref(&REQUEST, end).unwrap()
    }
}
impl Client for http::Client<Buffer> {
    fn request(&mut self, end: bool) -> h2::StreamId {
        self.http2_mut()
            .unwrap()
            .request_ref(&REQUEST, end)
            .unwrap()
    }
}
trait Server: Endpoint {
    fn respond(&mut self, stream: h2::StreamId, end: bool);
}
impl Server for h2::Server<Buffer> {
    fn respond(&mut self, stream: h2::StreamId, end: bool) {
        self.respond_ref(stream, &RESPONSE, end).unwrap();
    }
}
impl Server for http::Server<Buffer> {
    fn respond(&mut self, stream: h2::StreamId, end: bool) {
        self.http2_mut()
            .unwrap()
            .respond_ref(stream, &RESPONSE, end)
            .unwrap();
    }
}

fn settle(endpoint: &mut impl Endpoint, ports: &mut Ports, send_bytes: usize) {
    while let Some(cancel) = ports.cancels.pop() {
        if ports
            .read
            .as_ref()
            .is_some_and(|op| op.token() == cancel.original())
        {
            endpoint.read(
                ports
                    .read
                    .take()
                    .unwrap()
                    .complete(h2::ReadOutcome::Failed(h2::IoFailure::Cancelled)),
            );
        } else if ports
            .write
            .as_ref()
            .is_some_and(|op| op.token() == cancel.original())
        {
            endpoint
                .core()
                .complete_write(
                    ports
                        .write
                        .take()
                        .unwrap()
                        .complete(h2::WriteOutcome::Failed {
                            progress: h2::Progress::Exact(0),
                            error: h2::IoFailure::Cancelled,
                        }),
                )
                .unwrap();
        } else if let Some(index) = ports
            .alarms
            .iter()
            .position(|op| op.token() == cancel.original())
        {
            endpoint.wake(ports.alarms.swap_remove(index).complete(Duration::ZERO));
        } else {
            panic!("cancel without original");
        }
        endpoint.cancel(cancel.complete());
    }
    if let Some(close) = ports.close.take() {
        endpoint
            .core()
            .complete_close(close.complete(Ok(())))
            .unwrap();
    }
    let sibling_done = ports.slots.last().unwrap().retired;
    let mut index = 0;
    while index < ports.bodies.len() {
        let stream = ports.bodies[index].stream();
        let slot = stream.get() as usize / 2 % ports.slots.len();
        if ports.paused && !sibling_done && slot + 1 != ports.slots.len() {
            index += 1;
        } else {
            let bytes = ports.bodies[index].bytes().len();
            endpoint
                .core()
                .release_body(ports.bodies.swap_remove(index).release())
                .unwrap();
            if let Some(diagnostics) = &mut ports.diagnostics {
                diagnostics.body_released(bytes);
            }
        }
    }
    while let Some(permit) = ports.permits.pop() {
        let stream = permit.stream();
        let slot = ports.slot(permit.stream());
        let count = (send_bytes - slot.sent)
            .min(PAYLOAD.len())
            .min(permit.max_bytes());
        assert!(count > 0);
        slot.sent += count;
        let end = slot.sent == send_bytes;
        endpoint
            .core()
            .send(permit, &PAYLOAD[..count], end)
            .unwrap();
        if let Some(diagnostics) = &mut ports.diagnostics {
            diagnostics.admission(stream.get(), count, end);
        }
    }
}

fn transfer(
    from: &mut impl Endpoint,
    outgoing: &mut Ports,
    to: &mut impl Endpoint,
    incoming: &mut Ports,
    fragment: usize,
) -> bool {
    if outgoing.write.is_none() || incoming.read.is_none() {
        return false;
    }
    let write = outgoing.write.take().unwrap();
    let mut read = incoming.read.take().unwrap();
    let length = write.remaining().min(read.buffer_mut().len()).min(fragment);
    assert!(length > 0);
    let mut cursor = 0;
    #[cfg(test)]
    let payload_accepted = length.saturating_sub(write.slices()[0].len());
    for slice in write.slices() {
        let count = slice.len().min(length - cursor);
        read.buffer_mut()[cursor..cursor + count].copy_from_slice(&slice[..count]);
        if let Some(diagnostics) = &mut outgoing.diagnostics {
            diagnostics.wire(&slice[..count]);
        }
        cursor += count;
    }
    assert_eq!(cursor, length);
    outgoing.wire_bytes += length;
    from.core()
        .complete_write(write.complete(h2::WriteOutcome::Written(length)))
        .unwrap();
    #[cfg(test)]
    outgoing.overlap.transport_accepted(payload_accepted);
    to.read(read.complete(h2::ReadOutcome::Read(length)));
    true
}

struct Pair<C, S> {
    client: C,
    server: S,
    cp: Ports,
    sp: Ports,
    case: Case,
    fragment: usize,
}
impl<C: Client, S: Server> Pair<C, S> {
    fn tick(&mut self) -> bool {
        for diagnostics in [&mut self.cp.diagnostics, &mut self.sp.diagnostics]
            .into_iter()
            .flatten()
        {
            diagnostics.turn += 1;
        }
        let before = self.cp.events + self.sp.events;
        self.client.drive(&mut self.cp);
        self.server.drive(&mut self.sp);
        settle(&mut self.client, &mut self.cp, self.case.request);
        settle(&mut self.server, &mut self.sp, self.case.response);
        while let Some(stream) = self.sp.requests.pop() {
            self.server.respond(stream, self.case.response == 0);
            if self.sp.diagnostics.is_some() {
                let slot = self.sp.slot(stream);
                let incoming_ended = slot.ended;
                let received = slot.received;
                self.sp.diagnostics.as_mut().unwrap().head_command(
                    stream.get(),
                    self.case.response == 0,
                    incoming_ended,
                    received,
                );
            }
        }
        let forward = transfer(
            &mut self.client,
            &mut self.cp,
            &mut self.server,
            &mut self.sp,
            self.fragment,
        );
        let reverse = transfer(
            &mut self.server,
            &mut self.sp,
            &mut self.client,
            &mut self.cp,
            self.fragment,
        );
        forward || reverse || self.cp.events + self.sp.events != before
    }
    fn batch(&mut self) {
        self.cp.begin();
        self.sp.begin();
        let goal = self.cp.retired + self.cp.slots.len();
        for _ in 0..self.cp.slots.len() {
            let stream = self.client.request(self.case.request == 0);
            self.cp.slot(stream);
        }
        for _ in 0..20_000_000 {
            let progress = self.tick();
            if self.cp.diagnostics.as_ref().is_some_and(|d| d.failed)
                || self.sp.diagnostics.as_ref().is_some_and(|d| d.failed)
            {
                self.diagnostic_failure();
            }
            if self.cp.retired == goal && self.sp.retired == goal {
                #[cfg(test)]
                self.cp.overlap.complete_batch();
                return;
            }
            assert!(
                progress,
                "protocol/flow stall: client retired={} server retired={} goal={goal} bodies={}/{} read={}/{} write={}/{}",
                self.cp.retired,
                self.sp.retired,
                self.cp.bodies.len(),
                self.sp.bodies.len(),
                self.cp.read.is_some(),
                self.sp.read.is_some(),
                self.cp.write.is_some(),
                self.sp.write.is_some()
            );
        }
        panic!("bounded executor exhausted");
    }
    fn diagnostic_failure(&mut self) -> ! {
        for _ in 0..10000 {
            if !self.tick() {
                break;
            }
        }
        for (name, ports) in [("client", &self.cp), ("server", &self.sp)] {
            if let Some(diagnostics) = &ports.diagnostics {
                diagnostics.dump(name);
            }
            eprintln!(
                "{name} closed={} read={} write={} alarms={} cancels={} bodies={} permits={}",
                ports.closed,
                ports.read.is_some(),
                ports.write.is_some(),
                ports.alarms.len(),
                ports.cancels.len(),
                ports.bodies.len(),
                ports.permits.len()
            );
            for slot in &ports.slots {
                eprintln!(
                    "{name} stream={:?} sent={} returned={} received={} ended={} retired={}",
                    slot.stream, slot.sent, slot.returned, slot.received, slot.ended, slot.retired
                );
            }
            assert!(
                ports.closed
                    && ports.read.is_none()
                    && ports.write.is_none()
                    && ports.close.is_none()
                    && ports.alarms.is_empty()
                    && ports.cancels.is_empty()
                    && ports.bodies.is_empty()
                    && ports.permits.is_empty(),
                "diagnostic endpoint did not settle all owned operations"
            );
        }
        panic!("strict diagnostic failure after bounded original-operation settlement");
    }
    fn finish(&mut self) {
        for _ in 0..10000 {
            if !self.tick() {
                break;
            }
        }
        assert!(self.cp.write.is_none() && self.sp.write.is_none());
        self.client.read(
            self.cp
                .read
                .take()
                .expect("idle client read")
                .complete(h2::ReadOutcome::Eof),
        );
        self.server.read(
            self.sp
                .read
                .take()
                .expect("idle server read")
                .complete(h2::ReadOutcome::Eof),
        );
        for _ in 0..10000 {
            self.tick();
            if self.cp.closed && self.sp.closed {
                assert!(self.cp.read.is_none() && self.sp.read.is_none());
                assert!(self.cp.write.is_none() && self.sp.write.is_none());
                assert!(self.cp.alarms.is_empty() && self.sp.alarms.is_empty());
                assert!(self.cp.cancels.is_empty() && self.sp.cancels.is_empty());
                #[cfg(test)]
                self.cp.overlap.assert_complete();
                return;
            }
        }
        panic!("shutdown did not settle");
    }
}

fn config() -> h2::Config {
    h2::Config {
        http: h2::HttpLimits::new().set_max_active_streams(128),
        ..h2::Config::default()
    }
}

fn execute<C: Client, S: Server>(
    make: impl Fn() -> (C, S),
    case: Case,
    concurrency: usize,
    fragment: usize,
    batches: usize,
    phase: &str,
) -> serde_json::Value {
    let pair = || {
        let (client, server) = make();
        Pair {
            client,
            server,
            cp: Ports::new(false, concurrency, case),
            sp: Ports::new(true, concurrency, case),
            case,
            fragment,
        }
    };
    let (elapsed, exchanges, wire_bytes) = if phase == "construct" {
        assert_eq!(concurrency, 1);
        let mut warm = pair();
        warm.batch();
        warm.finish();
        let mut elapsed = Duration::ZERO;
        let mut wire_bytes = 0;
        for _ in 0..batches {
            let start = Instant::now();
            let mut p = pair();
            p.batch();
            elapsed += start.elapsed();
            wire_bytes += p.cp.wire_bytes + p.sp.wire_bytes;
            p.finish();
        }
        (elapsed, batches, wire_bytes)
    } else {
        assert_eq!(phase, "steady");
        let mut p = pair();
        for _ in 0..8 {
            p.batch();
        }
        let before = p.cp.wire_bytes + p.sp.wire_bytes;
        let start = Instant::now();
        for _ in 0..batches {
            p.batch();
        }
        let elapsed = start.elapsed();
        let bytes = p.cp.wire_bytes + p.sp.wire_bytes - before;
        assert_eq!(p.cp.retired, (batches + 8) * concurrency);
        assert_eq!(p.sp.retired, p.cp.retired);
        p.finish();
        (elapsed, batches * concurrency, bytes)
    };
    serde_json::json!({
        "schema": 1, "case": case.name, "phase": phase, "concurrency": concurrency,
        "fragment": fragment, "batches": batches, "exchanges": exchanges,
        "request_bytes": case.request, "response_bytes": case.response,
        "elapsed_ns": elapsed.as_nanos() as u64,
        "ns_per_exchange": elapsed.as_nanos() as f64 / exchanges as f64,
        "wire_bytes": wire_bytes, "payload_bytes": exchanges * (case.request + case.response),
        "warmup_batches": if phase == "steady" { 8 } else { 1 },
        "payload_storage": "static", "transport": "direct-slices-to-read-page",
        "all_payload_bytes_compared": true,
    })
}

pub fn run(
    mode: &str,
    case: Case,
    concurrency: usize,
    fragment: usize,
    batches: usize,
    phase: &str,
) -> serde_json::Value {
    let mut result = match mode {
        "direct" => execute(
            || {
                (
                    h2::Client::new(config(), Duration::ZERO).unwrap(),
                    h2::Server::new(config(), Duration::ZERO).unwrap(),
                )
            },
            case,
            concurrency,
            fragment,
            batches,
            phase,
        ),
        "selected" | "auto" => execute(
            || {
                let client =
                    http::Client::http2(h2::Client::new(config(), Duration::ZERO).unwrap());
                let server = if mode == "selected" {
                    http::Server::http2(h2::Server::new(config(), Duration::ZERO).unwrap())
                } else {
                    http::Server::detect(
                        http::DetectionConfig {
                            http1_connection: h1::ConnectionId {
                                slot: 1,
                                generation: 1,
                            },
                            http1_config: h1::Config::default(),
                            http1_buffer: vec![0; 65536],
                            http2_config: config(),
                            timeout: Duration::from_secs(1),
                        },
                        Duration::ZERO,
                    )
                    .unwrap()
                };
                (client, server)
            },
            case,
            concurrency,
            fragment,
            batches,
            phase,
        ),
        _ => panic!("unknown mode: {mode}"),
    };
    result["mode"] = mode.into();
    result
}
