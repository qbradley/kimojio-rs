mod support;
use kimojio_fsm_http1::*;
use support::*;

#[derive(Default, Debug)]
struct Observed {
    body: Vec<u8>,
    trailers: Vec<(String, Vec<u8>)>,
    leftover: Vec<u8>,
    status: Option<u16>,
    result: Option<Result<(), Failure>>,
    incoming: bool,
    head: bool,
}

fn segment<'a>(wire: &'a [u8], cursor: &mut usize, split: usize, capacity: usize) -> &'a [u8] {
    let boundary = if *cursor < split { split } else { wire.len() };
    let end = boundary.min(cursor.saturating_add(capacity));
    let bytes = &wire[*cursor..end];
    *cursor = end;
    bytes
}

fn decode_response(method: &str, wire: &[u8], split: usize, upgrade: bool) -> Observed {
    let mut client = client(config());
    let mut headers = vec![Header {
        name: "host",
        value: b"localhost",
    }];
    if upgrade {
        headers.extend([
            Header {
                name: "connection",
                value: b"upgrade",
            },
            Header {
                name: "upgrade",
                value: b"websocket",
            },
        ]);
    }
    client
        .request(get(method, BodyLength::Empty, false, &headers))
        .unwrap();
    let mut cursor = 0;
    let mut observed = Observed::default();
    for _ in 0..10_000 {
        let Some(event) = client.next(&mut Capture) else {
            panic!("decoder stalled: {observed:?}")
        };
        match event {
            Event::Read(mut op) => {
                let capacity = op.bytes_mut().len();
                let bytes = segment(wire, &mut cursor, split, capacity);
                client.complete_read(fill(op, bytes)).unwrap();
            }
            Event::Write(op) => client.complete_write(finish_write(op)).unwrap(),
            Event::Response(id, status, informational) => {
                if !informational {
                    observed.status = Some(status);
                    client.grant_body_credit(id, 1024).unwrap();
                }
            }
            Event::Body(op) => {
                assert!(!observed.incoming);
                let id = op.exchange();
                let n = op.bytes().len();
                observed.body.extend_from_slice(op.bytes());
                client.release_body(op.release(n)).unwrap();
                client.grant_body_credit(id, n).unwrap();
            }
            Event::Trailers(trailers) => {
                assert!(!observed.incoming);
                observed.trailers = trailers;
            }
            Event::Incoming(_) => observed.incoming = true,
            Event::Finished(finished) => {
                observed.result = Some(finished.result);
                observed.leftover.extend_from_slice(client.buffered_input());
                observed.leftover.extend_from_slice(&wire[cursor..]);
                return observed;
            }
            Event::Upgrade(_) => {
                observed.result = Some(Ok(()));
                observed
                    .leftover
                    .extend_from_slice(client.take_upgrade().unwrap().buffered.bytes());
                observed.leftover.extend_from_slice(&wire[cursor..]);
                return observed;
            }
            Event::Deadline(_) => {}
            other => panic!("unexpected response event {other:?}"),
        }
    }
    panic!("unbounded response progress");
}

fn latin1(value: &serde_json::Value) -> Vec<u8> {
    value
        .as_str()
        .unwrap_or_default()
        .chars()
        .map(|c| u8::try_from(u32::from(c)).unwrap())
        .collect()
}

#[test]
fn independent_response_corpus_at_every_transport_split() {
    let corpus: serde_json::Value =
        serde_json::from_str(include_str!("data/response_framing.json")).unwrap();
    for case in corpus["cases"].as_array().unwrap() {
        let wire = latin1(&case["wire"]);
        let name = case["name"].as_str().unwrap();
        for split in 0..=wire.len() {
            let observed = decode_response(
                case["method"].as_str().unwrap(),
                &wire,
                split,
                name == "upgrade-leftover",
            );
            if case["error"] == true {
                assert!(
                    observed.result.unwrap().is_err(),
                    "{name} split={split}: {observed:?}"
                );
            } else {
                assert_eq!(
                    observed.result,
                    Some(Ok(())),
                    "{name} split={split}: {observed:?}"
                );
                assert_eq!(
                    observed.status,
                    Some(case["status"].as_u64().unwrap() as u16),
                    "{name} split={split}"
                );
                assert_eq!(observed.body, latin1(&case["body"]), "{name} split={split}");
                assert_eq!(
                    observed.leftover,
                    latin1(&case["leftover"]),
                    "{name} split={split}"
                );
                if let Some(trailers) = case["trailers"].as_array() {
                    let expected: Vec<_> = trailers
                        .iter()
                        .map(|h| (h[0].as_str().unwrap().to_owned(), latin1(&h[1])))
                        .collect();
                    assert_eq!(observed.trailers, expected, "{name} split={split}");
                }
                assert!(observed.incoming, "{name}: no incoming completion");
            }
        }
    }
}

fn decode_request(wire: &[u8], split: usize, config: Config) -> Observed {
    decode_request_fragmented(wire, split, config, usize::MAX)
}

fn decode_request_fragmented(
    wire: &[u8],
    split: usize,
    config: Config,
    fragment: usize,
) -> Observed {
    let mut server = server(config);
    let mut cursor = 0;
    let mut observed = Observed::default();
    let mut version = Version::Http11;
    for _ in 0..10_000 {
        let Some(event) = server.next(&mut Capture) else {
            panic!("request stalled: {observed:?}")
        };
        match event {
            Event::Read(mut op) => {
                let capacity = op.bytes_mut().len().min(fragment);
                let bytes = segment(wire, &mut cursor, split, capacity);
                server.complete_read(fill(op, bytes)).unwrap();
            }
            Event::Write(op) => server.complete_write(finish_write(op)).unwrap(),
            Event::Request(id, parsed_version) => {
                version = parsed_version;
                observed.head = true;
                server.grant_body_credit(id, 1024).unwrap();
            }
            Event::Body(op) => {
                let id = op.exchange();
                let n = op.bytes().len();
                assert!(!observed.incoming);
                observed.body.extend_from_slice(op.bytes());
                server.release_body(op.release(n)).unwrap();
                server.grant_body_credit(id, n).unwrap();
            }
            Event::Incoming(id) => {
                observed.incoming = true;
                let mut response = response(BodyLength::Empty);
                response.head.version = version;
                server.respond(id, response).unwrap();
            }
            Event::Trailers(trailers) => {
                assert!(!observed.incoming);
                observed.trailers = trailers;
            }
            Event::Finished(finished) => {
                observed.result = Some(finished.result);
                observed.leftover.extend_from_slice(server.buffered_input());
                observed.leftover.extend_from_slice(&wire[cursor..]);
                return observed;
            }
            Event::Close(op) => server.complete_close(op.complete(Ok(()))).unwrap(),
            Event::Closed(result) => {
                observed.result = Some(result);
                return observed;
            }
            Event::Deadline(_) => {}
            other => panic!("unexpected request event {other:?}"),
        }
    }
    panic!("unbounded request progress");
}

#[test]
fn historical_request_smuggling_negatives_at_every_split() {
    let headers = [
        "Content-Length: +5",
        "Content-Length: -5",
        "Content-Length: 0x5",
        "Content-Length: ",
        "Content-Length: 18446744073709551616",
        "Content-Length: 5\r\nContent-Length: 6",
        "Content-Length: 5, 6",
        "Transfer-Encoding: chunked\r\nContent-Length: 0",
        "Transfer-Encoding: chunked, chunked",
        "Transfer-Encoding: ",
        "Transfer-Encoding: ,",
        "Transfer-Encoding: gzip, chunked",
        "Transfer-Encoding: chunked; q=1",
        "Transfer-Encoding: identity",
        "Host: duplicate",
        "Bad Header: x",
        "Content-Length : 5",
        "X-Fold: one\r\n two",
        "X-Control: \u{7f}",
    ];
    for header in headers {
        let wire = format!("POST / HTTP/1.1\r\nHost: localhost\r\n{header}\r\n\r\nabcde");
        for split in 0..=wire.len() {
            let result = decode_request(wire.as_bytes(), split, config());
            assert!(
                result.result.unwrap().is_err(),
                "accepted {header:?}, split={split}"
            );
        }
    }
    for wire in [
        b"GET / HTTP/1.1\r\n\r\n".as_slice(),
        b"GET / HTTP/1.1\r\nHost:\r\n\r\n",
        b"GET / HTTP/1.1\r\nHost: a b\r\n\r\n",
        b"GET / HTTP/1.2\r\nHost: a\r\n\r\n",
        b"GET / HTTP/1.1\nHost: a\n\n",
        b"GET / HTTP/1.1\r\nHost: a\r\n",
        b"POST / HTTP/1.0\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
    ] {
        for split in 0..=wire.len() {
            assert!(
                decode_request(wire, split, config())
                    .result
                    .unwrap()
                    .is_err(),
                "{wire:?} split={split}"
            );
        }
    }
}

#[test]
fn duplicate_lengths_and_chunk_extensions_are_delimited_exactly() {
    for headers in [
        "Content-Length: 3",
        "Content-Length: 03, 3\r\nContent-Length: 3",
    ] {
        let wire = format!("POST / HTTP/1.1\r\nHost: a\r\n{headers}\r\n\r\nabcNEXT");
        for split in 0..=wire.len() {
            let result = decode_request(wire.as_bytes(), split, config());
            assert_eq!(result.result, Some(Ok(())), "{headers} split={split}");
            assert_eq!(result.body, b"abc");
            assert_eq!(result.leftover, b"NEXT");
        }
    }
    for size in [
        "3",
        "03",
        "3;foo",
        "3 ; foo = bar",
        "3;foo=\"a;\\\"b\";second=ok",
    ] {
        let wire = format!(
            "POST / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: ,CHUNKED,\r\n\r\n{size}\r\nabc\r\n0;end=1\r\nX-End: yes\r\n\r\nNEXT"
        );
        for split in 0..=wire.len() {
            let result = decode_request(wire.as_bytes(), split, config());
            assert_eq!(
                result.result,
                Some(Ok(())),
                "{size:?} split={split}: {result:?}"
            );
            assert_eq!(result.body, b"abc");
            assert_eq!(result.leftover, b"NEXT");
            assert_eq!(result.trailers, [("x-end".into(), b"yes".to_vec())]);
        }
    }
}

#[test]
fn malformed_chunks_and_forbidden_trailers_never_complete() {
    for size in [
        "",
        "+3",
        "-3",
        "0x3",
        " 3",
        "3 ",
        "10000000000000000",
        "3;",
        "3; =x",
        "3;x=",
        "3;x=\"unterminated",
        "3;x=\"a\u{7f}\"",
        "3;x=bad value",
    ] {
        let wire = format!(
            "POST / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n{size}\r\nabc\r\n0\r\n\r\n"
        );
        for split in 0..=wire.len() {
            let result = decode_request(wire.as_bytes(), split, config());
            assert!(
                result.result.unwrap().is_err(),
                "accepted chunk {size:?} split={split}"
            );
            assert!(!result.incoming);
        }
    }
    for trailer in [
        "Content-Length: 0",
        "Transfer-Encoding: chunked",
        "Host: b",
        "Connection: close",
        "Content-Type: text/plain",
        "X-Hop: secret",
    ] {
        let wire = format!(
            "POST / HTTP/1.1\r\nHost: a\r\nConnection: X-Hop\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n{trailer}\r\n\r\n"
        );
        for split in 0..=wire.len() {
            assert!(
                decode_request(wire.as_bytes(), split, config())
                    .result
                    .unwrap()
                    .is_err(),
                "{trailer} split={split}"
            );
        }
    }
}

#[test]
fn independent_static_fixture_survives_bytewise_chunks_and_pipelining() {
    for trailers in [false, true] {
        let mut wire =
            b"GET /fixture.bin HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n"
                .to_vec();
        if trailers {
            wire.extend_from_slice(b"Trailer: X-End\r\n");
        }
        wire.extend_from_slice(b"\r\n3;part=first\r\none\r\n3\r\ntwo\r\n0\r\n");
        if trailers {
            wire.extend_from_slice(b"X-End: done\r\n");
        }
        wire.extend_from_slice(b"\r\n");
        let next = b"GET /empty.bin HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        wire.extend_from_slice(next);
        for fragment in [1, 2, 3, 7, 1024] {
            let observed = decode_request_fragmented(&wire, wire.len(), config(), fragment);
            assert_eq!(
                observed.result,
                Some(Ok(())),
                "fragment={fragment}, trailers={trailers}: {observed:?}"
            );
            assert_eq!(observed.body, b"onetwo");
            assert_eq!(observed.leftover, next);
            assert!(observed.incoming);
            if trailers {
                assert_eq!(observed.trailers, [("x-end".into(), b"done".to_vec())]);
            }
        }
    }
}

#[test]
fn aggregate_head_trailer_chunk_and_body_limits() {
    let wire = b"POST / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\n1\r\nb\r\n0\r\nX-End: yes\r\n\r\n";
    for limited in [
        Config {
            max_head_bytes: 65,
            ..config()
        },
        Config {
            max_headers: 2,
            ..config()
        },
        Config {
            max_body_bytes: 1,
            ..config()
        },
        Config {
            max_chunk_line_bytes: 3,
            max_chunk_metadata_bytes: 5,
            ..config()
        },
    ] {
        assert_eq!(
            decode_request(wire, wire.len(), limited).result,
            Some(Err(Failure::Limit))
        );
    }
}

#[test]
fn reset_content_accepts_only_empty_framing_at_every_split() {
    for (suffix, valid) in [
        ("\r\n", true),
        ("Content-Length: 0\r\n\r\n", true),
        ("Transfer-Encoding: chunked\r\n\r\n0\r\n\r\n", true),
        ("Content-Length: 1\r\n\r\nx", false),
        (
            "Transfer-Encoding: chunked\r\n\r\n1\r\nx\r\n0\r\n\r\n",
            false,
        ),
        ("\r\nx", false),
    ] {
        let wire = format!("HTTP/1.1 205 Reset Content\r\n{suffix}");
        for split in 0..=wire.len() {
            let observed = decode_response("GET", wire.as_bytes(), split, false);
            assert_eq!(
                observed.result.unwrap().is_ok(),
                valid,
                "{suffix:?} split={split}"
            );
            assert!(observed.body.is_empty());
        }
    }
}

#[test]
fn informational_response_sequences_are_bounded_and_preserve_final_framing() {
    let valid = b"HTTP/1.1 103 Early Hints\r\nLink: </style.css>\r\n\r\nHTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabcNEXT";
    for split in 0..=valid.len() {
        let result = decode_response("GET", valid, split, false);
        assert_eq!(result.result, Some(Ok(())));
        assert_eq!(result.status, Some(200));
        assert_eq!(result.body, b"abc");
        assert_eq!(result.leftover, b"NEXT");
    }
    for wire in [
        b"HTTP/1.1 100 Continue\r\nContent-Length: 0\r\n\r\n".to_vec(),
        b"HTTP/1.1 103 Hints\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec(),
        b"HTTP/1.1 099 Bad\r\n\r\n".to_vec(),
        b"HTTP/1.1 600 Bad\r\n\r\n".to_vec(),
        b"HTTP/1.1 103 Hints\r\n\r\n".repeat(17),
    ] {
        assert!(
            decode_response("GET", &wire, wire.len(), false)
                .result
                .unwrap()
                .is_err()
        );
    }
}
