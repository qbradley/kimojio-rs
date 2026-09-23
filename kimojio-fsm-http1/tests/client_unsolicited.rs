mod support;

use kimojio_fsm_http1::*;
use support::*;

const HOST: &[Header<'_>] = &[Header {
    name: "host",
    value: b"a",
}];

#[test]
fn buffered_unsolicited_bytes_disable_reuse_and_cannot_become_a_later_response() {
    let cases: &[(&str, &[u8], &[u8], u16)] = &[
        ("GET", b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n", b"", 200),
        ("GET", b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc", b"abc", 200),
        ("GET", b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\nX-End: done\r\n\r\n", b"abc", 200),
        ("HEAD", b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\n", b"", 200),
        ("GET", b"HTTP/1.1 204 No Content\r\n\r\n", b"", 204),
    ];
    for &(method, first, expected_body, status) in cases {
        for tail in [
            b"H".as_slice(),
            b"HTTP/1.1 299 Unsolicited\r\nContent-Length: 0\r\n\r\n",
        ] {
            let mut client = client(config());
            let request = client
                .request(get(method, BodyLength::Empty, false, HOST))
                .unwrap();
            let wire = [first, tail].concat();
            let mut supplied = false;
            let mut head_seen = false;
            let mut incoming_done = false;
            let mut finished = false;
            let mut closed = false;
            let mut body = Vec::new();
            for _ in 0..256 {
                match next_client(&mut client).expect("connection stalled") {
                    Event::Write(op) => {
                        assert!(!finished);
                        client.complete_write(op.complete(Ok(1))).unwrap();
                    }
                    Event::Read(op) => {
                        assert!(!supplied);
                        supplied = true;
                        client.complete_read(fill(op, &wire)).unwrap();
                    }
                    Event::Response(id, observed, false) => {
                        assert_eq!(id, request);
                        assert_eq!(observed, status);
                        assert!(!head_seen);
                        head_seen = true;
                        client.grant_body_credit(id, 1024).unwrap();
                    }
                    Event::Body(op) => {
                        let n = op.bytes().len();
                        body.extend_from_slice(op.bytes());
                        assert_eq!(
                            client.request(get("GET", BodyLength::Empty, false, HOST)),
                            Err(CommandError::InvalidState),
                        );
                        client.release_body(op.release(n)).unwrap();
                    }
                    Event::Trailers(headers) => {
                        assert_eq!(headers, [("X-End".to_ascii_lowercase(), b"done".to_vec())]);
                    }
                    Event::Incoming(id) => {
                        assert_eq!(id, request);
                        assert!(!incoming_done);
                        incoming_done = true;
                    }
                    Event::Finished(result) => {
                        assert_eq!(result.exchange, request);
                        assert_eq!(result.result, Ok(()));
                        assert!(!result.reusable, "method={method}, tail={tail:?}");
                        assert!(!finished && incoming_done);
                        finished = true;
                        assert_eq!(client.buffered_input(), tail);
                        assert_eq!(
                            client.request(get("GET", BodyLength::Empty, false, HOST)),
                            Err(CommandError::InvalidState),
                        );
                        assert_eq!(client.buffered_input(), tail);
                    }
                    Event::Close(op) => {
                        assert!(finished);
                        assert_eq!(
                            client.request(get("GET", BodyLength::Empty, false, HOST)),
                            Err(CommandError::InvalidState),
                        );
                        client.complete_close(op.complete(Ok(()))).unwrap();
                    }
                    Event::Closed(result) => {
                        assert_eq!(result, Ok(()));
                        closed = true;
                        break;
                    }
                    other => panic!("unexpected event: {other:?}"),
                }
            }
            assert!(head_seen && incoming_done && finished && closed);
            assert_eq!(body, expected_body);
            assert_eq!(
                client.request(get("GET", BodyLength::Empty, false, HOST)),
                Err(CommandError::InvalidState),
            );
            assert!(client.next(&mut Capture).is_none());
        }
    }
}

#[test]
fn clean_response_boundary_permits_a_subsequently_requested_response() {
    let mut client = client(config());
    for status in [200, 299] {
        let request = client
            .request(get("GET", BodyLength::Empty, false, HOST))
            .unwrap();
        let mut supplied = false;
        let mut head_seen = false;
        let mut finished = false;
        for _ in 0..32 {
            match next_client(&mut client).expect("exchange stalled") {
                Event::Write(op) => client.complete_write(finish_write(op)).unwrap(),
                Event::Read(op) => {
                    assert!(!supplied);
                    supplied = true;
                    let wire = format!("HTTP/1.1 {status} OK\r\nContent-Length: 0\r\n\r\n");
                    client.complete_read(fill(op, wire.as_bytes())).unwrap();
                }
                Event::Response(id, observed, false) => {
                    assert_eq!(id, request);
                    assert_eq!(observed, status);
                    assert!(!head_seen);
                    head_seen = true;
                }
                Event::Incoming(id) => assert_eq!(id, request),
                Event::Finished(result) => {
                    assert_eq!(result.exchange, request);
                    assert_eq!(result.result, Ok(()));
                    assert!(result.reusable);
                    assert!(client.buffered_input().is_empty());
                    finished = true;
                    break;
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert!(supplied && head_seen && finished);
        assert!(client.next(&mut Capture).is_none());
    }
}
