use kimojio_fsm_http1 as http;
use kimojio_fsm_websocket as ws;
use std::rc::Rc;

const REQUEST: &[u8] = b"GET /chat HTTP/1.1\r\nHost: example.test\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
const RESPONSE: &[u8] = b"HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n";
type Storage = Rc<[u8]>;

enum Event {
    Read(http::ReadOp<Vec<u8>>),
    Write(http::WriteOp<Storage>),
    Request(http::ExchangeId, ws::Handshake),
    Upgrade,
}
struct HttpPorts;
impl http::Ports<Vec<u8>, Storage> for HttpPorts {
    type Output = Event;
    fn read(&mut self, op: http::ReadOp<Vec<u8>>) -> Option<Event> {
        Some(Event::Read(op))
    }
    fn write(&mut self, op: http::WriteOp<Storage>) -> Option<Event> {
        Some(Event::Write(op))
    }
    fn readiness(&mut self, _: http::ReadinessOp) -> Option<Event> {
        panic!()
    }
    fn cancel(&mut self, _: http::CancelOp) -> Option<Event> {
        panic!()
    }
    fn close(&mut self, _: http::CloseOp) -> Option<Event> {
        panic!()
    }
    fn body(&mut self, _: http::BodyOp<Vec<u8>>) -> Option<Event> {
        panic!()
    }
    fn trailers(&mut self, _: http::ExchangeId, _: http::Headers<'_>) -> Option<Event> {
        panic!()
    }
    fn incoming_finished(&mut self, _: http::ExchangeId) -> Option<Event> {
        None
    }
    fn send_ready(&mut self, _: http::ExchangeId, _: usize) -> Option<Event> {
        panic!()
    }
    fn body_sent(&mut self, _: http::BodySent<Storage>) -> Option<Event> {
        panic!()
    }
    fn exchange_finished(&mut self, _: http::ExchangeFinished) -> Option<Event> {
        panic!()
    }
    fn deadline_changed(&mut self, _: Option<http::Deadline>) -> Option<Event> {
        None
    }
    fn upgrade_ready(&mut self, _: http::ExchangeId) -> Option<Event> {
        Some(Event::Upgrade)
    }
    fn closed(&mut self, _: http::ConnectionResult) -> Option<Event> {
        panic!()
    }
}
impl http::ServerPorts<Vec<u8>, Storage> for HttpPorts {
    fn request(&mut self, id: http::ExchangeId, head: http::RequestHead<'_>) -> Option<Event> {
        Some(Event::Request(id, ws::Handshake::validate(head).unwrap()))
    }
}

fn handoff(
    split: usize,
    write_step: usize,
    extra: &[u8],
) -> (http::Handoff<Vec<u8>>, Vec<u8>, usize) {
    let buffer = vec![0; 512];
    let address = buffer.as_ptr() as usize;
    let mut server = http::Server::<Vec<u8>, Storage>::with_output_type(
        http::ConnectionId {
            slot: 9,
            generation: 3,
        },
        http::Config::default(),
        buffer,
        http::Tick(0),
    )
    .unwrap();
    let request = [REQUEST, extra].concat();
    let mut cursor = 0;
    let mut wire = vec![];
    for _ in 0..10000 {
        match server
            .next(&mut HttpPorts)
            .expect("HTTP must make handshake progress")
        {
            Event::Read(mut op) => {
                let end = if cursor < split { split } else { request.len() };
                let count = end - cursor;
                assert!(count > 0);
                op.bytes_mut()[..count].copy_from_slice(&request[cursor..end]);
                cursor = end;
                server.complete_read(op.complete(Ok(count))).unwrap();
            }
            Event::Write(op) => {
                assert!(server.take_upgrade().is_err());
                let bytes = op.slices().concat();
                let count = bytes.len().min(write_step);
                wire.extend_from_slice(&bytes[..count]);
                server.complete_write(op.complete(Ok(count))).unwrap();
            }
            Event::Request(exchange, handshake) => handshake.accept(&mut server, exchange).unwrap(),
            Event::Upgrade => {
                assert_eq!(cursor, request.len());
                let handoff = server.take_upgrade().unwrap();
                assert!(server.take_upgrade().is_err());
                assert!(server.next(&mut HttpPorts).is_none());
                return (handoff, wire, address);
            }
        }
    }
    panic!("HTTP upgrade did not settle");
}

enum WsEvent {
    Chunk(ws::ChunkOp<Vec<u8>>),
    Finished(ws::MessageInfo),
    Read,
}
struct WsPorts;
impl ws::Ports<Vec<u8>, Storage> for WsPorts {
    type Output = WsEvent;
    fn read(&mut self, _: ws::ReadOp<Vec<u8>>) -> Option<WsEvent> {
        Some(WsEvent::Read)
    }
    fn write(&mut self, _: ws::WriteOp<Storage>) -> Option<WsEvent> {
        panic!()
    }
    fn readiness(&mut self, _: ws::ReadinessOp) -> Option<WsEvent> {
        panic!()
    }
    fn cancel(&mut self, _: ws::CancelOp) -> Option<WsEvent> {
        panic!()
    }
    fn close(&mut self, _: ws::CloseOp) -> Option<WsEvent> {
        panic!()
    }
    fn message_started(&mut self, _: ws::MessageInfo) -> Option<WsEvent> {
        None
    }
    fn chunk(&mut self, op: ws::ChunkOp<Vec<u8>>) -> Option<WsEvent> {
        Some(WsEvent::Chunk(op))
    }
    fn message_finished(&mut self, m: ws::MessageInfo) -> Option<WsEvent> {
        Some(WsEvent::Finished(m))
    }
    fn message_sent(&mut self, _: ws::MessageSent<Storage>) -> Option<WsEvent> {
        panic!()
    }
    fn peer_closed(&mut self, _: ws::CloseReason) -> Option<WsEvent> {
        panic!()
    }
    fn deadline_changed(&mut self, _: Option<ws::Deadline>) -> Option<WsEvent> {
        None
    }
    fn closed(&mut self, _: ws::ConnectionResult) -> Option<WsEvent> {
        panic!()
    }
}

#[test]
fn http_partial_101_writes_settle_before_exact_leftover_handoff() {
    let frame = b"\x81\x85\0\0\0\0hello";
    for split in 1..REQUEST.len() {
        let (handoff, wire, address) = handoff(split, 1, frame);
        assert_eq!(wire, RESPONSE);
        assert_eq!(handoff.buffered.bytes(), frame);
        let mut ws =
            ws::Server::<_, Storage>::from_handoff(handoff, ws::Config::default(), ws::Tick(0))
                .ok()
                .unwrap();
        let Some(WsEvent::Chunk(chunk)) = ws.next(&mut WsPorts) else {
            panic!()
        };
        assert_eq!(chunk.bytes(), b"hello");
        let chunk_address = chunk.bytes().as_ptr() as usize;
        assert!((address..address + 512).contains(&chunk_address));
        ws.release_chunk(chunk.release()).unwrap();
        let Some(WsEvent::Finished(info)) = ws.next(&mut WsPorts) else {
            panic!()
        };
        assert_eq!(info.length, 5);
        assert_eq!(info.kind, ws::MessageKind::Text);
        assert!(matches!(ws.next(&mut WsPorts), Some(WsEvent::Read)));
    }
}

#[test]
fn rejected_handoff_preserves_original_buffer_identity_and_range() {
    let frame = b"\x81\x80\0\0\0\0";
    let (handoff, _, address) = handoff(REQUEST.len() - 1, 7, frame);
    let error = ws::Server::<_, Storage>::from_handoff(
        handoff,
        ws::Config {
            max_buffer_bytes: 1,
            ..ws::Config::default()
        },
        ws::Tick(0),
    )
    .err()
    .unwrap();
    assert_eq!(error.reason, ws::RejectReason::Limit);
    assert_eq!(
        error.value.connection,
        ws::ConnectionId {
            slot: 9,
            generation: 3
        }
    );
    assert_eq!(error.value.buffer.as_ptr() as usize, address);
    assert_eq!(&error.value.buffer[error.value.range], frame);
}

fn headers() -> Vec<http::Header<'static>> {
    vec![
        http::Header {
            name: "Host",
            value: b"example.test",
        },
        http::Header {
            name: "Upgrade",
            value: b"websocket",
        },
        http::Header {
            name: "Connection",
            value: b"Upgrade",
        },
        http::Header {
            name: "Sec-WebSocket-Key",
            value: b"dGhlIHNhbXBsZSBub25jZQ==",
        },
        http::Header {
            name: "Sec-WebSocket-Version",
            value: b"13",
        },
    ]
}
fn validate(headers: &[http::Header<'_>]) -> Result<ws::Handshake, ws::HandshakeError> {
    ws::Handshake::validate(http::RequestHead {
        method: "GET",
        target: "/chat",
        version: http::Version::Http11,
        headers,
    })
}

#[test]
fn handshake_rejects_missing_duplicate_and_invalid_singletons() {
    for index in 0..5 {
        let mut values = headers();
        values.remove(index);
        assert!(validate(&values).is_err(), "{index}");
    }
    for index in [0, 3, 4] {
        let mut values = headers();
        values.push(values[index]);
        assert_eq!(
            validate(&values).unwrap_err(),
            ws::HandshakeError::BadRequest
        );
    }
    for value in [b"".as_slice(), b"!", b"Zm9v", b"dGhlIHNhbXBsZSBub25jZR=="] {
        let mut values = headers();
        values[3].value = value;
        assert_eq!(
            validate(&values).unwrap_err(),
            ws::HandshakeError::BadRequest,
            "{value:?}"
        );
    }
    for value in [b"".as_slice(), b"013", b"256", b"13,13", b"abc"] {
        let mut values = headers();
        values[4].value = value;
        assert_eq!(
            validate(&values).unwrap_err(),
            ws::HandshakeError::BadRequest
        );
    }
    let mut values = headers();
    values[4].value = b"12";
    assert_eq!(
        validate(&values).unwrap_err(),
        ws::HandshakeError::UnsupportedVersion
    );
}

#[test]
fn handshake_ignores_valid_extensions_but_rejects_malformed_offers() {
    for value in [
        b"permessage-deflate; client_max_window_bits".as_slice(),
        b"foo; x=\"abc\", bar; y=2",
        b"foo; x=\"a\\b\"",
    ] {
        let mut values = headers();
        values.push(http::Header {
            name: "sec-websocket-extensions",
            value,
        });
        assert!(validate(&values).is_ok(), "{value:?}");
    }
    for value in [
        b"".as_slice(),
        b"foo,",
        b";x",
        b"foo;",
        b"foo;=a",
        b"foo;x=",
        b"foo;x=\"\"",
        b"foo;x=\"a b\"",
        b"foo;x=\"unterminated",
        b"foo; x=\"a\\",
    ] {
        let mut values = headers();
        values.push(http::Header {
            name: "sec-websocket-extensions",
            value,
        });
        assert_eq!(
            validate(&values).unwrap_err(),
            ws::HandshakeError::BadRequest,
            "{value:?}"
        );
    }
}

#[test]
fn handshake_protocol_lists_are_case_sensitive_unique_tokens() {
    let mut values = headers();
    values.push(http::Header {
        name: "sec-websocket-protocol",
        value: b"Chat, chat",
    });
    assert!(validate(&values).is_ok());
    values.push(http::Header {
        name: "sec-websocket-protocol",
        value: b"chat",
    });
    assert!(validate(&values).is_err());
    for value in [
        b"".as_slice(),
        b"chat,",
        b"chat, chat",
        b"not a token",
        b"chat;v=1",
    ] {
        let mut values = headers();
        values.push(http::Header {
            name: "sec-websocket-protocol",
            value,
        });
        assert!(validate(&values).is_err(), "{value:?}");
    }
}

#[test]
fn handshake_rejects_body_and_expect_and_non_get_http11() {
    for (name, value) in [
        ("content-length", b"1".as_slice()),
        ("transfer-encoding", b"chunked"),
        ("expect", b"100-continue"),
    ] {
        let mut values = headers();
        values.push(http::Header { name, value });
        assert!(validate(&values).is_err());
    }
    let values = headers();
    for (method, version) in [
        ("POST", http::Version::Http11),
        ("GET", http::Version::Http10),
    ] {
        assert!(
            ws::Handshake::validate(http::RequestHead {
                method,
                target: "/",
                version,
                headers: &values
            })
            .is_err()
        );
    }
    let mut values = headers();
    values.push(http::Header {
        name: "content-length",
        value: b"0",
    });
    assert!(validate(&values).is_ok());
}
