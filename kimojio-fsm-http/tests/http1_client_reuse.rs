use kimojio_fsm_http::{
    ClientConnection, ClientEvent, ClientRequest, ExchangeId, HttpLimits, HttpProtocol,
    HttpVersion, ServerError, Step,
};

#[derive(Debug, Eq, PartialEq)]
enum Observed {
    NeedInput,
    Write(Vec<u8>),
    Head {
        exchange_id: ExchangeId,
        status: u16,
        version: HttpVersion,
    },
    Body {
        exchange_id: ExchangeId,
        bytes: Vec<u8>,
    },
    Trailers {
        exchange_id: ExchangeId,
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
            version,
            ..
        }) => Observed::Head {
            exchange_id,
            status,
            version,
        },
        Step::Event(ClientEvent::ResponseBody { exchange_id, chunk }) => Observed::Body {
            exchange_id,
            bytes: chunk.to_vec(),
        },
        Step::Event(ClientEvent::ResponseTrailers { exchange_id, .. }) => {
            Observed::Trailers { exchange_id }
        }
        Step::Event(ClientEvent::ResponseComplete { exchange_id }) => {
            Observed::Complete { exchange_id }
        }
        Step::Done => Observed::Done,
        _ => panic!("unexpected client step"),
    })
}

fn prepare_request(
    connection: &mut ClientConnection,
    method: &str,
    target: &str,
    body: &[u8],
) -> (ExchangeId, Vec<u8>) {
    let exchange_id = connection
        .prepare_request(ClientRequest {
            method,
            scheme: "http",
            authority: "example.test",
            target,
            headers: &[],
            body_len: Some(body.len()),
        })
        .unwrap();

    let wire = if body.is_empty() {
        let Observed::Write(head) = step(connection, &[]).unwrap() else {
            panic!("request head was not ready");
        };
        connection.consume(connection.consumed()).unwrap();
        head
    } else {
        assert!(
            connection
                .prepare_body_chunk(exchange_id, body.len())
                .unwrap()
        );
        let chunk = connection.body_chunk(exchange_id).unwrap();
        assert_eq!(chunk.payload_len(), body.len());
        let mut wire = chunk.header().to_vec();
        wire.extend_from_slice(body);
        wire.extend_from_slice(chunk.footer());
        connection.commit_body_chunk(exchange_id).unwrap();
        wire
    };
    (exchange_id, wire)
}

fn receive_response(
    connection: &mut ClientConnection,
    exchange_id: ExchangeId,
    input: &[u8],
) -> (u16, HttpVersion, Vec<u8>, usize) {
    let mut status = None;
    let mut version = None;
    let mut body = Vec::new();
    let mut offset = 0;

    for _ in 0..32 {
        let observed = step(connection, &input[offset..]).unwrap();
        let consumed = connection.consumed();
        connection.consume(consumed).unwrap();
        offset += consumed;
        match observed {
            Observed::Head {
                exchange_id: observed_exchange,
                status: observed_status,
                version: observed_version,
            } => {
                assert_eq!(observed_exchange, exchange_id);
                assert!(status.replace(observed_status).is_none());
                version = Some(observed_version);
            }
            Observed::Body {
                exchange_id: observed_exchange,
                bytes,
            } => {
                assert_eq!(observed_exchange, exchange_id);
                body.extend_from_slice(&bytes);
            }
            Observed::Trailers {
                exchange_id: observed_exchange,
            } => assert_eq!(observed_exchange, exchange_id),
            Observed::Complete {
                exchange_id: observed_exchange,
            } => {
                assert_eq!(observed_exchange, exchange_id);
                return (status.unwrap(), version.unwrap(), body, offset);
            }
            Observed::NeedInput if consumed != 0 => {}
            Observed::NeedInput => panic!("complete response bytes were already supplied"),
            Observed::Write(_) | Observed::Done => {
                panic!("HTTP/1 response decoding produced unexpected progress")
            }
        }
    }
    panic!("response did not complete");
}

#[test]
fn http11_reuses_connection_for_two_distinct_exchanges() {
    let mut connection = ClientConnection::new(HttpProtocol::Http1, HttpLimits::new());

    let (first_exchange, first_request) =
        prepare_request(&mut connection, "POST", "/one", b"request-one");
    assert!(first_request.starts_with(b"POST /one HTTP/1.1\r\n"));
    assert!(first_request.ends_with(b"request-one"));
    let first_response = b"HTTP/1.1 200 OK\r\ncontent-length: 12\r\n\r\nresponse-one";
    let (status, version, body, consumed) =
        receive_response(&mut connection, first_exchange, first_response);
    assert_eq!((status, version), (200, HttpVersion::Http11));
    assert_eq!(body, b"response-one");
    assert_eq!(consumed, first_response.len());
    assert!(connection.begin_next_exchange(first_exchange).unwrap());

    let (second_exchange, second_request) =
        prepare_request(&mut connection, "POST", "/two", b"request-two-two");
    assert!(second_request.starts_with(b"POST /two HTTP/1.1\r\n"));
    assert!(second_request.ends_with(b"request-two-two"));
    assert!(!second_request.ends_with(b"request-one"));
    let second_response = b"HTTP/1.1 201 Created\r\ncontent-length: 16\r\n\r\nresponse-two-two";
    let (status, version, body, consumed) =
        receive_response(&mut connection, second_exchange, second_response);
    assert_eq!((status, version), (201, HttpVersion::Http11));
    assert_eq!(body, b"response-two-two");
    assert_eq!(consumed, second_response.len());
    assert!(connection.begin_next_exchange(second_exchange).unwrap());
}

#[test]
fn connection_close_token_list_prevents_reuse() {
    let mut connection = ClientConnection::new(HttpProtocol::Http1, HttpLimits::new());
    let (exchange_id, _) = prepare_request(&mut connection, "GET", "/", &[]);
    let response = b"HTTP/1.1 204 No Content\r\n\
Connection: keep-alive, Upgrade\r\n\
connection: custom, CLOSE, another\r\n\r\n";

    let (_, _, body, consumed) = receive_response(&mut connection, exchange_id, response);
    assert!(body.is_empty());
    assert_eq!(consumed, response.len());
    assert!(!connection.begin_next_exchange(exchange_id).unwrap());
}

#[test]
fn http10_reuse_requires_keep_alive_response_option() {
    let mut closes = ClientConnection::new(HttpProtocol::Http1, HttpLimits::new());
    let (closes_exchange, _) = prepare_request(&mut closes, "GET", "/", &[]);
    let response = b"HTTP/1.0 204 No Content\r\n\r\n";
    let (_, version, _, consumed) = receive_response(&mut closes, closes_exchange, response);
    assert_eq!(version, HttpVersion::Http10);
    assert_eq!(consumed, response.len());
    assert!(!closes.begin_next_exchange(closes_exchange).unwrap());

    let mut persists = ClientConnection::new(HttpProtocol::Http1, HttpLimits::new());
    let (persists_exchange, _) = prepare_request(&mut persists, "GET", "/", &[]);
    let response = b"HTTP/1.0 204 No Content\r\nConnection: upgrade, KEEP-ALIVE, custom\r\n\r\n";
    let (_, version, _, consumed) = receive_response(&mut persists, persists_exchange, response);
    assert_eq!(version, HttpVersion::Http10);
    assert_eq!(consumed, response.len());
    assert!(persists.begin_next_exchange(persists_exchange).unwrap());
}

#[test]
fn incomplete_response_body_prevents_reuse() {
    let mut connection = ClientConnection::new(HttpProtocol::Http1, HttpLimits::new());
    let (exchange_id, _) = prepare_request(&mut connection, "GET", "/", &[]);
    let response = b"HTTP/1.1 200 OK\r\ncontent-length: 4\r\n\r\nab";
    let mut offset = 0;

    assert_eq!(
        step(&mut connection, &response[offset..]).unwrap(),
        Observed::Head {
            exchange_id,
            status: 200,
            version: HttpVersion::Http11,
        }
    );
    offset += connection.consumed();
    connection.consume(connection.consumed()).unwrap();
    assert_eq!(
        step(&mut connection, &response[offset..]).unwrap(),
        Observed::Body {
            exchange_id,
            bytes: b"ab".to_vec(),
        }
    );
    offset += connection.consumed();
    connection.consume(connection.consumed()).unwrap();
    assert_eq!(offset, response.len());
    assert_eq!(step(&mut connection, &[]).unwrap(), Observed::NeedInput);
    connection.consume(0).unwrap();

    assert!(!connection.begin_next_exchange(exchange_id).unwrap());
}

#[test]
fn eof_delimited_response_prevents_reuse() {
    let mut connection = ClientConnection::new(HttpProtocol::Http1, HttpLimits::new());
    let (exchange_id, _) = prepare_request(&mut connection, "GET", "/", &[]);
    let response = b"HTTP/1.1 200 OK\r\n\r\nclose-delimited";
    let mut offset = 0;

    assert!(matches!(
        step(&mut connection, &response[offset..]).unwrap(),
        Observed::Head {
            exchange_id: observed_exchange,
            status: 200,
            ..
        } if observed_exchange == exchange_id
    ));
    offset += connection.consumed();
    connection.consume(connection.consumed()).unwrap();
    assert_eq!(
        step(&mut connection, &response[offset..]).unwrap(),
        Observed::Body {
            exchange_id,
            bytes: b"close-delimited".to_vec(),
        }
    );
    offset += connection.consumed();
    connection.consume(connection.consumed()).unwrap();
    assert_eq!(offset, response.len());
    assert_eq!(step(&mut connection, &[]).unwrap(), Observed::NeedInput);
    connection.consume(0).unwrap();
    assert!(connection.finish_eof().unwrap());
    assert_eq!(
        step(&mut connection, &[]).unwrap(),
        Observed::Complete { exchange_id }
    );
    connection.consume(0).unwrap();

    assert!(!connection.begin_next_exchange(exchange_id).unwrap());
}

#[test]
fn unread_response_bytes_prevent_reuse() {
    let mut connection = ClientConnection::new(HttpProtocol::Http1, HttpLimits::new());
    let (exchange_id, _) = prepare_request(&mut connection, "GET", "/", &[]);
    let mut input = b"HTTP/1.1 200 OK\r\ncontent-length: 3\r\n\r\none".to_vec();
    input.extend_from_slice(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n");

    let (_, _, body, consumed) = receive_response(&mut connection, exchange_id, &input);
    assert_eq!(body, b"one");
    assert!(consumed < input.len());
    assert!(!connection.begin_next_exchange(exchange_id).unwrap());
}

#[test]
fn reset_discards_http1_decoder_and_exchange_accounting() {
    let limits = HttpLimits::new()
        .set_max_headers(3)
        .set_max_header_bytes(96)
        .set_max_body_bytes(7);
    let mut connection = ClientConnection::new(HttpProtocol::Http1, limits);

    let (first_exchange, _) = prepare_request(&mut connection, "GET", "/first", &[]);
    let first = b"HTTP/1.1 200 OK\r\n\
transfer-encoding: chunked\r\n\
x-first: one\r\n\r\n\
4\r\nabcd\r\n\
0\r\nx-trailer: one\r\n\r\n";
    let (_, _, body, consumed) = receive_response(&mut connection, first_exchange, first);
    assert_eq!(body, b"abcd");
    assert_eq!(consumed, first.len());
    assert!(connection.begin_next_exchange(first_exchange).unwrap());

    let (second_exchange, _) = prepare_request(&mut connection, "GET", "/second", &[]);
    let second = b"HTTP/1.1 200 OK\r\n\
transfer-encoding: chunked\r\n\
x-second: two\r\n\r\n\
7\r\ntwo-two\r\n\
0\r\nx-trailer: two\r\n\r\n";
    let (_, _, body, consumed) = receive_response(&mut connection, second_exchange, second);
    assert_eq!(body, b"two-two");
    assert_eq!(consumed, second.len());
    assert!(connection.begin_next_exchange(second_exchange).unwrap());

    let (third_exchange, _) = prepare_request(&mut connection, "HEAD", "/third", &[]);
    let third = b"HTTP/1.1 200 OK\r\ncontent-length: 7\r\n\r\n";
    let (_, _, body, consumed) = receive_response(&mut connection, third_exchange, third);
    assert!(body.is_empty());
    assert_eq!(consumed, third.len());
    assert!(connection.begin_next_exchange(third_exchange).unwrap());
}

#[test]
fn protocol_error_prevents_reuse() {
    let mut connection = ClientConnection::new(HttpProtocol::Http1, HttpLimits::new());
    let (exchange_id, _) = prepare_request(&mut connection, "GET", "/", &[]);
    let malformed = b"HTTP/1.1 200 OK\r\ncontent-length: 1\r\ncontent-length: 2\r\n\r\n";

    assert!(step(&mut connection, malformed).is_err());
    assert!(!connection.begin_next_exchange(exchange_id).unwrap());
}
