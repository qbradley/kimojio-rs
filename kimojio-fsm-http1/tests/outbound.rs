mod support;
use kimojio_fsm_http1::*;
use support::*;

fn collect_until_demand(server: &mut Server<B>, bytes: &mut Vec<u8>, step: usize) -> ExchangeId {
    loop {
        match next_server(server) {
            Some(Event::Write(op)) => {
                let offered = op.slices().concat();
                let n = offered.len().min(step);
                bytes.extend_from_slice(&offered[..n]);
                server.complete_write(op.complete(Ok(n))).unwrap();
            }
            Some(Event::Incoming(_)) => {}
            Some(Event::Demand(id, capacity)) => {
                assert!(capacity > 0);
                return id;
            }
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn chunk_prefix_payload_suffix_and_terminator_survive_all_short_write_sizes() {
    for step in 1..=35 {
        for trailers in [false, true] {
            let mut server = server(config());
            let id = feed_server(&mut server, b"GET / HTTP/1.1\r\nHost: a\r\n\r\n");
            server.respond(id, response(BodyLength::Streaming)).unwrap();
            let mut wire = Vec::new();
            assert_eq!(collect_until_demand(&mut server, &mut wire, step), id);
            for (index, payload) in [b"a".as_slice(), b"bc", b"012345678901234567890123456789012"]
                .into_iter()
                .enumerate()
            {
                server
                    .send_body(SendBody {
                        exchange: id,
                        buffer: payload.to_vec(),
                        range: 0..payload.len(),
                        end: index == 2 && !trailers,
                    })
                    .unwrap();
                loop {
                    match next_server(&mut server) {
                        Some(Event::Write(op)) => {
                            let offered = op.slices().concat();
                            let n = offered.len().min(step);
                            wire.extend_from_slice(&offered[..n]);
                            server.complete_write(op.complete(Ok(n))).unwrap();
                        }
                        Some(Event::Sent(sent)) => {
                            assert_eq!(sent.accepted, payload.len());
                            assert_eq!(sent.acceptance, Acceptance::Exact);
                            assert_eq!(sent.result, Ok(()));
                            assert_eq!(sent.buffer, payload);
                            break;
                        }
                        other => panic!("{other:?}"),
                    }
                }
                if index < 2 || trailers {
                    assert_eq!(collect_until_demand(&mut server, &mut wire, step), id);
                }
            }
            if trailers {
                server
                    .finish_body(
                        id,
                        &[Header {
                            name: "x-end",
                            value: b"yes",
                        }],
                    )
                    .unwrap();
            }
            loop {
                match next_server(&mut server) {
                    Some(Event::Write(op)) => {
                        let offered = op.slices().concat();
                        let n = offered.len().min(step);
                        wire.extend_from_slice(&offered[..n]);
                        server.complete_write(op.complete(Ok(n))).unwrap();
                    }
                    Some(Event::Finished(result)) => {
                        assert!(result.reusable);
                        break;
                    }
                    other => panic!("{other:?}"),
                }
            }
            let mut expected = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n1\r\na\r\n2\r\nbc\r\n21\r\n012345678901234567890123456789012\r\n0\r\n".to_vec();
            if trailers {
                expected.extend_from_slice(b"x-end: yes\r\n");
            }
            expected.extend_from_slice(b"\r\n");
            assert_eq!(wire, expected, "step={step} trailers={trailers}");
            let next_id = feed_server(&mut server, b"HEAD / HTTP/1.1\r\nHost: a\r\n\r\n");
            assert_ne!(id, next_id);
            server
                .respond(next_id, response(BodyLength::Known(100)))
                .unwrap();
            let Some(Event::Write(op)) = next_server(&mut server) else {
                panic!()
            };
            assert_eq!(
                op.slices().concat(),
                b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n"
            );
            server.complete_write(finish_write(op)).unwrap();
            assert!(matches!(next_server(&mut server), Some(Event::Incoming(_))));
            assert!(matches!(next_server(&mut server), Some(Event::Finished(_))));
        }
    }
}

#[test]
fn http10_persistence_and_close_delimited_streams_follow_wire_framing() {
    for keep_alive in [false, true] {
        for streaming in [false, true] {
            let mut server = server(config());
            let input = if keep_alive {
                b"GET / HTTP/1.0\r\nConnection: keep-alive\r\n\r\n".as_slice()
            } else {
                b"GET / HTTP/1.0\r\n\r\n"
            };
            let id = feed_server(&mut server, input);
            server
                .respond(
                    id,
                    Response {
                        head: ResponseHead {
                            version: Version::Http10,
                            ..response(BodyLength::Empty).head
                        },
                        body: if streaming {
                            BodyLength::Streaming
                        } else {
                            BodyLength::Known(0)
                        },
                    },
                )
                .unwrap();
            let Some(Event::Write(op)) = next_server(&mut server) else {
                panic!()
            };
            let wire = op.slices().concat();
            assert!(wire.starts_with(b"HTTP/1.0 200 OK\r\n"));
            if streaming || !keep_alive {
                assert!(wire.windows(19).any(|w| w == b"connection: close\r\n"));
            } else {
                assert!(wire.windows(24).any(|w| w == b"connection: keep-alive\r\n"));
            }
            server.complete_write(finish_write(op)).unwrap();
            assert!(matches!(next_server(&mut server), Some(Event::Incoming(_))));
            if streaming {
                assert!(matches!(
                    next_server(&mut server),
                    Some(Event::Demand(_, _))
                ));
                server.finish_body(id, &[]).unwrap();
            }
            let Some(Event::Finished(result)) = next_server(&mut server) else {
                panic!()
            };
            assert_eq!(result.reusable, keep_alive && !streaming);
        }
    }
}

#[test]
fn rejected_outgoing_storage_and_aggregate_limits_preserve_demand() {
    let mut server = server(Config {
        max_body_bytes: 3,
        ..config()
    });
    let id = feed_server(&mut server, b"GET / HTTP/1.1\r\nHost: a\r\n\r\n");
    server.respond(id, response(BodyLength::Streaming)).unwrap();
    let mut wire = Vec::new();
    collect_until_demand(&mut server, &mut wire, usize::MAX);
    let rejected = server
        .send_body(SendBody {
            exchange: id,
            buffer: vec![0; 65_537],
            range: 0..1,
            end: false,
        })
        .unwrap_err();
    assert_eq!(rejected.value.buffer.len(), 65_537);
    let rejected = server
        .send_body(SendBody {
            exchange: id,
            buffer: b"abcd".to_vec(),
            range: 0..4,
            end: true,
        })
        .unwrap_err();
    assert_eq!(rejected.reason, RejectReason::Limit);
    assert_eq!(rejected.value.buffer, b"abcd");
    assert!(
        next_server(&mut server).is_none(),
        "rejected commands must not duplicate demand"
    );
    server
        .send_body(SendBody {
            exchange: id,
            buffer: b"abc".to_vec(),
            range: 0..3,
            end: false,
        })
        .unwrap();
    let Some(Event::Write(op)) = next_server(&mut server) else {
        panic!()
    };
    server.complete_write(finish_write(op)).unwrap();
    assert!(matches!(next_server(&mut server), Some(Event::Sent(_))));
    assert!(matches!(
        next_server(&mut server),
        Some(Event::Demand(_, _))
    ));
    assert_eq!(
        server
            .send_body(SendBody {
                exchange: id,
                buffer: vec![1],
                range: 0..1,
                end: true
            })
            .unwrap_err()
            .reason,
        RejectReason::Limit
    );
    server.finish_body(id, &[]).unwrap();
}

#[test]
fn outgoing_head_limits_count_generated_headers_and_reject_before_acceptance() {
    let mut client = client(Config {
        max_headers: 1,
        ..config()
    });
    assert_eq!(
        client.request(get(
            "GET",
            BodyLength::Empty,
            false,
            &[Header {
                name: "host",
                value: b"a"
            }]
        )),
        Err(CommandError::Limit)
    );
    assert!(next_client(&mut client).is_none());
    let mut server = server(Config {
        max_head_bytes: 80,
        ..config()
    });
    let id = feed_server(&mut server, b"GET / HTTP/1.1\r\nHost: a\r\n\r\n");
    let huge = [b'a'; 81];
    let bad = Response {
        head: ResponseHead {
            headers: &[Header {
                name: "x-large",
                value: &huge,
            }],
            ..response(BodyLength::Empty).head
        },
        body: BodyLength::Empty,
    };
    assert_eq!(server.respond(id, bad), Err(CommandError::Limit));
    server.respond(id, response(BodyLength::Empty)).unwrap();
    assert!(matches!(next_server(&mut server), Some(Event::Write(_))));
}
