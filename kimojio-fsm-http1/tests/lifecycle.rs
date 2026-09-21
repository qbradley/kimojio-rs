mod support;
use kimojio_fsm_http1::*;
use support::*;

fn timed() -> Config {
    Config {
        head_timeout_ns: Some(100),
        body_timeout_ns: Some(30),
        idle_timeout_ns: Some(200),
        continue_timeout_ns: Some(10),
        ..Config::default()
    }
}

#[test]
fn reused_server_head_deadline_does_not_depend_on_an_idle_timer() {
    for idle_timeout_ns in [None, Some(200)] {
        for pipelined in [false, true] {
            let mut machine = server(Config {
                head_timeout_ns: Some(100),
                idle_timeout_ns,
                ..config()
            });
            let Some(Event::Read(read)) = next_server(&mut machine) else {
                panic!("expected first read")
            };
            machine
                .complete_read(fill(
                    read,
                    if pipelined {
                        b"GET / HTTP/1.1\r\nHost: a\r\n\r\nG"
                    } else {
                        b"GET / HTTP/1.1\r\nHost: a\r\n\r\n"
                    },
                ))
                .unwrap();
            let Some(Event::Request(exchange, _)) = next_server(&mut machine) else {
                panic!("expected request")
            };
            machine
                .respond(exchange, Response::new(200, "OK", &[], BodyLength::Empty))
                .unwrap();
            loop {
                match next_server(&mut machine).unwrap() {
                    Event::Write(write) => machine.complete_write(finish_write(write)).unwrap(),
                    Event::Incoming(_) => {}
                    Event::Finished(finished) => {
                        assert!(finished.reusable);
                        break;
                    }
                    other => panic!("unexpected event {other:?}"),
                }
            }
            machine.observe_time(Tick(10)).unwrap();
            if !pipelined {
                let Some(Event::Read(read)) = next_server(&mut machine) else {
                    panic!("expected reused read")
                };
                machine.complete_read(fill(read, b"G")).unwrap();
            }
            let mut deadlines = Vec::new();
            while let Some(event) = machine.next(&mut Capture) {
                match event {
                    Event::Deadline(Some(deadline)) => deadlines.push(deadline),
                    Event::Deadline(None) => {}
                    Event::Read(_) => break,
                    other => panic!("unexpected event {other:?}"),
                }
            }
            let head = deadlines.last().expect("missing head deadline");
            assert_eq!(head.at, Tick(110));
            machine.expire(*head, Tick(110)).unwrap();
        }
    }
}

fn deadline_client(client: &mut Client<B>) -> Deadline {
    let Some(Event::Deadline(Some(deadline))) = client.next(&mut Capture) else {
        panic!("missing deadline")
    };
    deadline
}

#[test]
fn idle_head_body_and_continue_deadlines_keep_their_own_policies() {
    let mut client = client(timed());
    let idle = deadline_client(&mut client);
    assert_eq!(idle.at, Tick(200));
    let id = client
        .request(get(
            "POST",
            BodyLength::Known(3),
            true,
            &[Header {
                name: "host",
                value: b"a",
            }],
        ))
        .unwrap();
    let head = deadline_client(&mut client);
    assert_eq!(head.at, Tick(100));
    assert_eq!(
        client.expire(idle, Tick(200)),
        Err(CommandError::StaleDeadline)
    );
    assert_eq!(
        client.expire(head, Tick(99)),
        Err(CommandError::EarlyDeadline)
    );
    let Some(Event::Write(op)) = next_client(&mut client) else {
        panic!()
    };
    client.complete_write(finish_write(op)).unwrap();
    let continuation = deadline_client(&mut client);
    assert_eq!(continuation.at, Tick(10));
    let Some(Event::Read(read)) = next_client(&mut client) else {
        panic!()
    };
    assert!(client.next(&mut Capture).is_none());
    client.expire(continuation, Tick(10)).unwrap();
    assert_eq!(
        deadline_client(&mut client).at,
        Tick(30),
        "continue fallback must preserve the independent upload-progress deadline"
    );
    assert!(matches!(next_client(&mut client), Some(Event::Demand(exchange, 3)) if exchange == id));
    client
        .send_body(SendBody {
            exchange: id,
            buffer: b"abc".to_vec(),
            range: 0..3,
            end: true,
        })
        .unwrap();
    let Some(Event::Write(op)) = next_client(&mut client) else {
        panic!()
    };
    client.complete_write(finish_write(op)).unwrap();
    assert!(matches!(next_client(&mut client), Some(Event::Sent(_))));
    client.observe_time(Tick(20)).unwrap();
    client
        .complete_read(fill(read, b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\n"))
        .unwrap();
    assert!(matches!(
        next_client(&mut client),
        Some(Event::Response(_, 200, false))
    ));
    let body = deadline_client(&mut client);
    assert_eq!(body.at, Tick(50));
    client.grant_body_credit(id, 3).unwrap();
    let Some(Event::Read(op)) = next_client(&mut client) else {
        panic!()
    };
    client.observe_time(Tick(25)).unwrap();
    client.complete_read(fill(op, b"abc")).unwrap();
    let progressed = deadline_client(&mut client);
    assert_eq!(progressed.at, Tick(55));
    assert_eq!(
        client.expire(body, Tick(50)),
        Err(CommandError::StaleDeadline)
    );
    let Some(Event::Body(op)) = next_client(&mut client) else {
        panic!()
    };
    client.release_body(op.release(3)).unwrap();
    assert!(matches!(next_client(&mut client), Some(Event::Incoming(_))));
    assert!(matches!(next_client(&mut client), Some(Event::Finished(_))));
    let idle = deadline_client(&mut client);
    assert_eq!(idle.at, Tick(225));
    client.expire(idle, Tick(225)).unwrap();
    let Some(Event::Close(op)) = next_client(&mut client) else {
        panic!()
    };
    client.complete_close(op.complete(Ok(()))).unwrap();
    assert!(matches!(
        next_client(&mut client),
        Some(Event::Closed(Err(Failure::Timeout)))
    ));
}

#[test]
fn head_timeout_is_not_a_continue_fallback_and_stale_time_does_not_advance_clock() {
    let mut client = client(Config {
        head_timeout_ns: Some(5),
        continue_timeout_ns: Some(10),
        ..timed()
    });
    let old = deadline_client(&mut client);
    client
        .request(get(
            "POST",
            BodyLength::Known(1),
            true,
            &[Header {
                name: "host",
                value: b"a",
            }],
        ))
        .unwrap();
    let head = deadline_client(&mut client);
    assert_eq!(
        client.expire(old, Tick(999)),
        Err(CommandError::StaleDeadline)
    );
    client.observe_time(Tick(1)).unwrap();
    let Some(Event::Write(op)) = next_client(&mut client) else {
        panic!()
    };
    client.complete_write(finish_write(op)).unwrap();
    let Some(Event::Read(read)) = next_client(&mut client) else {
        panic!()
    };
    client.expire(head, Tick(5)).unwrap();
    assert!(matches!(next_client(&mut client), Some(Event::Cancel(_))));
    let Some(Event::Finished(result)) = next_client(&mut client) else {
        panic!()
    };
    assert_eq!(result.result, Err(Failure::Timeout));
    assert!(next_client(&mut client).is_none());
    client
        .complete_read(read.complete(Err(IoError {
            kind: IoErrorKind::Cancelled,
            code: None,
        })))
        .unwrap();
    assert!(matches!(next_client(&mut client), Some(Event::Close(_))));
}

#[test]
fn server_idle_timer_becomes_head_timer_on_first_new_byte() {
    let mut server = server(timed());
    let id = feed_server(&mut server, b"GET / HTTP/1.1\r\nHost: a\r\n\r\n");
    server.respond(id, response(BodyLength::Empty)).unwrap();
    let Some(Event::Write(op)) = next_server(&mut server) else {
        panic!()
    };
    server.complete_write(finish_write(op)).unwrap();
    assert!(matches!(next_server(&mut server), Some(Event::Incoming(_))));
    assert!(matches!(next_server(&mut server), Some(Event::Finished(_))));
    let Some(Event::Deadline(Some(idle))) = server.next(&mut Capture) else {
        panic!()
    };
    assert_eq!(idle.at, Tick(200));
    let Some(Event::Read(op)) = next_server(&mut server) else {
        panic!()
    };
    server.observe_time(Tick(150)).unwrap();
    server.complete_read(fill(op, b"G")).unwrap();
    let Some(Event::Deadline(Some(head))) = server.next(&mut Capture) else {
        panic!()
    };
    assert_eq!(head.at, Tick(250));
    assert_eq!(
        server.expire(idle, Tick(200)),
        Err(CommandError::StaleDeadline)
    );
    server.expire(head, Tick(250)).unwrap();
    let Some(Event::Write(op)) = next_server(&mut server) else {
        panic!()
    };
    assert!(op.slices().concat().starts_with(b"HTTP/1.1 408 "));
    server.complete_write(finish_write(op)).unwrap();
    assert!(matches!(next_server(&mut server), Some(Event::Close(_))));
}

#[test]
fn early_final_upload_cross_product_never_reissues_unaccepted_suffix() {
    for wait_continue in [false, true] {
        for partial in [0, 2] {
            for cancel_result in [
                None,
                Some(IoErrorKind::Cancelled),
                Some(IoErrorKind::CancelledUnknownProgress),
            ] {
                let mut client = client(config());
                let id = client
                    .request(get(
                        "POST",
                        BodyLength::Known(5),
                        wait_continue,
                        &[Header {
                            name: "host",
                            value: b"a",
                        }],
                    ))
                    .unwrap();
                let Some(Event::Write(op)) = next_client(&mut client) else {
                    panic!()
                };
                client.complete_write(finish_write(op)).unwrap();
                let mut read = None;
                let mut demand = false;
                while let Some(event) = next_client(&mut client) {
                    match event {
                        Event::Read(op) => read = Some(op),
                        Event::Demand(_, _) => demand = true,
                        other => panic!("{other:?}"),
                    }
                }
                assert_eq!(demand, !wait_continue);
                let mut body_write = None;
                if !wait_continue {
                    client
                        .send_body(SendBody {
                            exchange: id,
                            buffer: b"hello".to_vec(),
                            range: 0..5,
                            end: true,
                        })
                        .unwrap();
                    let Some(Event::Write(op)) = next_client(&mut client) else {
                        panic!()
                    };
                    if partial != 0 {
                        client.complete_write(op.complete(Ok(partial))).unwrap();
                        let Some(Event::Write(op)) = next_client(&mut client) else {
                            panic!()
                        };
                        body_write = Some(op);
                    } else {
                        body_write = Some(op);
                    }
                }
                client
                    .complete_read(fill(
                        read.unwrap(),
                        b"HTTP/1.1 413 Content Too Large\r\nContent-Length: 2\r\n\r\nno",
                    ))
                    .unwrap();
                assert!(matches!(
                    next_client(&mut client),
                    Some(Event::Response(_, 413, false))
                ));
                if let Some(op) = body_write {
                    let Some(Event::Cancel(cancel)) = next_client(&mut client) else {
                        panic!()
                    };
                    assert_eq!(cancel.target, op.id());
                    let completion = if let Some(kind) = cancel_result {
                        op.complete(Err(IoError { kind, code: None }))
                    } else {
                        op.complete(Ok(1))
                    };
                    client.complete_write(completion).unwrap();
                    let Some(Event::Sent(sent)) = next_client(&mut client) else {
                        panic!()
                    };
                    assert_eq!(sent.result, Err(Failure::EarlyResponse));
                    assert_eq!(
                        sent.accepted,
                        partial + usize::from(cancel_result.is_none())
                    );
                    assert_eq!(
                        sent.acceptance,
                        if cancel_result == Some(IoErrorKind::CancelledUnknownProgress) {
                            Acceptance::LowerBound
                        } else {
                            Acceptance::Exact
                        }
                    );
                }
                client.grant_body_credit(id, 2).unwrap();
                let Some(Event::Body(op)) = next_client(&mut client) else {
                    panic!()
                };
                assert_eq!(op.bytes(), b"no");
                client.release_body(op.release(2)).unwrap();
                assert!(matches!(next_client(&mut client), Some(Event::Incoming(_))));
                let Some(Event::Finished(result)) = next_client(&mut client) else {
                    panic!()
                };
                assert_eq!(result.result, Ok(()));
                assert!(!result.reusable);
                assert!(matches!(next_client(&mut client), Some(Event::Close(_))));
            }
        }
    }
}

#[test]
fn buffered_final_response_suppresses_a_queued_unsent_body() {
    let mut client = client(config());
    let id = client
        .request(get(
            "POST",
            BodyLength::Known(5),
            false,
            &[Header {
                name: "host",
                value: b"a",
            }],
        ))
        .unwrap();
    let Some(Event::Write(op)) = next_client(&mut client) else {
        panic!()
    };
    client.complete_write(finish_write(op)).unwrap();
    assert!(matches!(
        next_client(&mut client),
        Some(Event::Demand(_, _))
    ));
    let Some(Event::Read(read)) = next_client(&mut client) else {
        panic!()
    };
    client
        .send_body(SendBody {
            exchange: id,
            buffer: b"hello".to_vec(),
            range: 0..5,
            end: true,
        })
        .unwrap();
    client
        .complete_read(fill(
            read,
            b"HTTP/1.1 417 Expectation Failed\r\nContent-Length: 0\r\n\r\n",
        ))
        .unwrap();
    assert!(matches!(
        next_client(&mut client),
        Some(Event::Response(_, 417, false))
    ));
    let Some(Event::Sent(sent)) = next_client(&mut client) else {
        panic!("queued body reached transport")
    };
    assert_eq!(sent.accepted, 0);
    assert_eq!(sent.result, Err(Failure::EarlyResponse));
}

#[test]
fn continue_and_final_in_one_read_do_not_start_an_upload() {
    let mut client = client(config());
    client
        .request(get(
            "POST",
            BodyLength::Known(5),
            true,
            &[Header {
                name: "host",
                value: b"a",
            }],
        ))
        .unwrap();
    let Some(Event::Write(op)) = next_client(&mut client) else {
        panic!()
    };
    client.complete_write(finish_write(op)).unwrap();
    let Some(Event::Read(op)) = next_client(&mut client) else {
        panic!()
    };
    client
        .complete_read(fill(
            op,
            b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n",
        ))
        .unwrap();
    assert!(matches!(
        next_client(&mut client),
        Some(Event::Response(_, 100, true))
    ));
    assert!(matches!(
        next_client(&mut client),
        Some(Event::Response(_, 403, false))
    ));
    assert!(matches!(next_client(&mut client), Some(Event::Incoming(_))));
    assert!(matches!(next_client(&mut client), Some(Event::Finished(_))));
}

#[test]
fn final_response_can_follow_queued_or_inflight_continue_without_loss() {
    for inflight in [false, true] {
        let mut server = server(config());
        let id = feed_server(
            &mut server,
            b"POST / HTTP/1.1\r\nHost: a\r\nExpect: 100-continue\r\nContent-Length: 3\r\n\r\n",
        );
        server.grant_body_credit(id, 3).unwrap();
        let pending = if inflight {
            let Some(Event::Write(op)) = next_server(&mut server) else {
                panic!()
            };
            Some(op)
        } else {
            server
                .inform(id, ResponseHead::new(100, "Continue", &[]))
                .unwrap();
            None
        };
        server
            .respond(
                id,
                Response {
                    head: ResponseHead {
                        status: 403,
                        reason: "Forbidden",
                        ..response(BodyLength::Empty).head
                    },
                    body: BodyLength::Empty,
                },
            )
            .unwrap();
        let mut wire = Vec::new();
        if let Some(op) = pending {
            wire.extend(op.slices().concat());
            server.complete_write(finish_write(op)).unwrap();
        }
        loop {
            match next_server(&mut server) {
                Some(Event::Write(op)) => {
                    wire.extend(op.slices().concat());
                    server.complete_write(finish_write(op)).unwrap();
                }
                Some(Event::Finished(result)) => {
                    assert!(!result.reusable);
                    break;
                }
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(wire, b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
    }
}

#[test]
fn upgrade_waits_for_every_handshake_byte_and_preserves_received_suffix() {
    for partial in 1..8 {
        let mut server = server(config());
        let id = feed_server(&mut server, b"GET / HTTP/1.1\r\nHost: a\r\nConnection: upgrade\r\nUpgrade: websocket\r\n\r\n\x81\x02hi");
        let upgrade = ResponseHead {
            version: Version::Http11,
            status: 101,
            reason: "Switching Protocols",
            headers: &[
                Header {
                    name: "connection",
                    value: b"upgrade",
                },
                Header {
                    name: "upgrade",
                    value: b"websocket",
                },
            ],
        };
        let bad = ResponseHead {
            headers: &[
                Header {
                    name: "connection",
                    value: b"upgrade",
                },
                Header {
                    name: "upgrade",
                    value: b"other",
                },
            ],
            ..upgrade
        };
        assert_eq!(
            server.accept_upgrade(id, bad),
            Err(CommandError::InvalidHead)
        );
        server.accept_upgrade(id, upgrade).unwrap();
        assert_eq!(server.take_upgrade().unwrap_err(), CommandError::NotReady);
        let mut wire = Vec::new();
        loop {
            match next_server(&mut server) {
                Some(Event::Write(op)) => {
                    let bytes = op.slices().concat();
                    let n = partial.min(bytes.len());
                    wire.extend_from_slice(&bytes[..n]);
                    assert_eq!(server.take_upgrade().unwrap_err(), CommandError::NotReady);
                    server.complete_write(op.complete(Ok(n))).unwrap();
                }
                Some(Event::Incoming(_)) => {}
                Some(Event::Upgrade(exchange)) => {
                    assert_eq!(exchange, id);
                    break;
                }
                other => panic!("{other:?}"),
            }
        }
        assert!(wire.ends_with(b"upgrade: websocket\r\n\r\n"));
        assert_eq!(
            server.take_upgrade().unwrap().buffered.bytes(),
            b"\x81\x02hi"
        );
        assert!(next_server(&mut server).is_none());
        assert_eq!(server.take_upgrade().unwrap_err(), CommandError::NotReady);
    }
}

#[test]
fn invalid_body_finish_and_response_commands_leave_valid_work_intact() {
    let mut server = server(config());
    let id = feed_server(&mut server, b"GET / HTTP/1.1\r\nHost: a\r\n\r\n");
    let bad = Response {
        head: ResponseHead {
            reason: "OK\r\nx-injected: yes",
            ..response(BodyLength::Empty).head
        },
        body: BodyLength::Empty,
    };
    assert_eq!(server.respond(id, bad), Err(CommandError::InvalidHead));
    server.respond(id, response(BodyLength::Streaming)).unwrap();
    let Some(Event::Write(op)) = next_server(&mut server) else {
        panic!()
    };
    let len = op.slices().iter().map(|s| s.len()).sum::<usize>();
    let rejected = server.complete_write(op.complete(Ok(len + 1))).unwrap_err();
    assert_eq!(rejected.reason, RejectReason::InvalidCount);
    let (op, _) = rejected.value.into_parts();
    server.complete_write(finish_write(op)).unwrap();
    assert!(matches!(next_server(&mut server), Some(Event::Incoming(_))));
    assert!(matches!(
        next_server(&mut server),
        Some(Event::Demand(_, _))
    ));
    assert_eq!(
        server.finish_body(
            id,
            &[Header {
                name: "content-length",
                value: b"0"
            }]
        ),
        Err(CommandError::InvalidHead)
    );
    let command = SendBody {
        exchange: id,
        buffer: vec![1, 2],
        range: 0..3,
        end: true,
    };
    assert_eq!(server.send_body(command).unwrap_err().value.buffer, [1, 2]);
    server
        .finish_body(
            id,
            &[Header {
                name: "x-end",
                value: b"yes",
            }],
        )
        .unwrap();
    let Some(Event::Write(op)) = next_server(&mut server) else {
        panic!()
    };
    assert_eq!(op.slices().concat(), b"0\r\nx-end: yes\r\n\r\n");
    server.complete_write(finish_write(op)).unwrap();
    assert!(matches!(next_server(&mut server), Some(Event::Finished(_))));
}

#[test]
fn cancellation_settles_read_and_write_in_both_completion_orders() {
    for read_first in [false, true] {
        for write_success in [false, true] {
            let mut client = client(config());
            let id = client
                .request(get(
                    "POST",
                    BodyLength::Known(3),
                    false,
                    &[Header {
                        name: "host",
                        value: b"a",
                    }],
                ))
                .unwrap();
            let Some(Event::Write(write)) = next_client(&mut client) else {
                panic!()
            };
            let Some(Event::Read(read)) = next_client(&mut client) else {
                panic!()
            };
            client.cancel_exchange(id).unwrap();
            let mut targets = Vec::new();
            for _ in 0..2 {
                let Some(Event::Cancel(cancel)) = next_client(&mut client) else {
                    panic!()
                };
                targets.push(cancel.target);
            }
            assert!(targets.contains(&read.id()) && targets.contains(&write.id()));
            let Some(Event::Finished(result)) = next_client(&mut client) else {
                panic!()
            };
            assert_eq!(result.result, Err(Failure::Cancelled));
            assert!(next_client(&mut client).is_none());
            let error = IoError {
                kind: IoErrorKind::Cancelled,
                code: None,
            };
            let read = read.complete(Err(error));
            let write = if write_success {
                finish_write(write)
            } else {
                write.complete(Err(error))
            };
            if read_first {
                client.complete_read(read).unwrap();
                assert!(next_client(&mut client).is_none());
                client.complete_write(write).unwrap();
            } else {
                client.complete_write(write).unwrap();
                assert!(next_client(&mut client).is_none());
                client.complete_read(read).unwrap();
            }
            let Some(Event::Close(close)) = next_client(&mut client) else {
                panic!()
            };
            assert!(next_client(&mut client).is_none());
            client.complete_close(close.complete(Ok(()))).unwrap();
            assert!(matches!(
                next_client(&mut client),
                Some(Event::Closed(Err(Failure::Cancelled)))
            ));
            assert_eq!(
                client
                    .complete_close(close.complete(Ok(())))
                    .unwrap_err()
                    .reason,
                RejectReason::Stale
            );
            client.shutdown(ShutdownMode::Abort);
            assert!(next_client(&mut client).is_none());
        }
    }
}

#[test]
fn late_body_release_after_failure_does_not_revive_exchange() {
    let mut server = server(config());
    let id = feed_server(
        &mut server,
        b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 3\r\n\r\nabc",
    );
    server.grant_body_credit(id, 3).unwrap();
    let Some(Event::Body(body)) = next_server(&mut server) else {
        panic!()
    };
    server.fail_source(id, Failure::Application).unwrap();
    assert!(matches!(next_server(&mut server), Some(Event::Finished(_))));
    assert!(
        next_server(&mut server).is_none(),
        "close must wait for the body lease"
    );
    assert_eq!(
        server.grant_body_credit(id, 1),
        Err(CommandError::StaleExchange)
    );
    let rejected = server.release_body(body.release(4)).unwrap_err();
    assert_eq!(rejected.reason, RejectReason::InvalidCount);
    assert!(next_server(&mut server).is_none());
    let (body, _) = rejected.value.into_parts();
    server.release_body(body.release(0)).unwrap();
    assert!(matches!(next_server(&mut server), Some(Event::Close(_))));
}

#[test]
fn partial_body_release_preserves_tail_and_metadata_order() {
    let mut server = server(config());
    let id = feed_server(&mut server, b"POST / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\nX-End: yes\r\n\r\nNEXT");
    server.grant_body_credit(id, 3).unwrap();
    let Some(Event::Body(body)) = next_server(&mut server) else {
        panic!()
    };
    assert_eq!(body.bytes(), b"abc");
    server.release_body(body.release(1)).unwrap();
    assert!(next_server(&mut server).is_none());
    server.grant_body_credit(id, 2).unwrap();
    let Some(Event::Body(body)) = next_server(&mut server) else {
        panic!()
    };
    assert_eq!(body.bytes(), b"bc");
    server.release_body(body.release(2)).unwrap();
    let Some(Event::Trailers(trailers)) = next_server(&mut server) else {
        panic!()
    };
    assert_eq!(trailers, [("x-end".into(), b"yes".to_vec())]);
    assert!(matches!(next_server(&mut server), Some(Event::Incoming(_))));
    assert_eq!(server.buffered_input(), b"NEXT");
}

#[test]
fn server_eof_before_request_body_end_and_transport_reset_fail_explicitly() {
    for reset in [false, true] {
        let mut server = server(config());
        let id = feed_server(
            &mut server,
            b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 3\r\n\r\n",
        );
        server.grant_body_credit(id, 3).unwrap();
        let Some(Event::Read(op)) = next_server(&mut server) else {
            panic!()
        };
        let error = IoError {
            kind: IoErrorKind::Reset,
            code: Some(104),
        };
        server
            .complete_read(op.complete(if reset { Err(error) } else { Ok(0) }))
            .unwrap();
        if !reset {
            let Some(Event::Write(op)) = next_server(&mut server) else {
                panic!()
            };
            assert!(op.slices().concat().starts_with(b"HTTP/1.1 400 "));
            server.complete_write(finish_write(op)).unwrap();
        }
        let Some(Event::Finished(result)) = next_server(&mut server) else {
            panic!()
        };
        assert_eq!(
            result.result,
            Err(if reset {
                Failure::Transport(error)
            } else {
                Failure::UnexpectedEof
            })
        );
        assert!(matches!(next_server(&mut server), Some(Event::Close(_))));
    }
}

#[test]
fn graceful_shutdown_flushes_body_and_disallows_reuse() {
    let mut server = server(config());
    let id = feed_server(&mut server, b"GET / HTTP/1.1\r\nHost: a\r\n\r\n");
    server.respond(id, response(BodyLength::Known(3))).unwrap();
    let Some(Event::Write(op)) = next_server(&mut server) else {
        panic!()
    };
    server.complete_write(finish_write(op)).unwrap();
    assert!(matches!(next_server(&mut server), Some(Event::Incoming(_))));
    assert!(matches!(
        next_server(&mut server),
        Some(Event::Demand(_, 3))
    ));
    server.shutdown(ShutdownMode::Graceful);
    server
        .send_body(SendBody {
            exchange: id,
            buffer: b"abc".to_vec(),
            range: 0..3,
            end: true,
        })
        .unwrap();
    let Some(Event::Write(op)) = next_server(&mut server) else {
        panic!()
    };
    server.complete_write(finish_write(op)).unwrap();
    assert!(matches!(next_server(&mut server), Some(Event::Sent(_))));
    let Some(Event::Finished(result)) = next_server(&mut server) else {
        panic!()
    };
    assert_eq!(result.result, Ok(()));
    assert!(!result.reusable);
    assert!(matches!(next_server(&mut server), Some(Event::Close(_))));
}

#[test]
fn canceled_upgrade_cannot_transfer_transport() {
    let mut server = server(config());
    let id = feed_server(
        &mut server,
        b"CONNECT localhost:443 HTTP/1.1\r\nHost: localhost\r\n\r\ntunnel",
    );
    server
        .accept_upgrade(
            id,
            ResponseHead {
                version: Version::Http11,
                status: 200,
                reason: "Connection Established",
                headers: &[],
            },
        )
        .unwrap();
    let Some(Event::Write(op)) = next_server(&mut server) else {
        panic!()
    };
    assert_eq!(
        op.slices().concat(),
        b"HTTP/1.1 200 Connection Established\r\n\r\n"
    );
    server.complete_write(finish_write(op)).unwrap();
    assert!(matches!(next_server(&mut server), Some(Event::Incoming(_))));
    assert!(matches!(next_server(&mut server), Some(Event::Upgrade(_))));
    server.cancel_exchange(id).unwrap();
    assert_eq!(server.take_upgrade().unwrap_err(), CommandError::NotReady);
    assert!(matches!(next_server(&mut server), Some(Event::Finished(_))));
    assert!(matches!(next_server(&mut server), Some(Event::Close(_))));
}
