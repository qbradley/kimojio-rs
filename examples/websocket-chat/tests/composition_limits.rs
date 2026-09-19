use kimojio_fsm_http1::{IoError, IoErrorKind, Tick};
use kimojio_fsm_websocket as ws;
use websocket_chat::{
    chat::{Buffer, Chat, Completion, Config, Identity, Io, Ports},
    hub::{ClientId, Error},
};

const REQUEST: &[u8] = b"GET /chat HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n";

enum Action {
    Io(ClientId, Io),
    Cancel(ClientId, Identity),
    Retired(ClientId),
}

struct Transport;
impl Ports for Transport {
    type Output = Action;
    fn io(&mut self, client: ClientId, operation: Io) -> Option<Action> {
        Some(Action::Io(client, operation))
    }
    fn cancel(&mut self, client: ClientId, target: Identity) -> Option<Action> {
        Some(Action::Cancel(client, target))
    }
    fn deadline_changed(&mut self, _: Option<Tick>) -> Option<Action> {
        None
    }
    fn retired(&mut self, client: ClientId) -> Option<Action> {
        Some(Action::Retired(client))
    }
    fn yield_turn(&mut self) -> Option<Action> {
        None
    }
}

struct Harness {
    chat: Chat,
    reads: [Option<ws::ReadOp<Buffer>>; 2],
    handshake: [Vec<u8>; 2],
    wire: [Vec<u8>; 2],
    retired: [bool; 2],
    shutting_down: bool,
}

impl Harness {
    fn pump(&mut self) {
        for _ in 0..10_000 {
            match self.chat.next(&mut Transport) {
                Some(Action::Io(id, Io::HttpRead(mut read))) => {
                    assert!(!self.shutting_down);
                    assert!(self.handshake[id.slot()].is_empty());
                    read.bytes_mut()[..REQUEST.len()].copy_from_slice(REQUEST);
                    self.chat
                        .complete(id, Completion::HttpRead(read.complete(Ok(REQUEST.len()))))
                        .unwrap();
                }
                Some(Action::Io(id, Io::HttpWrite(write))) => {
                    let bytes = write.slices().concat();
                    let count = bytes.len().min(37);
                    self.handshake[id.slot()].extend_from_slice(&bytes[..count]);
                    self.chat
                        .complete(id, Completion::HttpWrite(write.complete(Ok(count))))
                        .unwrap();
                }
                Some(Action::Io(id, Io::WsRead(mut read))) => {
                    assert!(!self.shutting_down);
                    assert!(self.handshake[id.slot()].starts_with(b"HTTP/1.1 101 "));
                    assert!(self.handshake[id.slot()].ends_with(b"\r\n\r\n"));
                    assert!(read.bytes_mut().len() <= 16 * 1024);
                    assert!(self.reads[id.slot()].replace(read).is_none());
                }
                Some(Action::Io(id, Io::WsWrite(write))) => {
                    assert!(!self.shutting_down);
                    let bytes = write.slices().concat();
                    let count = bytes.len().min(37);
                    self.wire[id.slot()].extend_from_slice(&bytes[..count]);
                    self.chat
                        .complete(id, Completion::WsWrite(write.complete(Ok(count))))
                        .unwrap();
                }
                Some(Action::Cancel(id, Identity::WebSocket(target))) => {
                    assert!(self.shutting_down);
                    let read = self.reads[id.slot()].take().unwrap();
                    assert_eq!(read.id(), target);
                    self.chat
                        .complete(
                            id,
                            Completion::WsRead(read.complete(Err(IoError {
                                kind: IoErrorKind::Cancelled,
                                code: None,
                            }))),
                        )
                        .unwrap();
                }
                Some(Action::Io(id, Io::WsClose(close))) => {
                    assert!(self.shutting_down);
                    assert!(self.reads[id.slot()].is_none());
                    self.chat
                        .complete(id, Completion::WsClose(close.complete(Ok(()))))
                        .unwrap();
                }
                Some(Action::Retired(id)) => {
                    assert!(!self.retired[id.slot()]);
                    self.retired[id.slot()] = true;
                }
                None => return,
                _ => panic!("unexpected protocol transition"),
            }
        }
        panic!("composition did not reach quiescence");
    }

    fn feed(&mut self, client: ClientId, bytes: &[u8]) -> usize {
        let mut read = self.reads[client.slot()].take().unwrap();
        let count = bytes.len().min(read.bytes_mut().len());
        read.bytes_mut()[..count].copy_from_slice(&bytes[..count]);
        self.chat
            .complete(client, Completion::WsRead(read.complete(Ok(count))))
            .unwrap();
        self.pump();
        count
    }
}

fn masked(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mask = [0x13, 0x27, 0x45, 0x61];
    let mut frame = vec![0x80 | opcode];
    if payload.len() < 126 {
        frame.push(0x80 | payload.len() as u8);
    } else {
        frame.push(0xfe);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    frame.extend_from_slice(&mask);
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % 4]),
    );
    frame
}

fn decode(wire: &[u8]) -> (u8, Vec<u8>) {
    let mut cursor = 0;
    let mut kind = None;
    let mut message = Vec::new();
    loop {
        let flags = wire[cursor];
        let opcode = flags & 0x0f;
        assert_eq!(flags & 0x70, 0);
        assert_eq!(wire[cursor + 1] & 0x80, 0);
        if kind.is_some() {
            assert_eq!(opcode, 0);
        } else {
            assert!(matches!(opcode, 1 | 2));
            kind = Some(opcode);
        }
        let mut length = usize::from(wire[cursor + 1]);
        cursor += 2;
        if length == 126 {
            length = usize::from(u16::from_be_bytes([wire[cursor], wire[cursor + 1]]));
            cursor += 2;
        }
        assert!(length <= 16 * 1024);
        message.extend_from_slice(&wire[cursor..cursor + length]);
        cursor += length;
        if flags & 0x80 != 0 {
            assert_eq!(cursor, wire.len());
            return (kind.unwrap(), message);
        }
    }
}

#[test]
fn public_limits_preserve_incremental_receive_and_exact_broadcast_lifecycle() {
    let mut invalid = Config::default();
    invalid.websocket.max_buffer_bytes = invalid.receive_bytes;
    assert_eq!(invalid.receive_bytes, 16 * 1024);
    assert_eq!(invalid.hub.max_message_bytes, 1024 * 1024);
    assert!(matches!(
        Chat::new(invalid.clone()),
        Err(Error::InvalidConfig)
    ));

    // The outgoing limit fits a complete message. The actual input allocation
    // remains 16KiB; no receive allocation grows to the outgoing limit.
    invalid.websocket.max_buffer_bytes = invalid.hub.max_message_bytes;
    invalid.hub.max_clients = 2;
    let mut harness = Harness {
        chat: Chat::new(invalid).unwrap(),
        reads: [None, None],
        handshake: [Vec::new(), Vec::new()],
        wire: [Vec::new(), Vec::new()],
        retired: [false; 2],
        shutting_down: false,
    };
    let empty = harness.chat.stats().used_bytes;
    let sender = harness.chat.admit().unwrap();
    let peer = harness.chat.admit().unwrap();
    harness.pump();
    assert_eq!(harness.chat.stats().active_clients, 2);
    let admitted = harness.chat.stats().used_bytes;

    let payload: Vec<u8> = (0..20_000).map(|index| (index % 251) as u8).collect();
    let frame = masked(2, &payload);
    let consumed = harness.feed(sender, &frame);
    assert_eq!(consumed, 16 * 1024);
    assert!(harness.wire.iter().all(Vec::is_empty));
    assert_eq!(harness.chat.stats().used_bytes, admitted + 16 * 1024);
    assert_eq!(
        harness.feed(sender, &frame[consumed..]),
        frame.len() - consumed
    );
    for wire in &harness.wire {
        assert_eq!(decode(wire), (2, payload.clone()));
    }
    assert_eq!(harness.chat.stats().active_clients, 2);
    assert_eq!(harness.chat.stats().used_bytes, admitted);
    assert_eq!(
        harness.chat.stats().peak_bytes,
        admitted + 16 * 1024 + 32 * 1024
    );

    harness.wire.iter_mut().for_each(Vec::clear);
    let follow_up = masked(1, b"still healthy");
    assert_eq!(harness.feed(peer, &follow_up), follow_up.len());
    for wire in &harness.wire {
        assert_eq!(decode(wire), (1, b"still healthy".to_vec()));
    }
    assert_eq!(harness.chat.stats().active_clients, 2);
    assert_eq!(harness.chat.stats().used_bytes, admitted);

    harness.shutting_down = true;
    harness.chat.shutdown(true);
    harness.pump();
    assert_eq!(harness.retired, [true; 2]);
    assert!(harness.reads.iter().all(Option::is_none));
    assert_eq!(harness.chat.stats().resident_clients, 0);
    assert_eq!(harness.chat.stats().used_bytes, empty);
}

#[test]
fn related_incompatible_limits_fail_before_admission() {
    let mut configuration = Config::default();
    configuration.websocket.max_message_bytes -= 1;
    assert!(matches!(
        Chat::new(configuration),
        Err(Error::InvalidConfig)
    ));

    let mut configuration = Config::default();
    configuration.http.max_buffer_bytes = configuration.receive_bytes - 1;
    assert!(matches!(
        Chat::new(configuration),
        Err(Error::InvalidConfig)
    ));

    let mut configuration = Config::default();
    configuration.websocket.max_frame_bytes =
        configuration.websocket.outgoing_frame_bytes as u64 - 1;
    assert!(matches!(
        Chat::new(configuration),
        Err(Error::InvalidConfig)
    ));

    let mut configuration = Config::default();
    configuration.hub.max_client_bytes = configuration.hub.max_message_bytes;
    assert!(matches!(
        Chat::new(configuration),
        Err(Error::InvalidConfig)
    ));
}
