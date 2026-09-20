mod support;

use kimojio_fsm_http1::*;
use support::*;

const POST: &[u8] = b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 3\r\n\r\n";
const NEXT: &[u8] = b"GET /second HTTP/1.1\r\nHost: a\r\n\r\n";

fn write(server: &mut Server<B>) -> WriteOp<B> {
    match next_server(server) {
        Some(Event::Write(op)) => op,
        event => panic!("expected write, got {event:?}"),
    }
}

fn read(server: &mut Server<B>) -> ReadOp<B> {
    match next_server(server) {
        Some(Event::Read(op)) => op,
        event => panic!("expected read, got {event:?}"),
    }
}

fn body(server: &mut Server<B>) -> BodyOp<B> {
    match next_server(server) {
        Some(Event::Body(op)) => op,
        event => panic!("expected body, got {event:?}"),
    }
}

fn incoming(server: &mut Server<B>, exchange: ExchangeId) {
    assert!(matches!(next_server(server), Some(Event::Incoming(id)) if id == exchange));
}

fn finished(server: &mut Server<B>, exchange: ExchangeId, reusable: bool) {
    let Some(Event::Finished(result)) = next_server(server) else {
        panic!("expected exchange completion")
    };
    assert_eq!(result.exchange, exchange);
    assert_eq!(result.result, Ok(()));
    assert_eq!(result.reusable, reusable);
}

fn second_request(server: &mut Server<B>, previous: ExchangeId) {
    let Some(Event::Request(exchange, Version::Http11)) = next_server(server) else {
        panic!("pipelined request must follow the first exchange completion")
    };
    assert_ne!(previous, exchange);
    assert_eq!(
        server.respond_duplex(previous, response(BodyLength::Empty)),
        Err(CommandError::StaleExchange)
    );
    server
        .respond(exchange, response(BodyLength::Empty))
        .unwrap();
    let op = write(server);
    server.complete_write(finish_write(op)).unwrap();
    incoming(server, exchange);
    finished(server, exchange, true);
}

fn closed(server: &mut Server<B>, expected: ConnectionResult) {
    let Some(Event::Close(op)) = next_server(server) else {
        panic!("expected settled transport close")
    };
    server.complete_close(op.complete(Ok(()))).unwrap();
    assert!(matches!(next_server(server), Some(Event::Closed(result)) if result == expected));
    assert!(next_server(server).is_none());
}

#[test]
fn both_completion_orders_preserve_leases_credit_and_pipelined_reuse() {
    for response_first in [false, true] {
        for chunked in [false, true] {
            let mut server = server(config());
            let request = if chunked {
                [
                    b"POST / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n".as_slice(),
                    NEXT,
                ]
                .concat()
            } else {
                [POST, b"abc", NEXT].concat()
            };
            let exchange = feed_server(&mut server, &request);
            server
                .respond_duplex(exchange, response(BodyLength::Empty))
                .unwrap();
            let head = write(&mut server);
            assert!(
                !String::from_utf8(head.slices().concat())
                    .unwrap()
                    .to_ascii_lowercase()
                    .contains("connection: close")
            );
            assert!(
                next_server(&mut server).is_none(),
                "no implicit body credit"
            );
            server.grant_body_credit(exchange, 3).unwrap();
            let lease = body(&mut server);
            assert_eq!(lease.bytes(), b"abc");

            server.complete_write(head.complete(Ok(1))).unwrap();
            let head = write(&mut server);
            if response_first {
                server.complete_write(finish_write(head)).unwrap();
                assert!(
                    next_server(&mut server).is_none(),
                    "lease prevents retirement"
                );
            } else {
                server.release_body(lease.release(3)).unwrap();
                if chunked {
                    assert!(matches!(next_server(&mut server), Some(Event::Trailers(_))));
                }
                incoming(&mut server, exchange);
                assert!(
                    next_server(&mut server).is_none(),
                    "write prevents retirement"
                );
                server.complete_write(finish_write(head)).unwrap();
                finished(&mut server, exchange, true);
                second_request(&mut server, exchange);
                continue;
            }
            server.release_body(lease.release(1)).unwrap();
            assert!(
                next_server(&mut server).is_none(),
                "credit was debited at issuance"
            );
            server.grant_body_credit(exchange, 2).unwrap();
            let lease = body(&mut server);
            assert_eq!(lease.bytes(), b"bc");
            server.release_body(lease.release(2)).unwrap();
            if chunked {
                assert!(matches!(next_server(&mut server), Some(Event::Trailers(_))));
            }
            incoming(&mut server, exchange);
            finished(&mut server, exchange, true);
            second_request(&mut server, exchange);
        }
    }
}

#[test]
fn final_payload_receipt_does_not_release_input_or_change_the_next_response_policy() {
    let mut server = server(config());
    let exchange = feed_server(&mut server, &[POST, b"abc", POST].concat());
    server.grant_body_credit(exchange, 3).unwrap();
    let lease = body(&mut server);
    server
        .respond_duplex(exchange, response(BodyLength::Known(2)))
        .unwrap();
    let head = write(&mut server);
    server.complete_write(finish_write(head)).unwrap();
    assert!(matches!(next_server(&mut server), Some(Event::Demand(id, 2)) if id == exchange));
    let outgoing = server
        .send_body(SendBody {
            exchange,
            buffer: b"ok".to_vec(),
            range: 0..2,
            end: true,
        })
        .unwrap();
    let payload = write(&mut server);
    server.complete_write(payload.complete(Ok(1))).unwrap();
    let tail = write(&mut server);
    assert_eq!(tail.slices().concat(), b"k");
    server.complete_write(finish_write(tail)).unwrap();
    let Some(Event::Sent(receipt)) = next_server(&mut server) else {
        panic!("missing output storage receipt")
    };
    assert_eq!(receipt.id, outgoing);
    assert_eq!(receipt.accepted, 2);
    assert_eq!(receipt.result, Ok(()));
    assert!(next_server(&mut server).is_none());
    server.release_body(lease.release(3)).unwrap();
    incoming(&mut server, exchange);
    finished(&mut server, exchange, true);

    let Some(Event::Request(second, _)) = next_server(&mut server) else {
        panic!("missing second request")
    };
    assert_ne!(exchange, second);
    server.respond(second, response(BodyLength::Empty)).unwrap();
    let head = write(&mut server);
    assert!(
        String::from_utf8(head.slices().concat())
            .unwrap()
            .to_ascii_lowercase()
            .contains("connection: close")
    );
    server.complete_write(finish_write(head)).unwrap();
    finished(&mut server, second, false);
    closed(&mut server, Ok(()));
}

#[test]
fn response_first_preserves_pending_read_and_stale_readiness_is_rejected_after_reuse() {
    let mut server = server(config());
    let exchange = feed_server(&mut server, POST);
    server.grant_body_credit(exchange, 3).unwrap();
    let pending = read(&mut server);
    server
        .complete_read(pending.complete(Err(IoError {
            kind: IoErrorKind::WouldBlock,
            code: None,
        })))
        .unwrap();
    let Some(Event::Ready(ready)) = next_server(&mut server) else {
        panic!()
    };
    server
        .respond_duplex(exchange, response(BodyLength::Empty))
        .unwrap();
    let head = write(&mut server);
    server.complete_write(finish_write(head)).unwrap();
    assert!(
        next_server(&mut server).is_none(),
        "readiness must not be cancelled"
    );
    server.complete_readiness(ready.complete(Ok(()))).unwrap();
    let pending = read(&mut server);
    assert!(
        next_server(&mut server).is_none(),
        "pending read must remain owned"
    );
    server
        .complete_read(fill(pending, &[b"abc".as_slice(), NEXT].concat()))
        .unwrap();
    let lease = body(&mut server);
    server.release_body(lease.release(3)).unwrap();
    incoming(&mut server, exchange);
    finished(&mut server, exchange, true);
    second_request(&mut server, exchange);
    assert_eq!(
        server
            .complete_readiness(ready.complete(Ok(())))
            .unwrap_err()
            .reason,
        RejectReason::Stale
    );
}

#[test]
fn duplex_continue_precedes_even_an_empty_final_without_prior_credit() {
    let mut server = server(config());
    let exchange = feed_server(
        &mut server,
        b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 3\r\nExpect: 100-continue\r\n\r\n",
    );
    server
        .respond_duplex(exchange, response(BodyLength::Empty))
        .unwrap();
    let mut wire = Vec::new();
    loop {
        match next_server(&mut server) {
            Some(Event::Write(op)) => {
                wire.push(op.slices().concat()[0]);
                server.complete_write(op.complete(Ok(1))).unwrap();
            }
            None => break,
            event => panic!("unexpected {event:?}"),
        }
    }
    let wire = String::from_utf8(wire).unwrap();
    assert!(wire.starts_with("HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\n"));
    assert_eq!(wire.matches("100 Continue").count(), 1);
    server.grant_body_credit(exchange, 3).unwrap();
    let pending = read(&mut server);
    server
        .complete_read(fill(pending, &[b"abc".as_slice(), NEXT].concat()))
        .unwrap();
    let lease = body(&mut server);
    server.release_body(lease.release(3)).unwrap();
    incoming(&mut server, exchange);
    finished(&mut server, exchange, true);
    second_request(&mut server, exchange);
}

#[test]
fn duplex_continue_limit_preserves_conservative_rejection_fallback() {
    let mut server = server(Config {
        max_informational_responses: 0,
        ..config()
    });
    let exchange = feed_server(
        &mut server,
        b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 3\r\nExpect: 100-continue\r\n\r\n",
    );
    assert_eq!(
        server.respond_duplex(exchange, response(BodyLength::Empty)),
        Err(CommandError::Limit)
    );
    server
        .respond(
            exchange,
            Response::new(417, "Expectation Failed", &[], BodyLength::Empty),
        )
        .unwrap();
    let head = write(&mut server);
    let wire = String::from_utf8(head.slices().concat()).unwrap();
    assert!(wire.starts_with("HTTP/1.1 417 Expectation Failed\r\n"));
    assert!(!wire.contains("100 Continue"));
    server.complete_write(finish_write(head)).unwrap();
    finished(&mut server, exchange, false);
    closed(&mut server, Ok(()));
}

#[test]
fn conservative_response_still_abandons_and_cancels_unread_request() {
    let mut server = server(config());
    let exchange = feed_server(&mut server, POST);
    server.grant_body_credit(exchange, 3).unwrap();
    let pending = read(&mut server);
    server
        .respond(
            exchange,
            Response::new(413, "Content Too Large", &[], BodyLength::Empty),
        )
        .unwrap();
    let head = write(&mut server);
    let wire = String::from_utf8(head.slices().concat()).unwrap();
    assert!(wire.to_ascii_lowercase().contains("connection: close"));
    server.complete_write(finish_write(head)).unwrap();
    assert!(
        matches!(next_server(&mut server), Some(Event::Cancel(op)) if op.target == pending.id())
    );
    assert!(next_server(&mut server).is_none());
    server
        .complete_read(fill(pending, &[b"abc".as_slice(), NEXT].concat()))
        .unwrap();
    finished(&mut server, exchange, false);
    closed(&mut server, Ok(()));
}

#[test]
fn duplex_abandonment_cancels_original_operations_and_never_reuses() {
    for read_first in [false, true] {
        let mut server = server(config());
        let exchange = feed_server(&mut server, POST);
        server.grant_body_credit(exchange, 3).unwrap();
        let pending = read(&mut server);
        server
            .respond_duplex(exchange, response(BodyLength::Empty))
            .unwrap();
        let head = write(&mut server);
        server.cancel_exchange(exchange).unwrap();
        assert!(
            matches!(next_server(&mut server), Some(Event::Cancel(op)) if op.target == pending.id())
        );
        assert!(
            matches!(next_server(&mut server), Some(Event::Cancel(op)) if op.target == head.id())
        );
        let Some(Event::Finished(result)) = next_server(&mut server) else {
            panic!()
        };
        assert_eq!(result.result, Err(Failure::Cancelled));
        assert!(!result.reusable);
        assert!(next_server(&mut server).is_none());
        let mut pending = Some(pending);
        if read_first {
            server
                .complete_read(fill(pending.take().unwrap(), b"abc"))
                .unwrap();
            assert!(next_server(&mut server).is_none());
        }
        server.complete_write(finish_write(head)).unwrap();
        if let Some(pending) = pending {
            assert!(next_server(&mut server).is_none());
            server.complete_read(fill(pending, b"abc")).unwrap();
        }
        closed(&mut server, Err(Failure::Cancelled));
    }
}

#[test]
fn duplex_failure_after_final_output_preserves_body_limits_and_framing_checks() {
    for (wire, failure) in [
        (b"4\r\nabcd\r\n0\r\n\r\n".as_slice(), Failure::Limit),
        (b"no\r\n".as_slice(), Failure::Protocol),
        (b"".as_slice(), Failure::UnexpectedEof),
    ] {
        let mut server = server(Config {
            max_body_bytes: 3,
            ..config()
        });
        let exchange = feed_server(
            &mut server,
            b"POST / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n",
        );
        server
            .respond_duplex(exchange, response(BodyLength::Empty))
            .unwrap();
        let head = write(&mut server);
        server.complete_write(finish_write(head)).unwrap();
        server.grant_body_credit(exchange, 3).unwrap();
        let pending = read(&mut server);
        server.complete_read(fill(pending, wire)).unwrap();
        let Some(Event::Finished(result)) = next_server(&mut server) else {
            panic!()
        };
        assert_eq!(result.result, Err(failure));
        assert!(!result.reusable);
        closed(&mut server, Err(failure));
    }
}

#[test]
fn duplex_body_timeout_remains_armed_after_final_output_and_held_lease() {
    let mut server = server(Config {
        body_timeout_ns: Some(10),
        ..config()
    });
    let exchange = feed_server(&mut server, &[POST, b"abc"].concat());
    let deadline = match server.next(&mut Capture) {
        Some(Event::Deadline(Some(deadline))) => deadline,
        other => panic!("expected body deadline, got {other:?}"),
    };
    server.grant_body_credit(exchange, 3).unwrap();
    let lease = body(&mut server);
    server
        .respond_duplex(exchange, response(BodyLength::Empty))
        .unwrap();
    let head = write(&mut server);
    server.complete_write(finish_write(head)).unwrap();
    assert!(next_server(&mut server).is_none());
    server.expire(deadline, deadline.at).unwrap();
    let Some(Event::Finished(result)) = next_server(&mut server) else {
        panic!()
    };
    assert_eq!(result.result, Err(Failure::Timeout));
    assert!(!result.reusable);
    assert!(next_server(&mut server).is_none());
    server.release_body(lease.release(3)).unwrap();
    closed(&mut server, Err(Failure::Timeout));
}

#[test]
fn duplex_does_not_override_connection_close_request_limit_or_shutdown() {
    for reason in 0..4 {
        let mut server = server(Config {
            max_requests: if reason == 2 { 1 } else { 100 },
            ..config()
        });
        let request = if reason == 0 {
            b"POST / HTTP/1.0\r\nContent-Length: 3\r\n\r\n".as_slice()
        } else {
            POST
        };
        let exchange = feed_server(&mut server, &[request, b"abc", NEXT].concat());
        if reason == 3 {
            server.shutdown(ShutdownMode::Graceful);
        }
        let headers = [Header {
            name: "connection",
            value: b"close",
        }];
        server
            .respond_duplex(
                exchange,
                Response::new(
                    200,
                    "OK",
                    if reason == 1 { &headers } else { &[] },
                    BodyLength::Empty,
                ),
            )
            .unwrap();
        let head = write(&mut server);
        server.complete_write(finish_write(head)).unwrap();
        assert!(next_server(&mut server).is_none());
        server.grant_body_credit(exchange, 3).unwrap();
        let lease = body(&mut server);
        server.release_body(lease.release(3)).unwrap();
        incoming(&mut server, exchange);
        finished(&mut server, exchange, false);
        closed(&mut server, Ok(()));
    }
}

#[test]
fn duplex_is_not_an_upgrade_path_and_invalid_selection_does_not_change_default() {
    let mut server = server(config());
    let exchange = feed_server(&mut server, POST);
    assert_eq!(
        server.respond_duplex(
            exchange,
            Response::new(101, "Switching Protocols", &[], BodyLength::Empty)
        ),
        Err(CommandError::InvalidState)
    );
    server
        .respond(exchange, response(BodyLength::Empty))
        .unwrap();
    let head = write(&mut server);
    server.complete_write(finish_write(head)).unwrap();
    finished(&mut server, exchange, false);
    closed(&mut server, Ok(()));
}
