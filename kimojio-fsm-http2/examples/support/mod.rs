#![allow(dead_code)]

use kimojio_fsm_http2::*;
use std::{collections::VecDeque, time::Duration};

/// This executor keeps I/O outstanding while application callbacks continue.
#[derive(Default)]
pub struct MemoryPorts {
    pub read: Option<ReadOp>,
    pub write: Option<WriteOp>,
    pub heads: Vec<(StreamId, HeadKind, Vec<H2HeaderField>, bool)>,
    pub bodies: VecDeque<BodyOp>,
    pub permits: VecDeque<SendPermit>,
    pub sent: Vec<Sent>,
    pub ends: Vec<ReceiveEnd>,
    pub stopped: Vec<(StreamId, SendStop)>,
    pub retired: Vec<StreamResult>,
    pub alarms: Vec<WakeOp>,
    pub cancels: Vec<CancelOp>,
    pub close: Option<CloseOp>,
    pub closed: Vec<ConnectionResult>,
    pub sequence: Vec<&'static str>,
}

impl Ports<Vec<u8>> for MemoryPorts {
    type Output = ();
    fn read(&mut self, op: ReadOp) -> Option<()> {
        assert!(self.read.replace(op).is_none());
        self.sequence.push("read");
        None
    }
    fn write(&mut self, op: WriteOp) -> Option<()> {
        assert!(self.write.replace(op).is_none());
        self.sequence.push("write");
        None
    }
    fn headers(&mut self, head: Head<'_>) -> Option<()> {
        self.heads.push((
            head.stream,
            head.kind,
            head.fields().map(H2RawHeaderRef::to_owned).collect(),
            head.end_stream,
        ));
        self.sequence.push("headers");
        None
    }
    fn body(&mut self, op: BodyOp) -> Option<()> {
        self.bodies.push_back(op);
        self.sequence.push("body");
        None
    }
    fn send_ready(&mut self, permit: SendPermit) -> Option<()> {
        self.permits.push_back(permit);
        self.sequence.push("send_ready");
        None
    }
    fn send_stopped(&mut self, stream: StreamId, reason: SendStop) -> Option<()> {
        self.stopped.push((stream, reason));
        self.sequence.push("send_stopped");
        None
    }
    fn sent(&mut self, result: Sent) -> Option<()> {
        self.sent.push(result);
        self.sequence.push("sent");
        None
    }
    fn ended(&mut self, end: ReceiveEnd) -> Option<()> {
        self.ends.push(end);
        self.sequence.push("ended");
        None
    }
    fn retired(&mut self, result: StreamResult) -> Option<()> {
        self.retired.push(result);
        self.sequence.push("retired");
        None
    }
    fn cancel(&mut self, op: CancelOp) -> Option<()> {
        self.cancels.push(op);
        self.sequence.push("cancel");
        None
    }
    fn wake(&mut self, op: WakeOp) -> Option<()> {
        self.alarms.push(op);
        self.sequence.push("wake");
        None
    }
    fn close(&mut self, op: CloseOp) -> Option<()> {
        assert!(self.close.replace(op).is_none());
        self.sequence.push("close");
        None
    }
    fn closed(&mut self, result: ConnectionResult) -> Option<()> {
        self.closed.push(result);
        self.sequence.push("closed");
        None
    }
    fn reschedule(&mut self) -> Option<()> {
        self.sequence.push("reschedule");
        Some(())
    }
}

pub fn step(
    connection: &mut Connection,
    ports: &mut MemoryPorts,
    incoming: &mut VecDeque<u8>,
    outgoing: &mut VecDeque<u8>,
    fragment: usize,
) -> bool {
    let before = ports.sequence.len();
    connection.next(ports);
    let mut progress = ports.sequence.len() != before;
    if let Some(op) = ports.write.take() {
        let bytes: Vec<_> = op
            .slices()
            .iter()
            .flat_map(|part| part.iter().copied())
            .take(fragment)
            .collect();
        assert!(!bytes.is_empty());
        let len = bytes.len();
        outgoing.extend(bytes);
        connection
            .complete_write(op.complete(WriteOutcome::Written(len)))
            .unwrap();
        progress = true;
    }
    if !incoming.is_empty()
        && let Some(mut op) = ports.read.take()
    {
        let len = incoming.len().min(op.buffer_mut().len()).min(fragment);
        for target in &mut op.buffer_mut()[..len] {
            *target = incoming.pop_front().unwrap();
        }
        connection
            .complete_read(op.complete(ReadOutcome::Read(len)))
            .unwrap();
        progress = true;
    }
    for cancel in std::mem::take(&mut ports.cancels) {
        if ports
            .read
            .as_ref()
            .is_some_and(|op| op.token() == cancel.original())
        {
            connection
                .complete_read(
                    ports
                        .read
                        .take()
                        .unwrap()
                        .complete(ReadOutcome::Failed(IoFailure::Cancelled)),
                )
                .unwrap();
        }
        if let Some(index) = ports
            .alarms
            .iter()
            .position(|op| op.token() == cancel.original())
        {
            let alarm = ports.alarms.remove(index);
            connection
                .complete_wake(alarm.complete(Duration::ZERO))
                .unwrap();
        }
        connection.complete_cancel(cancel.complete()).unwrap();
        progress = true;
    }
    if let Some(op) = ports.close.take() {
        connection.complete_close(op.complete(Ok(()))).unwrap();
        progress = true;
    }
    progress
}

pub struct Pair {
    pub client: Client,
    pub server: Server,
    pub client_ports: MemoryPorts,
    pub server_ports: MemoryPorts,
    pub to_client: VecDeque<u8>,
    pub to_server: VecDeque<u8>,
}
impl Pair {
    pub fn new(config: Config) -> Self {
        Self {
            client: Client::new(config.clone(), Duration::ZERO).unwrap(),
            server: Server::new(config, Duration::ZERO).unwrap(),
            client_ports: MemoryPorts::default(),
            server_ports: MemoryPorts::default(),
            to_client: VecDeque::new(),
            to_server: VecDeque::new(),
        }
    }
    pub fn pump(&mut self, fragment: usize) {
        for _ in 0..200_000 {
            let client = step(
                &mut self.client,
                &mut self.client_ports,
                &mut self.to_client,
                &mut self.to_server,
                fragment,
            );
            let server = step(
                &mut self.server,
                &mut self.server_ports,
                &mut self.to_server,
                &mut self.to_client,
                fragment,
            );
            if !client && !server {
                return;
            }
        }
        panic!("in-memory transport did not become idle");
    }
    pub fn release_all(&mut self) {
        while let Some(body) = self.client_ports.bodies.pop_front() {
            self.client.release_body(body.release()).unwrap();
        }
        while let Some(body) = self.server_ports.bodies.pop_front() {
            self.server.release_body(body.release()).unwrap();
        }
    }
}

pub fn request(method: &[u8]) -> Vec<H2HeaderField> {
    vec![
        H2HeaderField::new(b":method", method),
        H2HeaderField::new(b":scheme", b"https"),
        H2HeaderField::new(b":authority", b"example.test"),
        H2HeaderField::new(b":path", b"/"),
    ]
}

pub fn response(status: &[u8]) -> Vec<H2HeaderField> {
    vec![H2HeaderField::new(b":status", status)]
}
