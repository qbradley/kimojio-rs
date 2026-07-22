use kimojio_fsm_http::{
    ConnectionResponse, ExchangeId, H2ByteStreamEvent, H2Client, H2ErrorCode, H2ErrorScope,
    H2Frame, H2FrameOutcome, H2FrameType, H2HeaderBlockDecoder, H2HeaderBlockEncoder,
    H2HeaderField, H2OutboundCommit, H2Server, H2Setting, H2Settings, HttpLimits, HttpProtocol,
    ServerConnection, ServerError, ServerEvent, Step,
};

fn settings_frame() -> H2Frame {
    let mut payload = Vec::new();
    H2Settings::encode_payload(&[] as &[H2Setting], &mut payload);
    H2Frame {
        frame_type: H2FrameType::Settings,
        flags: 0,
        stream_id: 0,
        payload,
    }
}

fn initialize_server(server: &mut H2Server) {
    assert!(matches!(
        server.accept_frame_bytes_typed(settings_frame()).0,
        H2FrameOutcome::Event(H2ByteStreamEvent::Settings { .. })
    ));
}

fn request_fields(extra: &[H2HeaderField]) -> Vec<H2HeaderField> {
    let mut fields = vec![
        H2HeaderField::new(b":method", b"GET"),
        H2HeaderField::new(b":scheme", b"https"),
        H2HeaderField::new(b":authority", b"example.test"),
        H2HeaderField::new(b":path", b"/"),
    ];
    fields.extend_from_slice(extra);
    fields
}

fn headers_frame(
    encoder: &mut H2HeaderBlockEncoder,
    stream_id: u32,
    fields: &[H2HeaderField],
) -> H2Frame {
    H2Frame {
        frame_type: H2FrameType::Headers,
        flags: 0x5,
        stream_id,
        payload: encoder.try_encode_fields(fields).unwrap(),
    }
}

fn decode_reset(bytes: &[u8], stream_id: u32, error_code: H2ErrorCode) {
    let (reset, consumed) = H2Frame::decode(bytes).unwrap();
    assert_eq!(consumed, bytes.len());
    assert_eq!(reset.frame_type, H2FrameType::RstStream);
    assert_eq!(reset.stream_id, stream_id);
    assert_eq!(reset.payload, error_code.as_u32().to_be_bytes());
}

fn take_server_output(server: &mut H2Server, commit: H2OutboundCommit) -> Vec<u8> {
    let block = server.next_outbound_block().expect("queued response");
    assert_eq!(block.commit(), commit);
    let bytes = block.bytes().to_vec();
    server.acknowledge_outbound_block(commit).unwrap();
    bytes
}

#[test]
fn malformed_stream_is_reset_while_sibling_and_hpack_history_survive() {
    let mut server = H2Server::default();
    initialize_server(&mut server);
    let mut encoder = H2HeaderBlockEncoder::new();

    assert!(matches!(
        server
            .accept_frame_bytes(headers_frame(&mut encoder, 1, &request_fields(&[])))
            .unwrap()
            .0,
        Some(H2ByteStreamEvent::RequestHeaders { stream_id: 1, .. })
    ));

    let synchronized = H2HeaderField::new(b"x-synchronized", b"dynamic-value");
    let malformed = request_fields(&[
        synchronized.clone(),
        H2HeaderField::new(b"content-length", b"invalid"),
    ]);
    let (event, reset) = server
        .accept_frame_bytes(headers_frame(&mut encoder, 3, &malformed))
        .expect("a malformed request is a stream error");
    assert_eq!(event, None);
    decode_reset(&reset, 3, H2ErrorCode::ProtocolError);

    let response = server
        .response_headers_frame_with_raw_headers(1, 200, &[], true)
        .unwrap();
    let response = take_server_output(&mut server, response);
    let (response, consumed) = H2Frame::decode(&response).unwrap();
    assert_eq!(consumed, 9 + response.payload.len());
    assert_eq!(response.frame_type, H2FrameType::Headers);
    assert_eq!(response.stream_id, 1);
    let fields = H2HeaderBlockDecoder::new()
        .try_decode_with_limit(&response.payload, usize::MAX)
        .unwrap();
    assert!(
        fields
            .iter()
            .any(|field| field.name == b":status" && field.value == b"200")
    );

    assert!(matches!(
        server
            .accept_frame_bytes(headers_frame(
                &mut encoder,
                5,
                &request_fields(&[synchronized]),
            ))
            .unwrap()
            .0,
        Some(H2ByteStreamEvent::RequestHeaders { stream_id: 5, .. })
    ));
}

#[derive(Debug)]
enum DriverObserved {
    NeedInput,
    Write(Vec<u8>),
    Head(ExchangeId),
    Complete(ExchangeId),
}

fn driver_step(
    connection: &mut ServerConnection,
    input: &[u8],
) -> Result<DriverObserved, ServerError> {
    connection.step(input, |step| match step {
        Step::NeedInput => DriverObserved::NeedInput,
        Step::Write(bytes) => DriverObserved::Write(bytes.to_vec()),
        Step::Event(ServerEvent::RequestHead { exchange_id, .. }) => {
            DriverObserved::Head(exchange_id)
        }
        Step::Event(ServerEvent::RequestComplete { exchange_id }) => {
            DriverObserved::Complete(exchange_id)
        }
        _ => panic!("unexpected driver step"),
    })
}

fn acknowledge_driver_step(connection: &mut ServerConnection) {
    let consumed = connection.consumed();
    connection.consume(consumed).unwrap();
}

fn take_client_output(client: &mut H2Client, commit: H2OutboundCommit) -> Vec<u8> {
    let block = client.next_outbound_block().expect("queued request");
    assert_eq!(block.commit(), commit);
    let output = block.bytes().to_vec();
    client.acknowledge_outbound_block(commit).unwrap();
    output
}

fn open_driver_request(
    client: &mut H2Client,
    method: &str,
    path: &str,
    end_stream: bool,
) -> (u32, Vec<u8>) {
    let (stream_id, commit) = client
        .open_stream_with_raw_headers(method, "http", "example.test", path, &[], end_stream)
        .unwrap();
    (stream_id, take_client_output(client, commit))
}

#[test]
fn oversized_body_resets_only_its_driver_exchange() {
    let limits = HttpLimits::new().set_max_body_bytes(3);
    let mut connection = ServerConnection::new_with_protocol(HttpProtocol::Http2, limits);
    let mut client = H2Client::default();

    assert!(matches!(
        driver_step(&mut connection, &client.connection_preface()).unwrap(),
        DriverObserved::Write(_)
    ));
    acknowledge_driver_step(&mut connection);

    let (faulted_stream, request) = open_driver_request(&mut client, "POST", "/faulted", false);
    let DriverObserved::Head(faulted_exchange) = driver_step(&mut connection, &request).unwrap()
    else {
        panic!("faulted request head must be accepted before its oversized body");
    };
    acknowledge_driver_step(&mut connection);

    let (sibling_stream, request) = open_driver_request(&mut client, "GET", "/sibling", true);
    let DriverObserved::Head(sibling_exchange) = driver_step(&mut connection, &request).unwrap()
    else {
        panic!("sibling request head must be accepted");
    };
    acknowledge_driver_step(&mut connection);
    let DriverObserved::Complete(completed) = driver_step(&mut connection, &[]).unwrap() else {
        panic!("sibling request must complete");
    };
    assert_eq!(completed, sibling_exchange);
    acknowledge_driver_step(&mut connection);

    let oversized = client.data_frame(faulted_stream, b"four", true);
    let DriverObserved::Write(reset) = driver_step(&mut connection, &oversized).unwrap() else {
        panic!("oversized request body must produce RST_STREAM");
    };
    decode_reset(&reset, faulted_stream, H2ErrorCode::ProtocolError);
    acknowledge_driver_step(&mut connection);

    let handled = connection
        .take_handled_stream_error()
        .expect("driver must expose the locally reset exchange");
    assert_eq!(handled.exchange_id(), faulted_exchange);
    assert_eq!(
        handled.error(),
        &ServerError::BodyTooLarge {
            limit: 3,
            actual: 4
        }
    );
    assert_eq!(
        handled.protocol_error().scope(),
        H2ErrorScope::Stream(faulted_stream)
    );
    assert_eq!(connection.take_handled_stream_error(), None);

    assert_eq!(
        connection.prepare_response(
            faulted_exchange,
            ConnectionResponse {
                status: 200,
                reason: "OK",
                headers: &[],
                body_len: Some(0),
            },
        ),
        Err(ServerError::InvalidOutboundState)
    );
    assert!(
        !connection
            .prepare_response(
                sibling_exchange,
                ConnectionResponse {
                    status: 200,
                    reason: "OK",
                    headers: &[],
                    body_len: Some(0),
                },
            )
            .unwrap()
    );
    let DriverObserved::Write(response) = driver_step(&mut connection, &[]).unwrap() else {
        panic!("healthy sibling must receive its response");
    };
    let (response, consumed) = H2Frame::decode(&response).unwrap();
    assert_eq!(consumed, 9 + response.payload.len());
    assert_eq!(response.frame_type, H2FrameType::Headers);
    assert_eq!(response.stream_id, sibling_stream);
}

#[test]
fn short_content_length_resets_only_its_driver_exchange() {
    let mut connection =
        ServerConnection::new_with_protocol(HttpProtocol::Http2, HttpLimits::new());
    let mut client = H2Client::default();

    assert!(matches!(
        driver_step(&mut connection, &client.connection_preface()).unwrap(),
        DriverObserved::Write(_)
    ));
    acknowledge_driver_step(&mut connection);

    let (faulted_stream, commit) = client
        .open_stream_with_raw_headers(
            "POST",
            "http",
            "example.test",
            "/faulted",
            &[H2HeaderField::new(b"content-length", b"4")],
            false,
        )
        .unwrap();
    let request = take_client_output(&mut client, commit);
    let DriverObserved::Head(faulted_exchange) = driver_step(&mut connection, &request).unwrap()
    else {
        panic!("faulted request head must be accepted before its short body");
    };
    acknowledge_driver_step(&mut connection);

    let (sibling_stream, request) = open_driver_request(&mut client, "GET", "/sibling", true);
    let DriverObserved::Head(sibling_exchange) = driver_step(&mut connection, &request).unwrap()
    else {
        panic!("sibling request head must be accepted");
    };
    acknowledge_driver_step(&mut connection);
    let DriverObserved::Complete(completed) = driver_step(&mut connection, &[]).unwrap() else {
        panic!("sibling request must complete");
    };
    assert_eq!(completed, sibling_exchange);
    acknowledge_driver_step(&mut connection);

    let short = client.data_frame(faulted_stream, b"abc", true);
    let DriverObserved::Write(reset) = driver_step(&mut connection, &short).unwrap() else {
        panic!("short request body must produce RST_STREAM");
    };
    decode_reset(&reset, faulted_stream, H2ErrorCode::ProtocolError);
    acknowledge_driver_step(&mut connection);

    let handled = connection
        .take_handled_stream_error()
        .expect("driver must expose the invalid content length");
    assert_eq!(handled.exchange_id(), faulted_exchange);
    assert_eq!(handled.error(), &ServerError::InvalidContentLength);
    assert_eq!(
        handled.protocol_error().scope(),
        H2ErrorScope::Stream(faulted_stream)
    );

    assert!(
        !connection
            .prepare_response(
                sibling_exchange,
                ConnectionResponse {
                    status: 200,
                    reason: "OK",
                    headers: &[],
                    body_len: Some(0),
                },
            )
            .unwrap()
    );
    let DriverObserved::Write(response) = driver_step(&mut connection, &[]).unwrap() else {
        panic!("healthy sibling must receive its response");
    };
    let (response, consumed) = H2Frame::decode(&response).unwrap();
    assert_eq!(consumed, 9 + response.payload.len());
    assert_eq!(response.frame_type, H2FrameType::Headers);
    assert_eq!(response.stream_id, sibling_stream);
}

#[test]
fn hpack_decode_failure_remains_connection_fatal() {
    let mut server = H2Server::default();
    initialize_server(&mut server);

    assert_eq!(
        server.accept_frame_bytes(H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0x5,
            stream_id: 1,
            payload: vec![0x80],
        }),
        Err(ServerError::InvalidHpack)
    );

    let outcome = server
        .accept_frame_bytes_typed(headers_frame(
            &mut H2HeaderBlockEncoder::new(),
            3,
            &request_fields(&[]),
        ))
        .0;
    assert!(matches!(
        outcome,
        H2FrameOutcome::Error(error)
            if error.scope() == H2ErrorScope::Connection
                && error.error_code() == H2ErrorCode::CompressionError
    ));
}

#[test]
fn malformed_frame_length_remains_connection_fatal() {
    let mut server = H2Server::default();
    initialize_server(&mut server);

    let (outcome, output) = server.accept_frame_bytes_typed(H2Frame {
        frame_type: H2FrameType::RstStream,
        flags: 0,
        stream_id: 1,
        payload: vec![0; 3],
    });
    assert!(output.is_empty());
    assert!(matches!(
        outcome,
        H2FrameOutcome::Error(error)
            if error.scope() == H2ErrorScope::Connection
                && error.error_code() == H2ErrorCode::FrameSizeError
    ));
}
