mod support;
use kimojio_fsm_http1::*;
use support::*;

fn command(exchange: ExchangeId) -> SendBody<B> {
    SendBody {
        exchange,
        buffer: b"_abc_".to_vec(),
        range: 1..4,
        end: true,
    }
}

fn eager_server() -> (Server<B>, ExchangeId, BodyId, WriteOp<B>) {
    let mut server = server(config());
    let exchange = feed_server(&mut server, b"GET / HTTP/1.1\r\nHost: a\r\n\r\n");
    server
        .respond(exchange, response(BodyLength::Known(3)))
        .unwrap();
    let body = command(exchange);
    let payload = body.buffer.as_ptr().wrapping_add(1);
    let id = server.send_body_eager(body).unwrap();
    let Some(Event::Write(op)) = next_server(&mut server) else {
        panic!("eager admission must not require a demand callback")
    };
    assert_eq!(op.slices()[1].as_ptr(), payload);
    assert_eq!(op.slices()[1], b"abc");
    (server, exchange, id, op)
}

#[test]
fn eager_scatter_write_survives_every_partial_boundary_and_reuses_connection() {
    let expected = b"HTTP/1.1 200 OK\r\ncontent-length: 3\r\n\r\nabc";
    for split in 1..=expected.len() {
        let (mut server, exchange, body_id, op) = eager_server();
        let pointer = op.slices()[1].as_ptr().wrapping_sub(1);
        assert_eq!(op.slices().concat(), expected);
        server.complete_write(op.complete(Ok(split))).unwrap();
        if split < expected.len() {
            let Some(Event::Write(op)) = next_server(&mut server) else {
                panic!("missing remaining write")
            };
            assert_eq!(op.slices().concat(), &expected[split..]);
            server.complete_write(finish_write(op)).unwrap();
        }
        let Some(Event::Sent(receipt)) = next_server(&mut server) else {
            panic!("missing receipt")
        };
        assert_eq!(receipt.id, body_id);
        assert_eq!(receipt.buffer.as_ptr(), pointer);
        assert_eq!(receipt.accepted, 3);
        assert_eq!(receipt.acceptance, Acceptance::Exact);
        assert_eq!(receipt.result, Ok(()));
        assert!(matches!(next_server(&mut server), Some(Event::Incoming(id)) if id == exchange));
        assert!(
            matches!(next_server(&mut server), Some(Event::Finished(result)) if result.reusable)
        );
        let second = feed_server(&mut server, b"GET /second HTTP/1.1\r\nHost: a\r\n\r\n");
        assert_ne!(second, exchange);
        server
            .respond(second, response(BodyLength::Known(3)))
            .unwrap();
        server.send_body_eager(command(second)).unwrap();
        let Some(Event::Write(op)) = next_server(&mut server) else {
            panic!()
        };
        server.complete_write(finish_write(op)).unwrap();
        assert!(matches!(next_server(&mut server), Some(Event::Sent(_))));
        assert!(matches!(next_server(&mut server), Some(Event::Incoming(_))));
        assert!(
            matches!(next_server(&mut server), Some(Event::Finished(result)) if result.reusable)
        );
    }
}

#[test]
fn eager_cancellation_counts_only_payload_and_preserves_unknown_acceptance() {
    let head_len = b"HTTP/1.1 200 OK\r\ncontent-length: 3\r\n\r\n".len();
    for prefix in 0..head_len + 3 {
        for unknown in [false, true] {
            let (mut server, exchange, id, mut op) = eager_server();
            if prefix != 0 {
                server.complete_write(op.complete(Ok(prefix))).unwrap();
                op = match next_server(&mut server) {
                    Some(Event::Write(op)) => op,
                    other => panic!("{other:?}"),
                };
            }
            server.cancel_exchange(exchange).unwrap();
            assert!(
                matches!(next_server(&mut server), Some(Event::Cancel(cancel)) if cancel.target == op.id())
            );
            server
                .complete_write(op.complete(Err(IoError {
                    kind: if unknown {
                        IoErrorKind::CancelledUnknownProgress
                    } else {
                        IoErrorKind::Cancelled
                    },
                    code: None,
                })))
                .unwrap();
            let Some(Event::Sent(receipt)) = next_server(&mut server) else {
                panic!()
            };
            assert_eq!(receipt.id, id);
            assert_eq!(receipt.accepted, prefix.saturating_sub(head_len));
            assert_eq!(
                receipt.acceptance,
                if unknown {
                    Acceptance::LowerBound
                } else {
                    Acceptance::Exact
                }
            );
            assert_eq!(receipt.result, Err(Failure::Cancelled));
            assert!(
                matches!(next_server(&mut server), Some(Event::Finished(result)) if !result.reusable)
            );
            assert!(matches!(next_server(&mut server), Some(Event::Close(_))));
        }
    }
}

#[test]
fn eager_cancellation_retains_late_positive_progress_without_reissuing_output() {
    let head_len = b"HTTP/1.1 200 OK\r\ncontent-length: 3\r\n\r\n".len();
    for prior in [0, 1, head_len - 1, head_len, head_len + 1] {
        for late in 1..=head_len + 3 - prior {
            let (mut server, exchange, id, mut op) = eager_server();
            let pointer = op.slices()[1].as_ptr().wrapping_sub(1);
            if prior != 0 {
                server.complete_write(op.complete(Ok(prior))).unwrap();
                op = match next_server(&mut server) {
                    Some(Event::Write(op)) => op,
                    _ => panic!(),
                };
            }
            server.cancel_exchange(exchange).unwrap();
            assert!(
                matches!(next_server(&mut server), Some(Event::Cancel(cancel)) if cancel.target == op.id())
            );
            server.complete_write(op.complete(Ok(late))).unwrap();
            let Some(Event::Sent(receipt)) = next_server(&mut server) else {
                panic!()
            };
            assert_eq!(receipt.id, id);
            assert_eq!(receipt.buffer.as_ptr(), pointer);
            assert_eq!(receipt.accepted, (prior + late).saturating_sub(head_len));
            assert_eq!(receipt.acceptance, Acceptance::Exact);
            assert_eq!(receipt.result, Err(Failure::Cancelled));
            assert!(
                matches!(next_server(&mut server), Some(Event::Finished(result)) if !result.reusable)
            );
            let Some(Event::Close(op)) = next_server(&mut server) else {
                panic!()
            };
            server.complete_close(op.complete(Ok(()))).unwrap();
            assert!(matches!(
                next_server(&mut server),
                Some(Event::Closed(Err(Failure::Cancelled)))
            ));
            assert!(next_server(&mut server).is_none());
        }
    }
}

#[test]
fn eager_cancellation_before_issuance_returns_one_zero_receipt_without_a_write() {
    let mut server = server(config());
    let exchange = feed_server(&mut server, b"GET / HTTP/1.1\r\nHost: a\r\n\r\n");
    server
        .respond(exchange, response(BodyLength::Known(3)))
        .unwrap();
    let body = command(exchange);
    let pointer = body.buffer.as_ptr();
    let id = server.send_body_eager(body).unwrap();
    server.cancel_exchange(exchange).unwrap();
    let Some(Event::Sent(receipt)) = next_server(&mut server) else {
        panic!()
    };
    assert_eq!(receipt.id, id);
    assert_eq!(receipt.buffer.as_ptr(), pointer);
    assert_eq!(receipt.accepted, 0);
    assert_eq!(receipt.acceptance, Acceptance::Exact);
    assert_eq!(receipt.result, Err(Failure::Cancelled));
    assert!(matches!(next_server(&mut server), Some(Event::Finished(result)) if !result.reusable));
    let Some(Event::Close(op)) = next_server(&mut server) else {
        panic!()
    };
    server.complete_close(op.complete(Ok(()))).unwrap();
    assert!(matches!(
        next_server(&mut server),
        Some(Event::Closed(Err(Failure::Cancelled)))
    ));
    assert!(next_server(&mut server).is_none());
}

#[test]
fn eager_duplex_output_does_not_release_a_separate_incoming_lease() {
    for abandon in [false, true] {
        let mut server = server(config());
        let exchange = feed_server(
            &mut server,
            b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 6\r\n\r\nabc",
        );
        server.grant_body_credit(exchange, 3).unwrap();
        let Some(Event::Body(lease)) = next_server(&mut server) else {
            panic!()
        };
        server
            .respond_duplex(exchange, response(BodyLength::Known(3)))
            .unwrap();
        server.send_body_eager(command(exchange)).unwrap();
        let Some(Event::Write(op)) = next_server(&mut server) else {
            panic!()
        };
        assert_eq!(op.slices()[1], b"abc");
        server.complete_write(finish_write(op)).unwrap();
        assert!(
            matches!(next_server(&mut server), Some(Event::Sent(receipt)) if receipt.accepted == 3)
        );
        assert!(
            next_server(&mut server).is_none(),
            "no retirement with a retained input lease"
        );
        if abandon {
            server.cancel_exchange(exchange).unwrap();
            assert!(
                matches!(next_server(&mut server), Some(Event::Finished(result)) if !result.reusable && result.result == Err(Failure::Cancelled))
            );
            assert!(
                next_server(&mut server).is_none(),
                "close must wait for the input lease"
            );
            server.release_body(lease.release(3)).unwrap();
            let Some(Event::Close(op)) = next_server(&mut server) else {
                panic!()
            };
            server.complete_close(op.complete(Ok(()))).unwrap();
            assert!(matches!(
                next_server(&mut server),
                Some(Event::Closed(Err(Failure::Cancelled)))
            ));
        } else {
            server.release_body(lease.release(3)).unwrap();
            server.grant_body_credit(exchange, 3).unwrap();
            let Some(Event::Read(op)) = next_server(&mut server) else {
                panic!()
            };
            server
                .complete_read(fill(op, b"defGET /second HTTP/1.1\r\nHost: a\r\n\r\n"))
                .unwrap();
            let Some(Event::Body(lease)) = next_server(&mut server) else {
                panic!()
            };
            server.release_body(lease.release(3)).unwrap();
            assert!(
                matches!(next_server(&mut server), Some(Event::Incoming(id)) if id == exchange)
            );
            assert!(
                matches!(next_server(&mut server), Some(Event::Finished(result)) if result.reusable)
            );
            let Some(Event::Request(second, _)) = next_server(&mut server) else {
                panic!()
            };
            assert_ne!(exchange, second);
            server.respond(second, response(BodyLength::Empty)).unwrap();
            let Some(Event::Write(op)) = next_server(&mut server) else {
                panic!()
            };
            server.complete_write(finish_write(op)).unwrap();
            assert!(matches!(next_server(&mut server), Some(Event::Incoming(id)) if id == second));
            assert!(
                matches!(next_server(&mut server), Some(Event::Finished(result)) if result.reusable)
            );
        }
    }
}

#[test]
fn eager_rejection_is_transactional_and_keeps_suppression_and_chunk_framing() {
    for (method, status, length) in [
        ("HEAD", 200, BodyLength::Known(3)),
        ("GET", 304, BodyLength::Known(3)),
        ("GET", 204, BodyLength::Empty),
        ("GET", 200, BodyLength::Streaming),
    ] {
        let mut server = server(config());
        let exchange = feed_server(
            &mut server,
            format!("{method} / HTTP/1.1\r\nHost: a\r\n\r\n").as_bytes(),
        );
        server
            .respond(exchange, Response::new(status, "OK", &[], length))
            .unwrap();
        let body = command(exchange);
        let pointer = body.buffer.as_ptr();
        let rejected = server.send_body_eager(body).unwrap_err();
        assert_eq!(rejected.value.buffer.as_ptr(), pointer);
        assert_eq!(rejected.reason, RejectReason::NoCapacity);
        let Some(Event::Write(head)) = next_server(&mut server) else {
            panic!()
        };
        assert!(head.slices()[1].is_empty());
        server.complete_write(finish_write(head)).unwrap();
        assert!(matches!(next_server(&mut server), Some(Event::Incoming(_))));
        if length == BodyLength::Streaming {
            assert!(matches!(
                next_server(&mut server),
                Some(Event::Demand(_, _))
            ));
            server.send_body(rejected.value).unwrap();
            let Some(Event::Write(body)) = next_server(&mut server) else {
                panic!()
            };
            assert_eq!(body.slices().concat(), b"3\r\nabc\r\n");
            server.complete_write(finish_write(body)).unwrap();
            assert!(matches!(next_server(&mut server), Some(Event::Sent(_))));
            let Some(Event::Write(end)) = next_server(&mut server) else {
                panic!()
            };
            assert_eq!(end.slices().concat(), b"0\r\n\r\n");
            server.complete_write(finish_write(end)).unwrap();
        }
        assert!(
            matches!(next_server(&mut server), Some(Event::Finished(result)) if result.reusable)
        );
    }
}

#[test]
fn eager_client_preserves_expect_gate_and_upload_timeout_after_head_prefix() {
    let mut client = client(Config {
        body_timeout_ns: Some(10),
        ..config()
    });
    let request = get(
        "POST",
        BodyLength::Known(3),
        true,
        &[Header {
            name: "host",
            value: b"a",
        }],
    );
    let exchange = client.request(request).unwrap();
    let rejected = client.send_body_eager(command(exchange)).unwrap_err();
    assert_eq!(rejected.reason, RejectReason::NoCapacity);
    let Some(Event::Write(head)) = next_client(&mut client) else {
        panic!()
    };
    assert!(head.slices()[1].is_empty());
    client.complete_write(finish_write(head)).unwrap();
    let Some(Event::Read(read)) = next_client(&mut client) else {
        panic!()
    };
    client
        .complete_read(fill(read, b"HTTP/1.1 100 Continue\r\n\r\n"))
        .unwrap();
    assert!(matches!(
        next_client(&mut client),
        Some(Event::Response(_, 100, true))
    ));
    assert!(matches!(
        next_client(&mut client),
        Some(Event::Demand(_, 3))
    ));
    client.send_body(rejected.value).unwrap();
    let Some(Event::Write(body)) = next_client(&mut client) else {
        panic!()
    };
    assert_eq!(body.slices().concat(), b"abc");
    client.complete_write(finish_write(body)).unwrap();

    let mut client = support::client(Config {
        head_timeout_ns: Some(100),
        body_timeout_ns: Some(10),
        ..config()
    });
    let exchange = client
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
    client.send_body_eager(command(exchange)).unwrap();
    let Some(Event::Deadline(Some(deadline))) = client.next(&mut Capture) else {
        panic!()
    };
    assert_eq!(deadline.at, Tick(10));
    let Some(Event::Write(op)) = next_client(&mut client) else {
        panic!()
    };
    let head_len = op.slices()[0].len();
    client.complete_write(op.complete(Ok(head_len))).unwrap();
    client.expire(deadline, Tick(10)).unwrap();
    let Some(Event::Sent(receipt)) = next_client(&mut client) else {
        panic!()
    };
    assert_eq!(receipt.accepted, 0);
    assert_eq!(receipt.result, Err(Failure::Timeout));
}

#[test]
fn eager_upload_timeout_covers_an_outstanding_combined_write_without_head_deadline() {
    let mut client = client(Config {
        body_timeout_ns: Some(10),
        ..config()
    });
    let exchange = client
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
    client.send_body_eager(command(exchange)).unwrap();
    let Some(Event::Deadline(Some(deadline))) = client.next(&mut Capture) else {
        panic!()
    };
    assert_eq!(deadline.at, Tick(10));
    let Some(Event::Write(op)) = next_client(&mut client) else {
        panic!()
    };
    client.expire(deadline, Tick(10)).unwrap();
    assert!(
        matches!(next_client(&mut client), Some(Event::Cancel(cancel)) if cancel.target == op.id())
    );
    client
        .complete_write(op.complete(Err(IoError {
            kind: IoErrorKind::Cancelled,
            code: None,
        })))
        .unwrap();
    let Some(Event::Sent(receipt)) = next_client(&mut client) else {
        panic!()
    };
    assert_eq!(receipt.accepted, 0);
    assert_eq!(receipt.result, Err(Failure::Timeout));
}

#[test]
fn eager_early_response_cancels_original_upload_without_replay() {
    let mut client = client(config());
    let exchange = client
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
    client.send_body_eager(command(exchange)).unwrap();
    let Some(Event::Write(op)) = next_client(&mut client) else {
        panic!()
    };
    let Some(Event::Read(read)) = next_client(&mut client) else {
        panic!()
    };
    client
        .complete_read(fill(
            read,
            b"HTTP/1.1 413 Content Too Large\r\nContent-Length: 0\r\n\r\n",
        ))
        .unwrap();
    assert!(matches!(
        next_client(&mut client),
        Some(Event::Response(_, 413, false))
    ));
    assert!(
        matches!(next_client(&mut client), Some(Event::Cancel(cancel)) if cancel.target == op.id())
    );
    client
        .complete_write(op.complete(Err(IoError {
            kind: IoErrorKind::CancelledUnknownProgress,
            code: None,
        })))
        .unwrap();
    let Some(Event::Sent(receipt)) = next_client(&mut client) else {
        panic!()
    };
    assert_eq!(receipt.accepted, 0);
    assert_eq!(receipt.acceptance, Acceptance::LowerBound);
    assert_eq!(receipt.result, Err(Failure::EarlyResponse));
    assert!(matches!(next_client(&mut client), Some(Event::Incoming(_))));
    assert!(matches!(next_client(&mut client), Some(Event::Finished(result)) if !result.reusable));
}
