mod support;
use kimojio_fsm_http1::*;
use support::*;

const REQUEST: &[u8] = b"GET / HTTP/1.1\r\nHost: a\r\nConnection: upgrade\r\nUpgrade: websocket\r\n\r\n\x81\x02hi\x00\xff";
const PREFIX: &[u8] = b"\x81\x02hi\x00\xff";

fn upgrade() -> ResponseHead<'static> {
    ResponseHead::new(
        101,
        "Switching Protocols",
        &[
            Header {
                name: "connection",
                value: b"upgrade",
            },
            Header {
                name: "upgrade",
                value: b"websocket",
            },
        ],
    )
}

fn timed() -> Config {
    Config {
        head_timeout_ns: Some(100),
        body_timeout_ns: Some(10),
        ..config()
    }
}

fn ready(mut server: Server<B>) -> (Server<B>, ExchangeId, Option<Deadline>) {
    let mut id = None;
    let mut deadline = None;
    loop {
        match server.next(&mut Capture).expect("handshake must progress") {
            Event::Read(op) => server.complete_read(fill(op, REQUEST)).unwrap(),
            Event::Request(exchange, _) => {
                id = Some(exchange);
                server.accept_upgrade(exchange, upgrade()).unwrap();
            }
            Event::Write(op) => server.complete_write(finish_write(op)).unwrap(),
            Event::Deadline(value) => deadline = value.or(deadline),
            Event::Incoming(_) => {}
            Event::Upgrade(exchange) => {
                assert_eq!(Some(exchange), id);
                assert_eq!(server.buffered_input(), PREFIX);
                return (server, exchange, deadline);
            }
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn terminal_commands_revoke_ready_handoff_before_close_issue_and_after_close_completion() {
    for terminal in 0..5 {
        let (mut server, id, _) = ready(server(timed()));
        let failure = match terminal {
            0 => {
                server.shutdown(ShutdownMode::Abort);
                Failure::Cancelled
            }
            1 => {
                server.shutdown(ShutdownMode::Graceful);
                Failure::Cancelled
            }
            2 => {
                server.cancel_exchange(id).unwrap();
                Failure::Cancelled
            }
            3 => {
                server.fail_source(id, Failure::Protocol).unwrap();
                Failure::Protocol
            }
            _ => {
                server.fail_source(id, Failure::Timeout).unwrap();
                Failure::Timeout
            }
        };
        assert_eq!(server.take_upgrade().unwrap_err(), CommandError::NotReady);
        let Some(Event::Finished(result)) = next_server(&mut server) else {
            panic!()
        };
        assert_eq!(result.result, Err(failure));
        assert!(!result.reusable);
        let Some(Event::Close(op)) = next_server(&mut server) else {
            panic!("close must not become a second response or handoff")
        };
        assert_eq!(server.take_upgrade().unwrap_err(), CommandError::NotReady);
        assert!(next_server(&mut server).is_none());
        server.complete_close(op.complete(Ok(()))).unwrap();
        assert_eq!(server.take_upgrade().unwrap_err(), CommandError::NotReady);
        assert!(
            matches!(next_server(&mut server), Some(Event::Closed(Err(error))) if error == failure)
        );
        assert_eq!(server.take_upgrade().unwrap_err(), CommandError::NotReady);
        assert!(next_server(&mut server).is_none());
    }
}

#[test]
fn untouched_delayed_handoff_preserves_exact_bytes_and_excludes_all_later_close_authority() {
    let (mut server, id, former_deadline) = ready(server(timed()));
    let former_deadline = former_deadline.unwrap();
    assert_eq!(
        server.expire(former_deadline, Tick(1000)),
        Err(CommandError::StaleDeadline)
    );
    server.observe_time(Tick(1000)).unwrap();
    for _ in 0..3 {
        assert!(next_server(&mut server).is_none());
        assert_eq!(server.buffered_input(), PREFIX);
    }
    let rejected = server
        .send_body(SendBody {
            exchange: id,
            buffer: vec![7],
            range: 0..1,
            end: true,
        })
        .unwrap_err();
    assert_eq!(rejected.value.buffer, [7]);
    let handoff = server.take_upgrade().unwrap();
    assert_eq!(handoff.connection, server.connection_id());
    assert_eq!(handoff.buffered.bytes(), PREFIX);
    assert_eq!(server.take_upgrade().unwrap_err(), CommandError::NotReady);
    server.shutdown(ShutdownMode::Abort);
    server.shutdown(ShutdownMode::Graceful);
    assert_eq!(server.cancel_exchange(id), Err(CommandError::InvalidState));
    assert_eq!(
        server.fail_source(id, Failure::Timeout),
        Err(CommandError::InvalidState)
    );
    assert!(
        next_server(&mut server).is_none(),
        "HTTP no longer owns close authority"
    );
    assert_eq!(handoff.buffered.bytes(), PREFIX);
}

#[test]
fn handshake_timeout_cannot_be_revived_by_late_successful_write_completion() {
    for accepted in [0, 1, usize::MAX] {
        let mut server = server(timed());
        let mut deadline = None;
        let mut exchange = None;
        let mut write = loop {
            match server.next(&mut Capture).unwrap() {
                Event::Deadline(value) => deadline = value.or(deadline),
                Event::Read(op) => server.complete_read(fill(op, REQUEST)).unwrap(),
                Event::Request(id, _) => {
                    exchange = Some(id);
                    server.accept_upgrade(id, upgrade()).unwrap();
                }
                Event::Write(op) => break Some(op),
                other => panic!("{other:?}"),
            }
        };
        let length: usize = write
            .as_ref()
            .unwrap()
            .slices()
            .iter()
            .map(|part| part.len())
            .sum();
        if accepted != 0 {
            server
                .complete_write(write.take().unwrap().complete(Ok(accepted.min(length))))
                .unwrap();
            if accepted < length {
                let Some(Event::Write(op)) = next_server(&mut server) else {
                    panic!()
                };
                write = Some(op);
            }
        }
        server.expire(deadline.unwrap(), Tick(10)).unwrap();
        assert_eq!(server.take_upgrade().unwrap_err(), CommandError::NotReady);
        if let Some(op) = write.as_ref() {
            let Some(Event::Cancel(cancel)) = next_server(&mut server) else {
                panic!()
            };
            assert_eq!(cancel.target, op.id());
        }
        let Some(Event::Finished(result)) = next_server(&mut server) else {
            panic!()
        };
        assert_eq!(Some(result.exchange), exchange);
        assert_eq!(result.result, Err(Failure::Timeout));
        if let Some(op) = write {
            assert!(next_server(&mut server).is_none());
            server.complete_write(finish_write(op)).unwrap();
        }
        assert_eq!(server.take_upgrade().unwrap_err(), CommandError::NotReady);
        let Some(Event::Close(op)) = next_server(&mut server) else {
            panic!()
        };
        server.complete_close(op.complete(Ok(()))).unwrap();
        assert_eq!(server.take_upgrade().unwrap_err(), CommandError::NotReady);
    }
}

#[test]
fn client_shutdown_excludes_handoff_before_and_after_upgrade_notification() {
    for before_response in [false, true] {
        let mut client = client(config());
        let id = client
            .request(get(
                "GET",
                BodyLength::Empty,
                false,
                &[
                    Header {
                        name: "host",
                        value: b"a",
                    },
                    Header {
                        name: "connection",
                        value: b"upgrade",
                    },
                    Header {
                        name: "upgrade",
                        value: b"websocket",
                    },
                ],
            ))
            .unwrap();
        let Some(Event::Write(op)) = next_client(&mut client) else {
            panic!()
        };
        client.complete_write(finish_write(op)).unwrap();
        let Some(Event::Read(op)) = next_client(&mut client) else {
            panic!()
        };
        if before_response {
            client.shutdown(ShutdownMode::Graceful);
        }
        client.complete_read(fill(op, b"HTTP/1.1 101 Switching Protocols\r\nConnection: upgrade\r\nUpgrade: websocket\r\n\r\n\x81\x02hi\x00\xff")).unwrap();
        assert!(matches!(
            next_client(&mut client),
            Some(Event::Response(_, 101, false))
        ));
        if !before_response {
            assert!(matches!(next_client(&mut client), Some(Event::Incoming(_))));
            assert!(
                matches!(next_client(&mut client), Some(Event::Upgrade(exchange)) if exchange == id)
            );
            assert_eq!(client.buffered_input(), PREFIX);
            client.shutdown(ShutdownMode::Abort);
        }
        assert_eq!(client.take_upgrade().unwrap_err(), CommandError::NotReady);
        assert!(
            matches!(next_client(&mut client), Some(Event::Finished(result)) if result.result == Err(Failure::Cancelled))
        );
        let Some(Event::Close(op)) = next_client(&mut client) else {
            panic!()
        };
        assert_eq!(client.take_upgrade().unwrap_err(), CommandError::NotReady);
        client.complete_close(op.complete(Ok(()))).unwrap();
        assert_eq!(client.take_upgrade().unwrap_err(), CommandError::NotReady);
    }
}

#[test]
fn graceful_shutdown_before_upgrade_acceptance_preserves_normal_response_path() {
    let mut server = server(config());
    let id = feed_server(&mut server, REQUEST);
    server.shutdown(ShutdownMode::Graceful);
    assert_eq!(
        server.accept_upgrade(id, upgrade()),
        Err(CommandError::InvalidState)
    );
    server
        .respond(
            id,
            Response::new(503, "Service Unavailable", &[], BodyLength::Empty),
        )
        .unwrap();
    let Some(Event::Write(op)) = next_server(&mut server) else {
        panic!()
    };
    assert!(op.slices().concat().starts_with(b"HTTP/1.1 503 "));
    server.complete_write(finish_write(op)).unwrap();
    assert!(matches!(next_server(&mut server), Some(Event::Incoming(_))));
    assert!(matches!(next_server(&mut server), Some(Event::Finished(_))));
    assert!(matches!(next_server(&mut server), Some(Event::Close(_))));
    assert_eq!(server.take_upgrade().unwrap_err(), CommandError::NotReady);
}
