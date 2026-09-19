mod support;
use kimojio_fsm_http1::*;
use support::*;

#[test]
fn malformed_request_gets_one_complete_bounded_error_before_close() {
    for (request, expected_status, config) in [
        (b"GET / HTTP/1.1\r\n\r\n".as_slice(), 400, config()),
        (b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n".as_slice(), 400, config()),
        (b"GET / HTTP/1.1\r\nHost: a\r\nX-Extra: one\r\nX-Again: two\r\n\r\n".as_slice(), 431, Config { max_headers: 2, ..config() }),
        (b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 100\r\n\r\n".as_slice(), 413, Config { max_body_bytes: 3, ..config() }),
    ] {
        let mut server = server(config);
        let Some(Event::Read(op)) = next_server(&mut server) else { panic!() };
        server.complete_read(fill(op, request)).unwrap();
        let mut wire = Vec::new();
        let mut closed = false;
        for _ in 0..512 {
            match next_server(&mut server) {
                Some(Event::Write(op)) => {
                    let bytes = op.slices().concat();
                    assert!(!bytes.is_empty());
                    wire.push(bytes[0]);
                    assert!(next_server(&mut server).is_none(), "close must wait for error output");
                    server.complete_write(op.complete(Ok(1))).unwrap();
                }
                Some(Event::Finished(result)) => assert!(result.result.is_err()),
                Some(Event::Close(op)) => {
                    assert!(wire.ends_with(b"content-length: 0\r\nconnection: close\r\n\r\n"));
                    server.complete_close(op.complete(Ok(()))).unwrap();
                }
                Some(Event::Closed(result)) => { assert!(result.is_err()); closed = true; break; }
                other => panic!("{other:?}"),
            }
        }
        assert!(closed);
        assert!(wire.starts_with(format!("HTTP/1.1 {expected_status} ").as_bytes()));
        assert_eq!(wire.windows(9).filter(|bytes| *bytes == b"HTTP/1.1 ").count(), 1);
        assert!(next_server(&mut server).is_none());
    }
}

#[test]
fn mandatory_bad_request_cases_respond_without_eof_or_application_dispatch() {
    let cases = [
        b"GET / HTTP/1.1\r\n\r\n".as_slice(),
        b"GET / HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n",
        b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
        b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: -1\r\n\r\n",
        b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: a\r\n\r\n",
        b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 18446744073709551616\r\n\r\n",
        b"GET / HTTP/1.1\r\nHost: a\r\nBad Name: x\r\n\r\n",
        b"GET / HTTP/1.1\r\nHost: a\r\nX-Value: \0\r\n\r\n",
        b"G@T / HTTP/1.1\r\nHost: a\r\n\r\n",
        b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 0\r\nTransfer-Encoding: chunked\r\n\r\n",
        b"GET / HTTP/1.1\r\nHost: a\r\nX-Fold: first\r\n second\r\n\r\n",
        b"GET / HTTP/1.1\nHost: a\n\n",
        b"GET / HTTP/1.1\rHost: a\r\n\r\n",
    ];
    for bytes in cases {
        let mut server = server(config());
        let Some(Event::Read(op)) = next_server(&mut server) else {
            panic!()
        };
        server.complete_read(fill(op, bytes)).unwrap();
        let Some(Event::Write(op)) = next_server(&mut server) else {
            panic!("no immediate error response for {bytes:?}")
        };
        assert_eq!(
            op.slices().concat(),
            b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
        );
        server.complete_write(finish_write(op)).unwrap();
        assert!(matches!(next_server(&mut server), Some(Event::Close(_))));
    }
}

#[test]
fn ordinary_idle_eof_closes_without_error_response_or_request_callback() {
    let mut server = server(config());
    let Some(Event::Read(op)) = next_server(&mut server) else {
        panic!()
    };
    server.complete_read(op.complete(Ok(0))).unwrap();
    let Some(Event::Close(op)) = next_server(&mut server) else {
        panic!()
    };
    server.complete_close(op.complete(Ok(()))).unwrap();
    assert!(matches!(
        next_server(&mut server),
        Some(Event::Closed(Ok(())))
    ));
}

#[test]
fn invalid_chunk_after_inflight_continue_preserves_continue_then_error() {
    let mut server = server(config());
    let id = feed_server(
        &mut server,
        b"POST / HTTP/1.1\r\nHost: a\r\nExpect: 100-continue\r\nTransfer-Encoding: chunked\r\n\r\n",
    );
    server.grant_body_credit(id, 3).unwrap();
    let Some(Event::Write(continue_op)) = next_server(&mut server) else {
        panic!()
    };
    let mut wire = continue_op.slices().concat();
    let Some(Event::Read(op)) = next_server(&mut server) else {
        panic!()
    };
    server.complete_read(fill(op, b"zz\r\n")).unwrap();
    assert!(next_server(&mut server).is_none());
    server.complete_write(finish_write(continue_op)).unwrap();
    let Some(Event::Write(error_op)) = next_server(&mut server) else {
        panic!()
    };
    wire.extend(error_op.slices().concat());
    server.complete_write(finish_write(error_op)).unwrap();
    assert!(wire.starts_with(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 400 "));
    let Some(Event::Finished(result)) = next_server(&mut server) else {
        panic!()
    };
    assert_eq!(result.result, Err(Failure::Protocol));
    assert!(matches!(next_server(&mut server), Some(Event::Close(_))));
}

#[test]
fn already_started_final_response_never_gets_a_second_final() {
    let mut server = server(config());
    let id = feed_server(&mut server, b"GET / HTTP/1.1\r\nHost: a\r\n\r\n");
    server.respond(id, response(BodyLength::Known(3))).unwrap();
    let Some(Event::Write(op)) = next_server(&mut server) else {
        panic!()
    };
    server.complete_write(finish_write(op)).unwrap();
    server.fail_source(id, Failure::Protocol).unwrap();
    assert!(matches!(next_server(&mut server), Some(Event::Finished(_))));
    assert!(matches!(next_server(&mut server), Some(Event::Close(_))));
}

#[test]
fn unusable_transport_and_insufficient_error_budget_do_not_emit_error_writes() {
    let mut transport = server(config());
    let Some(Event::Read(op)) = next_server(&mut transport) else {
        panic!()
    };
    transport
        .complete_read(op.complete(Err(IoError {
            kind: IoErrorKind::Reset,
            code: None,
        })))
        .unwrap();
    assert!(matches!(next_server(&mut transport), Some(Event::Close(_))));

    let mut tiny = server(Config {
        max_head_bytes: 16,
        ..config()
    });
    let Some(Event::Read(op)) = next_server(&mut tiny) else {
        panic!()
    };
    tiny.complete_read(fill(op, b"invalid\r\n\r\n")).unwrap();
    assert!(matches!(next_server(&mut tiny), Some(Event::Close(_))));
}
