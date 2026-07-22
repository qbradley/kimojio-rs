use kimojio_fsm_http::{
    ClientConnection, ClientEvent, ClientRequest, ConnectionResponse, ExchangeId, H2Client,
    H2Frame, H2FrameType, H2HeaderBlockDecoder, H2OutboundCommit, Http1ConnectionDecoder,
    Http1ConnectionEvent, Http1HeaderScratch, HttpLimits, HttpProtocol, HttpVersion,
    ServerConnection, ServerError, ServerEvent, Step, project_h2_response_head,
};

#[derive(Debug, Eq, PartialEq)]
enum ServerObserved {
    NeedInput,
    Write(Vec<u8>),
    Head {
        exchange_id: ExchangeId,
        version: HttpVersion,
    },
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
        Step::Event(ServerEvent::RequestHead {
            exchange_id,
            version,
            ..
        }) => ServerObserved::Head {
            exchange_id,
            version,
        },
        Step::Event(ServerEvent::RequestComplete { exchange_id }) => {
            ServerObserved::Complete(exchange_id)
        }
        Step::Done => ServerObserved::Done,
        Step::Event(ServerEvent::RequestBody { .. }) => {
            panic!("bodyless test request produced body bytes")
        }
        _ => panic!("unexpected server step"),
    })
}

fn accept_http1_request(input: &[u8]) -> (ServerConnection, ExchangeId, HttpVersion) {
    let mut connection =
        ServerConnection::new_with_protocol(HttpProtocol::Http1, HttpLimits::new());
    let mut offset = 0;
    let mut exchange = None;
    let mut version = None;

    for _ in 0..16 {
        let observed = server_step(&mut connection, &input[offset..]).unwrap();
        let consumed = connection.consumed();
        connection.consume(consumed).unwrap();
        offset += consumed;
        match observed {
            ServerObserved::Head {
                exchange_id,
                version: observed_version,
            } => {
                exchange = Some(exchange_id);
                version = Some(observed_version);
            }
            ServerObserved::Complete(exchange_id) => {
                assert_eq!(Some(exchange_id), exchange);
                assert_eq!(offset, input.len());
                return (connection, exchange_id, version.unwrap());
            }
            ServerObserved::NeedInput if consumed != 0 => {}
            ServerObserved::NeedInput => panic!("complete request bytes were supplied"),
            ServerObserved::Write(_) | ServerObserved::Done => {
                panic!("HTTP/1 request decoding produced outbound progress")
            }
        }
    }
    panic!("request did not complete")
}

fn take_client_block(client: &mut H2Client, commit: H2OutboundCommit) -> Vec<u8> {
    let block = client.next_outbound_block().unwrap();
    assert_eq!(block.commit(), commit);
    let bytes = block.bytes().to_vec();
    client.acknowledge_outbound_block(commit).unwrap();
    bytes
}

fn accept_http2_request() -> (ServerConnection, ExchangeId, u32) {
    let mut peer = H2Client::default();
    let mut input = peer.connection_preface();
    let (stream_id, commit) = peer
        .open_stream_with_raw_headers("GET", "http", "example.test", "/", &[], true)
        .unwrap();
    input.extend_from_slice(&take_client_block(&mut peer, commit));

    let mut connection =
        ServerConnection::new_with_protocol(HttpProtocol::Http2, HttpLimits::new());
    let mut offset = 0;
    let mut exchange = None;
    let mut complete = false;
    for _ in 0..32 {
        let observed = server_step(&mut connection, &input[offset..]).unwrap();
        let consumed = connection.consumed();
        connection.consume(consumed).unwrap();
        offset += consumed;
        match observed {
            ServerObserved::Head {
                exchange_id,
                version,
            } => {
                assert_eq!(version, HttpVersion::Http2);
                exchange = Some(exchange_id);
            }
            ServerObserved::Complete(exchange_id) => {
                assert_eq!(Some(exchange_id), exchange);
                complete = true;
            }
            ServerObserved::Write(_) | ServerObserved::NeedInput => {}
            ServerObserved::Done => panic!("response was not prepared"),
        }
        if offset == input.len() && complete {
            return (connection, exchange.unwrap(), stream_id);
        }
    }
    panic!("HTTP/2 request did not complete")
}

fn prepare_response(
    connection: &mut ServerConnection,
    exchange_id: ExchangeId,
    body_len: Option<usize>,
) -> bool {
    connection
        .prepare_response(
            exchange_id,
            ConnectionResponse {
                status: 200,
                reason: "OK",
                headers: &[],
                body_len,
            },
        )
        .unwrap()
}

fn write_body_chunk(
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
        assert!(chunk.payload_len() <= payload.len());
        let mut wire = chunk.header().to_vec();
        wire.extend_from_slice(&payload[..chunk.payload_len()]);
        wire.extend_from_slice(chunk.footer());
        wire
    };
    connection.commit_body_chunk(exchange_id).unwrap();
    wire
}

fn decode_http1_body(wire: &[u8]) -> Vec<u8> {
    enum Observed {
        Progress(usize),
        Body(Vec<u8>, usize),
        Complete,
        NeedInput,
        ProtocolSwitch,
    }

    let mut decoder = Http1ConnectionDecoder::response("GET", HttpLimits::new());
    let mut scratch = Http1HeaderScratch::new(HttpLimits::new().max_headers());
    let mut offset = 0;
    let mut body = Vec::new();

    for _ in 0..16 {
        let event = scratch.with_input(&wire[offset..], |input, headers| {
            Ok::<_, ServerError>(match decoder.next_event(input, headers)? {
                Http1ConnectionEvent::Head { consumed, .. }
                | Http1ConnectionEvent::Trailers { consumed, .. } => Observed::Progress(consumed),
                Http1ConnectionEvent::Body { chunk, consumed } => {
                    Observed::Body(chunk.to_vec(), consumed)
                }
                Http1ConnectionEvent::Complete => Observed::Complete,
                Http1ConnectionEvent::NeedInput => Observed::NeedInput,
                Http1ConnectionEvent::ProtocolSwitch { .. } => Observed::ProtocolSwitch,
            })
        });
        match event.unwrap() {
            Observed::Progress(consumed) => offset += consumed,
            Observed::Body(chunk, consumed) => {
                body.extend_from_slice(&chunk);
                offset += consumed;
            }
            Observed::Complete => {
                assert_eq!(offset, wire.len());
                return body;
            }
            Observed::NeedInput => panic!("complete response bytes were supplied"),
            Observed::ProtocolSwitch => panic!("response unexpectedly switched protocols"),
        }
    }
    panic!("response did not complete")
}

#[test]
fn http11_unknown_length_emits_chunked_wire_reassembles_and_reuses() {
    let request = b"GET / HTTP/1.1\r\nhost: example.test\r\n\r\n";
    let (mut connection, exchange_id, version) = accept_http1_request(request);
    assert_eq!(version, HttpVersion::Http11);
    assert!(prepare_response(&mut connection, exchange_id, None));

    let mut wire = write_body_chunk(&mut connection, exchange_id, b"hello");
    wire.extend_from_slice(&write_body_chunk(&mut connection, exchange_id, b" world"));
    wire.extend_from_slice(&write_body_chunk(&mut connection, exchange_id, b""));

    assert_eq!(
        wire,
        b"HTTP/1.1 200 OK\r\n\
transfer-encoding: chunked\r\n\r\n\
5\r\nhello\r\n\
6\r\n world\r\n\
0\r\n\r\n"
    );
    assert!(!wire.windows(15).any(|bytes| bytes == b"content-length:"));
    assert_eq!(decode_http1_body(&wire), b"hello world");
    assert_eq!(
        server_step(&mut connection, &[]).unwrap(),
        ServerObserved::Done
    );
    assert!(connection.begin_next_exchange(exchange_id).unwrap());
}

#[test]
fn http10_unknown_length_is_close_delimited() {
    let request = b"GET / HTTP/1.0\r\nconnection: keep-alive\r\n\r\n";
    let (mut connection, exchange_id, version) = accept_http1_request(request);
    assert_eq!(version, HttpVersion::Http10);
    assert!(prepare_response(&mut connection, exchange_id, None));

    let mut wire = write_body_chunk(&mut connection, exchange_id, b"close me");
    wire.extend_from_slice(&write_body_chunk(&mut connection, exchange_id, b""));

    assert_eq!(
        wire,
        b"HTTP/1.0 200 OK\r\nconnection: close\r\n\r\nclose me"
    );
    assert!(!wire.windows(18).any(|bytes| bytes == b"transfer-encoding:"));
    assert!(!wire.windows(15).any(|bytes| bytes == b"content-length:"));
    assert_eq!(
        server_step(&mut connection, &[]).unwrap(),
        ServerObserved::Done
    );
    assert!(!connection.begin_next_exchange(exchange_id).unwrap());
}

#[test]
fn http2_unknown_length_omits_content_length_and_ends_stream() {
    let (mut connection, exchange_id, stream_id) = accept_http2_request();
    assert!(prepare_response(&mut connection, exchange_id, None));

    let ServerObserved::Write(header_bytes) = server_step(&mut connection, &[]).unwrap() else {
        panic!("response headers were not ready")
    };
    connection.consume(0).unwrap();
    let (headers, consumed) = H2Frame::decode(&header_bytes).unwrap();
    assert_eq!(consumed, header_bytes.len());
    assert_eq!(headers.frame_type, H2FrameType::Headers);
    assert_eq!(headers.stream_id, stream_id);
    assert_eq!(headers.flags & 0x1, 0);
    let fields = H2HeaderBlockDecoder::new()
        .try_decode_with_limit(&headers.payload, usize::MAX)
        .unwrap();
    let head = project_h2_response_head(&fields, HttpLimits::new()).unwrap();
    assert_eq!(head.status(), 200);
    assert_eq!(head.content_length(), None);

    let data_bytes = write_body_chunk(&mut connection, exchange_id, b"streamed");
    let (data, consumed) = H2Frame::decode(&data_bytes).unwrap();
    assert_eq!(consumed, data_bytes.len());
    assert_eq!(data.frame_type, H2FrameType::Data);
    assert_eq!(data.stream_id, stream_id);
    assert_eq!(data.flags & 0x1, 0);
    assert_eq!(data.payload, b"streamed");

    let end_bytes = write_body_chunk(&mut connection, exchange_id, b"");
    let (end, consumed) = H2Frame::decode(&end_bytes).unwrap();
    assert_eq!(consumed, end_bytes.len());
    assert_eq!(end.frame_type, H2FrameType::Data);
    assert_eq!(end.stream_id, stream_id);
    assert_eq!(end.flags & 0x1, 0x1);
    assert!(end.payload.is_empty());
    assert_eq!(
        server_step(&mut connection, &[]).unwrap(),
        ServerObserved::Done
    );
    assert!(connection.begin_next_exchange(exchange_id).unwrap());
}

#[test]
fn known_length_http1_responses_remain_byte_exact() {
    let cases = [
        (
            b"GET / HTTP/1.1\r\nhost: example.test\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello".as_slice(),
            true,
        ),
        (
            b"GET / HTTP/1.0\r\nconnection: keep-alive\r\n\r\n".as_slice(),
            b"HTTP/1.0 200 OK\r\ncontent-length: 5\r\nconnection: keep-alive\r\n\r\nhello"
                .as_slice(),
            true,
        ),
    ];

    for (request, expected, reusable) in cases {
        let (mut connection, exchange_id, _) = accept_http1_request(request);
        assert!(prepare_response(&mut connection, exchange_id, Some(5)));
        let wire = write_body_chunk(&mut connection, exchange_id, b"hello");
        assert_eq!(wire, expected);
        assert_eq!(
            server_step(&mut connection, &[]).unwrap(),
            ServerObserved::Done
        );
        assert_eq!(
            connection.begin_next_exchange(exchange_id).unwrap(),
            reusable
        );
    }
}

#[test]
fn known_length_http2_response_keeps_content_length_and_final_data_end_stream() {
    let (mut connection, exchange_id, stream_id) = accept_http2_request();
    assert!(prepare_response(&mut connection, exchange_id, Some(3)));

    let ServerObserved::Write(header_bytes) = server_step(&mut connection, &[]).unwrap() else {
        panic!("response headers were not ready")
    };
    connection.consume(0).unwrap();
    let (headers, consumed) = H2Frame::decode(&header_bytes).unwrap();
    assert_eq!(consumed, header_bytes.len());
    assert_eq!(headers.frame_type, H2FrameType::Headers);
    assert_eq!(headers.flags & 0x1, 0);
    let fields = H2HeaderBlockDecoder::new()
        .try_decode_with_limit(&headers.payload, usize::MAX)
        .unwrap();
    let head = project_h2_response_head(&fields, HttpLimits::new()).unwrap();
    assert_eq!(head.content_length(), Some(3));

    let data_bytes = write_body_chunk(&mut connection, exchange_id, b"abc");
    let (data, consumed) = H2Frame::decode(&data_bytes).unwrap();
    assert_eq!(consumed, data_bytes.len());
    assert_eq!(data.frame_type, H2FrameType::Data);
    assert_eq!(data.stream_id, stream_id);
    assert_eq!(data.flags & 0x1, 0x1);
    assert_eq!(data.payload, b"abc");
}

#[test]
fn abandoned_known_or_streaming_body_cannot_be_reused() {
    let request = b"GET / HTTP/1.1\r\nhost: example.test\r\n\r\n";
    let (mut known, known_exchange, _) = accept_http1_request(request);
    assert!(prepare_response(&mut known, known_exchange, Some(5)));
    let ServerObserved::Write(_) = server_step(&mut known, &[]).unwrap() else {
        panic!("known-length response head was not ready")
    };
    known.consume(0).unwrap();
    assert_eq!(
        known.begin_next_exchange(known_exchange),
        Err(ServerError::InvalidOutboundState)
    );

    let (mut streaming, streaming_exchange, _) = accept_http1_request(request);
    assert!(prepare_response(&mut streaming, streaming_exchange, None));
    let partial = write_body_chunk(&mut streaming, streaming_exchange, b"partial");
    assert!(partial.ends_with(b"7\r\npartial\r\n"));
    assert_eq!(
        streaming.begin_next_exchange(streaming_exchange),
        Err(ServerError::InvalidOutboundState)
    );
}

#[derive(Debug, Eq, PartialEq)]
enum ClientObserved {
    NeedInput,
    Write(Vec<u8>),
    Head(ExchangeId),
    Complete(ExchangeId),
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
        Step::Event(ClientEvent::ResponseComplete { exchange_id }) => {
            ClientObserved::Complete(exchange_id)
        }
        Step::Event(ClientEvent::ResponseBody { .. }) => panic!("unexpected response body"),
        Step::Done => panic!("unexpected terminal step"),
        _ => panic!("unexpected client step"),
    })
}

fn write_client_body_chunk(
    connection: &mut ClientConnection,
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
fn client_unknown_length_request_uses_chunked_framing_and_completes() {
    let mut connection = ClientConnection::new(HttpProtocol::Http1, HttpLimits::new());
    let exchange_id = connection
        .prepare_request(ClientRequest {
            method: "POST",
            scheme: "http",
            authority: "example.test",
            target: "/",
            headers: &[],
            body_len: None,
        })
        .unwrap();

    let mut wire = write_client_body_chunk(&mut connection, exchange_id, b"one");
    wire.extend_from_slice(&write_client_body_chunk(
        &mut connection,
        exchange_id,
        b"two",
    ));
    wire.extend_from_slice(&write_client_body_chunk(&mut connection, exchange_id, b""));
    assert_eq!(
        wire,
        b"POST / HTTP/1.1\r\n\
transfer-encoding: chunked\r\n\r\n\
3\r\none\r\n\
3\r\ntwo\r\n\
0\r\n\r\n"
    );

    let response = b"HTTP/1.1 204 No Content\r\n\r\n";
    assert_eq!(
        client_step(&mut connection, response).unwrap(),
        ClientObserved::Head(exchange_id)
    );
    let consumed = connection.consumed();
    connection.consume(consumed).unwrap();
    assert_eq!(
        client_step(&mut connection, &response[consumed..]).unwrap(),
        ClientObserved::Complete(exchange_id)
    );
    connection.consume(0).unwrap();
    assert!(connection.begin_next_exchange(exchange_id).unwrap());
}
