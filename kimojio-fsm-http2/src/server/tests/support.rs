//! Shared HTTP/2 test fixtures and frame builders.

use crate::Header;
use crate::server::*;

pub(crate) fn encoded_frame(
    frame_type: H2FrameType,
    flags: u8,
    stream_id: u32,
    payload: Vec<u8>,
) -> Vec<u8> {
    let mut bytes = Vec::new();
    H2Frame {
        frame_type,
        flags,
        stream_id,
        payload,
    }
    .encode(&mut bytes);
    bytes
}

/// RFC 9113 section 8.4: only servers promise streams, so a client-sent
/// PUSH_PROMISE is a connection error of type PROTOCOL_ERROR. It must not
/// be mistaken for an unknown extension frame, which would be ignored.
pub(crate) fn take_server_output(server: &mut H2Server, commit: H2OutboundCommit) -> Vec<u8> {
    let block = server.next_outbound_block().unwrap();
    assert_eq!(block.commit(), commit);
    let bytes = block.bytes().to_vec();
    server.acknowledge_outbound_block(commit).unwrap();
    bytes
}

pub(crate) fn take_client_output(client: &mut H2Client, commit: H2OutboundCommit) -> Vec<u8> {
    let block = client.next_outbound_block().unwrap();
    assert_eq!(block.commit(), commit);
    let bytes = block.bytes().to_vec();
    client.acknowledge_outbound_block(commit).unwrap();
    bytes
}

pub(crate) fn open_stream_bytes(
    client: &mut H2Client,
    method: &str,
    scheme: &str,
    authority: &str,
    path: &str,
    headers: &[Header<'_>],
    end_stream: bool,
) -> Result<(u32, Vec<u8>), ServerError> {
    let fields = headers
        .iter()
        .map(|header| H2HeaderField {
            name: header.name.as_bytes().to_vec(),
            value: header.value.as_bytes().to_vec(),
            sensitive: false,
        })
        .collect::<Vec<_>>();
    let (stream_id, commit) = client
        .open_stream_with_raw_headers(method, scheme, authority, path, &fields, end_stream)
        .map_err(ServerError::from)?;
    Ok((stream_id, take_client_output(client, commit)))
}

pub(crate) fn response_headers_bytes(
    server: &mut H2Server,
    stream_id: u32,
    status: u16,
    headers: &[Header<'_>],
    end_stream: bool,
) -> Vec<u8> {
    let fields = headers
        .iter()
        .map(|header| H2HeaderField {
            name: header.name.as_bytes().to_vec(),
            value: header.value.as_bytes().to_vec(),
            sensitive: false,
        })
        .collect::<Vec<_>>();
    let commit = server
        .response_headers_frame_with_raw_headers(stream_id, status, &fields, end_stream)
        .unwrap();
    take_server_output(server, commit)
}

pub(crate) fn response_frames_bytes(
    server: &mut H2Server,
    stream_id: u32,
    status: u16,
    headers: &[Header<'_>],
    body: &[u8],
    end_stream: bool,
) -> Vec<u8> {
    let fields = headers
        .iter()
        .map(|header| H2HeaderField {
            name: header.name.as_bytes().to_vec(),
            value: header.value.as_bytes().to_vec(),
            sensitive: false,
        })
        .collect::<Vec<_>>();
    let commit = server
        .response_frames_with_raw_headers(stream_id, status, &fields, body, end_stream)
        .unwrap();
    take_server_output(server, commit)
}

pub(crate) fn trailers_bytes(
    client: &mut H2Client,
    stream_id: u32,
    headers: &[Header<'_>],
) -> Vec<u8> {
    let fields = headers
        .iter()
        .map(|header| H2HeaderField {
            name: header.name.as_bytes().to_vec(),
            value: header.value.as_bytes().to_vec(),
            sensitive: false,
        })
        .collect::<Vec<_>>();
    let commit = client
        .trailers_frame_with_raw_headers(stream_id, &fields)
        .unwrap();
    take_client_output(client, commit)
}

pub(crate) fn client_response_headers(stream_id: u32, status: u16, end_stream: bool) -> Vec<u8> {
    let status = status.to_string();
    let mut output = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Headers,
        flags: if end_stream { 0x5 } else { 0x4 },
        stream_id,
        payload: encode_hpack_raw_header_block(&[H2RawHeader::new(":status", status)]),
    }
    .encode(&mut output);
    output
}

pub(crate) fn client_data_bytes(stream_id: u32, payload: &[u8], end_stream: bool) -> Vec<u8> {
    let mut output = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Data,
        flags: if end_stream { 0x1 } else { 0 },
        stream_id,
        payload: payload.to_vec(),
    }
    .encode(&mut output);
    output
}

pub(crate) fn decode_single_frame(bytes: &[u8]) -> H2Frame {
    let (frame, consumed) = H2Frame::decode(bytes).unwrap();
    assert_eq!(consumed, bytes.len());
    frame
}

pub(crate) fn assert_pending_header_eq(left: &H2Server, right: &H2Server) {
    match (
        &left.endpoint.header_block.pending,
        &right.endpoint.header_block.pending,
    ) {
        (None, None) => {}
        (Some(left_pending), Some(right_pending)) => {
            assert_eq!(left_pending.stream_id, right_pending.stream_id);
            assert_eq!(left_pending.flags, right_pending.flags);
            assert_eq!(left_pending.block, right_pending.block);
            assert_eq!(left_pending.encoded_len, right_pending.encoded_len);
            assert_eq!(
                left_pending.continuation_frames,
                right_pending.continuation_frames
            );
            assert_eq!(left_pending.self_dependency, right_pending.self_dependency);
        }
        _ => panic!("pending header-block state diverged"),
    }
}

pub(crate) fn assert_server_protocol_eq(left: &H2Server, right: &H2Server) {
    assert_eq!(left.control_diagnostics(), right.control_diagnostics());
    assert_eq!(left.settings(), right.settings());
    assert_eq!(left.local_settings_state(), right.local_settings_state());
    assert_eq!(
        left.endpoint.outbound_shutdown,
        right.endpoint.outbound_shutdown
    );
    assert_eq!(
        left.endpoint.received_goaway_last_stream_id,
        right.endpoint.received_goaway_last_stream_id
    );
    assert_eq!(left.max_peer_stream_id, right.max_peer_stream_id);
    assert_eq!(
        left.max_processed_peer_stream_id,
        right.max_processed_peer_stream_id
    );
    assert_eq!(left.endpoint.settings_seen, right.endpoint.settings_seen);
    assert_eq!(
        left.endpoint.last_protocol_error,
        right.endpoint.last_protocol_error
    );
    assert_eq!(
        left.endpoint.terminal_protocol_error,
        right.endpoint.terminal_protocol_error
    );
    assert_eq!(
        left.endpoint.send_connection_window.available(),
        right.endpoint.send_connection_window.available()
    );
    assert_eq!(left.endpoint.streams, right.endpoint.streams);
    assert_eq!(left.endpoint.tombstones, right.endpoint.tombstones);
    assert_pending_header_eq(left, right);
}

pub(crate) fn assert_single_control_frame(output: &[u8], frame_type: H2FrameType, flags: u8) {
    let (frame, consumed) = H2Frame::decode(output).unwrap();
    assert_eq!(consumed, output.len());
    assert_eq!(frame.frame_type, frame_type);
    assert_eq!(frame.flags, flags);
    assert_eq!(frame.stream_id, 0);
}

type DispatchPair = (
    H2FrameOutcome<H2ByteStreamEvent>,
    H2FrameOutcome<H2ByteStreamEvent>,
    Vec<u8>,
);

pub(crate) fn dispatch_pair(
    accept: &mut H2Server,
    classify: &mut H2Server,
    frame: H2Frame,
) -> DispatchPair {
    let (accept_outcome, accept_output) = accept.accept_frame_bytes_ref_typed(frame.as_ref());
    let classify_outcome = classify.classify_frame_bytes_ref_typed(frame.as_ref());
    (
        accept_outcome.map_event(H2ByteStreamEventRef::into_owned),
        classify_outcome.map_event(H2ByteStreamEventRef::into_owned),
        accept_output,
    )
}

pub(crate) fn decode_h2_frames(mut input: &[u8]) -> Vec<H2Frame> {
    let mut frames = Vec::new();
    while !input.is_empty() {
        let (frame, consumed) = H2Frame::decode(input).unwrap();
        assert_ne!(consumed, 0);
        frames.push(frame);
        input = &input[consumed..];
    }
    frames
}

pub(crate) fn assert_goaway(frame: &H2Frame, last_stream_id: u32, error_code: H2ErrorCode) {
    assert_eq!(frame.frame_type, H2FrameType::Goaway);
    assert_eq!(frame.flags, 0);
    assert_eq!(frame.stream_id, 0);
    assert_eq!(frame.payload.len(), 8);
    assert_eq!(
        u32::from_be_bytes(frame.payload[..4].try_into().unwrap()) & 0x7fff_ffff,
        last_stream_id
    );
    assert_eq!(
        u32::from_be_bytes(frame.payload[4..].try_into().unwrap()),
        error_code.as_u32()
    );
}

pub(crate) fn hex_bytes(input: &str) -> Vec<u8> {
    let input: String = input.chars().filter(|c| !c.is_whitespace()).collect();
    assert_eq!(input.len() % 2, 0);
    input
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|digits| {
            let high = hex_value(digits[0]);
            let low = hex_value(digits[1]);
            (high << 4) | low
        })
        .collect()
}

pub(crate) fn hex_value(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => panic!("invalid hex digit"),
    }
}

pub(crate) fn h2_server_after_preface() -> H2Server {
    let mut client = H2Client::default();
    let mut server = H2Server::default();
    let preface = client.connection_preface();
    server.accept_event(&preface).unwrap();
    server
}

pub(crate) fn h2_client_after_server_settings() -> H2Client {
    let mut client = H2Client::default();
    client.accept(&h2_settings_frame(&[])).unwrap();
    client
}

pub(crate) fn h2_ping_frame() -> Vec<u8> {
    let mut output = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Ping,
        flags: 0,
        stream_id: 0,
        payload: [0; 8].to_vec(),
    }
    .encode(&mut output);
    output
}

pub(crate) fn h2_ping_ack_frame(payload: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Ping,
        flags: 0x1,
        stream_id: 0,
        payload: payload.to_vec(),
    }
    .encode(&mut output);
    output
}

pub(crate) fn h2_settings_frame(settings: &[H2Setting]) -> Vec<u8> {
    let mut payload = Vec::new();
    H2Settings::encode_payload(settings, &mut payload);
    let mut output = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Settings,
        flags: 0,
        stream_id: 0,
        payload,
    }
    .encode(&mut output);
    output
}

pub(crate) fn h2_settings_ack_frame() -> Vec<u8> {
    let mut output = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Settings,
        flags: 0x1,
        stream_id: 0,
        payload: Vec::new(),
    }
    .encode(&mut output);
    output
}

pub(crate) fn h2_window_update_frame(stream_id: u32, increment: u32) -> Vec<u8> {
    let mut output = Vec::new();
    H2Frame {
        frame_type: H2FrameType::WindowUpdate,
        flags: 0,
        stream_id,
        payload: increment.to_be_bytes().to_vec(),
    }
    .encode(&mut output);
    output
}

pub(crate) fn h2_reset_frame(stream_id: u32) -> Vec<u8> {
    let mut output = Vec::new();
    H2Frame {
        frame_type: H2FrameType::RstStream,
        flags: 0,
        stream_id,
        payload: 8_u32.to_be_bytes().to_vec(),
    }
    .encode(&mut output);
    output
}

pub(crate) fn h2_discarded_data_inputs(stream_id: u32) -> [(Vec<u8>, usize); 4] {
    let encode = |flags, payload: Vec<u8>| {
        let flow_control_len = payload.len();
        let mut output = Vec::new();
        H2Frame {
            frame_type: H2FrameType::Data,
            flags,
            stream_id,
            payload,
        }
        .encode(&mut output);
        (output, flow_control_len)
    };
    [
        encode(0, Vec::new()),
        encode(0, b"data".to_vec()),
        encode(0x8, vec![0]),
        encode(0x8, vec![2, b'x', 0, 0]),
    ]
}

pub(crate) fn h2_goaway_frame(last_stream_id: u32) -> Vec<u8> {
    let mut output = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Goaway,
        flags: 0,
        stream_id: 0,
        payload: [last_stream_id.to_be_bytes(), 0_u32.to_be_bytes()].concat(),
    }
    .encode(&mut output);
    output
}

pub(crate) fn h2_response_headers_frame(stream_id: u32) -> Vec<u8> {
    let mut server = H2Server::default();
    response_headers_bytes(&mut server, stream_id, 200, &[], false)
}

pub(crate) fn h2_priority_frame(stream_id: u32) -> Vec<u8> {
    let mut output = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Priority,
        flags: 0,
        stream_id,
        payload: [0; 5].to_vec(),
    }
    .encode(&mut output);
    output
}

pub(crate) fn hpack_literal_with_indexing(name: &str, value: &str) -> Vec<u8> {
    let mut block = Vec::new();
    block.push(0x40);
    hpack_push_string(&mut block, name.as_bytes());
    hpack_push_string(&mut block, value.as_bytes());
    block
}
