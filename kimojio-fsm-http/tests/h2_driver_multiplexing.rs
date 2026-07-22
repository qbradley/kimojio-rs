use std::collections::{BTreeMap, BTreeSet};

use kimojio_fsm_http::{
    ConnectionResponse, ExchangeId, H2_DEFAULT_MAX_ACTIVE_STREAMS, H2Client, H2ErrorCode, H2Frame,
    H2FrameType, H2HeaderBlockDecoder, H2HeaderField, H2Limits, H2OutboundCommit, HttpLimits,
    HttpProtocol, HttpVersion, ServerConnection, ServerError, ServerEvent, Step,
    project_h2_response_head,
};

#[derive(Debug, Eq, PartialEq)]
enum Observed {
    NeedInput,
    Write(Vec<u8>),
    Head {
        exchange_id: ExchangeId,
        method: Vec<u8>,
        target: Vec<u8>,
        version: HttpVersion,
    },
    Body {
        exchange_id: ExchangeId,
        chunk: Vec<u8>,
    },
    Complete {
        exchange_id: ExchangeId,
    },
    Done,
}

fn server_step(connection: &mut ServerConnection, input: &[u8]) -> Result<Observed, ServerError> {
    connection.step(input, |step| match step {
        Step::NeedInput => Observed::NeedInput,
        Step::Write(bytes) => Observed::Write(bytes.to_vec()),
        Step::Event(ServerEvent::RequestHead {
            exchange_id,
            method,
            target,
            version,
            ..
        }) => Observed::Head {
            exchange_id,
            method: method.to_vec(),
            target: target.to_vec(),
            version,
        },
        Step::Event(ServerEvent::RequestBody { exchange_id, chunk }) => Observed::Body {
            exchange_id,
            chunk: chunk.to_vec(),
        },
        Step::Event(ServerEvent::RequestComplete { exchange_id }) => {
            Observed::Complete { exchange_id }
        }
        Step::Done => Observed::Done,
        _ => panic!("unexpected server step variant"),
    })
}

fn take_client_block(client: &mut H2Client, commit: H2OutboundCommit) -> Vec<u8> {
    let block = client
        .next_outbound_block()
        .expect("client must queue the request header block");
    assert_eq!(block.commit(), commit);
    let bytes = block.bytes().to_vec();
    client.acknowledge_outbound_block(commit).unwrap();
    bytes
}

fn open_request(
    client: &mut H2Client,
    method: &str,
    target: &str,
    body_len: usize,
    end_stream: bool,
) -> (u32, Vec<u8>) {
    let content_length = body_len.to_string();
    let headers = [H2HeaderField::new(
        b"content-length",
        content_length.as_bytes(),
    )];
    let (stream_id, commit) = client
        .open_stream_with_raw_headers(method, "http", "example.test", target, &headers, end_stream)
        .unwrap();
    (stream_id, take_client_block(client, commit))
}

fn collect_request_events(
    connection: &mut ServerConnection,
    input: &[u8],
    expected_heads: usize,
    expected_completions: usize,
) -> Vec<Observed> {
    let mut offset = 0;
    let mut heads = 0;
    let mut completions = 0;
    let mut events = Vec::new();

    for _ in 0..10_000 {
        let observed = server_step(connection, &input[offset..]).unwrap();
        let consumed = connection.consumed();
        match &observed {
            Observed::Head { .. } => {
                heads += 1;
                events.push(observed);
            }
            Observed::Body { .. } => events.push(observed),
            Observed::Complete { .. } => {
                completions += 1;
                events.push(observed);
            }
            Observed::Write(bytes) => assert!(!bytes.is_empty()),
            Observed::NeedInput => {
                panic!("driver requested more input before all expected request events")
            }
            Observed::Done => panic!("driver finished before responses were prepared"),
        }
        connection.consume(consumed).unwrap();
        offset += consumed;
        assert!(offset <= input.len());

        if offset == input.len() && heads == expected_heads && completions == expected_completions {
            return events;
        }
        assert!(heads <= expected_heads);
        assert!(completions <= expected_completions);
    }

    panic!("request event collection did not converge")
}

fn take_response_header(
    connection: &mut ServerConnection,
    decoder: &mut H2HeaderBlockDecoder,
) -> (u32, u16, bool) {
    let Observed::Write(bytes) = server_step(connection, &[]).unwrap() else {
        panic!("prepared response headers must be written");
    };
    assert_eq!(connection.consumed(), 0);
    connection.consume(0).unwrap();

    let (frame, consumed) = H2Frame::decode(&bytes).unwrap();
    assert_eq!(consumed, bytes.len());
    assert_eq!(frame.frame_type, H2FrameType::Headers);
    let fields = decoder
        .try_decode_with_limit(&frame.payload, usize::MAX)
        .unwrap();
    let head = project_h2_response_head(&fields, HttpLimits::new()).unwrap();
    (frame.stream_id, head.status(), frame.flags & 0x1 != 0)
}

fn prepare_and_commit_body(
    connection: &mut ServerConnection,
    exchange_id: ExchangeId,
    payload: &[u8],
) -> H2Frame {
    assert!(
        connection
            .prepare_body_chunk(exchange_id, payload.len())
            .unwrap()
    );
    let wire = {
        let chunk = connection
            .body_chunk(exchange_id)
            .expect("prepared exchange must expose its body framing");
        assert_eq!(chunk.payload_len(), payload.len());
        let mut wire = chunk.header().to_vec();
        wire.extend_from_slice(payload);
        wire
    };
    connection.commit_body_chunk(exchange_id).unwrap();

    let (frame, consumed) = H2Frame::decode(&wire).unwrap();
    assert_eq!(consumed, wire.len());
    frame
}

fn reset_code_for_stream(bytes: &[u8], stream_id: u32) -> Option<u32> {
    let mut offset = 0;
    while offset < bytes.len() {
        let (frame, consumed) = H2Frame::decode(&bytes[offset..]).unwrap();
        offset += consumed;
        if frame.frame_type == H2FrameType::RstStream && frame.stream_id == stream_id {
            let payload: [u8; 4] = frame.payload.try_into().unwrap();
            return Some(u32::from_be_bytes(payload));
        }
    }
    None
}

#[test]
fn concurrent_http2_request_heads_receive_distinct_exchange_ids_before_any_response() {
    let mut client = H2Client::default();
    let mut input = client.connection_preface();
    let (first_stream, first) = open_request(&mut client, "GET", "/one", 0, true);
    input.extend_from_slice(&first);
    let (second_stream, second) = open_request(&mut client, "GET", "/three", 0, true);
    input.extend_from_slice(&second);
    let (third_stream, third) = open_request(&mut client, "GET", "/five", 0, true);
    input.extend_from_slice(&third);

    let mut connection = ServerConnection::new(HttpLimits::new());
    let events = collect_request_events(&mut connection, &input, 3, 3);
    let heads = events
        .iter()
        .filter_map(|event| match event {
            Observed::Head {
                exchange_id,
                method,
                target,
                version,
            } => Some((
                exchange_id.as_u64(),
                method.as_slice(),
                target.as_slice(),
                *version,
            )),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(
        heads,
        vec![
            (
                u64::from(first_stream),
                b"GET".as_slice(),
                b"/one".as_slice(),
                HttpVersion::Http2,
            ),
            (
                u64::from(second_stream),
                b"GET".as_slice(),
                b"/three".as_slice(),
                HttpVersion::Http2,
            ),
            (
                u64::from(third_stream),
                b"GET".as_slice(),
                b"/five".as_slice(),
                HttpVersion::Http2,
            ),
        ]
    );
    assert_eq!(
        heads
            .iter()
            .map(|(exchange_id, ..)| *exchange_id)
            .collect::<BTreeSet<_>>()
            .len(),
        3
    );
}

#[test]
fn interleaved_http2_data_is_routed_only_to_its_own_exchange() {
    let mut client = H2Client::default();
    let mut input = client.connection_preface();
    let (first_stream, first) = open_request(&mut client, "POST", "/one", 9, false);
    input.extend_from_slice(&first);
    let (second_stream, second) = open_request(&mut client, "POST", "/three", 10, false);
    input.extend_from_slice(&second);
    let (third_stream, third) = open_request(&mut client, "POST", "/five", 4, false);
    input.extend_from_slice(&third);
    input.extend_from_slice(&client.data_frame(first_stream, b"one-", false));
    input.extend_from_slice(&client.data_frame(second_stream, b"three-", false));
    input.extend_from_slice(&client.data_frame(first_stream, b"alpha", true));
    input.extend_from_slice(&client.data_frame(third_stream, b"five", true));
    input.extend_from_slice(&client.data_frame(second_stream, b"beta", true));

    let mut connection = ServerConnection::new(HttpLimits::new());
    let events = collect_request_events(&mut connection, &input, 3, 3);
    let body_events = events
        .iter()
        .filter_map(|event| match event {
            Observed::Body { exchange_id, chunk } => Some((exchange_id.as_u64(), chunk.as_slice())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        body_events,
        vec![
            (u64::from(first_stream), b"one-".as_slice()),
            (u64::from(second_stream), b"three-".as_slice()),
            (u64::from(first_stream), b"alpha".as_slice()),
            (u64::from(third_stream), b"five".as_slice()),
            (u64::from(second_stream), b"beta".as_slice()),
        ]
    );

    let mut bodies = BTreeMap::<u64, Vec<u8>>::new();
    for (exchange_id, chunk) in body_events {
        bodies
            .entry(exchange_id)
            .or_default()
            .extend_from_slice(chunk);
    }
    assert_eq!(bodies[&u64::from(first_stream)], b"one-alpha");
    assert_eq!(bodies[&u64::from(second_stream)], b"three-beta");
    assert_eq!(bodies[&u64::from(third_stream)], b"five");

    let completed = events
        .iter()
        .filter_map(|event| match event {
            Observed::Complete { exchange_id } => Some(exchange_id.as_u64()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        completed,
        BTreeSet::from([
            u64::from(first_stream),
            u64::from(second_stream),
            u64::from(third_stream),
        ])
    );
}

#[test]
fn http2_responses_can_finish_in_application_selected_stream_order() {
    let mut client = H2Client::default();
    let mut input = client.connection_preface();
    let (first_stream, first) = open_request(&mut client, "GET", "/one", 0, true);
    input.extend_from_slice(&first);
    let (second_stream, second) = open_request(&mut client, "GET", "/three", 0, true);
    input.extend_from_slice(&second);
    let (third_stream, third) = open_request(&mut client, "GET", "/five", 0, true);
    input.extend_from_slice(&third);

    let mut connection = ServerConnection::new(HttpLimits::new());
    let events = collect_request_events(&mut connection, &input, 3, 3);
    let exchanges = events
        .iter()
        .filter_map(|event| match event {
            Observed::Head { exchange_id, .. } => Some((exchange_id.as_u64(), *exchange_id)),
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    let first = exchanges[&u64::from(first_stream)];
    let second = exchanges[&u64::from(second_stream)];
    let third = exchanges[&u64::from(third_stream)];

    let response_order = [
        (second, 203, b"three".as_slice()),
        (first, 201, b"one".as_slice()),
        (third, 206, b"five".as_slice()),
    ];
    for (exchange_id, status, body) in response_order {
        assert!(
            connection
                .prepare_response(
                    exchange_id,
                    ConnectionResponse {
                        status,
                        reason: "OK",
                        headers: &[],
                        body_len: Some(body.len()),
                    },
                )
                .unwrap()
        );
    }

    let mut decoder = H2HeaderBlockDecoder::new();
    assert_eq!(
        take_response_header(&mut connection, &mut decoder),
        (second_stream, 203, false)
    );
    assert_eq!(
        take_response_header(&mut connection, &mut decoder),
        (first_stream, 201, false)
    );
    assert_eq!(
        take_response_header(&mut connection, &mut decoder),
        (third_stream, 206, false)
    );

    assert!(!connection.prepare_body_chunk(first, b"one".len()).unwrap());
    assert!(connection.body_chunk(first).is_none());
    let second_body = prepare_and_commit_body(&mut connection, second, b"three");
    assert_eq!(second_body.frame_type, H2FrameType::Data);
    assert_eq!(second_body.stream_id, second_stream);
    assert_eq!(second_body.flags & 0x1, 0x1);
    assert_eq!(second_body.payload, b"three");

    assert!(!connection.prepare_body_chunk(third, b"five".len()).unwrap());
    assert!(connection.body_chunk(third).is_none());
    let first_body = prepare_and_commit_body(&mut connection, first, b"one");
    assert_eq!(first_body.frame_type, H2FrameType::Data);
    assert_eq!(first_body.stream_id, first_stream);
    assert_eq!(first_body.flags & 0x1, 0x1);
    assert_eq!(first_body.payload, b"one");

    let third_body = prepare_and_commit_body(&mut connection, third, b"five");
    assert_eq!(third_body.frame_type, H2FrameType::Data);
    assert_eq!(third_body.stream_id, third_stream);
    assert_eq!(third_body.flags & 0x1, 0x1);
    assert_eq!(third_body.payload, b"five");
    assert_eq!(server_step(&mut connection, &[]).unwrap(), Observed::Done);

    assert!(connection.begin_next_exchange(second).unwrap());
    assert!(connection.begin_next_exchange(first).unwrap());
    assert!(connection.begin_next_exchange(third).unwrap());
}

#[test]
fn resetting_one_http2_stream_leaves_sibling_exchanges_usable() {
    let mut client = H2Client::default();
    let mut input = client.connection_preface();
    let (sibling_stream, sibling) = open_request(&mut client, "GET", "/sibling", 0, true);
    input.extend_from_slice(&sibling);
    let (reset_stream, reset_request) = open_request(&mut client, "POST", "/reset", 1, false);
    input.extend_from_slice(&reset_request);

    let mut connection = ServerConnection::new(HttpLimits::new());
    let events = collect_request_events(&mut connection, &input, 2, 1);
    let exchanges = events
        .iter()
        .filter_map(|event| match event {
            Observed::Head { exchange_id, .. } => Some((exchange_id.as_u64(), *exchange_id)),
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    let sibling_exchange = exchanges[&u64::from(sibling_stream)];
    let reset_exchange = exchanges[&u64::from(reset_stream)];

    let mut reset = Vec::new();
    H2Frame {
        frame_type: H2FrameType::RstStream,
        flags: 0,
        stream_id: reset_stream,
        payload: H2ErrorCode::Cancel.as_u32().to_be_bytes().to_vec(),
    }
    .encode(&mut reset);
    assert_eq!(
        server_step(&mut connection, &reset),
        Err(ServerError::PeerReset {
            stream_id: reset_stream,
            error_code: H2ErrorCode::Cancel.as_u32(),
        })
    );
    assert_eq!(connection.cancel_exchange().unwrap(), Some(reset_exchange));
    let consumed = connection.consumed();
    assert_eq!(consumed, reset.len());
    connection.consume(consumed).unwrap();
    assert!(connection.begin_next_exchange(reset_exchange).unwrap());

    assert!(
        !connection
            .prepare_response(
                sibling_exchange,
                ConnectionResponse {
                    status: 204,
                    reason: "No Content",
                    headers: &[],
                    body_len: Some(0),
                },
            )
            .unwrap()
    );
    let mut decoder = H2HeaderBlockDecoder::new();
    assert_eq!(
        take_response_header(&mut connection, &mut decoder),
        (sibling_stream, 204, true)
    );
    assert_eq!(server_step(&mut connection, &[]).unwrap(), Observed::Done);
    assert!(connection.begin_next_exchange(sibling_exchange).unwrap());
}

#[test]
fn http2_concurrency_ceiling_rejects_the_excess_stream() {
    let peer_limit = H2_DEFAULT_MAX_ACTIVE_STREAMS + 1;
    let mut client = H2Client::with_limits(H2Limits {
        max_active_streams: peer_limit,
        ..H2Limits::default()
    })
    .unwrap();
    let mut accepted_input = client.connection_preface();
    let mut accepted_streams = Vec::with_capacity(H2_DEFAULT_MAX_ACTIVE_STREAMS);
    for index in 0..H2_DEFAULT_MAX_ACTIVE_STREAMS {
        let target = format!("/stream/{index}");
        let (stream_id, request) = open_request(&mut client, "GET", &target, 0, true);
        accepted_streams.push(stream_id);
        accepted_input.extend_from_slice(&request);
    }
    let (excess_stream, excess_input) = open_request(&mut client, "GET", "/excess", 0, true);

    let mut connection = ServerConnection::new(HttpLimits::new());
    let events = collect_request_events(
        &mut connection,
        &accepted_input,
        H2_DEFAULT_MAX_ACTIVE_STREAMS,
        H2_DEFAULT_MAX_ACTIVE_STREAMS,
    );
    let observed_streams = events
        .iter()
        .filter_map(|event| match event {
            Observed::Head { exchange_id, .. } => {
                Some(u32::try_from(exchange_id.as_u64()).unwrap())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(observed_streams, accepted_streams);

    let mut offset = 0;
    let mut rejected = false;
    for _ in 0..16 {
        match server_step(&mut connection, &excess_input[offset..]) {
            Err(_) => {
                rejected = true;
                break;
            }
            Ok(observed) => {
                let consumed = connection.consumed();
                match observed {
                    Observed::Write(bytes) => {
                        if let Some(error_code) = reset_code_for_stream(&bytes, excess_stream) {
                            assert_eq!(error_code, H2ErrorCode::RefusedStream.as_u32());
                            rejected = true;
                        }
                    }
                    Observed::Head { exchange_id, .. } => {
                        assert_ne!(exchange_id.as_u64(), u64::from(excess_stream));
                    }
                    Observed::Body { exchange_id, .. } | Observed::Complete { exchange_id } => {
                        assert_ne!(exchange_id.as_u64(), u64::from(excess_stream));
                    }
                    Observed::NeedInput | Observed::Done => {}
                }
                connection.consume(consumed).unwrap();
                offset += consumed;
                assert!(offset <= excess_input.len());
                if rejected {
                    break;
                }
            }
        }
    }
    assert!(
        rejected,
        "the stream above the active-stream ceiling was accepted"
    );
}

#[test]
fn http1_requests_complete_with_monotonic_synthetic_exchange_ids() {
    let input = b"POST /submit HTTP/1.1\r\nhost: example.test\r\ncontent-length: 3\r\n\r\nabc";
    let mut connection = ServerConnection::new(HttpLimits::new());
    let events = collect_request_events(&mut connection, input, 1, 1);
    assert_eq!(connection.protocol(), Some(HttpProtocol::Http1));

    let Observed::Head {
        exchange_id,
        method,
        target,
        version,
    } = &events[0]
    else {
        panic!("HTTP/1 request must begin with its head");
    };
    assert_eq!(exchange_id.as_u64(), 1);
    assert_eq!(method, b"POST");
    assert_eq!(target, b"/submit");
    assert_eq!(*version, HttpVersion::Http11);
    assert_eq!(
        events[1],
        Observed::Body {
            exchange_id: *exchange_id,
            chunk: b"abc".to_vec(),
        }
    );
    assert_eq!(
        events[2],
        Observed::Complete {
            exchange_id: *exchange_id,
        }
    );

    assert!(
        connection
            .prepare_response(
                *exchange_id,
                ConnectionResponse {
                    status: 200,
                    reason: "OK",
                    headers: &[],
                    body_len: Some(2),
                },
            )
            .unwrap()
    );
    assert!(connection.prepare_body_chunk(*exchange_id, 2).unwrap());
    let wire = {
        let chunk = connection.body_chunk(*exchange_id).unwrap();
        assert_eq!(chunk.payload_len(), 2);
        let mut wire = chunk.header().to_vec();
        wire.extend_from_slice(b"ok");
        wire
    };
    let response = String::from_utf8(wire).unwrap().to_ascii_lowercase();
    assert!(response.starts_with("http/1.1 200 ok\r\n"));
    assert!(response.contains("content-length: 2\r\n"));
    assert!(response.ends_with("\r\n\r\nok"));

    connection.commit_body_chunk(*exchange_id).unwrap();
    assert_eq!(server_step(&mut connection, &[]).unwrap(), Observed::Done);
    assert!(connection.begin_next_exchange(*exchange_id).unwrap());

    let Observed::Head {
        exchange_id: second,
        target,
        ..
    } = server_step(
        &mut connection,
        b"GET /next HTTP/1.1\r\nhost: example.test\r\n\r\n",
    )
    .unwrap()
    else {
        panic!("the persistent HTTP/1 connection did not decode its next request");
    };
    assert_eq!(second.as_u64(), 2);
    assert_eq!(target, b"/next");
}
