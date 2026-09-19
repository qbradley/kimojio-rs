use super::*;

enum Output {
    Io(ClientId, Io),
    Cancel(ClientId, Identity),
    Retired(ClientId),
}
struct Root;
impl Ports for Root {
    type Output = Output;
    fn io(&mut self, client: ClientId, op: Io) -> Option<Output> {
        Some(Output::Io(client, op))
    }
    fn cancel(&mut self, client: ClientId, op: Identity) -> Option<Output> {
        Some(Output::Cancel(client, op))
    }
    fn deadline_changed(&mut self, _: Option<http::Tick>) -> Option<Output> {
        None
    }
    fn retired(&mut self, client: ClientId) -> Option<Output> {
        Some(Output::Retired(client))
    }
    fn yield_turn(&mut self) -> Option<Output> {
        None
    }
}

const REQUEST: &[u8] = b"GET /chat HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n";

#[test]
fn coalesced_input_survives_handoff_after_every_partial_101_write() {
    let mut chat = Chat::new(Config::default()).unwrap();
    let client = chat.admit().unwrap();
    let Some(Output::Io(read_client, Io::HttpRead(mut read))) = chat.next(&mut Root) else {
        panic!("read");
    };
    assert_eq!(read_client, client);
    let mut input = REQUEST.to_vec();
    input.extend([0x81, 0x82, 1, 2, 3, 4, b'h' ^ 1, b'i' ^ 2]);
    read.bytes_mut()[..input.len()].copy_from_slice(&input);
    chat.complete(client, Completion::HttpRead(read.complete(Ok(input.len()))))
        .unwrap();
    let mut handshake = Vec::new();
    let mut echo = Vec::new();
    let mut pending_read = None;
    for _ in 0..200 {
        match chat.next(&mut Root) {
            Some(Output::Io(_, Io::HttpWrite(write))) => {
                assert_eq!(chat.stats().active_clients, 0);
                let bytes: Vec<u8> = write.slices().concat();
                let count = bytes.len().min(3);
                handshake.extend_from_slice(&bytes[..count]);
                chat.complete(client, Completion::HttpWrite(write.complete(Ok(count))))
                    .unwrap();
            }
            Some(Output::Io(_, Io::WsWrite(write))) => {
                assert!(handshake.starts_with(b"HTTP/1.1 101 "));
                assert!(handshake.ends_with(b"\r\n\r\n"));
                echo.extend(write.slices().concat());
                let count = write.slices().iter().map(|bytes| bytes.len()).sum();
                chat.complete(client, Completion::WsWrite(write.complete(Ok(count))))
                    .unwrap();
            }
            Some(Output::Io(_, Io::WsRead(read))) => pending_read = Some(read),
            None if !echo.is_empty() => break,
            None => panic!("stalled before echo"),
            _ => panic!("unexpected transition"),
        }
    }
    assert_eq!(echo, b"\x81\x02hi");
    assert_eq!(chat.stats().active_clients, 1);
    chat.shutdown(true);
    let mut saw_cancel = false;
    let mut retired = false;
    for _ in 0..30 {
        match chat.next(&mut Root) {
            Some(Output::Cancel(id, Identity::WebSocket(target))) => {
                assert_eq!(id, client);
                let read = pending_read.take().unwrap();
                assert_eq!(target, read.id());
                saw_cancel = true;
                chat.complete(
                    client,
                    Completion::WsRead(read.complete(Err(http::IoError {
                        kind: http::IoErrorKind::Cancelled,
                        code: None,
                    }))),
                )
                .unwrap();
            }
            Some(Output::Io(_, Io::WsClose(close))) => {
                assert!(saw_cancel);
                chat.complete(client, Completion::WsClose(close.complete(Ok(()))))
                    .unwrap();
            }
            Some(Output::Retired(id)) => {
                assert_eq!(id, client);
                retired = true;
                break;
            }
            None => {}
            _ => panic!("new operation after abort"),
        }
    }
    assert!(retired);
    assert_eq!(chat.stats().resident_clients, 0);
}

fn upgraded(chat: &mut Chat) -> ClientId {
    let id = chat.admit().unwrap();
    // Test fixture starts at the framing boundary; production only uses the
    // owned HTTP handoff path exercised by the preceding test.
    chat.client_mut(id).unwrap().phase = Phase::WebSocket(Box::new(
        WebSocket::new(
            http::ConnectionId {
                slot: id.slot() as u64,
                generation: id.generation(),
            },
            chat.config.websocket.clone(),
            vec![0; chat.config.receive_bytes].into_boxed_slice(),
            http::Tick(0),
        )
        .unwrap(),
    ));
    chat.hub.activate(id).unwrap();
    id
}

#[test]
fn typed_ws_send_rejection_after_observed_credit_returns_payload_to_hub() {
    let mut chat = Chat::new(Config::default()).unwrap();
    let closing = upgraded(&mut chat);
    let healthy = upgraded(&mut chat);
    chat.hub.begin(healthy, hub::Kind::Text).unwrap();
    chat.hub.append(healthy, b"whole").unwrap();
    chat.hub.finish(healthy).unwrap();
    let Some(HubEvent::Send(delivery)) = chat.hub.next(&mut HubPorts) else {
        panic!("send");
    };
    assert_eq!(delivery.id.client(), closing);
    let Phase::WebSocket(server) = &mut chat.client_mut(closing).unwrap().phase else {
        panic!()
    };
    assert!(server.can_send());
    server
        .close(ws::CloseReason::new(1000, "").unwrap())
        .unwrap();
    assert!(!server.can_send());
    chat.deliver(delivery);
    assert!(chat.client_mut(closing).unwrap().delivery.is_none());
    assert_eq!(chat.hub.stats().active_clients, 1);
    let Some(HubEvent::Send(next)) = chat.hub.next(&mut HubPorts) else {
        panic!("healthy send");
    };
    assert_eq!(next.id.client(), healthy);
    chat.deliver(next);
    assert!(chat.client_mut(healthy).unwrap().delivery.is_some());
    assert_eq!(chat.hub.stats().resident_clients, 2);
}

#[test]
fn invalid_websocket_configuration_is_rejected_before_admission() {
    let mut config = Config::default();
    config.websocket.outgoing_frame_bytes = 0;
    assert!(matches!(Chat::new(config), Err(hub::Error::InvalidConfig)));
}

#[test]
fn outbound_buffer_limit_must_fit_every_admissible_hub_message() {
    let mut config = Config::default();
    config.websocket.max_buffer_bytes = config.receive_bytes;
    assert!(matches!(
        Chat::new(config.clone()),
        Err(hub::Error::InvalidConfig)
    ));
    config.websocket.max_buffer_bytes = config.hub.max_message_bytes;
    let mut chat = Chat::new(config).unwrap();
    let sender = upgraded(&mut chat);
    let recipient = upgraded(&mut chat);
    chat.hub.begin(sender, hub::Kind::Binary).unwrap();
    chat.hub.append(sender, &[0x5a; 20_000]).unwrap();
    assert_eq!(chat.hub.finish(sender).unwrap().recipients, 2);
    for id in [sender, recipient] {
        let Some(HubEvent::Send(delivery)) = chat.hub.next(&mut HubPorts) else {
            panic!("complete-message delivery");
        };
        assert_eq!(delivery.id.client(), id);
        chat.deliver(delivery);
        assert!(chat.client_mut(id).unwrap().delivery.is_some());
    }
    assert_eq!(chat.hub.stats().active_clients, 2);
}

#[test]
fn wrong_generation_completion_is_returned_owned_without_touching_live_http() {
    let mut first = Chat::new(Config::default()).unwrap();
    let mut second = Chat::new(Config::default()).unwrap();
    let id = first.admit().unwrap();
    let old = second.admit().unwrap();
    second.hub.closed(old).unwrap();
    second.connections[old.slot()] = None;
    let replacement = second.admit().unwrap();
    let Some(Output::Io(_, Io::HttpRead(read))) = first.next(&mut Root) else {
        panic!()
    };
    let returned = second
        .complete(id, Completion::HttpRead(read.complete(Ok(0))))
        .unwrap_err();
    assert_eq!(second.client_mut(replacement).unwrap().id, replacement);
    first.complete(id, returned).unwrap();
}
