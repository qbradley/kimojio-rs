use kimojio_fsm_http::{
    H2ByteClientEvent, H2ByteStreamEvent, H2Client, H2ErrorCode, H2Frame, H2FrameOutcome,
    H2FrameType, H2HeaderBlockEncoder, H2HeaderField, H2Limits, H2OutboundCommit, H2Server,
    H2Setting, H2SettingId, H2Settings,
};

fn settings_frame(settings: &[H2Setting]) -> H2Frame {
    let mut payload = Vec::new();
    H2Settings::encode_payload(settings, &mut payload);
    H2Frame {
        frame_type: H2FrameType::Settings,
        flags: 0,
        stream_id: 0,
        payload,
    }
}

fn limited_server() -> H2Server {
    H2Server::with_limits(H2Limits {
        max_active_streams: 1,
        ..H2Limits::default()
    })
    .unwrap()
}

fn initialize_server(server: &mut H2Server) {
    assert!(matches!(
        server.accept_frame_bytes_typed(settings_frame(&[])).0,
        H2FrameOutcome::Event(H2ByteStreamEvent::Settings { .. })
    ));
}

fn initialize_client(client: &mut H2Client) {
    assert!(matches!(
        client.accept_frame_bytes_typed(settings_frame(&[])).0,
        H2FrameOutcome::Event(H2ByteClientEvent::Settings { .. })
    ));
}

fn take_client_output(client: &mut H2Client, commit: H2OutboundCommit) -> Vec<u8> {
    let block = client.next_outbound_block().expect("queued client output");
    assert_eq!(block.commit(), commit);
    let output = block.bytes().to_vec();
    client.acknowledge_outbound_block(commit).unwrap();
    output
}

fn take_server_output(server: &mut H2Server, commit: H2OutboundCommit) -> Vec<u8> {
    let block = server.next_outbound_block().expect("queued server output");
    assert_eq!(block.commit(), commit);
    let output = block.bytes().to_vec();
    server.acknowledge_outbound_block(commit).unwrap();
    output
}

fn open_request(client: &mut H2Client, path: &str) -> (u32, Vec<u8>) {
    let (stream_id, commit) = client
        .open_stream_with_raw_headers("GET", "https", "example.test", path, &[], true)
        .unwrap();
    (stream_id, take_client_output(client, commit))
}

fn decode_single_frame(bytes: &[u8]) -> H2Frame {
    let (frame, consumed) = H2Frame::decode(bytes).unwrap();
    assert_eq!(consumed, bytes.len());
    frame
}

fn assert_refused_reset(bytes: &[u8], stream_id: u32) -> H2Frame {
    let reset = decode_single_frame(bytes);
    assert_eq!(reset.frame_type, H2FrameType::RstStream);
    assert_eq!(reset.stream_id, stream_id);
    assert_eq!(
        reset.payload,
        H2ErrorCode::RefusedStream.as_u32().to_be_bytes()
    );
    reset
}

fn request_fields(extra: H2HeaderField) -> Vec<H2HeaderField> {
    vec![
        H2HeaderField::new(b":method", b"GET"),
        H2HeaderField::new(b":scheme", b"https"),
        H2HeaderField::new(b":authority", b"example.test"),
        H2HeaderField::new(b":path", b"/"),
        extra,
    ]
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

#[test]
fn over_limit_stream_is_refused_without_harming_connection_or_sibling() {
    let mut client = H2Client::default();
    let mut server = limited_server();
    let (_, _, preface_output) = server.accept_event(&client.connection_preface()).unwrap();
    let (settings, consumed) = H2Frame::decode(&preface_output).unwrap();
    assert_eq!(settings.frame_type, H2FrameType::Settings);
    assert!(consumed < preface_output.len());
    assert!(
        H2Settings::decode_payload(&settings.payload)
            .unwrap()
            .contains(&H2Setting::new(H2SettingId::MaxConcurrentStreams, 1))
    );
    initialize_client(&mut client);

    let (first_stream, first_headers) = open_request(&mut client, "/first");
    assert!(matches!(
        server.accept_event_bytes(&first_headers).unwrap().0,
        Some(H2ByteStreamEvent::RequestHeaders { stream_id, .. })
            if stream_id == first_stream
    ));
    let (refused_stream, refused_headers) = open_request(&mut client, "/refused");
    let (event, consumed, reset_bytes) = server.accept_event_bytes(&refused_headers).unwrap();
    assert_eq!(event, None);
    assert_eq!(consumed, refused_headers.len());
    let reset = assert_refused_reset(&reset_bytes, refused_stream);
    assert_eq!(server.highest_processed_stream_id(), refused_stream);

    assert!(matches!(
        client.accept_frame_bytes(reset).unwrap().0,
        H2ByteClientEvent::Reset {
            stream_id,
            error_code
        } if stream_id == refused_stream
            && error_code == H2ErrorCode::RefusedStream.as_u32()
    ));
    let response = server
        .response_headers_frame_with_raw_headers(first_stream, 200, &[], true)
        .unwrap();
    let response = take_server_output(&mut server, response);
    assert!(matches!(
        client
            .accept_frame_bytes(decode_single_frame(&response))
            .unwrap()
            .0,
        H2ByteClientEvent::ResponseHeaders {
            stream_id,
            end_stream: true,
            ..
        } if stream_id == first_stream
    ));
}

#[test]
fn stream_slot_reopens_after_active_stream_closes() {
    let mut client = H2Client::default();
    initialize_client(&mut client);
    let mut server = limited_server();
    initialize_server(&mut server);

    let (first_stream, first_headers) = open_request(&mut client, "/first");
    assert!(
        server
            .accept_frame_bytes(decode_single_frame(&first_headers))
            .unwrap()
            .0
            .is_some()
    );
    let (refused_stream, refused_headers) = open_request(&mut client, "/refused");
    let (event, reset) = server
        .accept_frame_bytes(decode_single_frame(&refused_headers))
        .unwrap();
    assert_eq!(event, None);
    assert_refused_reset(&reset, refused_stream);

    server.finish_response_stream(first_stream);
    let (next_stream, next_headers) = open_request(&mut client, "/next");
    assert!(matches!(
        server
            .accept_frame_bytes(decode_single_frame(&next_headers))
            .unwrap()
            .0,
        Some(H2ByteStreamEvent::RequestHeaders { stream_id, .. })
            if stream_id == next_stream
    ));
}

#[test]
fn refused_headers_advance_connection_hpack_history() {
    let mut server = limited_server();
    initialize_server(&mut server);
    let mut encoder = H2HeaderBlockEncoder::new();

    let first = request_fields(H2HeaderField::new(b"x-first", b"value"));
    assert!(matches!(
        server
            .accept_frame_bytes_typed(headers_frame(&mut encoder, 1, &first))
            .0,
        H2FrameOutcome::Event(H2ByteStreamEvent::RequestHeaders { stream_id: 1, .. })
    ));

    let refused = H2HeaderField::new(b"x-refused", b"dynamic-value");
    let (outcome, reset) = server.accept_frame_bytes_typed(headers_frame(
        &mut encoder,
        3,
        &request_fields(refused.clone()),
    ));
    assert_eq!(outcome, H2FrameOutcome::Ignored);
    assert_refused_reset(&reset, 3);

    server.finish_response_stream(1);
    let subsequent = server
        .accept_frame_bytes_typed(headers_frame(
            &mut encoder,
            5,
            &request_fields(refused.clone()),
        ))
        .0;
    let H2FrameOutcome::Event(H2ByteStreamEvent::RequestHeaders { headers, .. }) = subsequent
    else {
        panic!("header block after refusal must decode with shared HPACK history")
    };
    assert!(headers.contains(&refused));
}
