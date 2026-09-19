use kimojio_fsm_http1 as http;
use kimojio_fsm_websocket::*;
use std::rc::Rc;

type Storage = Rc<[u8]>;
type Machine = Server<Vec<u8>, Storage>;

#[derive(Debug)]
enum Event {
    Read(ReadOp<Vec<u8>>),
    Write(WriteOp<Storage>),
    Ready(ReadinessOp),
    Cancel(CancelOp),
    Close(CloseOp),
    Start(MessageInfo),
    Chunk(ChunkOp<Vec<u8>>),
    Finish(MessageInfo),
    Sent(MessageSent<Storage>),
    Peer(CloseReason),
    Deadline(Option<Deadline>),
    Closed(ConnectionResult),
}
struct PortsImpl;
impl Ports<Vec<u8>, Storage> for PortsImpl {
    type Output = Event;
    fn read(&mut self, op: ReadOp<Vec<u8>>) -> Option<Event> {
        Some(Event::Read(op))
    }
    fn write(&mut self, op: WriteOp<Storage>) -> Option<Event> {
        Some(Event::Write(op))
    }
    fn readiness(&mut self, op: ReadinessOp) -> Option<Event> {
        Some(Event::Ready(op))
    }
    fn cancel(&mut self, op: CancelOp) -> Option<Event> {
        Some(Event::Cancel(op))
    }
    fn close(&mut self, op: CloseOp) -> Option<Event> {
        Some(Event::Close(op))
    }
    fn message_started(&mut self, m: MessageInfo) -> Option<Event> {
        Some(Event::Start(m))
    }
    fn chunk(&mut self, op: ChunkOp<Vec<u8>>) -> Option<Event> {
        Some(Event::Chunk(op))
    }
    fn message_finished(&mut self, m: MessageInfo) -> Option<Event> {
        Some(Event::Finish(m))
    }
    fn message_sent(&mut self, r: MessageSent<Storage>) -> Option<Event> {
        Some(Event::Sent(r))
    }
    fn peer_closed(&mut self, r: CloseReason) -> Option<Event> {
        Some(Event::Peer(r))
    }
    fn deadline_changed(&mut self, d: Option<Deadline>) -> Option<Event> {
        Some(Event::Deadline(d))
    }
    fn closed(&mut self, r: ConnectionResult) -> Option<Event> {
        Some(Event::Closed(r))
    }
}

fn machine(config: Config, capacity: usize) -> Machine {
    Server::new(
        ConnectionId {
            slot: 1,
            generation: 1,
        },
        config,
        vec![0; capacity],
        Tick(0),
    )
    .unwrap()
}

fn masked(opcode: u8, fin: bool, bytes: &[u8]) -> Vec<u8> {
    let mut frame = vec![opcode | if fin { 128 } else { 0 }];
    match bytes.len() {
        0..=125 => frame.push(128 | bytes.len() as u8),
        126..=65535 => {
            frame.push(128 | 126);
            frame.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
        }
        _ => {
            frame.push(128 | 127);
            frame.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
        }
    }
    let mask = [0x37, 0xfa, 0x21, 0x3d];
    frame.extend_from_slice(&mask);
    frame.extend(bytes.iter().enumerate().map(|(i, b)| b ^ mask[i & 3]));
    frame
}

struct Harness {
    machine: Machine,
    read: Option<ReadOp<Vec<u8>>>,
    write: Option<WriteOp<Storage>>,
    hold_writes: bool,
    wire: Vec<u8>,
    messages: Vec<(MessageKind, Vec<u8>)>,
    current: Option<(MessageKind, Vec<u8>)>,
    sent: Vec<MessageSent<Storage>>,
    peer: Option<CloseReason>,
    closed: Option<ConnectionResult>,
    deadline: Option<Deadline>,
    write_step: usize,
}
impl Harness {
    fn new(config: Config, capacity: usize) -> Self {
        Self {
            machine: machine(config, capacity),
            read: None,
            write: None,
            hold_writes: false,
            wire: vec![],
            messages: vec![],
            current: None,
            sent: vec![],
            peer: None,
            closed: None,
            deadline: None,
            write_step: usize::MAX,
        }
    }
    fn pump(&mut self) {
        for _ in 0..100_000 {
            let Some(event) = self.machine.next(&mut PortsImpl) else {
                return;
            };
            match event {
                Event::Read(op) => {
                    assert!(self.read.replace(op).is_none());
                }
                Event::Write(op) if self.hold_writes => {
                    assert!(self.write.replace(op).is_none());
                }
                Event::Write(op) => {
                    let bytes: Vec<u8> = op.slices().concat();
                    let count = self.write_step.min(bytes.len());
                    self.wire.extend_from_slice(&bytes[..count]);
                    self.machine.complete_write(op.complete(Ok(count))).unwrap();
                }
                Event::Ready(op) => self
                    .machine
                    .complete_readiness(op.complete(Ok(())))
                    .unwrap(),
                Event::Cancel(op) => {
                    let error = Err(IoError {
                        kind: IoErrorKind::Cancelled,
                        code: None,
                    });
                    if self.read.as_ref().is_some_and(|r| r.id() == op.target) {
                        self.machine
                            .complete_read(self.read.take().unwrap().complete(error))
                            .unwrap();
                    } else if self.write.as_ref().is_some_and(|w| w.id() == op.target) {
                        self.machine
                            .complete_write(self.write.take().unwrap().complete(error))
                            .unwrap();
                    } else {
                        panic!("unknown cancellation {op:?}");
                    }
                }
                Event::Close(op) => self.machine.complete_close(op.complete(Ok(()))).unwrap(),
                Event::Start(info) => {
                    assert_eq!(info.length, 0);
                    self.current = Some((info.kind, vec![]));
                }
                Event::Chunk(op) => {
                    self.current
                        .as_mut()
                        .unwrap()
                        .1
                        .extend_from_slice(op.bytes());
                    self.machine.release_chunk(op.release()).unwrap();
                }
                Event::Finish(info) => {
                    let message = self.current.take().unwrap();
                    assert_eq!(message.1.len() as u64, info.length);
                    self.messages.push(message);
                }
                Event::Sent(receipt) => self.sent.push(receipt),
                Event::Peer(reason) => self.peer = Some(reason),
                Event::Deadline(deadline) => self.deadline = deadline,
                Event::Closed(result) => self.closed = Some(result),
            }
        }
        panic!("non-quiescent machine");
    }
    fn feed(&mut self, mut bytes: &[u8], step: usize) {
        self.pump();
        while !bytes.is_empty() {
            let Some(mut op) = self.read.take() else {
                return;
            };
            let count = bytes.len().min(op.bytes_mut().len()).min(step);
            op.bytes_mut()[..count].copy_from_slice(&bytes[..count]);
            self.machine.complete_read(op.complete(Ok(count))).unwrap();
            bytes = &bytes[count..];
            self.pump();
        }
    }
}

#[test]
fn rfc_handshake_vector() {
    let headers = [
        http::Header {
            name: "host",
            value: b"server.example.com",
        },
        http::Header {
            name: "upgrade",
            value: b"websocket",
        },
        http::Header {
            name: "connection",
            value: b"keep-alive, Upgrade",
        },
        http::Header {
            name: "sec-websocket-key",
            value: b"dGhlIHNhbXBsZSBub25jZQ==",
        },
        http::Header {
            name: "sec-websocket-version",
            value: b"13",
        },
    ];
    let handshake = Handshake::validate(http::RequestHead {
        method: "GET",
        target: "/chat",
        version: http::Version::Http11,
        headers: &headers,
    })
    .unwrap();
    assert_eq!(handshake.accept_value(), b"s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
}

#[test]
fn bytewise_text_and_binary() {
    let mut h = Harness::new(Config::default(), 32);
    h.feed(&masked(1, true, "hello ☃".as_bytes()), 1);
    h.feed(&masked(2, true, &[0, 255, 128]), 1);
    assert_eq!(
        h.messages,
        [
            (MessageKind::Text, "hello ☃".as_bytes().to_vec()),
            (MessageKind::Binary, vec![0, 255, 128])
        ]
    );
}

#[test]
fn immutable_output_returns_same_storage_after_short_writes() {
    let mut h = Harness::new(Config::default(), 32);
    h.write_step = 1;
    let payload: Storage = Rc::from(b"hello".as_slice());
    h.machine
        .send_message(SendMessage {
            kind: MessageKind::Text,
            buffer: payload.clone(),
            range: 0..5,
        })
        .unwrap();
    h.pump();
    assert_eq!(h.wire, b"\x81\x05hello");
    assert!(Rc::ptr_eq(&h.sent[0].buffer, &payload));
    assert_eq!(h.sent[0].accepted, 5);
    assert_eq!(h.sent[0].result, Ok(()));
    assert!(h.machine.can_send());
}

#[test]
fn fragmented_utf8_all_splits_with_control_interruptions() {
    let mut stream = masked(1, false, b"a\xf0");
    stream.extend(masked(9, true, b"ping"));
    stream.extend(masked(0, false, b"\x9f"));
    stream.extend(masked(10, true, b"unsolicited"));
    stream.extend(masked(0, true, b"\x8c\x8db"));
    for split in 0..=stream.len() {
        let mut h = Harness::new(Config::default(), 256);
        h.feed(&stream[..split], usize::MAX);
        h.feed(&stream[split..], usize::MAX);
        assert_eq!(
            h.messages,
            [(MessageKind::Text, "a🌍b".as_bytes().to_vec())],
            "split {split}"
        );
        assert_eq!(h.wire, b"\x8a\x04ping", "split {split}");
    }
}

#[test]
fn payload_lengths_and_masks_across_receive_buffers() {
    for length in [0, 1, 125, 126, 127, 65535, 65536] {
        let bytes: Vec<u8> = (0..length).map(|n| n as u8).collect();
        for step in [1, 7, 1024] {
            let mut h = Harness::new(Config::default(), 1024);
            h.feed(&masked(2, true, &bytes), step);
            assert_eq!(h.messages, [(MessageKind::Binary, bytes.clone())]);
        }
    }
}

#[test]
fn invalid_headers_close_with_1002_without_waiting_for_payload() {
    let cases = [
        vec![0x81, 0],                        // unmasked empty
        vec![0xc1, 0x80],                     // reserved bit
        vec![0x83, 0x80],                     // reserved opcode
        vec![0x09, 0x80],                     // fragmented ping
        vec![0x89, 0xfe],                     // overlong control
        vec![0x82, 0xfe, 0, 125, 0, 0, 0, 0], // nonminimal length
        vec![0x82, 0xff, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 0, 0, 0, 0],
        vec![0x82, 0xff, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        masked(0, true, b"orphan"),
    ];
    for bytes in cases {
        for step in [1, usize::MAX] {
            let mut h = Harness::new(Config::default(), 128);
            h.feed(&bytes, step);
            assert_eq!(h.wire, [0x88, 2, 3, 0xea], "bytes {bytes:?}");
            assert_eq!(h.closed.unwrap().result, Err(Failure::Protocol));
            assert!(h.messages.is_empty());
        }
    }
}

#[test]
fn interleaved_data_messages_are_rejected() {
    let mut bytes = masked(1, false, b"one");
    bytes.extend(masked(2, true, b"two"));
    let mut h = Harness::new(Config::default(), 128);
    h.feed(&bytes, usize::MAX);
    assert_eq!(h.closed.unwrap().result, Err(Failure::Protocol));
    assert!(h.messages.is_empty());
}

#[test]
fn invalid_utf8_never_completes_a_message() {
    for bytes in [
        b"\xc0\x80".as_slice(),
        b"\xed\xa0\x80",
        b"\xf4\x90\x80\x80",
        b"\xf5\x80\x80\x80",
        b"\x80",
        b"\xe2\x82",
        b"\xe0\x80\x80",
        b"\xf0\x80\x80\x80",
    ] {
        let mut h = Harness::new(Config::default(), 32);
        h.feed(&masked(1, true, bytes), 1);
        assert!(h.messages.is_empty(), "{bytes:?}");
        assert_eq!(h.closed.unwrap().result, Err(Failure::InvalidUtf8));
        assert_eq!(h.wire, [0x88, 2, 3, 0xef]);
    }
}

#[test]
fn empty_fragments_and_messages_preserve_boundaries() {
    let mut bytes = masked(1, true, b"");
    bytes.extend(masked(2, false, b""));
    bytes.extend(masked(0, false, b""));
    bytes.extend(masked(0, true, b""));
    let mut h = Harness::new(Config::default(), 128);
    h.feed(&bytes, usize::MAX);
    assert_eq!(
        h.messages,
        [(MessageKind::Text, vec![]), (MessageKind::Binary, vec![])]
    );
}

#[test]
fn frame_message_and_fragment_limits() {
    let cases = [
        (
            Config {
                max_message_bytes: 3,
                ..Config::default()
            },
            masked(2, true, b"1234"),
        ),
        (
            Config {
                max_frame_bytes: 3,
                outgoing_frame_bytes: 3,
                ..Config::default()
            },
            masked(2, true, b"1234"),
        ),
        (
            Config {
                max_fragments: 2,
                ..Config::default()
            },
            [
                masked(2, false, b""),
                masked(0, false, b""),
                masked(0, true, b""),
            ]
            .concat(),
        ),
        (
            Config {
                max_message_bytes: 3,
                ..Config::default()
            },
            [masked(2, false, b"12"), masked(0, true, b"34")].concat(),
        ),
    ];
    for (config, bytes) in cases {
        let mut h = Harness::new(config, 128);
        h.feed(&bytes, 1);
        assert!(h.messages.is_empty());
        assert_eq!(h.closed.unwrap().result, Err(Failure::Limit));
        assert_eq!(h.wire, [0x88, 2, 3, 0xf1]);
    }
}

#[test]
fn close_validation_and_code_ranges() {
    for code in [0u16, 999, 1004, 1005, 1006, 1015, 1016, 2999, 5000, 65535] {
        let mut h = Harness::new(Config::default(), 128);
        h.feed(&masked(8, true, &code.to_be_bytes()), 1);
        assert_eq!(
            h.closed.unwrap().result,
            Err(Failure::Protocol),
            "code {code}"
        );
    }
    for payload in [vec![1], vec![3, 0xe8, 0xff]] {
        let mut h = Harness::new(Config::default(), 128);
        h.feed(&masked(8, true, &payload), 1);
        assert!(h.closed.unwrap().result.is_err());
    }
    for code in [
        1000u16, 1001, 1002, 1003, 1007, 1008, 1009, 1010, 1011, 1012, 1013, 1014, 3000, 3999,
        4000, 4999,
    ] {
        let mut h = Harness::new(Config::default(), 128);
        let mut payload = code.to_be_bytes().to_vec();
        payload.extend_from_slice("理由".as_bytes());
        h.feed(&masked(8, true, &payload), 1);
        assert_eq!(h.peer.unwrap().code(), Some(code));
        assert_eq!(h.peer.unwrap().reason(), "理由");
        assert!(h.closed.unwrap().clean, "{code}");
        let response = if code == 1010 {
            vec![0x88, 2, 3, 0xe8]
        } else {
            [vec![0x88, payload.len() as u8], payload].concat()
        };
        assert_eq!(h.wire, response);
    }
}

#[test]
fn empty_close_and_eof_have_distinct_results() {
    let mut h = Harness::new(Config::default(), 128);
    h.feed(&masked(8, true, b""), 1);
    assert_eq!(h.wire, [0x88, 0]);
    assert_eq!(h.peer.unwrap().code(), None);
    assert!(h.closed.unwrap().clean);
    let mut h = Harness::new(Config::default(), 128);
    h.pump();
    h.machine
        .complete_read(h.read.take().unwrap().complete(Ok(0)))
        .unwrap();
    h.pump();
    assert_eq!(h.closed.unwrap().result, Err(Failure::UnexpectedEof));
    assert_eq!(h.closed.unwrap().peer_close, None);
    assert!(h.wire.is_empty());
}

#[test]
fn locally_initiated_close_waits_and_handles_ping() {
    let mut h = Harness::new(Config::default(), 128);
    h.machine
        .close(CloseReason::new(1000, "done").unwrap())
        .unwrap();
    h.pump();
    assert!(h.closed.is_none());
    h.feed(&masked(9, true, b"?"), 1);
    assert_eq!(h.wire, b"\x88\x06\x03\xe8done\x8a\x01?");
    h.feed(&masked(8, true, &[3, 0xe8]), 1);
    assert!(h.closed.unwrap().clean);
}

#[test]
fn close_discards_coalesced_data_and_does_not_reply_to_late_ping() {
    let mut h = Harness::new(Config::default(), 128);
    let bytes = [
        masked(8, true, &[3, 0xe8]),
        masked(1, true, b"forbidden"),
        masked(9, true, b"?"),
    ]
    .concat();
    h.feed(&bytes, usize::MAX);
    assert!(h.messages.is_empty());
    assert_eq!(h.wire, [0x88, 2, 3, 0xe8]);
}

#[test]
fn outgoing_fragmentation_and_empty_messages() {
    let mut h = Harness::new(
        Config {
            outgoing_frame_bytes: 2,
            ..Config::default()
        },
        128,
    );
    h.write_step = 1;
    h.machine
        .send_message(SendMessage {
            kind: MessageKind::Text,
            buffer: Rc::from(b"hello".as_slice()),
            range: 0..5,
        })
        .unwrap();
    h.pump();
    assert_eq!(h.wire, b"\x01\x02he\x00\x02ll\x80\x01o");
    assert_eq!(h.sent[0].accepted, 5);
    h.wire.clear();
    h.machine
        .send_message(SendMessage {
            kind: MessageKind::Binary,
            buffer: Rc::from([]),
            range: 0..0,
        })
        .unwrap();
    h.pump();
    assert_eq!(h.wire, b"\x82\x00");
    assert_eq!(h.sent[1].accepted, 0);
}

#[test]
fn pending_write_does_not_block_read_or_control_priority() {
    let mut h = Harness::new(
        Config {
            outgoing_frame_bytes: 2,
            ..Config::default()
        },
        128,
    );
    h.hold_writes = true;
    h.machine
        .send_message(SendMessage {
            kind: MessageKind::Text,
            buffer: Rc::from(b"four".as_slice()),
            range: 0..4,
        })
        .unwrap();
    h.pump();
    h.feed(
        &[masked(1, true, b"other"), masked(9, true, b"!")].concat(),
        usize::MAX,
    );
    assert_eq!(h.messages, [(MessageKind::Text, b"other".to_vec())]);
    let op = h.write.take().unwrap();
    let bytes = op.slices().concat();
    assert_eq!(bytes, b"\x01\x02fo");
    h.machine
        .complete_write(op.complete(Ok(bytes.len())))
        .unwrap();
    h.hold_writes = false;
    h.pump();
    assert_eq!(h.wire, b"\x8a\x01!\x80\x02ur");
}

#[test]
fn latest_pending_ping_is_bounded_and_unsolicited_pong_is_silent() {
    let mut h = Harness::new(Config::default(), 128);
    h.hold_writes = true;
    h.feed(&masked(9, true, b"first"), usize::MAX);
    h.feed(
        &[masked(9, true, b"second"), masked(9, true, b"last")].concat(),
        usize::MAX,
    );
    let op = h.write.take().unwrap();
    let bytes = op.slices().concat();
    assert_eq!(bytes, b"\x8a\x05first");
    h.machine
        .complete_write(op.complete(Ok(bytes.len())))
        .unwrap();
    h.hold_writes = false;
    h.pump();
    assert_eq!(h.wire, b"\x8a\x04last");
    h.wire.clear();
    h.feed(&masked(10, true, b"unsolicited"), 1);
    assert!(h.wire.is_empty());
}

#[test]
fn invalid_send_returns_same_storage_and_capacity_recovers() {
    let mut h = Harness::new(Config::default(), 128);
    let payload: Storage = Rc::from([0xff]);
    let rejected = h
        .machine
        .send_message(SendMessage {
            kind: MessageKind::Text,
            buffer: payload.clone(),
            range: 0..1,
        })
        .unwrap_err();
    assert!(Rc::ptr_eq(&payload, &rejected.value.buffer));
    assert!(h.machine.can_send());
    let rejected = h
        .machine
        .send_message(SendMessage {
            kind: MessageKind::Binary,
            buffer: payload.clone(),
            range: 0..2,
        })
        .unwrap_err();
    assert_eq!(rejected.reason, RejectReason::InvalidRange);
    h.machine
        .send_message(SendMessage {
            kind: MessageKind::Binary,
            buffer: payload.clone(),
            range: 0..1,
        })
        .unwrap();
    let rejected = h
        .machine
        .send_message(SendMessage {
            kind: MessageKind::Binary,
            buffer: payload.clone(),
            range: 0..1,
        })
        .unwrap_err();
    assert_eq!(rejected.reason, RejectReason::NoCapacity);
    assert!(Rc::ptr_eq(&payload, &rejected.value.buffer));
    h.pump();
    assert!(h.machine.can_send());
}

fn next_event(machine: &mut Machine) -> Option<Event> {
    loop {
        match machine.next(&mut PortsImpl) {
            Some(Event::Deadline(_)) => {}
            event => return event,
        }
    }
}

#[test]
fn held_chunk_allows_writes_and_blocks_close_until_release() {
    let mut m = machine(Config::default(), 128);
    let Some(Event::Read(mut read)) = next_event(&mut m) else {
        panic!()
    };
    let bytes = masked(1, true, b"held");
    read.bytes_mut()[..bytes.len()].copy_from_slice(&bytes);
    m.complete_read(read.complete(Ok(bytes.len()))).unwrap();
    assert!(matches!(next_event(&mut m), Some(Event::Start(_))));
    let Some(Event::Chunk(chunk)) = next_event(&mut m) else {
        panic!()
    };
    m.send_message(SendMessage {
        kind: MessageKind::Text,
        buffer: Rc::from(b"reply".as_slice()),
        range: 0..5,
    })
    .unwrap();
    let Some(Event::Write(write)) = next_event(&mut m) else {
        panic!()
    };
    let count = write.slices().iter().map(|s| s.len()).sum();
    m.complete_write(write.complete(Ok(count))).unwrap();
    assert!(matches!(next_event(&mut m), Some(Event::Sent(_))));
    assert!(next_event(&mut m).is_none());
    m.abort(Failure::Cancelled);
    assert!(next_event(&mut m).is_none());
    assert_eq!(chunk.bytes(), b"held");
    m.release_chunk(chunk.release()).unwrap();
    let Some(Event::Close(close)) = next_event(&mut m) else {
        panic!()
    };
    m.complete_close(close.complete(Ok(()))).unwrap();
    assert!(matches!(
        next_event(&mut m),
        Some(Event::Closed(ConnectionResult {
            result: Err(Failure::Cancelled),
            ..
        }))
    ));
}

#[test]
fn none_callbacks_issue_each_operation_once_and_allow_duplex_progress() {
    struct Collector {
        read: Option<ReadOp<Vec<u8>>>,
        write: Option<WriteOp<Storage>>,
    }
    impl Ports<Vec<u8>, Storage> for Collector {
        type Output = std::convert::Infallible;
        fn read(&mut self, op: ReadOp<Vec<u8>>) -> Option<Self::Output> {
            assert!(self.read.replace(op).is_none());
            None
        }
        fn write(&mut self, op: WriteOp<Storage>) -> Option<Self::Output> {
            assert!(self.write.replace(op).is_none());
            None
        }
        fn readiness(&mut self, _: ReadinessOp) -> Option<Self::Output> {
            panic!()
        }
        fn cancel(&mut self, _: CancelOp) -> Option<Self::Output> {
            panic!()
        }
        fn close(&mut self, _: CloseOp) -> Option<Self::Output> {
            panic!()
        }
        fn message_started(&mut self, _: MessageInfo) -> Option<Self::Output> {
            panic!()
        }
        fn chunk(&mut self, _: ChunkOp<Vec<u8>>) -> Option<Self::Output> {
            panic!()
        }
        fn message_finished(&mut self, _: MessageInfo) -> Option<Self::Output> {
            panic!()
        }
        fn message_sent(&mut self, _: MessageSent<Storage>) -> Option<Self::Output> {
            panic!()
        }
        fn peer_closed(&mut self, _: CloseReason) -> Option<Self::Output> {
            panic!()
        }
        fn deadline_changed(&mut self, _: Option<Deadline>) -> Option<Self::Output> {
            None
        }
        fn closed(&mut self, _: ConnectionResult) -> Option<Self::Output> {
            panic!()
        }
    }
    let mut m = machine(Config::default(), 128);
    m.send_message(SendMessage {
        kind: MessageKind::Binary,
        buffer: Rc::from([1]),
        range: 0..1,
    })
    .unwrap();
    let mut collector = Collector {
        read: None,
        write: None,
    };
    assert!(m.next(&mut collector).is_none());
    assert!(collector.read.is_some() && collector.write.is_some());
    for _ in 0..10 {
        assert!(m.next(&mut collector).is_none());
    }
}

#[test]
fn cancellation_waits_for_both_completions_in_either_order() {
    for write_first in [false, true] {
        let mut m = machine(Config::default(), 128);
        m.send_message(SendMessage {
            kind: MessageKind::Binary,
            buffer: Rc::from([1, 2, 3]),
            range: 0..3,
        })
        .unwrap();
        let Some(Event::Write(write)) = next_event(&mut m) else {
            panic!()
        };
        let Some(Event::Read(read)) = next_event(&mut m) else {
            panic!()
        };
        m.abort(Failure::Cancelled);
        let Some(Event::Cancel(cancel_read)) = next_event(&mut m) else {
            panic!()
        };
        let Some(Event::Cancel(cancel_write)) = next_event(&mut m) else {
            panic!()
        };
        assert_eq!(cancel_read.target, read.id());
        assert_eq!(cancel_write.target, write.id());
        assert!(next_event(&mut m).is_none());
        let mut read = Some(read);
        let mut write = Some(write);
        for first in [true, false] {
            if first == write_first {
                // A real successful write may win the cancellation race.
                m.complete_write(write.take().unwrap().complete(Ok(5)))
                    .unwrap();
                let Some(Event::Sent(receipt)) = next_event(&mut m) else {
                    panic!()
                };
                assert_eq!(receipt.accepted, 3);
                assert_eq!(receipt.result, Err(Failure::Cancelled));
            } else {
                m.complete_read(read.take().unwrap().complete(Err(IoError {
                    kind: IoErrorKind::Cancelled,
                    code: None,
                })))
                .unwrap();
            }
            if first {
                assert!(next_event(&mut m).is_none());
            }
        }
        let Some(Event::Close(close)) = next_event(&mut m) else {
            panic!()
        };
        m.complete_close(close.complete(Ok(()))).unwrap();
        assert!(m.complete_close(close.complete(Ok(()))).is_err());
        assert!(matches!(next_event(&mut m), Some(Event::Closed(_))));
        assert!(next_event(&mut m).is_none());
    }
}

#[test]
fn invalid_completions_preserve_live_state_and_storage() {
    let mut m = machine(Config::default(), 32);
    let Some(Event::Read(read)) = next_event(&mut m) else {
        panic!()
    };
    let rejected = m.complete_read(read.complete(Ok(33))).unwrap_err();
    assert_eq!(rejected.reason, RejectReason::InvalidCount);
    let (read, _) = rejected.value.into_parts();
    assert!(next_event(&mut m).is_none());
    let mut other: Machine = Server::new(
        ConnectionId {
            slot: 2,
            generation: 1,
        },
        Config::default(),
        vec![0; 32],
        Tick(0),
    )
    .unwrap();
    let rejected = other.complete_read(read.complete(Ok(0))).unwrap_err();
    assert_eq!(rejected.reason, RejectReason::WrongConnection);
    let (read, _) = rejected.value.into_parts();
    m.complete_read(read.complete(Err(IoError {
        kind: IoErrorKind::Interrupted,
        code: None,
    })))
    .unwrap();
    assert!(matches!(next_event(&mut m), Some(Event::Read(_))));
    let payload: Storage = Rc::from([4, 5]);
    m.send_message(SendMessage {
        kind: MessageKind::Binary,
        buffer: payload.clone(),
        range: 0..2,
    })
    .unwrap();
    let Some(Event::Write(write)) = next_event(&mut m) else {
        panic!()
    };
    let rejected = m.complete_write(write.complete(Ok(5))).unwrap_err();
    assert_eq!(rejected.reason, RejectReason::InvalidCount);
    let (write, _) = rejected.value.into_parts();
    assert_eq!(write.slices()[1].as_ptr(), payload.as_ptr());
    m.complete_write(write.complete(Ok(4))).unwrap();
    let Some(Event::Sent(receipt)) = next_event(&mut m) else {
        panic!()
    };
    assert!(Rc::ptr_eq(&receipt.buffer, &payload));
}

#[test]
fn interrupted_and_would_block_preserve_exact_write_cursor() {
    let mut m = machine(Config::default(), 128);
    m.send_message(SendMessage {
        kind: MessageKind::Binary,
        buffer: Rc::from([7, 8]),
        range: 0..2,
    })
    .unwrap();
    let Some(Event::Write(write)) = next_event(&mut m) else {
        panic!()
    };
    m.complete_write(write.complete(Ok(3))).unwrap();
    let Some(Event::Write(write)) = next_event(&mut m) else {
        panic!()
    };
    assert_eq!(write.slices().concat(), [8]);
    m.complete_write(write.complete(Err(IoError {
        kind: IoErrorKind::WouldBlock,
        code: None,
    })))
    .unwrap();
    let Some(Event::Ready(ready)) = next_event(&mut m) else {
        panic!()
    };
    assert_eq!(ready.direction, Direction::Write);
    assert!(matches!(next_event(&mut m), Some(Event::Read(_))));
    assert!(next_event(&mut m).is_none());
    m.complete_readiness(ready.complete(Ok(()))).unwrap();
    assert!(m.complete_readiness(ready.complete(Ok(()))).is_err());
    let Some(Event::Write(write)) = next_event(&mut m) else {
        panic!()
    };
    assert_eq!(write.slices().concat(), [8]);
    m.complete_write(write.complete(Err(IoError {
        kind: IoErrorKind::Interrupted,
        code: None,
    })))
    .unwrap();
    let Some(Event::Write(write)) = next_event(&mut m) else {
        panic!()
    };
    assert_eq!(write.slices().concat(), [8]);
    m.complete_write(write.complete(Ok(1))).unwrap();
    let Some(Event::Sent(receipt)) = next_event(&mut m) else {
        panic!()
    };
    assert_eq!(receipt.accepted, 2);
    assert_eq!(receipt.acceptance, Acceptance::Exact);
}

#[test]
fn unknown_write_progress_is_terminal_and_returns_lower_bound() {
    for kind in [
        IoErrorKind::UnknownProgress,
        IoErrorKind::CancelledUnknownProgress,
    ] {
        let mut h = Harness::new(Config::default(), 128);
        h.hold_writes = true;
        h.machine
            .send_message(SendMessage {
                kind: MessageKind::Binary,
                buffer: Rc::from([7, 8, 9]),
                range: 0..3,
            })
            .unwrap();
        h.pump();
        h.machine
            .complete_write(h.write.take().unwrap().complete(Ok(3)))
            .unwrap();
        h.pump();
        let op = h.write.take().unwrap();
        assert_eq!(op.slices().concat(), [8, 9]);
        h.machine
            .complete_write(op.complete(Err(IoError { kind, code: None })))
            .unwrap();
        h.pump();
        assert_eq!(h.sent[0].accepted, 1);
        assert_eq!(h.sent[0].acceptance, Acceptance::LowerBound);
        assert!(h.closed.unwrap().result.is_err());
        assert!(h.write.is_none());
    }
}

#[test]
fn source_failure_returns_unsent_storage_and_emits_1011() {
    let mut h = Harness::new(Config::default(), 128);
    h.machine
        .send_message(SendMessage {
            kind: MessageKind::Binary,
            buffer: Rc::from([7]),
            range: 0..1,
        })
        .unwrap();
    h.machine.fail_source();
    h.pump();
    assert_eq!(h.sent[0].accepted, 0);
    assert_eq!(h.sent[0].result, Err(Failure::Application));
    assert_eq!(h.wire, [0x88, 2, 3, 0xf3]);
    assert_eq!(h.closed.unwrap().result, Err(Failure::Application));
}

#[test]
fn zero_write_and_transport_error_settle_without_retry() {
    for result in [
        Ok(0),
        Err(IoError {
            kind: IoErrorKind::Reset,
            code: Some(104),
        }),
    ] {
        let mut h = Harness::new(Config::default(), 128);
        h.hold_writes = true;
        h.machine
            .send_message(SendMessage {
                kind: MessageKind::Binary,
                buffer: Rc::from([7]),
                range: 0..1,
            })
            .unwrap();
        h.pump();
        h.machine
            .complete_write(h.write.take().unwrap().complete(result))
            .unwrap();
        h.pump();
        assert_eq!(h.sent.len(), 1);
        assert_eq!(h.sent[0].accepted, 0);
        assert!(h.closed.unwrap().result.is_err());
        assert!(h.write.is_none());
    }
}

#[test]
fn deadlines_reject_early_stale_and_regressed_observations() {
    let mut h = Harness::new(
        Config {
            idle_timeout_ns: Some(10),
            ..Config::default()
        },
        128,
    );
    h.pump();
    let old = h.deadline.unwrap();
    assert_eq!(old.at, Tick(10));
    assert_eq!(
        h.machine.expire(old, Tick(9)),
        Err(CommandError::EarlyDeadline)
    );
    h.machine.observe_time(Tick(1)).unwrap();
    h.feed(&masked(10, true, b""), usize::MAX);
    let current = h.deadline.unwrap();
    assert_eq!(current.at, Tick(11));
    assert_eq!(
        h.machine.expire(old, Tick(10)),
        Err(CommandError::StaleDeadline)
    );
    assert_eq!(
        h.machine.observe_time(Tick(0)),
        Err(CommandError::TimeRegression)
    );
    h.machine.expire(current, Tick(11)).unwrap();
    h.pump();
    assert_eq!(h.closed.unwrap().result, Err(Failure::Timeout));
}

#[test]
fn frame_message_write_and_close_deadlines_are_independent() {
    let config = Config {
        idle_timeout_ns: None,
        frame_timeout_ns: Some(10),
        message_timeout_ns: Some(20),
        write_timeout_ns: Some(30),
        close_timeout_ns: Some(40),
        ..Config::default()
    };
    let mut h = Harness::new(config.clone(), 128);
    h.feed(&[0x81], 1);
    assert_eq!(h.deadline.unwrap().at, Tick(10));
    h.machine.expire(h.deadline.unwrap(), Tick(10)).unwrap();
    h.pump();
    assert_eq!(h.closed.unwrap().result, Err(Failure::Timeout));
    let mut h = Harness::new(config.clone(), 128);
    h.feed(&masked(1, false, b"a"), usize::MAX);
    assert_eq!(h.deadline.unwrap().at, Tick(20));
    h.machine.observe_time(Tick(5)).unwrap();
    h.feed(&masked(9, true, b"?"), usize::MAX);
    assert_eq!(h.deadline.unwrap().at, Tick(20));
    h.machine.expire(h.deadline.unwrap(), Tick(20)).unwrap();
    h.pump();
    assert_eq!(h.closed.unwrap().result, Err(Failure::Timeout));
    let mut h = Harness::new(config.clone(), 128);
    h.hold_writes = true;
    h.machine
        .send_message(SendMessage {
            kind: MessageKind::Binary,
            buffer: Rc::from([1]),
            range: 0..1,
        })
        .unwrap();
    h.pump();
    assert_eq!(h.deadline.unwrap().at, Tick(30));
    h.machine.expire(h.deadline.unwrap(), Tick(30)).unwrap();
    h.pump();
    assert_eq!(h.closed.unwrap().result, Err(Failure::Timeout));
    let mut h = Harness::new(config, 128);
    h.machine.close(CloseReason::empty()).unwrap();
    h.pump();
    assert_eq!(h.deadline.unwrap().at, Tick(40));
    h.machine.expire(h.deadline.unwrap(), Tick(40)).unwrap();
    h.pump();
    assert_eq!(h.closed.unwrap().result, Err(Failure::Timeout));
}

#[test]
fn close_never_splices_into_a_partially_written_frame() {
    let mut h = Harness::new(
        Config {
            outgoing_frame_bytes: 2,
            ..Config::default()
        },
        128,
    );
    h.hold_writes = true;
    h.machine
        .send_message(SendMessage {
            kind: MessageKind::Binary,
            buffer: Rc::from([1, 2, 3, 4]),
            range: 0..4,
        })
        .unwrap();
    h.pump();
    let op = h.write.take().unwrap();
    assert_eq!(op.slices().concat(), [2, 2, 1, 2]);
    h.wire.push(2);
    h.machine.complete_write(op.complete(Ok(1))).unwrap();
    h.machine
        .close(CloseReason::new(1000, "").unwrap())
        .unwrap();
    h.hold_writes = false;
    h.pump();
    assert_eq!(h.wire, [2, 2, 1, 2, 0x88, 2, 3, 0xe8]);
    assert_eq!(h.sent[0].accepted, 2);
    assert_eq!(h.sent[0].result, Err(Failure::Closing));
    assert!(h.closed.is_none());
    h.feed(&masked(8, true, &[3, 0xe8]), usize::MAX);
    assert!(h.closed.unwrap().clean);
}

#[test]
fn read_readiness_cancellation_does_not_abort_error_close() {
    let mut m = machine(Config::default(), 128);
    let Some(Event::Read(read)) = next_event(&mut m) else {
        panic!()
    };
    m.complete_read(read.complete(Err(IoError {
        kind: IoErrorKind::WouldBlock,
        code: None,
    })))
    .unwrap();
    let Some(Event::Ready(ready)) = next_event(&mut m) else {
        panic!()
    };
    assert_eq!(ready.direction, Direction::Read);
    m.fail_source();
    let Some(Event::Cancel(cancel)) = next_event(&mut m) else {
        panic!()
    };
    assert_eq!(cancel.target, ready.id());
    m.complete_readiness(ready.complete(Err(IoError {
        kind: IoErrorKind::Cancelled,
        code: None,
    })))
    .unwrap();
    let Some(Event::Write(write)) = next_event(&mut m) else {
        panic!()
    };
    assert_eq!(write.slices().concat(), [0x88, 2, 3, 0xf3]);
    m.complete_write(write.complete(Ok(4))).unwrap();
    assert!(matches!(next_event(&mut m), Some(Event::Close(_))));
}

#[test]
fn unknown_progress_after_abort_preserves_lower_bound_receipt() {
    let mut m = machine(Config::default(), 128);
    m.send_message(SendMessage {
        kind: MessageKind::Binary,
        buffer: Rc::from([1, 2]),
        range: 0..2,
    })
    .unwrap();
    let Some(Event::Write(write)) = next_event(&mut m) else {
        panic!()
    };
    m.abort(Failure::Cancelled);
    assert!(matches!(next_event(&mut m), Some(Event::Cancel(_))));
    m.complete_write(write.complete(Err(IoError {
        kind: IoErrorKind::UnknownProgress,
        code: None,
    })))
    .unwrap();
    let Some(Event::Sent(receipt)) = next_event(&mut m) else {
        panic!()
    };
    assert_eq!(receipt.result, Err(Failure::Cancelled));
    assert_eq!(receipt.acceptance, Acceptance::LowerBound);
    assert_eq!(receipt.accepted, 0);
    assert!(matches!(next_event(&mut m), Some(Event::Close(_))));
}

#[test]
fn zero_message_limit_accepts_only_empty_messages() {
    let mut h = Harness::new(
        Config {
            max_message_bytes: 0,
            ..Config::default()
        },
        1,
    );
    h.feed(&masked(1, true, b""), 1);
    assert_eq!(h.messages, [(MessageKind::Text, vec![])]);
    h.feed(&masked(2, true, b"x"), 1);
    assert_eq!(h.closed.unwrap().result, Err(Failure::Limit));
}

#[test]
fn invalid_constructor_and_close_commands_preserve_state() {
    let buffer = vec![0; 4];
    let address = buffer.as_ptr();
    let error = Machine::new(
        ConnectionId {
            slot: 1,
            generation: 1,
        },
        Config {
            max_buffer_bytes: 3,
            ..Config::default()
        },
        buffer,
        Tick(0),
    )
    .err()
    .unwrap();
    assert_eq!(error.value.as_ptr(), address);
    for code in [1004, 1005, 1006, 1010, 1015, 5000] {
        assert_eq!(CloseReason::new(code, ""), Err(CommandError::InvalidClose));
    }
    assert!(CloseReason::new(1000, &"x".repeat(124)).is_err());
    assert!(CloseReason::new(1000, &"x".repeat(123)).is_ok());
    let mut h = Harness::new(Config::default(), 128);
    h.machine.close(CloseReason::empty()).unwrap();
    assert_eq!(
        h.machine.close(CloseReason::empty()),
        Err(CommandError::InvalidState)
    );
}

#[test]
fn large_outgoing_headers_are_minimal_and_unmasked() {
    for length in [125, 126, 65535, 65536] {
        let mut h = Harness::new(
            Config {
                outgoing_frame_bytes: 65536,
                ..Config::default()
            },
            128,
        );
        let payload: Storage = Rc::from(vec![42; length]);
        h.machine
            .send_message(SendMessage {
                kind: MessageKind::Binary,
                buffer: payload,
                range: 0..length,
            })
            .unwrap();
        h.pump();
        let header = if length < 126 {
            vec![0x82, 125]
        } else if length <= 65535 {
            [vec![0x82, 126], (length as u16).to_be_bytes().to_vec()].concat()
        } else {
            [vec![0x82, 127], (length as u64).to_be_bytes().to_vec()].concat()
        };
        assert_eq!(&h.wire[..header.len()], &header);
        assert_eq!(h.wire.len(), length + header.len());
        assert_eq!(h.sent[0].accepted, length);
    }
}
