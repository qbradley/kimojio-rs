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
    chat.client_mut(id).unwrap().phase = Phase::WebSocket {
        server: Box::new(
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
        ),
        delivery: None,
    };
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
    let Phase::WebSocket { server, .. } = &mut chat.client_mut(closing).unwrap().phase else {
        panic!()
    };
    assert!(server.can_send());
    server
        .close(ws::CloseReason::new(1000, "").unwrap())
        .unwrap();
    assert!(!server.can_send());
    chat.deliver(delivery);
    assert!(matches!(
        chat.client_mut(closing).unwrap().phase,
        Phase::WebSocket { delivery: None, .. }
    ));
    assert_eq!(chat.hub.stats().active_clients, 1);
    let Some(HubEvent::Send(next)) = chat.hub.next(&mut HubPorts) else {
        panic!("healthy send");
    };
    assert_eq!(next.id.client(), healthy);
    chat.deliver(next);
    assert!(matches!(
        chat.client_mut(healthy).unwrap().phase,
        Phase::WebSocket {
            delivery: Some(_),
            ..
        }
    ));
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
        assert!(matches!(
            chat.client_mut(id).unwrap().phase,
            Phase::WebSocket {
                delivery: Some(_),
                ..
            }
        ));
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

#[derive(Debug, Eq, PartialEq)]
enum ModelEvent {
    Io(Identity),
    Cancel(Identity),
    Deadline(Option<http::Tick>),
    Retired(ClientId),
}

struct ModelRoot {
    yielding: bool,
    operations: Vec<Io>,
    trace: Vec<ModelEvent>,
}

impl Ports for ModelRoot {
    type Output = ();
    fn io(&mut self, _: ClientId, operation: Io) -> Option<()> {
        self.trace.push(ModelEvent::Io(operation.identity()));
        self.operations.push(operation);
        self.yielding.then_some(())
    }
    fn cancel(&mut self, _: ClientId, identity: Identity) -> Option<()> {
        self.trace.push(ModelEvent::Cancel(identity));
        self.yielding.then_some(())
    }
    fn deadline_changed(&mut self, deadline: Option<http::Tick>) -> Option<()> {
        self.trace.push(ModelEvent::Deadline(deadline));
        self.yielding.then_some(())
    }
    fn retired(&mut self, client: ClientId) -> Option<()> {
        self.trace.push(ModelEvent::Retired(client));
        self.yielding.then_some(())
    }
    fn yield_turn(&mut self) -> Option<()> {
        self.yielding.then_some(())
    }
}

fn drain(chat: &mut Chat, root: &mut ModelRoot) {
    for _ in 0..32 {
        if chat.next(root).is_none() {
            assert!(chat.ready.is_empty());
            return;
        }
    }
    panic!("one-connection composition did not quiesce");
}

fn cancellation_schedule(schedule: [u8; 3], success: bool, yielding: bool) -> Vec<ModelEvent> {
    let mut chat = Chat::new(Config::default()).unwrap();
    let baseline = chat.stats().used_bytes;
    let id = upgraded(&mut chat);
    chat.hub.begin(id, hub::Kind::Binary).unwrap();
    chat.hub.append(id, b"lease").unwrap();
    chat.hub.finish(id).unwrap();
    chat.mark_hub();
    let mut root = ModelRoot {
        yielding,
        operations: Vec::new(),
        trace: Vec::new(),
    };
    drain(&mut chat, &mut root);
    assert_eq!(
        root.operations.len(),
        2,
        "duplex read and write before settlement"
    );
    let read_position = root
        .operations
        .iter()
        .position(|op| matches!(op, Io::WsRead(_)))
        .unwrap();
    let Io::WsRead(read) = root.operations.remove(read_position) else {
        unreachable!()
    };
    let Io::WsWrite(write) = root.operations.pop().unwrap() else {
        panic!("write")
    };
    let read_id = Identity::WebSocket(read.id());
    let write_id = Identity::WebSocket(write.id());
    let length = write.slices().iter().map(|slice| slice.len()).sum();
    let mut read = Some(read);
    let mut write = Some(write);
    for action in schedule {
        match action {
            0 => chat.shutdown(true),
            1 => chat
                .complete(
                    id,
                    Completion::WsRead(read.take().unwrap().complete(Err(http::IoError {
                        code: None,
                        kind: http::IoErrorKind::Cancelled,
                    }))),
                )
                .unwrap(),
            2 => chat
                .complete(
                    id,
                    Completion::WsWrite(write.take().unwrap().complete(if success {
                        Ok(length)
                    } else {
                        Err(http::IoError {
                            code: None,
                            kind: http::IoErrorKind::Reset,
                        })
                    })),
                )
                .unwrap(),
            _ => unreachable!(),
        }
        let before = root.trace.len();
        drain(&mut chat, &mut root);
        for event in &root.trace[before..] {
            match event {
                ModelEvent::Cancel(target) if *target == read_id => assert!(read.is_some()),
                ModelEvent::Cancel(target) if *target == write_id => assert!(write.is_some()),
                ModelEvent::Cancel(_) => panic!("cancellation must target an original operation"),
                ModelEvent::Retired(_) => panic!("retirement before transport settlement"),
                _ => {}
            }
        }
        assert_eq!(chat.stats().resident_clients, 1);
        for operation in &root.operations {
            assert!(
                matches!(operation, Io::WsClose(_)),
                "no replacement I/O on failed transport"
            );
            assert!(
                read.is_none() && write.is_none(),
                "close joins both originals"
            );
        }
    }
    assert_eq!(root.operations.len(), 1);
    let Io::WsClose(close) = root.operations.pop().unwrap() else {
        unreachable!()
    };
    assert!(
        matches!(
            chat.client_mut(id).unwrap().phase,
            Phase::WebSocket { delivery: None, .. }
        ),
        "payload receipt precedes transport retirement"
    );
    chat.complete(id, Completion::WsClose(close.complete(Ok(()))))
        .unwrap();
    drain(&mut chat, &mut root);
    assert_eq!(
        root.trace
            .iter()
            .filter(|e| matches!(e, ModelEvent::Retired(_)))
            .count(),
        1
    );
    assert_eq!(root.trace.last(), Some(&ModelEvent::Retired(id)));
    for identity in [read_id, write_id] {
        assert!(
            root.trace
                .iter()
                .filter(|e| **e == ModelEvent::Cancel(identity))
                .count()
                <= 1
        );
    }
    assert!(chat.deadline().is_none());
    assert_eq!(chat.stats().resident_clients, 0);
    assert_eq!(chat.stats().used_bytes, baseline);
    root.trace
}

#[test]
fn bounded_composite_settlement_preserves_exact_yield_traces() {
    for schedule in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        for success in [false, true] {
            assert_eq!(
                cancellation_schedule(schedule, success, false),
                cancellation_schedule(schedule, success, true),
                "schedule {schedule:?}, success={success}"
            );
        }
    }
}

#[test]
fn shutdown_at_completed_handshake_never_reactivates_a_removed_recipient() {
    for abort in [false, true] {
        for yielding in [false, true] {
            let mut chat = Chat::new(Config::default()).unwrap();
            let baseline = chat.stats().used_bytes;
            let id = chat.admit().unwrap();
            let mut root = ModelRoot {
                yielding,
                operations: Vec::new(),
                trace: Vec::new(),
            };
            drain(&mut chat, &mut root);
            let Io::HttpRead(mut read) = root.operations.pop().unwrap() else {
                panic!("HTTP read")
            };
            read.bytes_mut()[..REQUEST.len()].copy_from_slice(REQUEST);
            chat.complete(id, Completion::HttpRead(read.complete(Ok(REQUEST.len()))))
                .unwrap();
            drain(&mut chat, &mut root);
            let Io::HttpWrite(write) = root.operations.pop().unwrap() else {
                panic!("101 write")
            };
            let count = write.slices().iter().map(|s| s.len()).sum();
            chat.complete(id, Completion::HttpWrite(write.complete(Ok(count))))
                .unwrap();
            chat.shutdown(abort);
            drain(&mut chat, &mut root);
            assert_eq!(chat.stats().active_clients, 0);
            assert!(
                matches!(chat.client_mut(id).unwrap().phase, Phase::Http(_)),
                "HTTP shutdown owns a not-yet-transferred handoff"
            );
            chat.shutdown(true);
            drain(&mut chat, &mut root);
            assert_eq!(root.operations.len(), 1);
            let Io::HttpClose(close) = root.operations.pop().unwrap() else {
                panic!("only HTTP close after completed handshake shutdown")
            };
            chat.complete(id, Completion::HttpClose(close.complete(Ok(()))))
                .unwrap();
            drain(&mut chat, &mut root);
            assert_eq!(chat.stats().resident_clients, 0);
            assert_eq!(chat.stats().used_bytes, baseline);
            assert!(root.operations.is_empty());
        }
    }
}

#[test]
fn late_deadline_from_retired_generation_cannot_expire_reused_slot() {
    let mut chat = Chat::new(Config::default()).unwrap();
    let old = chat.admit().unwrap();
    let mut root = ModelRoot {
        yielding: false,
        operations: Vec::new(),
        trace: Vec::new(),
    };
    drain(&mut chat, &mut root);
    let stale = chat.deadlines[0];
    let Io::HttpRead(read) = root.operations.pop().unwrap() else {
        panic!("old read")
    };
    chat.shutdown(true);
    chat.complete(old, Completion::HttpRead(read.complete(Ok(0))))
        .unwrap();
    drain(&mut chat, &mut root);
    let Io::HttpClose(close) = root.operations.pop().unwrap() else {
        panic!("old close")
    };
    chat.complete(old, Completion::HttpClose(close.complete(Ok(()))))
        .unwrap();
    drain(&mut chat, &mut root);
    assert!(chat.deadlines.is_empty());

    let now = http::Tick(stale.deadline.at().0 + 1);
    chat.observe_time(now);
    let new = chat.admit().unwrap();
    assert_eq!(old.slot(), new.slot());
    assert_ne!(old.generation(), new.generation());
    drain(&mut chat, &mut root);
    let next_deadline = chat.deadline();
    assert!(next_deadline.is_some_and(|at| at > now));
    chat.schedule(old, Some(stale.deadline));
    let before = root.trace.len();
    chat.expire_due(now);
    drain(&mut chat, &mut root);
    assert_eq!(chat.deadline(), next_deadline);
    assert!(
        root.trace[before..]
            .iter()
            .all(|event| matches!(event, ModelEvent::Deadline(_)))
    );
    assert_eq!(root.operations.len(), 1);
    assert_eq!(chat.client_mut(new).unwrap().id, new);
    assert!(chat.ready.is_empty());
}
