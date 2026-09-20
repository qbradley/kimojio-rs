//! HTTP/2 frame codec tests.

use super::*;
use crate::server::*;

#[test]
fn h2_frame_type_five_decodes_as_push_promise() {
    assert_eq!(H2FrameType::from_raw(5), H2FrameType::PushPromise);
    assert_eq!(H2FrameType::from_u8(5), Ok(H2FrameType::PushPromise));
    assert_eq!(H2FrameType::PushPromise.as_u8(), 5);
}

/// A stream the peer reset gets no reset-tolerance window. Frames that
/// arrive afterwards cannot be in flight, so they are protocol violations
/// rather than a benign race.
#[test]
fn h2_settings_decode_apply_and_encode_stack_compatible_fields() {
    let settings = [
        H2Setting::new(H2SettingId::EnablePush, 0),
        H2Setting::new(H2SettingId::InitialWindowSize, 1024),
        H2Setting::new(H2SettingId::MaxFrameSize, 32 * 1024),
        H2Setting::new(H2SettingId::MaxHeaderListSize, 65_536),
    ];
    let mut payload = Vec::new();
    H2Settings::encode_payload(&settings, &mut payload);
    let decoded = H2Settings::decode_payload(&payload).unwrap();

    let mut applied = H2Settings::default();
    applied.apply_all(&decoded).unwrap();
    assert!(!applied.enable_push);
    assert_eq!(applied.initial_window_size, 1024);
    assert_eq!(applied.max_frame_size, 32 * 1024);
    assert_eq!(applied.max_header_list_size, 65_536);
    assert_eq!(H2FrameType::from_u8(0), Ok(H2FrameType::Data));
    assert_eq!(H2FrameType::from_u8(0xff), Err(ServerError::InvalidFrame));
    assert_eq!(H2FrameType::from_raw(0xff), H2FrameType::Unknown(0xff));
}

#[test]
fn h2_decode_preserves_unknown_extension_frames() {
    let mut frame = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Unknown(0x21),
        flags: 0,
        stream_id: 0,
        payload: b"ext".to_vec(),
    }
    .encode(&mut frame);

    let (decoded, consumed) = H2Frame::decode(&frame).unwrap();

    assert_eq!(consumed, frame.len());
    assert_eq!(decoded.frame_type, H2FrameType::Unknown(0x21));
    assert_eq!(decoded.payload, b"ext");
}

#[test]
fn h2_borrowed_decode_reuses_the_input_payload() {
    let mut input = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Unknown(0x21),
        flags: 0x5,
        stream_id: 7,
        payload: b"borrowed-payload".to_vec(),
    }
    .encode(&mut input);

    let (frame, consumed) = H2FrameRef::decode(&input).unwrap();

    assert_eq!(consumed, input.len());
    assert_eq!(frame.frame_type, H2FrameType::Unknown(0x21));
    assert_eq!(frame.flags, 0x5);
    assert_eq!(frame.stream_id, 7);
    assert_eq!(frame.payload, b"borrowed-payload");
    assert_eq!(frame.payload.as_ptr(), input[9..].as_ptr());

    let owned = frame.to_owned();
    assert_eq!(owned.payload, frame.payload);
    assert_ne!(owned.payload.as_ptr(), frame.payload.as_ptr());
    assert_eq!(
        H2FrameRef::decode(&input[..input.len() - 1]),
        Err(ServerError::NeedMore)
    );
}

#[test]
fn h2_decode_with_max_frame_size_rejects_oversized_payload() {
    let mut frame = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Ping,
        flags: 0,
        stream_id: 0,
        payload: [0; 8].to_vec(),
    }
    .encode(&mut frame);

    assert_eq!(
        H2Frame::decode_with_max_frame_size(&frame, 7),
        Err(ServerError::InvalidFrame)
    );
    assert_eq!(
        H2Frame::decode_outcome_with_max_frame_size(&frame, 7),
        H2DecodeOutcome::Error(H2ProtocolError::connection(
            H2ErrorCode::FrameSizeError,
            "HTTP/2 frame exceeds configured maximum frame size",
        ))
    );
    assert_eq!(
        H2FrameRef::decode_outcome_with_max_frame_size(&frame, 7),
        H2FrameRefDecodeOutcome::Error(H2ProtocolError::connection(
            H2ErrorCode::FrameSizeError,
            "HTTP/2 frame exceeds configured maximum frame size",
        ))
    );
}

#[test]
fn h2_settings_payload_limit_rejects_too_many_entries() {
    let mut payload = Vec::new();
    H2Settings::encode_payload(
        &[
            H2Setting::new(H2SettingId::InitialWindowSize, 1024),
            H2Setting::new(H2SettingId::MaxHeaderListSize, 2048),
        ],
        &mut payload,
    );

    assert_eq!(
        H2Settings::decode_payload_with_limit(&payload, 1),
        Err(ServerError::InvalidFrame)
    );
}

#[test]
fn corpus_frame_inputs_cover_unknown_incomplete_and_invalid_control_frames() {
    assert_eq!(H2Frame::decode(&[0, 0, 1]), Err(ServerError::NeedMore));

    let mut unknown = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Unknown(0x21),
        flags: 0,
        stream_id: 0,
        payload: b"abc".to_vec(),
    }
    .encode(&mut unknown);
    let (frame, consumed) = H2Frame::decode(&unknown).unwrap();
    assert_eq!(consumed, unknown.len());
    assert_eq!(frame.frame_type, H2FrameType::Unknown(0x21));

    let mut invalid_settings = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Settings,
        flags: 0,
        stream_id: 0,
        payload: vec![0, 1, 2],
    }
    .encode(&mut invalid_settings);
    let mut server = h2_server_after_preface();
    assert_eq!(
        server.accept_event(&invalid_settings),
        Err(ServerError::InvalidFrame)
    );

    assert_eq!(
        window_update_increment(&0_u32.to_be_bytes()),
        Err(ServerError::InvalidFrame)
    );
}
