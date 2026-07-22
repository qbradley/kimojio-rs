use kimojio_fsm_http::{
    ClientConnection, ClientEvent, ClientRequest, ConnectionResponse, ExchangeId,
    H2ByteStreamEvent, H2Client, H2Frame, H2FrameType, H2HeaderBlockDecoder, H2HeaderField,
    H2OutboundCommit, H2Server, HeaderRef, HttpLimits, HttpProtocol, ServerConnection, ServerError,
    ServerEvent, Step,
};

type Fields = Vec<(Vec<u8>, Vec<u8>, bool)>;

#[derive(Debug, Eq, PartialEq)]
enum ServerObserved {
    NeedInput,
    Write(Vec<u8>),
    Head(ExchangeId),
    Body(ExchangeId, Vec<u8>),
    Trailers(ExchangeId, Fields),
    Complete(ExchangeId),
    Done,
}

fn server_step(
    connection: &mut ServerConnection,
    input: &[u8],
) -> Result<ServerObserved, ServerError> {
    connection.step(input, |step| match step {
        Step::NeedInput => ServerObserved::NeedInput,
        Step::Write(bytes) => ServerObserved::Write(bytes.to_vec()),
        Step::Event(ServerEvent::RequestHead { exchange_id, .. }) => {
            ServerObserved::Head(exchange_id)
        }
        Step::Event(ServerEvent::RequestBody { exchange_id, chunk }) => {
            ServerObserved::Body(exchange_id, chunk.to_vec())
        }
        Step::Event(ServerEvent::RequestTrailers {
            exchange_id,
            headers,
        }) => ServerObserved::Trailers(
            exchange_id,
            headers
                .iter()
                .map(|header| {
                    (
                        header.name().to_vec(),
                        header.value().to_vec(),
                        header.sensitive(),
                    )
                })
                .collect(),
        ),
        Step::Event(ServerEvent::RequestComplete { exchange_id }) => {
            ServerObserved::Complete(exchange_id)
        }
        Step::Done => ServerObserved::Done,
        _ => panic!("unexpected server step"),
    })
}

#[derive(Debug, Eq, PartialEq)]
enum ClientObserved {
    NeedInput,
    Write(Vec<u8>),
    Head(ExchangeId),
    Body(ExchangeId, Vec<u8>),
    Trailers(ExchangeId, Fields),
    Complete(ExchangeId),
    Done,
}

fn client_step(
    connection: &mut ClientConnection,
    input: &[u8],
) -> Result<ClientObserved, ServerError> {
    connection.step(input, |step| match step {
        Step::NeedInput => ClientObserved::NeedInput,
        Step::Write(bytes) => ClientObserved::Write(bytes.to_vec()),
        Step::Event(ClientEvent::ResponseHead { exchange_id, .. }) => {
            ClientObserved::Head(exchange_id)
        }
        Step::Event(ClientEvent::ResponseBody { exchange_id, chunk }) => {
            ClientObserved::Body(exchange_id, chunk.to_vec())
        }
        Step::Event(ClientEvent::ResponseTrailers {
            exchange_id,
            headers,
        }) => ClientObserved::Trailers(
            exchange_id,
            headers
                .iter()
                .map(|header| {
                    (
                        header.name().to_vec(),
                        header.value().to_vec(),
                        header.sensitive(),
                    )
                })
                .collect(),
        ),
        Step::Event(ClientEvent::ResponseComplete { exchange_id }) => {
            ClientObserved::Complete(exchange_id)
        }
        Step::Done => ClientObserved::Done,
        _ => panic!("unexpected client step"),
    })
}

fn collect_server_request(
    connection: &mut ServerConnection,
    input: &[u8],
) -> (ExchangeId, Vec<ServerObserved>, usize) {
    let mut offset = 0;
    let mut exchange_id = None;
    let mut observed = Vec::new();
    for _ in 0..64 {
        let event = server_step(connection, &input[offset..]).unwrap();
        let consumed = connection.consumed();
        connection.consume(consumed).unwrap();
        offset += consumed;
        match &event {
            ServerObserved::Head(id) => {
                assert!(exchange_id.replace(*id).is_none());
                observed.push(event);
            }
            ServerObserved::Body(..) | ServerObserved::Trailers(..) => observed.push(event),
            ServerObserved::Complete(id) => {
                assert_eq!(Some(*id), exchange_id);
                observed.push(event);
                return (exchange_id.unwrap(), observed, offset);
            }
            ServerObserved::Write(bytes) => assert!(!bytes.is_empty()),
            ServerObserved::NeedInput if consumed != 0 => {}
            ServerObserved::NeedInput => panic!("complete request bytes were supplied"),
            ServerObserved::Done => panic!("request completed before its completion event"),
        }
    }
    panic!("request decoding did not converge")
}

fn finish_empty_response(connection: &mut ServerConnection, exchange_id: ExchangeId) {
    assert!(
        !connection
            .prepare_response(
                exchange_id,
                ConnectionResponse {
                    status: 204,
                    reason: "No Content",
                    headers: &[],
                    body_len: Some(0),
                },
            )
            .unwrap()
    );
    for _ in 0..16 {
        match server_step(connection, &[]).unwrap() {
            ServerObserved::Write(bytes) => {
                assert!(!bytes.is_empty());
                connection.consume(connection.consumed()).unwrap();
            }
            ServerObserved::Done => return,
            event => panic!("unexpected response progress: {event:?}"),
        }
    }
    panic!("empty response did not finish")
}

fn prepare_client_get(connection: &mut ClientConnection, target: &str) -> (ExchangeId, Vec<u8>) {
    let headers = [HeaderRef::new(b"host", b"example.test")];
    let exchange_id = connection
        .prepare_request(ClientRequest {
            method: "GET",
            scheme: "http",
            authority: "example.test",
            target,
            headers: &headers,
            body_len: Some(0),
        })
        .unwrap();
    let mut output = Vec::new();
    for _ in 0..16 {
        match client_step(connection, &[]).unwrap() {
            ClientObserved::Write(bytes) => {
                output.extend_from_slice(&bytes);
                connection.consume(connection.consumed()).unwrap();
            }
            ClientObserved::NeedInput => {
                connection.consume(connection.consumed()).unwrap();
                return (exchange_id, output);
            }
            event => panic!("unexpected request progress: {event:?}"),
        }
    }
    panic!("request output did not drain")
}

fn collect_client_response(
    connection: &mut ClientConnection,
    exchange_id: ExchangeId,
    input: &[u8],
) -> (Vec<ClientObserved>, Vec<u8>, usize) {
    let mut offset = 0;
    let mut events = Vec::new();
    let mut control = Vec::new();
    for _ in 0..64 {
        let event = client_step(connection, &input[offset..]).unwrap();
        let consumed = connection.consumed();
        connection.consume(consumed).unwrap();
        offset += consumed;
        match &event {
            ClientObserved::Head(id)
            | ClientObserved::Body(id, _)
            | ClientObserved::Trailers(id, _)
            | ClientObserved::Complete(id) => {
                assert_eq!(*id, exchange_id);
                let complete = matches!(event, ClientObserved::Complete(_));
                events.push(event);
                if complete {
                    return (events, control, offset);
                }
            }
            ClientObserved::Write(bytes) => control.extend_from_slice(bytes),
            ClientObserved::NeedInput if consumed != 0 => {}
            ClientObserved::NeedInput => panic!("complete response bytes were supplied"),
            ClientObserved::Done => panic!("response completed without a completion event"),
        }
    }
    panic!("response decoding did not converge")
}

fn take_client_block(client: &mut H2Client, commit: H2OutboundCommit) -> Vec<u8> {
    let block = client.next_outbound_block().unwrap();
    assert_eq!(block.commit(), commit);
    let bytes = block.bytes().to_vec();
    client.acknowledge_outbound_block(commit).unwrap();
    bytes
}

fn take_server_block(server: &mut H2Server, commit: H2OutboundCommit) -> Vec<u8> {
    let block = server.next_outbound_block().unwrap();
    assert_eq!(block.commit(), commit);
    let bytes = block.bytes().to_vec();
    server.acknowledge_outbound_block(commit).unwrap();
    bytes
}

fn accept_h2_request(server: &mut H2Server, input: &[u8]) -> (u32, Vec<u8>) {
    let mut offset = 0;
    let mut stream_id = None;
    let mut control = Vec::new();
    while offset < input.len() {
        let (event, consumed, output) = server.accept_event_bytes(&input[offset..]).unwrap();
        assert_ne!(consumed, 0);
        offset += consumed;
        control.extend_from_slice(&output);
        if let Some(H2ByteStreamEvent::RequestHeaders {
            stream_id: observed,
            ..
        }) = event
        {
            assert!(stream_id.replace(observed).is_none());
        }
    }
    (stream_id.unwrap(), control)
}

fn accept_http1_request(connection: &mut ServerConnection, input: &[u8]) -> (ExchangeId, usize) {
    let (exchange_id, _, consumed) = collect_server_request(connection, input);
    (exchange_id, consumed)
}

fn write_server_body(
    connection: &mut ServerConnection,
    exchange_id: ExchangeId,
    payload: &[u8],
) -> Vec<u8> {
    assert!(
        connection
            .prepare_body_chunk(exchange_id, payload.len())
            .unwrap()
    );
    let wire = {
        let chunk = connection.body_chunk(exchange_id).unwrap();
        assert_eq!(chunk.payload_len(), payload.len());
        let mut wire = chunk.header().to_vec();
        wire.extend_from_slice(payload);
        wire.extend_from_slice(chunk.footer());
        wire
    };
    connection.commit_body_chunk(exchange_id).unwrap();
    wire
}

#[test]
fn http1_request_trailers_precede_completion_and_do_not_leak_on_reuse() {
    let mut connection =
        ServerConnection::new_with_protocol(HttpProtocol::Http1, HttpLimits::new());
    let first = b"POST /one HTTP/1.1\r\n\
host: example.test\r\n\
transfer-encoding: chunked\r\n\r\n\
3\r\none\r\n\
0\r\nDigest: first\r\nX-Count: 1\r\n\r\n";
    let (first_id, events, consumed) = collect_server_request(&mut connection, first);
    assert_eq!(consumed, first.len());
    assert_eq!(
        events,
        [
            ServerObserved::Head(first_id),
            ServerObserved::Body(first_id, b"one".to_vec()),
            ServerObserved::Trailers(
                first_id,
                vec![
                    (b"Digest".to_vec(), b"first".to_vec(), false),
                    (b"X-Count".to_vec(), b"1".to_vec(), false),
                ],
            ),
            ServerObserved::Complete(first_id),
        ]
    );
    finish_empty_response(&mut connection, first_id);
    assert!(connection.begin_next_exchange(first_id).unwrap());

    let second = b"POST /two HTTP/1.1\r\n\
host: example.test\r\n\
transfer-encoding: chunked\r\n\r\n\
3\r\ntwo\r\n\
0\r\n\r\n";
    let (second_id, events, consumed) = collect_server_request(&mut connection, second);
    assert_eq!(consumed, second.len());
    assert_eq!(
        events,
        [
            ServerObserved::Head(second_id),
            ServerObserved::Body(second_id, b"two".to_vec()),
            ServerObserved::Complete(second_id),
        ]
    );
}

#[test]
fn http2_request_trailers_precede_completion_and_empty_case_has_no_event() {
    let mut peer = H2Client::default();
    let mut input = peer.connection_preface();
    let (stream_id, commit) = peer
        .open_stream_with_raw_headers("POST", "http", "example.test", "/", &[], false)
        .unwrap();
    input.extend_from_slice(&take_client_block(&mut peer, commit));
    input.extend_from_slice(&peer.data_frame(stream_id, b"body", false));
    let trailers = [
        H2HeaderField::new(b"digest", b"second"),
        H2HeaderField::new(b"x-count", b"4").with_sensitive(true),
    ];
    let commit = peer
        .trailers_frame_with_raw_headers(stream_id, &trailers)
        .unwrap();
    input.extend_from_slice(&take_client_block(&mut peer, commit));

    let mut connection =
        ServerConnection::new_with_protocol(HttpProtocol::Http2, HttpLimits::new());
    let (exchange_id, events, consumed) = collect_server_request(&mut connection, &input);
    assert_eq!(consumed, input.len());
    assert_eq!(
        events,
        [
            ServerObserved::Head(exchange_id),
            ServerObserved::Body(exchange_id, b"body".to_vec()),
            ServerObserved::Trailers(
                exchange_id,
                vec![
                    (b"digest".to_vec(), b"second".to_vec(), false),
                    (b"x-count".to_vec(), b"4".to_vec(), true),
                ],
            ),
            ServerObserved::Complete(exchange_id),
        ]
    );

    let mut peer = H2Client::default();
    let mut input = peer.connection_preface();
    let (_, commit) = peer
        .open_stream_with_raw_headers("GET", "http", "example.test", "/", &[], true)
        .unwrap();
    input.extend_from_slice(&take_client_block(&mut peer, commit));
    let mut connection =
        ServerConnection::new_with_protocol(HttpProtocol::Http2, HttpLimits::new());
    let (exchange_id, events, consumed) = collect_server_request(&mut connection, &input);
    assert_eq!(consumed, input.len());
    assert_eq!(
        events,
        [
            ServerObserved::Head(exchange_id),
            ServerObserved::Complete(exchange_id),
        ]
    );
}

#[test]
fn http1_response_trailers_precede_completion_and_do_not_leak_on_reuse() {
    let mut connection = ClientConnection::new(HttpProtocol::Http1, HttpLimits::new());
    let (first_id, _) = prepare_client_get(&mut connection, "/one");
    let first = b"HTTP/1.1 200 OK\r\n\
transfer-encoding: chunked\r\n\r\n\
3\r\none\r\n\
0\r\nDigest: first\r\n\r\n";
    let (events, _, consumed) = collect_client_response(&mut connection, first_id, first);
    assert_eq!(consumed, first.len());
    assert_eq!(
        events,
        [
            ClientObserved::Head(first_id),
            ClientObserved::Body(first_id, b"one".to_vec()),
            ClientObserved::Trailers(
                first_id,
                vec![(b"Digest".to_vec(), b"first".to_vec(), false)],
            ),
            ClientObserved::Complete(first_id),
        ]
    );
    assert!(connection.begin_next_exchange(first_id).unwrap());

    let (second_id, _) = prepare_client_get(&mut connection, "/two");
    let second = b"HTTP/1.1 200 OK\r\n\
transfer-encoding: chunked\r\n\r\n\
3\r\ntwo\r\n\
0\r\n\r\n";
    let (events, _, consumed) = collect_client_response(&mut connection, second_id, second);
    assert_eq!(consumed, second.len());
    assert_eq!(
        events,
        [
            ClientObserved::Head(second_id),
            ClientObserved::Body(second_id, b"two".to_vec()),
            ClientObserved::Complete(second_id),
        ]
    );
}

#[test]
fn http2_response_trailers_precede_completion_and_no_trailer_case_has_no_event() {
    let mut connection = ClientConnection::new(HttpProtocol::Http2, HttpLimits::new());
    let (exchange_id, request) = prepare_client_get(&mut connection, "/trailers");
    let mut server = H2Server::default();
    let (stream_id, mut response) = accept_h2_request(&mut server, &request);
    assert_eq!(exchange_id.as_u64(), u64::from(stream_id));

    let commit = server
        .response_headers_frame_with_raw_headers(stream_id, 200, &[], false)
        .unwrap();
    response.extend_from_slice(&take_server_block(&mut server, commit));
    response.extend_from_slice(&server.data_frame(stream_id, b"ok", false));
    let trailers = [
        H2HeaderField::new(b"grpc-status", b"0"),
        H2HeaderField::new(b"grpc-message", b"done").with_sensitive(true),
    ];
    let commit = server
        .trailers_frame_with_raw_headers(stream_id, &trailers)
        .unwrap();
    response.extend_from_slice(&take_server_block(&mut server, commit));

    let (events, _, consumed) = collect_client_response(&mut connection, exchange_id, &response);
    assert_eq!(consumed, response.len());
    assert_eq!(
        events,
        [
            ClientObserved::Head(exchange_id),
            ClientObserved::Body(exchange_id, b"ok".to_vec()),
            ClientObserved::Trailers(
                exchange_id,
                vec![
                    (b"grpc-status".to_vec(), b"0".to_vec(), false),
                    (b"grpc-message".to_vec(), b"done".to_vec(), true),
                ],
            ),
            ClientObserved::Complete(exchange_id),
        ]
    );

    let mut connection = ClientConnection::new(HttpProtocol::Http2, HttpLimits::new());
    let (exchange_id, request) = prepare_client_get(&mut connection, "/plain");
    let mut server = H2Server::default();
    let (stream_id, mut response) = accept_h2_request(&mut server, &request);
    let commit = server
        .response_headers_frame_with_raw_headers(stream_id, 204, &[], true)
        .unwrap();
    response.extend_from_slice(&take_server_block(&mut server, commit));
    let (events, _, consumed) = collect_client_response(&mut connection, exchange_id, &response);
    assert_eq!(consumed, response.len());
    assert_eq!(
        events,
        [
            ClientObserved::Head(exchange_id),
            ClientObserved::Complete(exchange_id),
        ]
    );
}

#[test]
fn forbidden_http1_inbound_trailer_policies_are_unchanged() {
    let request = b"POST / HTTP/1.1\r\n\
host: example.test\r\n\
transfer-encoding: chunked\r\n\r\n\
0\r\nX-Keep: discarded\r\nContent-Length: 0\r\n\r\n";
    let mut server = ServerConnection::new_with_protocol(HttpProtocol::Http1, HttpLimits::new());
    let (exchange_id, events, consumed) = collect_server_request(&mut server, request);
    assert_eq!(consumed, request.len());
    assert_eq!(
        events,
        [
            ServerObserved::Head(exchange_id),
            ServerObserved::Complete(exchange_id),
        ]
    );

    let mut client = ClientConnection::new(HttpProtocol::Http1, HttpLimits::new());
    let (exchange_id, _) = prepare_client_get(&mut client, "/");
    let response = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n\
0\r\nContent-Length: 0\r\n\r\n";
    assert_eq!(
        client_step(&mut client, response).unwrap(),
        ClientObserved::Head(exchange_id)
    );
    let consumed = client.consumed();
    client.consume(consumed).unwrap();
    assert_eq!(
        client_step(&mut client, &response[consumed..]),
        Err(ServerError::MalformedMessage)
    );
}

#[test]
fn server_http1_trailers_are_chunked_wire_and_round_trip_through_own_client() {
    let mut client = ClientConnection::new(HttpProtocol::Http1, HttpLimits::new());
    let (exchange_id, request) = prepare_client_get(&mut client, "/round-trip");
    let mut server = ServerConnection::new_with_protocol(HttpProtocol::Http1, HttpLimits::new());
    let (server_exchange, consumed) = accept_http1_request(&mut server, &request);
    assert_eq!(consumed, request.len());
    assert_eq!(server_exchange, exchange_id);
    assert!(
        server
            .prepare_response(
                server_exchange,
                ConnectionResponse {
                    status: 200,
                    reason: "OK",
                    headers: &[],
                    body_len: None,
                },
            )
            .unwrap()
    );

    let mut wire = write_server_body(&mut server, server_exchange, b"abc");
    server
        .set_response_trailers(
            server_exchange,
            &[
                HeaderRef::new(b"Digest", b"sha-256=abc"),
                HeaderRef::new(b"X-Count", b"3"),
            ],
        )
        .unwrap();
    wire.extend_from_slice(&write_server_body(&mut server, server_exchange, b""));
    assert_eq!(
        wire,
        b"HTTP/1.1 200 OK\r\n\
transfer-encoding: chunked\r\n\r\n\
3\r\nabc\r\n\
0\r\nDigest: sha-256=abc\r\nX-Count: 3\r\n\r\n"
    );
    assert_eq!(server_step(&mut server, &[]).unwrap(), ServerObserved::Done);

    let (events, _, consumed) = collect_client_response(&mut client, exchange_id, &wire);
    assert_eq!(consumed, wire.len());
    assert_eq!(
        events,
        [
            ClientObserved::Head(exchange_id),
            ClientObserved::Body(exchange_id, b"abc".to_vec()),
            ClientObserved::Trailers(
                exchange_id,
                vec![
                    (b"Digest".to_vec(), b"sha-256=abc".to_vec(), false),
                    (b"X-Count".to_vec(), b"3".to_vec(), false),
                ],
            ),
            ClientObserved::Complete(exchange_id),
        ]
    );
}

#[test]
fn server_http2_trailers_own_end_stream_after_final_data() {
    let mut peer = H2Client::default();
    let mut input = peer.connection_preface();
    let (stream_id, commit) = peer
        .open_stream_with_raw_headers("GET", "http", "example.test", "/", &[], true)
        .unwrap();
    input.extend_from_slice(&take_client_block(&mut peer, commit));
    let mut server = ServerConnection::new_with_protocol(HttpProtocol::Http2, HttpLimits::new());
    let (exchange_id, _, consumed) = collect_server_request(&mut server, &input);
    assert_eq!(consumed, input.len());
    assert!(
        server
            .prepare_response(
                exchange_id,
                ConnectionResponse {
                    status: 200,
                    reason: "OK",
                    headers: &[],
                    body_len: Some(3),
                },
            )
            .unwrap()
    );
    server
        .set_response_trailers(
            exchange_id,
            &[
                HeaderRef::new(b"grpc-status", b"0"),
                HeaderRef::new(b"grpc-message", b"done").with_sensitive(true),
            ],
        )
        .unwrap();

    let ServerObserved::Write(head_bytes) = server_step(&mut server, &[]).unwrap() else {
        panic!("response head was not ready")
    };
    server.consume(server.consumed()).unwrap();
    let (head, consumed) = H2Frame::decode(&head_bytes).unwrap();
    assert_eq!(consumed, head_bytes.len());
    assert_eq!(head.frame_type, H2FrameType::Headers);
    assert_eq!(head.flags & 0x1, 0);
    let mut decoder = H2HeaderBlockDecoder::new();
    decoder
        .try_decode_with_limit(&head.payload, usize::MAX)
        .unwrap();

    let data_bytes = write_server_body(&mut server, exchange_id, b"abc");
    let (data, consumed) = H2Frame::decode(&data_bytes).unwrap();
    assert_eq!(consumed, data_bytes.len());
    assert_eq!(data.frame_type, H2FrameType::Data);
    assert_eq!(data.stream_id, stream_id);
    assert_eq!(data.flags & 0x1, 0);
    assert_eq!(data.payload, b"abc");

    let ServerObserved::Write(trailer_bytes) = server_step(&mut server, &[]).unwrap() else {
        panic!("trailer block was not ready")
    };
    let (trailers, consumed) = H2Frame::decode(&trailer_bytes).unwrap();
    assert_eq!(consumed, trailer_bytes.len());
    assert_eq!(trailers.frame_type, H2FrameType::Headers);
    assert_eq!(trailers.stream_id, stream_id);
    assert_eq!(trailers.flags & 0x1, 0x1);
    let fields = decoder
        .try_decode_with_limit(&trailers.payload, usize::MAX)
        .unwrap();
    assert_eq!(
        fields,
        [
            H2HeaderField::new(b"grpc-status", b"0"),
            H2HeaderField::new(b"grpc-message", b"done").with_sensitive(true),
        ]
    );
    server.consume(server.consumed()).unwrap();
    assert_eq!(server_step(&mut server, &[]).unwrap(), ServerObserved::Done);
}

#[test]
fn set_response_trailers_rejects_fixed_forbidden_and_finished_responses() {
    let request = b"GET / HTTP/1.1\r\nhost: example.test\r\n\r\n";
    let mut fixed = ServerConnection::new_with_protocol(HttpProtocol::Http1, HttpLimits::new());
    let (fixed_id, _) = accept_http1_request(&mut fixed, request);
    assert!(
        fixed
            .prepare_response(
                fixed_id,
                ConnectionResponse {
                    status: 200,
                    reason: "OK",
                    headers: &[],
                    body_len: Some(1),
                },
            )
            .unwrap()
    );
    assert_eq!(
        fixed.set_response_trailers(fixed_id, &[HeaderRef::new(b"x", b"y")]),
        Err(ServerError::InvalidOutboundState)
    );

    let mut streaming = ServerConnection::new_with_protocol(HttpProtocol::Http1, HttpLimits::new());
    let (streaming_id, _) = accept_http1_request(&mut streaming, request);
    assert!(
        streaming
            .prepare_response(
                streaming_id,
                ConnectionResponse {
                    status: 200,
                    reason: "OK",
                    headers: &[],
                    body_len: None,
                },
            )
            .unwrap()
    );
    assert_eq!(
        streaming.set_response_trailers(streaming_id, &[HeaderRef::new(b"Content-Length", b"0")],),
        Err(ServerError::InvalidHeader)
    );
    write_server_body(&mut streaming, streaming_id, b"");
    assert_eq!(
        streaming.set_response_trailers(streaming_id, &[HeaderRef::new(b"x", b"late")]),
        Err(ServerError::InvalidOutboundState)
    );

    let mut peer = H2Client::default();
    let mut input = peer.connection_preface();
    let (_, commit) = peer
        .open_stream_with_raw_headers("GET", "http", "example.test", "/", &[], true)
        .unwrap();
    input.extend_from_slice(&take_client_block(&mut peer, commit));
    let mut h2 = ServerConnection::new_with_protocol(HttpProtocol::Http2, HttpLimits::new());
    let (h2_id, _, _) = collect_server_request(&mut h2, &input);
    assert!(
        h2.prepare_response(
            h2_id,
            ConnectionResponse {
                status: 200,
                reason: "OK",
                headers: &[],
                body_len: Some(1),
            },
        )
        .unwrap()
    );
    assert_eq!(
        h2.set_response_trailers(h2_id, &[HeaderRef::new(b"transfer-encoding", b"chunked")],),
        Err(ServerError::InvalidHeader)
    );
}

#[test]
fn outbound_http1_trailers_share_head_count_and_byte_limits() {
    let request = b"GET / HTTP/1.1\r\nhost: example.test\r\n\r\n";
    let limits = HttpLimits::new().set_max_headers(1);
    let mut count_limited = ServerConnection::new_with_protocol(HttpProtocol::Http1, limits);
    let (exchange_id, _) = accept_http1_request(&mut count_limited, request);
    assert!(
        count_limited
            .prepare_response(
                exchange_id,
                ConnectionResponse {
                    status: 200,
                    reason: "OK",
                    headers: &[],
                    body_len: None,
                },
            )
            .unwrap()
    );
    assert_eq!(
        count_limited.set_response_trailers(exchange_id, &[HeaderRef::new(b"x", b"y")]),
        Err(ServerError::TooManyHeaders {
            limit: 1,
            actual: 2,
        })
    );

    let limits = HttpLimits::new().set_max_header_bytes(56);
    let mut byte_limited = ServerConnection::new_with_protocol(HttpProtocol::Http1, limits);
    let (exchange_id, _) = accept_http1_request(&mut byte_limited, request);
    assert!(
        byte_limited
            .prepare_response(
                exchange_id,
                ConnectionResponse {
                    status: 200,
                    reason: "OK",
                    headers: &[],
                    body_len: None,
                },
            )
            .unwrap()
    );
    assert!(matches!(
        byte_limited.set_response_trailers(exchange_id, &[HeaderRef::new(b"x", b"y")]),
        Err(ServerError::HeaderTooLarge {
            limit: 56,
            actual
        }) if actual > 56
    ));

    let limits = HttpLimits::new().set_max_headers(1);
    let mut peer = H2Client::default();
    let mut input = peer.connection_preface();
    let (_, commit) = peer
        .open_stream_with_raw_headers("GET", "http", "example.test", "/", &[], true)
        .unwrap();
    input.extend_from_slice(&take_client_block(&mut peer, commit));
    let mut h2 = ServerConnection::new_with_protocol(HttpProtocol::Http2, limits);
    let (exchange_id, _, _) = collect_server_request(&mut h2, &input);
    assert!(
        h2.prepare_response(
            exchange_id,
            ConnectionResponse {
                status: 200,
                reason: "OK",
                headers: &[],
                body_len: Some(1),
            },
        )
        .unwrap()
    );
    assert_eq!(
        h2.set_response_trailers(exchange_id, &[HeaderRef::new(b"x", b"y")]),
        Err(ServerError::TooManyHeaders {
            limit: 1,
            actual: 2,
        })
    );
}

#[test]
fn suppressed_http2_request_does_not_surface_body_trailers_or_completion() {
    let mut peer = H2Client::default();
    let mut head = peer.connection_preface();
    let expectation = [H2HeaderField::new(b"expect", b"something-else")];
    let (stream_id, commit) = peer
        .open_stream_with_raw_headers("POST", "http", "example.test", "/", &expectation, false)
        .unwrap();
    head.extend_from_slice(&take_client_block(&mut peer, commit));
    let mut tail = peer.data_frame(stream_id, b"ignored", false);
    let commit = peer
        .trailers_frame_with_raw_headers(stream_id, &[H2HeaderField::new(b"x-ignored", b"yes")])
        .unwrap();
    tail.extend_from_slice(&take_client_block(&mut peer, commit));

    let mut connection =
        ServerConnection::new_with_protocol(HttpProtocol::Http2, HttpLimits::new());
    let mut offset = 0;
    let exchange_id = loop {
        let event = server_step(&mut connection, &head[offset..]).unwrap();
        let consumed = connection.consumed();
        connection.consume(consumed).unwrap();
        offset += consumed;
        match event {
            ServerObserved::Head(exchange_id) => break exchange_id,
            ServerObserved::Write(_) | ServerObserved::NeedInput if consumed != 0 => {}
            event => panic!("unexpected request-head progress: {event:?}"),
        }
    };
    assert_eq!(offset, head.len());
    assert!(
        !connection
            .prepare_response(
                exchange_id,
                ConnectionResponse {
                    status: 417,
                    reason: "Expectation Failed",
                    headers: &[],
                    body_len: Some(0),
                },
            )
            .unwrap()
    );

    let mut offset = 0;
    for _ in 0..32 {
        let event = server_step(&mut connection, &tail[offset..]).unwrap();
        if event == ServerObserved::Done {
            assert_eq!(offset, tail.len());
            return;
        }
        let consumed = connection.consumed();
        connection.consume(consumed).unwrap();
        offset += consumed;
        match event {
            ServerObserved::Write(_) | ServerObserved::NeedInput => {}
            event => panic!("suppressed request leaked an event: {event:?}"),
        }
    }
    panic!("suppressed request did not finish")
}
