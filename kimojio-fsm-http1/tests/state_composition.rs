mod support;

use kimojio_fsm_http1::*;
use support::*;

#[test]
fn informational_completion_does_not_abandon_input_before_final_output_settles() {
    let mut server = server(config());
    let exchange = feed_server(
        &mut server,
        b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 3\r\n\r\n",
    );
    server.grant_body_credit(exchange, 3).unwrap();
    server
        .inform(exchange, ResponseHead::new(103, "Early Hints", &[]))
        .unwrap();
    let Some(Event::Write(informational)) = next_server(&mut server) else {
        panic!()
    };
    let Some(Event::Read(read)) = next_server(&mut server) else {
        panic!()
    };
    server.complete_read(fill(read, b"abc")).unwrap();
    server
        .respond(exchange, response(BodyLength::Empty))
        .unwrap();
    server.complete_write(finish_write(informational)).unwrap();
    let Some(Event::Write(final_head)) = next_server(&mut server) else {
        panic!()
    };
    let Some(Event::Body(body)) = next_server(&mut server) else {
        panic!("input must remain eligible while the final head is in flight")
    };
    assert_eq!(body.bytes(), b"abc");
    server.release_body(body.release(3)).unwrap();
    assert!(matches!(next_server(&mut server), Some(Event::Incoming(id)) if id == exchange));
    assert!(next_server(&mut server).is_none());
    server.complete_write(finish_write(final_head)).unwrap();
    let Some(Event::Finished(finished)) = next_server(&mut server) else {
        panic!()
    };
    assert_eq!(finished.result, Ok(()));
    assert!(!finished.reusable);
    let Some(Event::Close(close)) = next_server(&mut server) else {
        panic!()
    };
    server.complete_close(close.complete(Ok(()))).unwrap();
    assert!(matches!(
        next_server(&mut server),
        Some(Event::Closed(Ok(())))
    ));
    assert!(next_server(&mut server).is_none());
}

#[test]
fn repeated_shutdown_preserves_the_original_close_and_single_notification() {
    for abort in [false, true] {
        let mut server = server(config());
        server.shutdown(ShutdownMode::Graceful);
        let Some(Event::Close(close)) = next_server(&mut server) else {
            panic!()
        };
        for _ in 0..3 {
            server.shutdown(if abort {
                ShutdownMode::Abort
            } else {
                ShutdownMode::Graceful
            });
            assert!(next_server(&mut server).is_none());
        }
        server.complete_close(close.complete(Ok(()))).unwrap();
        let Some(Event::Closed(result)) = next_server(&mut server) else {
            panic!()
        };
        assert_eq!(
            result,
            if abort {
                Err(Failure::Cancelled)
            } else {
                Ok(())
            }
        );
        server.shutdown(ShutdownMode::Abort);
        assert!(next_server(&mut server).is_none());
    }
}

#[test]
fn closing_waits_for_both_receive_lease_and_write_receipt_in_either_order() {
    for release_first in [false, true] {
        let mut server = server(config());
        let exchange = feed_server(
            &mut server,
            b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 3\r\n\r\nabc",
        );
        server.grant_body_credit(exchange, 3).unwrap();
        let Some(Event::Body(body)) = next_server(&mut server) else {
            panic!()
        };
        server
            .respond(exchange, response(BodyLength::Known(3)))
            .unwrap();
        let Some(Event::Write(head)) = next_server(&mut server) else {
            panic!()
        };
        server.complete_write(finish_write(head)).unwrap();
        assert!(matches!(next_server(&mut server), Some(Event::Demand(id, 3)) if id == exchange));
        server
            .send_body(SendBody {
                exchange,
                buffer: b"xyz".to_vec(),
                range: 0..3,
                end: true,
            })
            .unwrap();
        let Some(Event::Write(write)) = next_server(&mut server) else {
            panic!()
        };
        let write_id = write.id();
        server.shutdown(ShutdownMode::Abort);
        assert!(
            matches!(next_server(&mut server), Some(Event::Cancel(op)) if op.target == write_id)
        );
        let Some(Event::Finished(finished)) = next_server(&mut server) else {
            panic!()
        };
        assert_eq!(finished.result, Err(Failure::Cancelled));
        assert!(!finished.reusable);
        assert!(next_server(&mut server).is_none());

        let mut body = Some(body);
        if release_first {
            server
                .release_body(body.take().unwrap().release(3))
                .unwrap();
            assert!(next_server(&mut server).is_none());
        }
        server
            .complete_write(write.complete(Err(IoError {
                kind: IoErrorKind::Cancelled,
                code: None,
            })))
            .unwrap();
        let Some(Event::Sent(receipt)) = next_server(&mut server) else {
            panic!()
        };
        assert_eq!(receipt.buffer, b"xyz");
        assert_eq!(receipt.accepted, 0);
        assert_eq!(receipt.result, Err(Failure::Cancelled));
        if let Some(body) = body {
            assert!(next_server(&mut server).is_none());
            server.release_body(body.release(3)).unwrap();
        }
        let Some(Event::Close(close)) = next_server(&mut server) else {
            panic!()
        };
        server.complete_close(close.complete(Ok(()))).unwrap();
        assert!(matches!(
            next_server(&mut server),
            Some(Event::Closed(Err(Failure::Cancelled)))
        ));
        assert!(next_server(&mut server).is_none());
    }
}

#[test]
fn empty_stream_can_end_while_the_continue_gate_is_still_waiting() {
    let mut client = client(config());
    let exchange = client
        .request(get(
            "POST",
            BodyLength::Streaming,
            true,
            &[Header {
                name: "host",
                value: b"a",
            }],
        ))
        .unwrap();
    let Some(Event::Write(head)) = next_client(&mut client) else {
        panic!()
    };
    client.complete_write(finish_write(head)).unwrap();
    let Some(Event::Read(read)) = next_client(&mut client) else {
        panic!()
    };
    assert!(next_client(&mut client).is_none());
    client.finish_body(exchange, &[]).unwrap();
    let Some(Event::Write(end)) = next_client(&mut client) else {
        panic!()
    };
    assert_eq!(
        end.slices()
            .iter()
            .flat_map(|part| part.iter().copied())
            .collect::<Vec<_>>(),
        b"0\r\n\r\n"
    );
    client.complete_write(finish_write(end)).unwrap();
    client
        .complete_read(fill(
            read,
            b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
        ))
        .unwrap();
    assert!(
        matches!(next_client(&mut client), Some(Event::Response(id, 100, true)) if id == exchange)
    );
    assert!(
        matches!(next_client(&mut client), Some(Event::Response(id, 200, false)) if id == exchange)
    );
    assert!(matches!(next_client(&mut client), Some(Event::Incoming(id)) if id == exchange));
    let Some(Event::Finished(finished)) = next_client(&mut client) else {
        panic!()
    };
    assert_eq!(finished.exchange, exchange);
    assert_eq!(finished.result, Ok(()));
    assert!(finished.reusable);
    assert!(next_client(&mut client).is_none());
}
