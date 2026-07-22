use std::collections::{BTreeMap, BTreeSet};

use kimojio_fsm_http::{
    ClientConnection, ClientEvent, ClientIdleStatus, ClientRequest, ExchangeId, H2ByteStreamEvent,
    H2ErrorCode, H2Frame, H2FrameType, H2HeaderField, H2Limits, H2Server, HttpErrorKind,
    HttpLimits, HttpProtocol, ServerError, Step,
};

#[derive(Debug, Eq, PartialEq)]
enum Observed {
    NeedInput,
    Write(Vec<u8>),
    Head {
        exchange_id: ExchangeId,
        status: u16,
    },
    Body {
        exchange_id: ExchangeId,
        bytes: Vec<u8>,
    },
    Complete {
        exchange_id: ExchangeId,
    },
    Done,
}

fn step(connection: &mut ClientConnection, input: &[u8]) -> Result<Observed, ServerError> {
    connection.step(input, |step| match step {
        Step::NeedInput => Observed::NeedInput,
        Step::Write(bytes) => Observed::Write(bytes.to_vec()),
        Step::Event(ClientEvent::ResponseHead {
            exchange_id,
            status,
            ..
        }) => Observed::Head {
            exchange_id,
            status,
        },
        Step::Event(ClientEvent::ResponseBody { exchange_id, chunk }) => Observed::Body {
            exchange_id,
            bytes: chunk.to_vec(),
        },
        Step::Event(ClientEvent::ResponseComplete { exchange_id }) => {
            Observed::Complete { exchange_id }
        }
        Step::Done => Observed::Done,
        _ => panic!("unexpected client step"),
    })
}

fn start_get(connection: &mut ClientConnection, target: &str) -> ExchangeId {
    connection
        .prepare_request(ClientRequest {
            method: "GET",
            scheme: "http",
            authority: "example.test",
            target,
            headers: &[],
            body_len: Some(0),
        })
        .unwrap()
}

fn start_post(connection: &mut ClientConnection, target: &str, body_len: usize) -> ExchangeId {
    connection
        .prepare_request(ClientRequest {
            method: "POST",
            scheme: "http",
            authority: "example.test",
            target,
            headers: &[],
            body_len: Some(body_len),
        })
        .unwrap()
}

fn drain_request_output(connection: &mut ClientConnection) -> Vec<u8> {
    let mut wire = Vec::new();
    for _ in 0..16 {
        match step(connection, &[]).unwrap() {
            Observed::Write(bytes) => {
                wire.extend_from_slice(&bytes);
                connection.consume(connection.consumed()).unwrap();
            }
            Observed::NeedInput => {
                connection.consume(connection.consumed()).unwrap();
                return wire;
            }
            observed => panic!("unexpected request progress: {observed:?}"),
        }
    }
    panic!("request output did not drain");
}

fn prepare_get(connection: &mut ClientConnection, target: &str) -> (ExchangeId, Vec<u8>) {
    let exchange_id = start_get(connection, target);
    (exchange_id, drain_request_output(connection))
}

fn accept_requests(server: &mut H2Server, input: &[u8]) -> (Vec<u32>, Vec<u8>) {
    let mut offset = 0;
    let mut stream_ids = Vec::new();
    let mut control = Vec::new();

    for _ in 0..64 {
        let (event, consumed, output) = server.accept_event_bytes(&input[offset..]).unwrap();
        assert_ne!(consumed, 0);
        offset += consumed;
        control.extend_from_slice(&output);
        if let Some(H2ByteStreamEvent::RequestHeaders {
            stream_id: observed,
            ..
        }) = event
        {
            stream_ids.push(observed);
        }
        if offset == input.len() {
            assert_eq!(offset, input.len());
            return (stream_ids, control);
        }
    }
    panic!("server did not receive requests");
}

fn accept_request(server: &mut H2Server, input: &[u8]) -> (u32, Vec<u8>) {
    let (stream_ids, control) = accept_requests(server, input);
    let [stream_id] = stream_ids.as_slice() else {
        panic!("server did not receive exactly one request");
    };
    (*stream_id, control)
}

fn accept_control(server: &mut H2Server, input: &[u8]) {
    let mut offset = 0;
    while offset < input.len() {
        let (event, consumed, output) = server.accept_event_bytes(&input[offset..]).unwrap();
        assert_ne!(consumed, 0);
        assert!(!matches!(
            event,
            Some(H2ByteStreamEvent::RequestHeaders { .. })
        ));
        assert!(output.is_empty());
        offset += consumed;
    }
}

fn receive_control(connection: &mut ClientConnection, input: &[u8]) -> Vec<u8> {
    let mut offset = 0;
    let mut outbound = Vec::new();

    for _ in 0..32 {
        if offset == input.len() {
            return outbound;
        }
        let observed = step(connection, &input[offset..]).unwrap();
        let consumed = connection.consumed();
        connection.consume(consumed).unwrap();
        offset += consumed;
        match observed {
            Observed::Write(bytes) => outbound.extend_from_slice(&bytes),
            Observed::NeedInput if consumed != 0 => {}
            observed => panic!("unexpected control progress: {observed:?}"),
        }
    }
    panic!("client did not consume control input");
}

fn response_bytes(
    server: &mut H2Server,
    stream_id: u32,
    body: &[u8],
    mut prefix: Vec<u8>,
) -> Vec<u8> {
    let content_length = body.len().to_string();
    let fields = [H2HeaderField::new(
        b"content-length",
        content_length.as_bytes(),
    )];
    let commit = server
        .response_headers_frame_with_raw_headers(stream_id, 200, &fields, body.is_empty())
        .unwrap();
    let block = server.next_outbound_block().unwrap();
    assert_eq!(block.commit(), commit);
    prefix.extend_from_slice(block.bytes());
    server.acknowledge_outbound_block(commit).unwrap();
    if !body.is_empty() {
        prefix.extend_from_slice(&server.data_frame(stream_id, body, true));
    }
    prefix
}

fn receive_response(
    connection: &mut ClientConnection,
    exchange_id: ExchangeId,
    input: &[u8],
) -> (Vec<u8>, Vec<u8>, usize) {
    let mut body = Vec::new();
    let mut outbound = Vec::new();
    let mut offset = 0;
    let mut head_seen = false;

    for _ in 0..32 {
        let observed = step(connection, &input[offset..]).unwrap();
        let consumed = connection.consumed();
        connection.consume(consumed).unwrap();
        offset += consumed;
        match observed {
            Observed::Write(bytes) => outbound.extend_from_slice(&bytes),
            Observed::Head {
                exchange_id: observed_exchange,
                status,
            } => {
                assert_eq!(observed_exchange, exchange_id);
                assert_eq!(status, 200);
                assert!(!head_seen);
                head_seen = true;
            }
            Observed::Body {
                exchange_id: observed_exchange,
                bytes,
            } => {
                assert_eq!(observed_exchange, exchange_id);
                body.extend_from_slice(&bytes);
            }
            Observed::Complete {
                exchange_id: observed_exchange,
            } => {
                assert_eq!(observed_exchange, exchange_id);
                assert!(head_seen);
                return (body, outbound, offset);
            }
            Observed::NeedInput if consumed != 0 => {}
            Observed::NeedInput => panic!("complete response bytes were already supplied"),
            Observed::Done => panic!("response completed without a completion event"),
        }
    }
    panic!("response did not complete");
}

fn complete_exchange(connection: &mut ClientConnection, server: &mut H2Server) -> u32 {
    let (exchange_id, request) = prepare_get(connection, "/prime");
    let (stream_id, control) = accept_request(server, &request);
    assert_eq!(exchange_id.as_u64(), u64::from(stream_id));
    let response = response_bytes(server, stream_id, b"prime", control);
    let (body, client_control, consumed) = receive_response(connection, exchange_id, &response);
    assert_eq!(body, b"prime");
    assert_eq!(consumed, response.len());
    accept_control(server, &client_control);
    assert!(connection.begin_next_exchange(exchange_id).unwrap());
    stream_id
}

fn frame(frame_type: H2FrameType, flags: u8, stream_id: u32, payload: &[u8]) -> Vec<u8> {
    let mut wire = Vec::new();
    H2Frame {
        frame_type,
        flags,
        stream_id,
        payload: payload.to_vec(),
    }
    .encode(&mut wire);
    wire
}

#[test]
fn sequential_exchanges_use_increasing_client_stream_ids() {
    let mut connection = ClientConnection::new(HttpProtocol::Http2, HttpLimits::new());
    let mut server = H2Server::default();

    let (first_exchange, request) = prepare_get(&mut connection, "/one");
    let (first_stream, control) = accept_request(&mut server, &request);
    assert_eq!(first_exchange.as_u64(), u64::from(first_stream));
    let response = response_bytes(&mut server, first_stream, b"first", control);
    let (body, client_control, consumed) =
        receive_response(&mut connection, first_exchange, &response);
    assert_eq!(body, b"first");
    assert_eq!(consumed, response.len());
    accept_control(&mut server, &client_control);
    assert!(connection.begin_next_exchange(first_exchange).unwrap());

    let (second_exchange, request) = prepare_get(&mut connection, "/two");
    let (second_stream, control) = accept_request(&mut server, &request);
    assert_eq!(second_exchange.as_u64(), u64::from(second_stream));
    assert_eq!(second_stream, first_stream + 2);
    assert!(second_stream > first_stream);
    let response = response_bytes(&mut server, second_stream, b"second", control);
    let (body, client_control, consumed) =
        receive_response(&mut connection, second_exchange, &response);
    assert_eq!(body, b"second");
    assert_eq!(consumed, response.len());
    accept_control(&mut server, &client_control);
    assert!(connection.begin_next_exchange(second_exchange).unwrap());
}

#[test]
fn concurrent_exchanges_receive_interleaved_responses() {
    let mut connection = ClientConnection::new(HttpProtocol::Http2, HttpLimits::new());
    let mut server = H2Server::default();

    let first_exchange = start_get(&mut connection, "/one");
    let second_exchange = start_get(&mut connection, "/two");
    let requests = drain_request_output(&mut connection);
    let mut request_offset = 0;
    let mut streams = Vec::new();
    let mut response = Vec::new();
    while request_offset < requests.len() {
        let (event, consumed, control) = server
            .accept_event_bytes(&requests[request_offset..])
            .unwrap();
        assert_ne!(consumed, 0);
        request_offset += consumed;
        response.extend_from_slice(&control);
        if let Some(H2ByteStreamEvent::RequestHeaders { stream_id, .. }) = event {
            streams.push(stream_id);
        }
    }
    let [first_stream, second_stream] = streams.as_slice() else {
        panic!("server did not receive both requests");
    };
    let (first_stream, second_stream) = (*first_stream, *second_stream);
    assert_eq!(first_exchange.as_u64(), u64::from(first_stream));
    assert_eq!(second_exchange.as_u64(), u64::from(second_stream));

    let fields = [H2HeaderField::new(b"content-length", b"2")];
    let first_commit = server
        .response_headers_frame_with_raw_headers(first_stream, 200, &fields, false)
        .unwrap();
    let second_commit = server
        .response_headers_frame_with_raw_headers(second_stream, 200, &fields, false)
        .unwrap();
    for commit in [first_commit, second_commit] {
        let block = server.next_outbound_block().unwrap();
        assert_eq!(block.commit(), commit);
        response.extend_from_slice(block.bytes());
        server.acknowledge_outbound_block(commit).unwrap();
    }
    response.extend_from_slice(&server.data_frame(first_stream, b"a", false));
    response.extend_from_slice(&server.data_frame(second_stream, b"b", false));
    response.extend_from_slice(&server.data_frame(first_stream, b"1", true));
    response.extend_from_slice(&server.data_frame(second_stream, b"2", true));

    let mut offset = 0;
    let mut first_body = Vec::new();
    let mut second_body = Vec::new();
    let mut heads = Vec::new();
    let mut completed = Vec::new();
    let mut client_control = Vec::new();
    for _ in 0..64 {
        let observed = step(&mut connection, &response[offset..]).unwrap();
        let consumed = connection.consumed();
        connection.consume(consumed).unwrap();
        offset += consumed;
        match observed {
            Observed::Write(bytes) => client_control.extend_from_slice(&bytes),
            Observed::Head {
                exchange_id,
                status,
            } => {
                assert_eq!(status, 200);
                heads.push(exchange_id);
            }
            Observed::Body { exchange_id, bytes } if exchange_id == first_exchange => {
                first_body.extend_from_slice(&bytes);
            }
            Observed::Body { exchange_id, bytes } if exchange_id == second_exchange => {
                second_body.extend_from_slice(&bytes);
            }
            Observed::Complete { exchange_id } => completed.push(exchange_id),
            Observed::NeedInput if consumed != 0 => {}
            Observed::NeedInput => panic!("complete response bytes were already supplied"),
            Observed::Done => break,
            Observed::Body { exchange_id, .. } => {
                panic!("unexpected exchange body: {exchange_id:?}")
            }
        }
        if completed.len() == 2 {
            break;
        }
    }

    assert_eq!(offset, response.len());
    assert_eq!(heads, [first_exchange, second_exchange]);
    assert_eq!(first_body, b"a1");
    assert_eq!(second_body, b"b2");
    assert_eq!(completed, [first_exchange, second_exchange]);
    assert_eq!(step(&mut connection, &[]).unwrap(), Observed::Done);
    accept_control(&mut server, &client_control);
    assert!(connection.begin_next_exchange(first_exchange).unwrap());
    assert!(connection.begin_next_exchange(second_exchange).unwrap());
}

#[test]
fn concurrent_exchanges_complete_and_retire_out_of_order() {
    let mut connection = ClientConnection::new(HttpProtocol::Http2, HttpLimits::new());
    let mut server = H2Server::default();

    let first_exchange = start_get(&mut connection, "/one");
    let second_exchange = start_get(&mut connection, "/two");
    let requests = drain_request_output(&mut connection);
    let (streams, control) = accept_requests(&mut server, &requests);
    let [first_stream, second_stream] = streams.as_slice() else {
        panic!("server did not receive both requests");
    };
    assert_eq!(first_exchange.as_u64(), u64::from(*first_stream));
    assert_eq!(second_exchange.as_u64(), u64::from(*second_stream));
    let client_control = receive_control(&mut connection, &control);
    accept_control(&mut server, &client_control);

    let second_response = response_bytes(&mut server, *second_stream, b"second", Vec::new());
    let (body, client_control, consumed) =
        receive_response(&mut connection, second_exchange, &second_response);
    assert_eq!(body, b"second");
    assert_eq!(consumed, second_response.len());
    accept_control(&mut server, &client_control);
    assert!(connection.begin_next_exchange(second_exchange).unwrap());

    let first_response = response_bytes(&mut server, *first_stream, b"first", Vec::new());
    let (body, client_control, consumed) =
        receive_response(&mut connection, first_exchange, &first_response);
    assert_eq!(body, b"first");
    assert_eq!(consumed, first_response.len());
    accept_control(&mut server, &client_control);
    assert!(connection.begin_next_exchange(first_exchange).unwrap());
}

#[test]
fn peer_reset_isolated_to_one_concurrent_exchange() {
    let mut connection = ClientConnection::new(HttpProtocol::Http2, HttpLimits::new());
    let mut server = H2Server::default();

    let reset_exchange = start_get(&mut connection, "/reset");
    let sibling_exchange = start_get(&mut connection, "/sibling");
    let requests = drain_request_output(&mut connection);
    let (streams, control) = accept_requests(&mut server, &requests);
    let [reset_stream, sibling_stream] = streams.as_slice() else {
        panic!("server did not receive both requests");
    };
    assert_eq!(reset_exchange.as_u64(), u64::from(*reset_stream));
    assert_eq!(sibling_exchange.as_u64(), u64::from(*sibling_stream));
    let client_control = receive_control(&mut connection, &control);
    accept_control(&mut server, &client_control);

    let mut response = server
        .rst_stream_frame_with_code(*reset_stream, H2ErrorCode::Cancel)
        .unwrap();
    let reset_len = response.len();
    response.extend_from_slice(&response_bytes(
        &mut server,
        *sibling_stream,
        b"healthy",
        Vec::new(),
    ));

    assert_eq!(
        step(&mut connection, &response),
        Err(ServerError::PeerReset {
            stream_id: *reset_stream,
            error_code: H2ErrorCode::Cancel.as_u32(),
        })
    );
    assert_eq!(connection.consumed(), reset_len);
    connection.consume(reset_len).unwrap();
    assert!(connection.begin_next_exchange(reset_exchange).unwrap());

    let sibling_response = &response[reset_len..];
    let (body, client_control, consumed) =
        receive_response(&mut connection, sibling_exchange, sibling_response);
    assert_eq!(body, b"healthy");
    assert_eq!(consumed, sibling_response.len());
    accept_control(&mut server, &client_control);
    assert!(connection.begin_next_exchange(sibling_exchange).unwrap());
}

#[test]
fn concurrent_request_bodies_rotate_fairly_without_cross_contamination() {
    const BODY_LEN: usize = 16_385;

    let mut connection = ClientConnection::new(HttpProtocol::Http2, HttpLimits::new());
    let mut server = H2Server::default();
    let first_body = vec![b'a'; BODY_LEN];
    let second_body = (0..BODY_LEN)
        .map(|index| b'0' + u8::try_from(index % 10).unwrap())
        .collect::<Vec<_>>();

    let first_exchange = start_post(&mut connection, "/one", first_body.len());
    let second_exchange = start_post(&mut connection, "/two", second_body.len());
    let requests = drain_request_output(&mut connection);
    let (streams, control) = accept_requests(&mut server, &requests);
    let [first_stream, second_stream] = streams.as_slice() else {
        panic!("server did not receive both requests");
    };
    assert_eq!(first_exchange.as_u64(), u64::from(*first_stream));
    assert_eq!(second_exchange.as_u64(), u64::from(*second_stream));
    let client_control = receive_control(&mut connection, &control);
    accept_control(&mut server, &client_control);

    let bodies = [
        (first_exchange, *first_stream, &first_body),
        (second_exchange, *second_stream, &second_body),
    ];
    let mut sent = BTreeMap::from([(first_exchange, 0usize), (second_exchange, 0usize)]);
    let mut turns = Vec::new();
    let mut body_wire = Vec::new();

    for _ in 0..8 {
        let mut progressed = false;
        for (exchange_id, stream_id, body) in bodies {
            let sent_len = sent[&exchange_id];
            if sent_len == body.len() {
                continue;
            }
            if !connection
                .prepare_body_chunk(exchange_id, body.len() - sent_len)
                .unwrap()
            {
                assert!(connection.body_chunk(exchange_id).is_none());
                continue;
            }
            let (wire, payload_len) = {
                let chunk = connection.body_chunk(exchange_id).unwrap();
                let payload_len = chunk.payload_len();
                let mut wire = chunk.header().to_vec();
                wire.extend_from_slice(&body[sent_len..sent_len + payload_len]);
                (wire, payload_len)
            };
            let (decoded, consumed) = H2Frame::decode(&wire).unwrap();
            assert_eq!(consumed, wire.len());
            assert_eq!(decoded.frame_type, H2FrameType::Data);
            assert_eq!(decoded.stream_id, stream_id);
            assert_eq!(
                decoded.payload.as_slice(),
                &body[sent_len..sent_len + payload_len]
            );
            assert_eq!(
                decoded.flags & 0x1 != 0,
                sent_len + payload_len == body.len()
            );
            body_wire.extend_from_slice(&wire);
            connection.commit_body_chunk(exchange_id).unwrap();
            sent.insert(exchange_id, sent_len + payload_len);
            turns.push(exchange_id);
            progressed = true;
        }
        if sent.values().all(|sent_len| *sent_len == BODY_LEN) {
            break;
        }
        assert!(progressed, "request-body scheduler made no progress");
    }

    assert_eq!(
        turns,
        [
            first_exchange,
            second_exchange,
            first_exchange,
            second_exchange,
        ]
    );
    assert!(sent.values().all(|sent_len| *sent_len == BODY_LEN));

    let mut offset = 0;
    let mut received = BTreeMap::<u32, Vec<u8>>::new();
    let mut completed = BTreeSet::new();
    let mut server_control = Vec::new();
    while offset < body_wire.len() {
        let (event, consumed, output) = server.accept_event_bytes(&body_wire[offset..]).unwrap();
        assert_ne!(consumed, 0);
        offset += consumed;
        server_control.extend_from_slice(&output);
        if let Some(H2ByteStreamEvent::Data {
            stream_id,
            payload,
            end_stream,
            ..
        }) = event
        {
            received
                .entry(stream_id)
                .or_default()
                .extend_from_slice(&payload);
            if end_stream {
                completed.insert(stream_id);
            }
        }
    }
    assert_eq!(received[first_stream], first_body);
    assert_eq!(received[second_stream], second_body);
    assert_eq!(completed, BTreeSet::from([*first_stream, *second_stream]));
    let client_control = receive_control(&mut connection, &server_control);
    accept_control(&mut server, &client_control);
}

#[test]
fn many_concurrent_streams_route_every_response_to_its_exchange() {
    const STREAM_COUNT: usize = 8;

    let mut connection = ClientConnection::new(HttpProtocol::Http2, HttpLimits::new());
    let mut server = H2Server::default();
    let exchanges = (0..STREAM_COUNT)
        .map(|index| start_get(&mut connection, &format!("/stream/{index}")))
        .collect::<Vec<_>>();
    let requests = drain_request_output(&mut connection);
    let (streams, control) = accept_requests(&mut server, &requests);
    assert_eq!(streams.len(), STREAM_COUNT);
    for (exchange_id, stream_id) in exchanges.iter().zip(&streams) {
        assert_eq!(exchange_id.as_u64(), u64::from(*stream_id));
    }
    let client_control = receive_control(&mut connection, &control);
    accept_control(&mut server, &client_control);

    let fields = [H2HeaderField::new(b"content-length", b"4")];
    let commits = streams
        .iter()
        .map(|stream_id| {
            server
                .response_headers_frame_with_raw_headers(*stream_id, 200, &fields, false)
                .unwrap()
        })
        .collect::<Vec<_>>();
    let mut response = Vec::new();
    for commit in commits {
        let block = server.next_outbound_block().unwrap();
        assert_eq!(block.commit(), commit);
        response.extend_from_slice(block.bytes());
        server.acknowledge_outbound_block(commit).unwrap();
    }

    let first_chunks = (0..STREAM_COUNT)
        .map(|index| format!("a{index}").into_bytes())
        .collect::<Vec<_>>();
    let second_chunks = (0..STREAM_COUNT)
        .map(|index| format!("b{index}").into_bytes())
        .collect::<Vec<_>>();
    let order = [7usize, 0, 5, 2, 6, 1, 4, 3];
    for index in order {
        response.extend_from_slice(&server.data_frame(streams[index], &first_chunks[index], false));
    }
    for index in order.into_iter().rev() {
        response.extend_from_slice(&server.data_frame(streams[index], &second_chunks[index], true));
    }

    let mut offset = 0;
    let mut heads = BTreeSet::new();
    let mut bodies = BTreeMap::<ExchangeId, Vec<u8>>::new();
    let mut completed = BTreeSet::new();
    let mut client_control = Vec::new();
    for _ in 0..256 {
        let observed = step(&mut connection, &response[offset..]).unwrap();
        let consumed = connection.consumed();
        connection.consume(consumed).unwrap();
        offset += consumed;
        match observed {
            Observed::Write(bytes) => client_control.extend_from_slice(&bytes),
            Observed::Head {
                exchange_id,
                status,
            } => {
                assert_eq!(status, 200);
                heads.insert(exchange_id);
            }
            Observed::Body { exchange_id, bytes } => {
                bodies
                    .entry(exchange_id)
                    .or_default()
                    .extend_from_slice(&bytes);
            }
            Observed::Complete { exchange_id } => {
                completed.insert(exchange_id);
            }
            Observed::NeedInput if consumed != 0 => {}
            Observed::NeedInput => panic!("complete response bytes were already supplied"),
            Observed::Done => panic!("driver finished before every completion event"),
        }
        if completed.len() == STREAM_COUNT {
            break;
        }
    }

    assert_eq!(offset, response.len());
    assert_eq!(heads, exchanges.iter().copied().collect());
    assert_eq!(completed, exchanges.iter().copied().collect());
    for (index, exchange_id) in exchanges.iter().enumerate() {
        let expected = [
            first_chunks[index].as_slice(),
            second_chunks[index].as_slice(),
        ]
        .concat();
        assert_eq!(bodies[exchange_id], expected);
    }
    accept_control(&mut server, &client_control);
    for exchange_id in exchanges.into_iter().rev() {
        assert!(connection.begin_next_exchange(exchange_id).unwrap());
    }
}

#[test]
fn http1_rejects_a_second_request_until_the_first_is_retired() {
    let mut connection = ClientConnection::new(HttpProtocol::Http1, HttpLimits::new());
    let first_exchange = start_get(&mut connection, "/one");

    assert_eq!(
        connection.prepare_request(ClientRequest {
            method: "GET",
            scheme: "http",
            authority: "example.test",
            target: "/two",
            headers: &[],
            body_len: Some(0),
        }),
        Err(ServerError::InvalidOutboundState)
    );

    let request = drain_request_output(&mut connection);
    assert!(
        std::str::from_utf8(&request)
            .unwrap()
            .starts_with("GET /one HTTP/1.1\r\n")
    );
    let response = b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n";
    let (body, outbound, consumed) = receive_response(&mut connection, first_exchange, response);
    assert!(body.is_empty());
    assert!(outbound.is_empty());
    assert_eq!(consumed, response.len());
    assert!(connection.begin_next_exchange(first_exchange).unwrap());

    let second_exchange = start_get(&mut connection, "/two");
    assert!(second_exchange.as_u64() > first_exchange.as_u64());
}

#[test]
fn peer_concurrency_limit_rejects_only_the_excess_request() {
    let mut connection = ClientConnection::new(HttpProtocol::Http2, HttpLimits::new());
    let mut server = H2Server::with_limits(H2Limits {
        max_active_streams: 2,
        ..H2Limits::default()
    })
    .unwrap();

    let (first_exchange, request) = prepare_get(&mut connection, "/one");
    let (first_stream, control) = accept_request(&mut server, &request);
    let client_control = receive_control(&mut connection, &control);
    accept_control(&mut server, &client_control);

    let (second_exchange, request) = prepare_get(&mut connection, "/two");
    let (second_stream, control) = accept_request(&mut server, &request);
    let client_control = receive_control(&mut connection, &control);
    accept_control(&mut server, &client_control);

    assert_eq!(
        connection.prepare_request(ClientRequest {
            method: "GET",
            scheme: "http",
            authority: "example.test",
            target: "/excess",
            headers: &[],
            body_len: Some(0),
        }),
        Err(ServerError::InvalidFrame)
    );

    let response = response_bytes(&mut server, first_stream, b"first", Vec::new());
    let (body, client_control, consumed) =
        receive_response(&mut connection, first_exchange, &response);
    assert_eq!(body, b"first");
    assert_eq!(consumed, response.len());
    accept_control(&mut server, &client_control);
    assert!(connection.begin_next_exchange(first_exchange).unwrap());

    let response = response_bytes(&mut server, second_stream, b"second", Vec::new());
    let (body, client_control, consumed) =
        receive_response(&mut connection, second_exchange, &response);
    assert_eq!(body, b"second");
    assert_eq!(consumed, response.len());
    accept_control(&mut server, &client_control);
    assert!(connection.begin_next_exchange(second_exchange).unwrap());
}

#[test]
fn received_goaway_prevents_another_exchange() {
    let mut connection = ClientConnection::new(HttpProtocol::Http2, HttpLimits::new());
    let mut server = H2Server::default();
    let (exchange_id, request) = prepare_get(&mut connection, "/");
    let (stream_id, mut response) = accept_request(&mut server, &request);
    let fields = [H2HeaderField::new(b"content-length", b"1")];
    let commit = server
        .response_headers_frame_with_raw_headers(stream_id, 200, &fields, false)
        .unwrap();
    let block = server.next_outbound_block().unwrap();
    assert_eq!(block.commit(), commit);
    response.extend_from_slice(block.bytes());
    server.acknowledge_outbound_block(commit).unwrap();
    response.extend_from_slice(&server.goaway_frame(stream_id, 0).unwrap());

    let mut offset = 0;
    loop {
        match step(&mut connection, &response[offset..]) {
            Ok(observed) => {
                let consumed = connection.consumed();
                connection.consume(consumed).unwrap();
                offset += consumed;
                assert!(!matches!(
                    observed,
                    Observed::Complete { .. } | Observed::Done
                ));
            }
            Err(error) => {
                assert_eq!(error.classify().kind(), HttpErrorKind::PeerGoaway);
                break;
            }
        }
    }

    assert!(!connection.begin_next_exchange(exchange_id).unwrap());
}

#[test]
fn idle_control_frames_remain_reusable_and_return_required_acks() {
    let mut connection = ClientConnection::new(HttpProtocol::Http2, HttpLimits::new());
    let mut server = H2Server::default();
    complete_exchange(&mut connection, &mut server);

    let settings = frame(H2FrameType::Settings, 0, 0, &[]);
    let (status, consumed, output) = connection.process_idle_input(&settings).unwrap();
    assert_eq!(status, ClientIdleStatus::Reusable);
    assert_eq!(consumed, settings.len());
    assert_eq!(output, frame(H2FrameType::Settings, 0x1, 0, &[]));

    let payload = *b"idle1234";
    let ping = frame(H2FrameType::Ping, 0, 0, &payload);
    let (status, consumed, output) = connection.process_idle_input(&ping).unwrap();
    assert_eq!(status, ClientIdleStatus::Reusable);
    assert_eq!(consumed, ping.len());
    assert_eq!(output, frame(H2FrameType::Ping, 0x1, 0, &payload));

    let window_update = frame(H2FrameType::WindowUpdate, 0, 0, &1024u32.to_be_bytes());
    let (status, consumed, output) = connection.process_idle_input(&window_update).unwrap();
    assert_eq!(status, ClientIdleStatus::Reusable);
    assert_eq!(consumed, window_update.len());
    assert!(output.is_empty());

    assert!(!prepare_get(&mut connection, "/next").1.is_empty());
}

#[test]
fn idle_goaway_and_application_data_are_not_reusable() {
    let mut goaway_connection = ClientConnection::new(HttpProtocol::Http2, HttpLimits::new());
    let mut goaway_server = H2Server::default();
    let last_stream_id = complete_exchange(&mut goaway_connection, &mut goaway_server);
    let goaway = goaway_server.goaway_frame(last_stream_id, 0).unwrap();
    let (status, consumed, output) = goaway_connection.process_idle_input(&goaway).unwrap();
    assert_eq!(status, ClientIdleStatus::NotReusable);
    assert_eq!(consumed, goaway.len());
    assert!(output.is_empty());

    let mut data_connection = ClientConnection::new(HttpProtocol::Http2, HttpLimits::new());
    let mut data_server = H2Server::default();
    complete_exchange(&mut data_connection, &mut data_server);
    let unsolicited = frame(H2FrameType::Data, 0x1, 3, b"smuggled");
    assert!(data_connection.process_idle_input(&unsolicited).is_err());
}

fn start_streaming_post(connection: &mut ClientConnection, target: &str) -> ExchangeId {
    connection
        .prepare_request(ClientRequest {
            method: "POST",
            scheme: "http",
            authority: "example.test",
            target,
            headers: &[],
            body_len: None,
        })
        .unwrap()
}

/// DRIVER-007. A streaming request body has no declared length, so the driver
/// only learns how much a stream has to send when its adapter offers a chunk.
/// Crediting those bytes to the calling exchange alone made every streaming
/// sibling permanently ineligible, so the scheduler could never rotate and the
/// first stream the adapter happened to offer took the whole connection.
#[test]
fn concurrent_streaming_request_bodies_rotate_fairly() {
    const CHUNK: usize = 4_096;
    const CHUNKS: usize = 4;

    let mut connection = ClientConnection::new(HttpProtocol::Http2, HttpLimits::new());
    let mut server = H2Server::default();
    let first_exchange = start_streaming_post(&mut connection, "/one");
    let second_exchange = start_streaming_post(&mut connection, "/two");
    let requests = drain_request_output(&mut connection);
    let (streams, control) = accept_requests(&mut server, &requests);
    let [first_stream, second_stream] = streams.as_slice() else {
        panic!("server did not receive both requests");
    };
    let client_control = receive_control(&mut connection, &control);
    accept_control(&mut server, &client_control);

    let first_body = vec![b'a'; CHUNK];
    let second_body = vec![b'b'; CHUNK];
    let bodies = [
        (first_exchange, *first_stream, &first_body),
        (second_exchange, *second_stream, &second_body),
    ];
    let mut sent = BTreeMap::from([(first_exchange, 0usize), (second_exchange, 0usize)]);
    let mut turns = Vec::new();

    // Both streams always have exactly one chunk on offer, mirroring an adapter
    // whose two body sources are equally ready.
    for _ in 0..(CHUNKS * 2) {
        let mut progressed = false;
        // Declare every stream's readiness first, as a multiplexing adapter
        // must, so the scheduler can see the siblings it is not being asked
        // about on this call.
        for (exchange_id, _, body) in bodies {
            let available = if sent[&exchange_id] == CHUNK * CHUNKS {
                0
            } else {
                body.len()
            };
            connection.note_request_body_available(exchange_id, available);
        }
        for (exchange_id, stream_id, body) in bodies {
            if sent[&exchange_id] == CHUNK * CHUNKS {
                continue;
            }
            if !connection
                .prepare_body_chunk(exchange_id, body.len())
                .unwrap()
            {
                continue;
            }
            let payload_len = {
                let chunk = connection.body_chunk(exchange_id).unwrap();
                let mut wire = chunk.header().to_vec();
                wire.extend_from_slice(&body[..chunk.payload_len()]);
                let (decoded, _) = H2Frame::decode(&wire).unwrap();
                assert_eq!(decoded.stream_id, stream_id);
                chunk.payload_len()
            };
            connection.commit_body_chunk(exchange_id).unwrap();
            sent.insert(exchange_id, sent[&exchange_id] + payload_len);
            turns.push(exchange_id);
            progressed = true;
            break;
        }
        if sent.values().all(|sent_len| *sent_len == CHUNK * CHUNKS) {
            break;
        }
        assert!(
            progressed,
            "streaming request-body scheduler made no progress"
        );
    }

    // Equal totals are not fairness: one stream monopolizing and then the other
    // draining also totals equally. The turn order itself has to alternate while
    // both streams still have bytes to send.
    let expected = (0..CHUNKS * 2)
        .map(|turn| {
            if turn % 2 == 0 {
                first_exchange
            } else {
                second_exchange
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(
        turns, expected,
        "streaming uploads did not rotate: one stream took consecutive turns"
    );
    assert!(sent.values().all(|sent_len| *sent_len == CHUNK * CHUNKS));
}
