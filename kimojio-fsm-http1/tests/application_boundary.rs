mod support;
use kimojio_fsm_http1::*;
use support::*;

#[test]
fn credit_authorizes_exactly_one_continue_without_header_scanning_by_application() {
    let mut server = server(config());
    let id = feed_server(&mut server, b"POST / HTTP/1.1\r\nHost: a\r\nExpect: 100-Continue\r\nExpect: 100-continue\r\nContent-Length: 3\r\n\r\n");
    assert!(
        next_server(&mut server).is_none(),
        "no authorization, no continue"
    );
    server.grant_body_credit(id, 3).unwrap();
    let Some(Event::Write(op)) = next_server(&mut server) else {
        panic!()
    };
    assert_eq!(op.slices().concat(), b"HTTP/1.1 100 Continue\r\n\r\n");
    server.complete_write(finish_write(op)).unwrap();
    let Some(Event::Read(op)) = next_server(&mut server) else {
        panic!()
    };
    server.complete_read(fill(op, b"abc")).unwrap();
    let Some(Event::Body(op)) = next_server(&mut server) else {
        panic!()
    };
    server.release_body(op.release(3)).unwrap();
    assert!(matches!(next_server(&mut server), Some(Event::Incoming(_))));
    server.grant_body_credit(id, 3).unwrap();
    assert!(next_server(&mut server).is_none());
    assert_eq!(
        server.inform(id, ResponseHead::new(100, "Continue", &[])),
        Err(CommandError::InvalidState)
    );
}

#[test]
fn fully_buffered_or_empty_bodies_never_receive_an_unsolicited_continue() {
    for body in [
        b"Content-Length: 3\r\n\r\nabc".as_slice(),
        b"Transfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n",
        b"Transfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        b"Content-Length: 0\r\n\r\n",
        b"\r\n",
    ] {
        let mut wire = b"POST / HTTP/1.1\r\nHost: a\r\nExpect: 100-continue\r\n".to_vec();
        wire.extend_from_slice(body);
        let mut server = server(config());
        let id = feed_server(&mut server, &wire);
        server.grant_body_credit(id, 1024).unwrap();
        loop {
            match next_server(&mut server) {
                Some(Event::Body(op)) => {
                    let n = op.bytes().len();
                    server.release_body(op.release(n)).unwrap();
                }
                Some(Event::Trailers(_)) => {}
                Some(Event::Incoming(_)) => break,
                other => panic!("fully received body emitted {other:?}"),
            }
        }
        server.grant_body_credit(id, 1).unwrap();
        assert!(next_server(&mut server).is_none());
        server
            .respond(id, Response::new(200, "OK", &[], BodyLength::Empty))
            .unwrap();
        let Some(Event::Write(op)) = next_server(&mut server) else {
            panic!()
        };
        assert!(op.slices().concat().starts_with(b"HTTP/1.1 200 "));
    }
}

#[test]
fn unsupported_expectations_are_owned_by_http_and_never_dispatch_to_application() {
    for expectation in ["other", "100-continue, other", "", "100-continue; x=1"] {
        let mut server = server(config());
        let Some(Event::Read(op)) = next_server(&mut server) else {
            panic!()
        };
        let wire = format!(
            "POST / HTTP/1.1\r\nHost: a\r\nExpect: {expectation}\r\nContent-Length: 3\r\n\r\n"
        );
        server.complete_read(fill(op, wire.as_bytes())).unwrap();
        let Some(Event::Write(op)) = next_server(&mut server) else {
            panic!()
        };
        assert_eq!(
            op.slices().concat(),
            b"HTTP/1.1 417 Expectation Failed\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
        );
        server.complete_write(finish_write(op)).unwrap();
        assert!(matches!(next_server(&mut server), Some(Event::Close(_))));
    }
}

#[test]
fn one_final_response_is_accepted_behind_queued_or_inflight_informational_output() {
    for inflight in [false, true] {
        let mut server = server(config());
        let id = feed_server(
            &mut server,
            b"POST / HTTP/1.1\r\nHost: a\r\nExpect: 100-continue\r\nContent-Length: 3\r\n\r\n",
        );
        server
            .inform(
                id,
                ResponseHead {
                    version: Version::Http10,
                    ..ResponseHead::new(103, "Early Hints", &[])
                },
            )
            .unwrap();
        let pending = if inflight {
            let Some(Event::Write(op)) = next_server(&mut server) else {
                panic!()
            };
            Some(op)
        } else {
            None
        };
        server.grant_body_credit(id, 3).unwrap();
        server
            .respond(id, Response::new(403, "Forbidden", &[], BodyLength::Empty))
            .unwrap();
        assert_eq!(
            server.respond(id, Response::new(200, "OK", &[], BodyLength::Empty)),
            Err(CommandError::InvalidState)
        );
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
                Some(Event::Finished(_)) => break,
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(wire, b"HTTP/1.1 103 Early Hints\r\n\r\nHTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
    }
}

#[test]
fn final_response_suppresses_unissued_continue_and_wire_version_is_machine_owned() {
    for version in [Version::Http10, Version::Http11] {
        let mut server = server(config());
        let wire = if version == Version::Http10 {
            b"POST / HTTP/1.0\r\nExpect: 100-continue\r\nContent-Length: 3\r\n\r\n".as_slice()
        } else {
            b"POST / HTTP/1.1\r\nHost: a\r\nExpect: 100-continue\r\nContent-Length: 3\r\n\r\n"
        };
        let id = feed_server(&mut server, wire);
        server.grant_body_credit(id, 3).unwrap();
        server
            .respond(
                id,
                Response {
                    head: ResponseHead {
                        version: if version == Version::Http10 {
                            Version::Http11
                        } else {
                            Version::Http10
                        },
                        ..ResponseHead::new(403, "Forbidden", &[])
                    },
                    body: BodyLength::Empty,
                },
            )
            .unwrap();
        let Some(Event::Write(op)) = next_server(&mut server) else {
            panic!()
        };
        let expected = if version == Version::Http10 {
            b"HTTP/1.0 403 ".as_slice()
        } else {
            b"HTTP/1.1 403 "
        };
        assert!(op.slices().concat().starts_with(expected));
        assert!(
            !op.slices()
                .concat()
                .windows(12)
                .any(|bytes| bytes == b"100 Continue")
        );
    }
}
