use kimojio_fsm_http::{
    ClientConnection, ClientEvent, ClientRequest, ConnectionResponse, ExchangeId, H2Client,
    H2Frame, H2FrameType, H2HeaderBlockDecoder, H2HeaderField, H2OutboundCommit, HeaderRef,
    HttpLimits, HttpProtocol, HttpVersion, RequestExpectation, ServerConnection, ServerError,
    ServerEvent, Step, project_h2_response_head,
};

#[derive(Debug, Eq, PartialEq)]
enum ServerObserved {
    NeedInput,
    Write(Vec<u8>),
    Head {
        exchange_id: ExchangeId,
        version: HttpVersion,
        expectation: RequestExpectation,
    },
    Body {
        exchange_id: ExchangeId,
        bytes: Vec<u8>,
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
            expectation,
            ..
        }) => ServerObserved::Head {
            exchange_id,
            version,
            expectation,
        },
        Step::Event(ServerEvent::RequestBody { exchange_id, chunk }) => ServerObserved::Body {
            exchange_id,
            bytes: chunk.to_vec(),
        },
        Step::Event(ServerEvent::RequestComplete { exchange_id }) => {
            ServerObserved::Complete(exchange_id)
        }
        Step::Done => ServerObserved::Done,
        _ => panic!("unexpected server step"),
    })
}

fn acknowledge_server(connection: &mut ServerConnection) -> usize {
    let consumed = connection.consumed();
    connection.consume(consumed).unwrap();
    consumed
}

fn request_head(
    connection: &mut ServerConnection,
    input: &[u8],
) -> (ExchangeId, HttpVersion, RequestExpectation, usize) {
    let ServerObserved::Head {
        exchange_id,
        version,
        expectation,
    } = server_step(connection, input).unwrap()
    else {
        panic!("request head was not surfaced");
    };
    let consumed = acknowledge_server(connection);
    (exchange_id, version, expectation, consumed)
}

#[test]
fn server_http11_continue_then_accepts_body() {
    let request = b"POST /upload HTTP/1.1\r\n\
host: example.test\r\n\
content-length: 5\r\n\
expect: 100-continue\r\n\r\n\
hello";
    let mut connection =
        ServerConnection::new_with_protocol(HttpProtocol::Http1, HttpLimits::new());
    let (exchange_id, version, expectation, offset) = request_head(&mut connection, request);
    assert_eq!(version, HttpVersion::Http11);
    assert_eq!(expectation, RequestExpectation::Continue);

    assert!(connection.prepare_continue(exchange_id).unwrap());
    assert_eq!(
        server_step(&mut connection, &request[offset..]).unwrap(),
        ServerObserved::Write(b"HTTP/1.1 100 Continue\r\n\r\n".to_vec())
    );
    assert_eq!(acknowledge_server(&mut connection), 0);
    assert_eq!(
        server_step(&mut connection, &request[offset..]).unwrap(),
        ServerObserved::Body {
            exchange_id,
            bytes: b"hello".to_vec(),
        }
    );
    assert_eq!(acknowledge_server(&mut connection), 5);
    assert_eq!(
        server_step(&mut connection, &[]).unwrap(),
        ServerObserved::Complete(exchange_id)
    );
}

#[test]
fn server_final_rejection_on_headers_never_surfaces_body() {
    let request = b"POST /upload HTTP/1.1\r\n\
host: example.test\r\n\
content-length: 12\r\n\
expect: 100-continue\r\n\r\n\
large-upload";
    let mut connection =
        ServerConnection::new_with_protocol(HttpProtocol::Http1, HttpLimits::new());
    let (exchange_id, _, expectation, offset) = request_head(&mut connection, request);
    assert_eq!(expectation, RequestExpectation::Continue);
    assert!(offset < request.len());

    assert!(
        !connection
            .prepare_response(
                exchange_id,
                ConnectionResponse {
                    status: 413,
                    reason: "Content Too Large",
                    headers: &[],
                    body_len: Some(0),
                },
            )
            .unwrap()
    );
    let ServerObserved::Write(response) = server_step(&mut connection, &request[offset..]).unwrap()
    else {
        panic!("final rejection was not ready");
    };
    assert!(response.starts_with(b"HTTP/1.1 413 Content Too Large\r\n"));
    assert!(
        response
            .windows(b"connection: close\r\n".len())
            .any(|bytes| bytes == b"connection: close\r\n")
    );
    assert_eq!(acknowledge_server(&mut connection), 0);
    assert_eq!(
        server_step(&mut connection, &request[offset..]).unwrap(),
        ServerObserved::Done
    );
    assert!(!connection.begin_next_exchange(exchange_id).unwrap());
}

/// RFC 9110 section 10.1.1 requires an HTTP/1.0 `100-continue` expectation to
/// be ignored, so it is reported as `None` and the body flows without gating.
#[test]
fn server_http10_expectation_is_ignored_and_never_emits_informational_response() {
    let request = b"POST /upload HTTP/1.0\r\n\
content-length: 4\r\n\
expect: 100-continue\r\n\r\n\
body";
    let mut connection =
        ServerConnection::new_with_protocol(HttpProtocol::Http1, HttpLimits::new());
    let (exchange_id, version, expectation, offset) = request_head(&mut connection, request);
    assert_eq!(version, HttpVersion::Http10);
    assert_eq!(expectation, RequestExpectation::None);
    assert!(!connection.prepare_continue(exchange_id).unwrap());
    assert_eq!(
        server_step(&mut connection, &request[offset..]).unwrap(),
        ServerObserved::Body {
            exchange_id,
            bytes: b"body".to_vec(),
        }
    );
}

#[test]
fn server_refuses_unrecognized_expectations_with_417() {
    for expect in ["something-else", "100-continue, something-else"] {
        let request = format!(
            "POST /upload HTTP/1.1\r\n\
             host: example.test\r\n\
             content-length: 4\r\n\
             expect: {expect}\r\n\r\n\
             body"
        );
        let mut connection =
            ServerConnection::new_with_protocol(HttpProtocol::Http1, HttpLimits::new());
        let (exchange_id, _, expectation, offset) =
            request_head(&mut connection, request.as_bytes());
        assert_eq!(expectation, RequestExpectation::Unsupported);

        let ServerObserved::Write(response) =
            server_step(&mut connection, &request.as_bytes()[offset..]).unwrap()
        else {
            panic!("unsupported expectation was not rejected");
        };
        assert!(response.starts_with(b"HTTP/1.1 417 Expectation Failed\r\n"));
        assert_eq!(acknowledge_server(&mut connection), 0);
        assert_eq!(
            server_step(&mut connection, &request.as_bytes()[offset..]).unwrap(),
            ServerObserved::Done
        );
        assert!(!connection.begin_next_exchange(exchange_id).unwrap());
    }
}

#[test]
fn server_parses_case_insensitive_expectations_across_field_lines() {
    let request = b"POST /upload HTTP/1.1\r\n\
host: example.test\r\n\
content-length: 1\r\n\
Expect: , 100-CONTINUE,,\r\n\
expect:\t100-continue\t,\r\n\r\n\
x";
    let mut connection =
        ServerConnection::new_with_protocol(HttpProtocol::Http1, HttpLimits::new());
    let (exchange_id, _, expectation, _) = request_head(&mut connection, request);
    assert_eq!(expectation, RequestExpectation::Continue);
    assert!(connection.prepare_continue(exchange_id).unwrap());
}

#[test]
fn server_defaults_to_continue_when_caller_ignores_expectation() {
    let request = b"POST /upload HTTP/1.1\r\n\
host: example.test\r\n\
content-length: 1\r\n\
expect: 100-continue\r\n\r\n\
x";
    let mut connection =
        ServerConnection::new_with_protocol(HttpProtocol::Http1, HttpLimits::new());
    let ServerObserved::Head { exchange_id, .. } = server_step(&mut connection, request).unwrap()
    else {
        panic!("request head was not surfaced");
    };
    let offset = acknowledge_server(&mut connection);

    assert_eq!(
        server_step(&mut connection, &request[offset..]).unwrap(),
        ServerObserved::Write(b"HTTP/1.1 100 Continue\r\n\r\n".to_vec())
    );
    assert_eq!(acknowledge_server(&mut connection), 0);
    assert_eq!(
        server_step(&mut connection, &request[offset..]).unwrap(),
        ServerObserved::Body {
            exchange_id,
            bytes: b"x".to_vec(),
        }
    );
}

#[derive(Debug, Eq, PartialEq)]
enum ClientObserved {
    NeedInput,
    Write(Vec<u8>),
    Head {
        exchange_id: ExchangeId,
        status: u16,
    },
    Complete {
        exchange_id: ExchangeId,
    },
    Done,
}

fn client_step(
    connection: &mut ClientConnection,
    input: &[u8],
) -> Result<ClientObserved, ServerError> {
    connection.step(input, |step| match step {
        Step::NeedInput => ClientObserved::NeedInput,
        Step::Write(bytes) => ClientObserved::Write(bytes.to_vec()),
        Step::Event(ClientEvent::ResponseHead {
            exchange_id,
            status,
            ..
        }) => ClientObserved::Head {
            exchange_id,
            status,
        },
        Step::Event(ClientEvent::ResponseComplete { exchange_id }) => {
            ClientObserved::Complete { exchange_id }
        }
        Step::Done => ClientObserved::Done,
        Step::Event(ClientEvent::ResponseBody { .. }) => panic!("unexpected response body"),
        _ => panic!("unexpected client step"),
    })
}

fn expecting_client(body_len: usize) -> (ClientConnection, ExchangeId) {
    let headers = [HeaderRef::new(b"expect", b"100-continue")];
    let mut connection = ClientConnection::new(HttpProtocol::Http1, HttpLimits::new());
    let exchange_id = connection
        .prepare_request(ClientRequest {
            method: "POST",
            scheme: "http",
            authority: "example.test",
            target: "/upload",
            headers: &headers,
            body_len: Some(body_len),
        })
        .unwrap();
    (connection, exchange_id)
}

fn send_client_head(
    connection: &mut ClientConnection,
    exchange_id: ExchangeId,
    body_len: usize,
) -> Vec<u8> {
    assert!(
        !connection
            .prepare_body_chunk(exchange_id, body_len)
            .unwrap()
    );
    let ClientObserved::Write(head) = client_step(connection, &[]).unwrap() else {
        panic!("request head was not emitted separately");
    };
    connection.consume(connection.consumed()).unwrap();
    assert!(connection.is_waiting_for_continue(exchange_id));
    head
}

#[test]
fn client_waits_for_continue_before_sending_body() {
    let (mut connection, exchange_id) = expecting_client(5);
    let head = send_client_head(&mut connection, exchange_id, 5);
    assert!(head.ends_with(b"\r\n\r\n"));
    assert!(!head.ends_with(b"hello"));

    let response = b"HTTP/1.1 100 Continue\r\n\r\n";
    assert_eq!(
        client_step(&mut connection, response).unwrap(),
        ClientObserved::NeedInput
    );
    assert_eq!(connection.consumed(), response.len());
    connection.consume(connection.consumed()).unwrap();
    assert!(!connection.is_waiting_for_continue(exchange_id));
    assert!(connection.prepare_body_chunk(exchange_id, 5).unwrap());
    let chunk = connection.body_chunk(exchange_id).unwrap();
    assert!(chunk.header().is_empty());
    assert_eq!(chunk.payload_len(), 5);
    connection.commit_body_chunk(exchange_id).unwrap();
}

/// A declared body that was never sent leaves the peer's framing state
/// unknowable: if it means to read and discard that body it will swallow the
/// head of the next request, so the connection must be retired even when the
/// peer signalled keep-alive. The server half refuses reuse in the mirror
/// situation for the same reason.
#[test]
fn client_final_response_suppresses_unsent_body_and_retires_connection() {
    let (mut connection, exchange_id) = expecting_client(12);
    send_client_head(&mut connection, exchange_id, 12);

    let response = b"HTTP/1.1 417 Expectation Failed\r\ncontent-length: 0\r\n\r\n";
    assert_eq!(
        client_step(&mut connection, response).unwrap(),
        ClientObserved::Head {
            exchange_id,
            status: 417,
        }
    );
    assert_eq!(connection.consumed(), response.len());
    connection.consume(connection.consumed()).unwrap();
    assert!(!connection.is_waiting_for_continue(exchange_id));
    assert!(!connection.prepare_body_chunk(exchange_id, 12).unwrap());
    assert_eq!(
        client_step(&mut connection, &[]).unwrap(),
        ClientObserved::Complete { exchange_id }
    );
    connection.consume(0).unwrap();
    assert!(!connection.begin_next_exchange(exchange_id).unwrap());
}

/// An exchange that sent its whole body still reuses the connection, so the
/// rule above does not simply disable reuse for every expecting request.
#[test]
fn client_reuses_connection_after_sending_the_expected_body() {
    let (mut connection, exchange_id) = expecting_client(4);
    send_client_head(&mut connection, exchange_id, 4);
    connection.proceed_with_body(exchange_id).unwrap();
    assert!(connection.prepare_body_chunk(exchange_id, 4).unwrap());
    connection.commit_body_chunk(exchange_id).unwrap();

    let response = b"HTTP/1.1 204 No Content\r\n\r\n";
    assert_eq!(
        client_step(&mut connection, response).unwrap(),
        ClientObserved::Head {
            exchange_id,
            status: 204,
        }
    );
    connection.consume(connection.consumed()).unwrap();
    assert_eq!(
        client_step(&mut connection, &[]).unwrap(),
        ClientObserved::Complete { exchange_id }
    );
    connection.consume(0).unwrap();
    assert!(connection.begin_next_exchange(exchange_id).unwrap());
}

#[test]
fn client_adapter_can_release_body_after_external_wait() {
    let (mut connection, exchange_id) = expecting_client(4);
    send_client_head(&mut connection, exchange_id, 4);
    connection.proceed_with_body(exchange_id).unwrap();
    assert!(!connection.is_waiting_for_continue(exchange_id));
    assert!(connection.prepare_body_chunk(exchange_id, 4).unwrap());
}

fn take_h2_client_block(client: &mut H2Client, commit: H2OutboundCommit) -> Vec<u8> {
    let block = client.next_outbound_block().unwrap();
    assert_eq!(block.commit(), commit);
    let bytes = block.bytes().to_vec();
    client.acknowledge_outbound_block(commit).unwrap();
    bytes
}

fn h2_request(expect: &[u8], body: &[u8]) -> Vec<u8> {
    let mut client = H2Client::default();
    let mut wire = client.connection_preface();
    let content_length = body.len().to_string();
    let headers = [
        H2HeaderField::new(b"content-length", content_length.as_bytes()),
        H2HeaderField::new(b"expect", expect),
    ];
    let (stream_id, commit) = client
        .open_stream_with_raw_headers("POST", "https", "example.test", "/upload", &headers, false)
        .unwrap();
    wire.extend_from_slice(&take_h2_client_block(&mut client, commit));
    wire.extend_from_slice(&client.data_frame(stream_id, body, true));
    wire
}

#[test]
fn http2_valid_expectation_is_reported_without_gating_data() {
    let input = h2_request(b"100-continue", b"body");
    let mut connection = ServerConnection::new(HttpLimits::new());
    let mut offset = 0;
    let mut exchange_id = None;
    let mut body = None;
    let mut complete = false;

    for _ in 0..32 {
        let observed = server_step(&mut connection, &input[offset..]).unwrap();
        let consumed = acknowledge_server(&mut connection);
        offset += consumed;
        match observed {
            ServerObserved::Write(bytes) => {
                let (frame, _) = H2Frame::decode(&bytes).unwrap();
                assert_ne!(frame.frame_type, H2FrameType::Headers);
            }
            ServerObserved::Head {
                exchange_id: observed,
                version,
                expectation,
            } => {
                assert_eq!(version, HttpVersion::Http2);
                assert_eq!(expectation, RequestExpectation::Continue);
                exchange_id = Some(observed);
            }
            ServerObserved::Body {
                exchange_id: observed,
                bytes,
            } => {
                assert_eq!(Some(observed), exchange_id);
                body = Some(bytes);
            }
            ServerObserved::Complete(observed) => {
                assert_eq!(Some(observed), exchange_id);
                complete = true;
            }
            ServerObserved::NeedInput if consumed != 0 => {}
            ServerObserved::NeedInput => panic!("complete HTTP/2 request was already supplied"),
            ServerObserved::Done => panic!("no response was prepared"),
        }
        if offset == input.len() && complete {
            break;
        }
    }

    assert_eq!(offset, input.len());
    assert_eq!(body, Some(b"body".to_vec()));
    assert!(complete);
}

#[test]
fn http2_unsupported_expectation_gets_417_and_discards_data() {
    let input = h2_request(b"100-continue, other", b"body");
    let mut connection = ServerConnection::new(HttpLimits::new());
    let mut decoder = H2HeaderBlockDecoder::new();
    let mut offset = 0;
    let mut exchange_id = None;
    let mut status = None;
    let mut done = false;

    for _ in 0..48 {
        let observed = server_step(&mut connection, &input[offset..]).unwrap();
        let consumed = connection.consumed();
        if observed != ServerObserved::Done {
            connection.consume(consumed).unwrap();
        }
        offset += consumed;
        match observed {
            ServerObserved::Write(bytes) => {
                let mut frame_offset = 0;
                while frame_offset < bytes.len() {
                    let (frame, used) = H2Frame::decode(&bytes[frame_offset..]).unwrap();
                    frame_offset += used;
                    if frame.frame_type == H2FrameType::Headers {
                        let fields = decoder
                            .try_decode_with_limit(&frame.payload, usize::MAX)
                            .unwrap();
                        status = Some(
                            project_h2_response_head(&fields, HttpLimits::new())
                                .unwrap()
                                .status(),
                        );
                    }
                }
            }
            ServerObserved::Head {
                exchange_id: observed,
                expectation,
                ..
            } => {
                assert_eq!(expectation, RequestExpectation::Unsupported);
                exchange_id = Some(observed);
            }
            ServerObserved::Body { .. } => {
                panic!("rejected HTTP/2 request body was surfaced")
            }
            ServerObserved::Complete(_) => {
                panic!("automatically rejected request completed as an application request")
            }
            ServerObserved::Done => {
                done = true;
                break;
            }
            ServerObserved::NeedInput => {}
        }
    }

    assert_eq!(status, Some(417));
    assert!(done);
    assert_eq!(offset, input.len());
    assert!(
        connection
            .begin_next_exchange(exchange_id.unwrap())
            .unwrap()
    );
}
