use kimojio_fsm_http::{
    ConnectionResponse, ExchangeId, Http1ConnectionDecoder, Http1ConnectionEvent,
    Http1HeaderScratch, HttpLimits, HttpProtocol, HttpVersion, ServerConnection, ServerError,
    ServerEvent, Step,
};

#[derive(Debug, Eq, PartialEq)]
enum Observed {
    NeedInput,
    Write(Vec<u8>),
    Head {
        exchange_id: ExchangeId,
        target: Vec<u8>,
        version: HttpVersion,
    },
    Body {
        exchange_id: ExchangeId,
        bytes: Vec<u8>,
    },
    Complete(ExchangeId),
    Done,
}

fn step(connection: &mut ServerConnection, input: &[u8]) -> Result<Observed, ServerError> {
    connection.step(input, |step| match step {
        Step::NeedInput => Observed::NeedInput,
        Step::Write(bytes) => Observed::Write(bytes.to_vec()),
        Step::Done => Observed::Done,
        Step::Event(ServerEvent::RequestHead {
            exchange_id,
            target,
            version,
            ..
        }) => Observed::Head {
            exchange_id,
            target: target.to_vec(),
            version,
        },
        Step::Event(ServerEvent::RequestBody { exchange_id, chunk }) => Observed::Body {
            exchange_id,
            bytes: chunk.to_vec(),
        },
        Step::Event(ServerEvent::RequestComplete { exchange_id }) => {
            Observed::Complete(exchange_id)
        }
        Step::Event(_) => panic!("unexpected server event"),
        _ => panic!("unexpected server step"),
    })
}

fn receive_request(
    connection: &mut ServerConnection,
    input: &[u8],
    offset: &mut usize,
) -> (ExchangeId, Vec<u8>, Vec<u8>, HttpVersion) {
    let mut exchange_id = None;
    let mut target = Vec::new();
    let mut version = None;
    let mut body = Vec::new();

    for _ in 0..32 {
        let observed = step(connection, &input[*offset..]).unwrap();
        let consumed = connection.consumed();
        connection.consume(consumed).unwrap();
        *offset += consumed;
        match observed {
            Observed::Head {
                exchange_id: observed_id,
                target: observed_target,
                version: observed_version,
            } => {
                assert!(exchange_id.replace(observed_id).is_none());
                target = observed_target;
                version = Some(observed_version);
            }
            Observed::Body {
                exchange_id: observed_id,
                bytes,
            } => {
                assert_eq!(Some(observed_id), exchange_id);
                body.extend_from_slice(&bytes);
            }
            Observed::Complete(observed_id) => {
                assert_eq!(Some(observed_id), exchange_id);
                return (observed_id, target, body, version.unwrap());
            }
            Observed::NeedInput => panic!("complete request bytes were already supplied"),
            Observed::Write(_) | Observed::Done => {
                panic!("request decoding produced outbound progress")
            }
        }
    }
    panic!("request did not complete");
}

fn respond(
    connection: &mut ServerConnection,
    exchange_id: ExchangeId,
    body: &[u8],
) -> (Vec<u8>, bool) {
    let sends_body = connection
        .prepare_response(
            exchange_id,
            ConnectionResponse {
                status: 200,
                reason: "OK",
                headers: &[],
                body_len: Some(body.len()),
            },
        )
        .unwrap();
    let mut wire = Vec::new();
    if sends_body {
        assert!(
            connection
                .prepare_body_chunk(exchange_id, body.len())
                .unwrap()
        );
        let chunk = connection.body_chunk(exchange_id).unwrap();
        wire.extend_from_slice(chunk.header());
        wire.extend_from_slice(&body[..chunk.payload_len()]);
        wire.extend_from_slice(chunk.footer());
        connection.commit_body_chunk(exchange_id).unwrap();
    } else {
        let Observed::Write(head) = step(connection, &[]).unwrap() else {
            panic!("header-only response did not produce a write");
        };
        wire = head;
        connection.consume(connection.consumed()).unwrap();
    }
    assert_eq!(step(connection, &[]).unwrap(), Observed::Done);
    let persists = connection.begin_next_exchange(exchange_id).unwrap();
    (wire, persists)
}

fn request_decoder_events(
    decoder: &mut Http1ConnectionDecoder,
    scratch: &mut Http1HeaderScratch,
    input: &[u8],
) -> (Vec<u8>, usize) {
    enum DecoderObserved {
        Progress(usize),
        Body(Vec<u8>, usize),
        Complete,
        NeedInput,
        ProtocolSwitch,
    }

    let mut body = Vec::new();
    let mut offset = 0;
    for _ in 0..16 {
        let event = scratch.with_input(&input[offset..], |input, headers| {
            match decoder.next_event(input, headers).unwrap() {
                Http1ConnectionEvent::Head { consumed, .. }
                | Http1ConnectionEvent::Trailers { consumed, .. } => {
                    DecoderObserved::Progress(consumed)
                }
                Http1ConnectionEvent::Body { chunk, consumed } => {
                    DecoderObserved::Body(chunk.to_vec(), consumed)
                }
                Http1ConnectionEvent::Complete => DecoderObserved::Complete,
                Http1ConnectionEvent::NeedInput => DecoderObserved::NeedInput,
                Http1ConnectionEvent::ProtocolSwitch { .. } => DecoderObserved::ProtocolSwitch,
            }
        });
        match event {
            DecoderObserved::Progress(consumed) => offset += consumed,
            DecoderObserved::Body(chunk, consumed) => {
                body.extend_from_slice(&chunk);
                offset += consumed;
            }
            DecoderObserved::Complete => return (body, offset),
            DecoderObserved::NeedInput => panic!("complete message bytes were supplied"),
            DecoderObserved::ProtocolSwitch => panic!("request switched protocols"),
        }
    }
    panic!("decoder did not complete");
}

#[test]
fn http11_serves_sequential_requests_without_body_cross_contamination() {
    let requests = [
        (
            b"POST /one HTTP/1.1\r\nhost: example.test\r\ncontent-length: 3\r\n\r\none".as_slice(),
            b"/one".as_slice(),
            b"one".as_slice(),
        ),
        (
            b"POST /two HTTP/1.1\r\nhost: example.test\r\ncontent-length: 7\r\n\r\ntwo-two",
            b"/two",
            b"two-two",
        ),
        (
            b"POST /three HTTP/1.1\r\nhost: example.test\r\ncontent-length: 5\r\n\r\nthree",
            b"/three",
            b"three",
        ),
    ];
    let mut connection =
        ServerConnection::new_with_protocol(HttpProtocol::Http1, HttpLimits::new());

    for (request, expected_target, expected_body) in requests {
        let mut offset = 0;
        let (exchange_id, target, body, version) =
            receive_request(&mut connection, request, &mut offset);
        assert_eq!(offset, request.len());
        assert_eq!(target, expected_target);
        assert_eq!(body, expected_body);
        assert_eq!(version, HttpVersion::Http11);
        let (response, persists) = respond(&mut connection, exchange_id, &body);
        assert!(persists);
        assert!(response.ends_with(expected_body));
        assert!(
            !response
                .windows(19)
                .any(|bytes| bytes == b"connection: close\r\n")
        );
    }
}

#[test]
fn pipelined_requests_are_consumed_from_the_existing_input() {
    let input = b"POST /first HTTP/1.1\r\nhost: example.test\r\ncontent-length: 3\r\n\r\none\
GET /second HTTP/1.1\r\nhost: example.test\r\n\r\n";
    let mut connection =
        ServerConnection::new_with_protocol(HttpProtocol::Http1, HttpLimits::new());
    let mut offset = 0;

    let (first, target, body, _) = receive_request(&mut connection, input, &mut offset);
    assert_eq!(target, b"/first");
    assert_eq!(body, b"one");
    assert!(offset < input.len());
    assert!(respond(&mut connection, first, b"first").1);

    let (second, target, body, _) = receive_request(&mut connection, input, &mut offset);
    assert_eq!(target, b"/second");
    assert!(body.is_empty());
    assert_eq!(offset, input.len());
    assert!(respond(&mut connection, second, b"second").1);
}

#[test]
fn http11_close_option_in_a_token_list_closes_after_the_response() {
    let request =
        b"GET / HTTP/1.1\r\nhost: example.test\r\nconnection: Upgrade, CLOSE, keep-alive\r\n\r\n";
    let mut connection =
        ServerConnection::new_with_protocol(HttpProtocol::Http1, HttpLimits::new());
    let (exchange_id, _, _, _) = receive_request(&mut connection, request, &mut 0);
    let (response, persists) = respond(&mut connection, exchange_id, b"done");

    assert!(!persists);
    assert!(
        response
            .windows(b"connection: close\r\n".len())
            .any(|bytes| bytes == b"connection: close\r\n")
    );
}

#[test]
fn http10_closes_by_default_but_keep_alive_token_list_persists() {
    let mut closes = ServerConnection::new_with_protocol(HttpProtocol::Http1, HttpLimits::new());
    let request = b"GET /default HTTP/1.0\r\n\r\n";
    let (exchange_id, _, _, version) = receive_request(&mut closes, request, &mut 0);
    assert_eq!(version, HttpVersion::Http10);
    let (response, persists) = respond(&mut closes, exchange_id, b"");
    assert!(!persists);
    assert!(
        response
            .windows(b"connection: close\r\n".len())
            .any(|bytes| bytes == b"connection: close\r\n")
    );

    let mut persists = ServerConnection::new_with_protocol(HttpProtocol::Http1, HttpLimits::new());
    let request = b"GET /persistent HTTP/1.0\r\nconnection: , keep-alive, Upgrade,\r\n\r\n";
    let (exchange_id, _, _, _) = receive_request(&mut persists, request, &mut 0);
    let (response, reusable) = respond(&mut persists, exchange_id, b"");
    assert!(reusable);
    assert!(
        response
            .windows(b"connection: keep-alive\r\n".len())
            .any(|bytes| bytes == b"connection: keep-alive\r\n")
    );
}

#[test]
fn maximum_requests_per_connection_closes_at_the_configured_response() {
    let limits = HttpLimits::new().set_max_requests_per_connection(2);
    let mut connection = ServerConnection::new_with_protocol(HttpProtocol::Http1, limits);

    let mut offset = 0;
    let first_request = b"GET /one HTTP/1.1\r\nhost: example.test\r\n\r\n";
    let (first, _, _, _) = receive_request(&mut connection, first_request, &mut offset);
    let (first_response, persists) = respond(&mut connection, first, b"one");
    assert!(persists);
    assert!(
        !first_response
            .windows(b"connection: close\r\n".len())
            .any(|bytes| bytes == b"connection: close\r\n")
    );

    offset = 0;
    let second_request = b"GET /two HTTP/1.1\r\nhost: example.test\r\n\r\n";
    let (second, _, _, _) = receive_request(&mut connection, second_request, &mut offset);
    let (second_response, persists) = respond(&mut connection, second, b"two");
    assert!(!persists);
    assert!(
        second_response
            .windows(b"connection: close\r\n".len())
            .any(|bytes| bytes == b"connection: close\r\n")
    );
}

#[test]
fn shutdown_started_before_response_disables_reuse() {
    let request = b"GET / HTTP/1.1\r\nhost: example.test\r\n\r\n";
    let mut connection =
        ServerConnection::new_with_protocol(HttpProtocol::Http1, HttpLimits::new());
    let (exchange_id, _, _, _) = receive_request(&mut connection, request, &mut 0);
    assert!(!connection.begin_shutdown().unwrap());

    let (response, persists) = respond(&mut connection, exchange_id, b"done");
    assert!(!persists);
    assert!(
        response
            .windows(b"connection: close\r\n".len())
            .any(|bytes| bytes == b"connection: close\r\n")
    );
}

#[test]
fn incomplete_request_body_disables_reuse() {
    let mut connection =
        ServerConnection::new_with_protocol(HttpProtocol::Http1, HttpLimits::new());
    let input = b"POST / HTTP/1.1\r\nhost: example.test\r\ncontent-length: 4\r\n\r\nbody";
    let Observed::Head { exchange_id, .. } = step(&mut connection, input).unwrap() else {
        panic!("request head was not decoded");
    };
    connection.consume(connection.consumed()).unwrap();

    let (response, _) = {
        assert!(
            !connection
                .prepare_response(
                    exchange_id,
                    ConnectionResponse {
                        status: 200,
                        reason: "OK",
                        headers: &[],
                        body_len: Some(0),
                    },
                )
                .unwrap()
        );
        let Observed::Write(response) = step(&mut connection, &[]).unwrap() else {
            panic!("response head was not emitted");
        };
        connection.consume(0).unwrap();
        (response, ())
    };
    assert!(
        response
            .windows(b"connection: close\r\n".len())
            .any(|bytes| bytes == b"connection: close\r\n")
    );
    assert_eq!(
        connection.begin_next_exchange(exchange_id),
        Err(ServerError::InvalidOutboundState)
    );
}

#[test]
fn decoder_reset_discards_body_header_chunk_and_trailer_accounting() {
    let limits = HttpLimits::new().set_max_headers(3).set_max_body_bytes(4);
    let mut decoder = Http1ConnectionDecoder::request(limits);
    let mut scratch = Http1HeaderScratch::new(3);
    let first = b"POST /first HTTP/1.1\r\nhost: a\r\ntransfer-encoding: chunked\r\n\r\n\
4\r\nabcd\r\n0\r\nx-trailer: one\r\n\r\n";
    let (body, consumed) = request_decoder_events(&mut decoder, &mut scratch, first);
    assert_eq!(body, b"abcd");
    assert_eq!(consumed, first.len());

    decoder.begin_next_message().unwrap();
    let second = b"POST /second HTTP/1.1\r\nhost: b\r\ncontent-length: 4\r\n\r\nwxyz";
    let (body, consumed) = request_decoder_events(&mut decoder, &mut scratch, second);
    assert_eq!(body, b"wxyz");
    assert_eq!(consumed, second.len());
}
