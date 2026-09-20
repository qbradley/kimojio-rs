//! HTTP/2 server and client connection tests.

use std::time::Duration;

use super::*;
use crate::server::*;
use crate::{Header, HttpErrorKind, HttpLimits};

#[test]
fn h2_rejects_a_push_promise_sent_by_the_client() {
    let mut server = h2_server_after_preface();

    // Promised stream 2 followed by an indexed header block.
    let mut payload = 2u32.to_be_bytes().to_vec();
    payload.extend_from_slice(&[0x82, 0x86, 0x84]);
    let frame = encoded_frame(H2FrameType::PushPromise, 0x4, 1, payload);

    assert_eq!(server.accept_event(&frame), Err(ServerError::InvalidFrame));

    let reported = server
        .take_reported_protocol_error()
        .expect("a client PUSH_PROMISE must report a protocol error");
    assert_eq!(reported.error_code(), H2ErrorCode::ProtocolError);
    assert_eq!(reported.scope(), H2ErrorScope::Connection);
}

/// PUSH_PROMISE has to decode as its own frame type. Decoding it as an
/// unknown extension type is what previously caused it to be ignored.
#[test]
fn h2_data_after_a_peer_reset_is_a_stream_closed_error() {
    let mut server = h2_server_after_preface();
    let headers = encode_hpack_request_headers("POST", "https", "example.com", "/", &[]);
    assert!(
        server
            .accept_event(&encoded_frame(H2FrameType::Headers, 0x4, 1, headers))
            .is_ok()
    );

    let reset = encoded_frame(H2FrameType::RstStream, 0, 1, 8u32.to_be_bytes().to_vec());
    assert!(server.accept_event(&reset).is_ok());

    let data = encoded_frame(H2FrameType::Data, 0, 1, b"late".to_vec());
    assert_eq!(server.accept_event(&data), Err(ServerError::InvalidFrame));

    let reported = server
        .take_reported_protocol_error()
        .expect("DATA on a peer-reset stream must report a protocol error");
    assert_eq!(reported.error_code(), H2ErrorCode::StreamClosed);
    assert_eq!(reported.scope(), H2ErrorScope::Stream(1));
}

/// A stream this side reset keeps its tolerance window, because the peer
/// may already have DATA in flight that it could not have withheld.
#[test]
fn h2_data_after_a_local_reset_is_discarded() {
    let mut server = h2_server_after_preface();
    let headers = encode_hpack_request_headers("POST", "https", "example.com", "/", &[]);
    assert!(
        server
            .accept_event(&encoded_frame(H2FrameType::Headers, 0x4, 1, headers))
            .is_ok()
    );
    server.close_stream(1);

    let data = encoded_frame(H2FrameType::Data, 0, 1, b"race".to_vec());
    let (event, _, _) = server.accept_event(&data).unwrap();

    assert_eq!(
        event,
        Some(H2StreamEvent::DiscardedData {
            stream_id: 1,
            flow_control_len: 4,
        })
    );
}

#[test]
fn outbound_acknowledgement_rejects_invalid_state_distinctly() {
    let mut server = H2Server::default();

    assert_eq!(
        server.acknowledge_outbound_block(H2OutboundCommit { sequence: 1 }),
        Err(ServerError::InvalidOutboundState)
    );
}

#[test]
fn h2_accepts_preface_settings_and_get_headers() {
    let mut input = Vec::new();
    input.extend_from_slice(CLIENT_PREFACE);
    H2Frame {
        frame_type: H2FrameType::Settings,
        flags: 0,
        stream_id: 0,
        payload: Vec::new(),
    }
    .encode(&mut input);
    H2Frame {
        frame_type: H2FrameType::Headers,
        flags: 0x5,
        stream_id: 1,
        payload: vec![0x82, 0x86, 0x84],
    }
    .encode(&mut input);

    let mut server = H2Server::default();
    let (request, consumed, output) = server.accept(&input).unwrap();
    assert_eq!(consumed, input.len());
    let request = request.unwrap();
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, "/");
    assert!(!output.is_empty());

    let response = response_frames_bytes(&mut server, 1, 200, &[], b"hello", true);
    let (headers, used) = H2Frame::decode(&response).unwrap();
    assert_eq!(headers.frame_type, H2FrameType::Headers);
    assert_eq!(headers.flags, 0x4);
    let (data, _) = H2Frame::decode(&response[used..]).unwrap();
    assert_eq!(data.frame_type, H2FrameType::Data);
    assert_eq!(data.flags, 0x1);
    assert_eq!(data.payload, b"hello");
}

#[test]
fn h2_accepts_preface_split_across_inputs() {
    let mut client = h2_client_after_server_settings();
    let input = client.connection_preface();
    let mut server = H2Server::default();

    let (event, consumed, output) = server.accept_event(&input[..5]).unwrap();
    assert_eq!(event, None);
    assert_eq!(consumed, 0);
    assert!(output.is_empty());

    let (event, consumed, output) = server.accept_event(&input).unwrap();
    assert_eq!(event, None);
    assert_eq!(consumed, input.len());
    assert!(!output.is_empty());
}

#[test]
fn h2_accepts_curl_huffman_headers_after_window_update() {
    let input = hex_bytes(
        "505249202a20485454502f322e300d0a0d0a534d0d0a0d0a\
             000012040000000000000300000064000400010000000200000000\
             0000040800000000003e7f000100002b0105000000018286418a\
             089d5c0b8170dc64010f048c627a46460d5485f2bce9a68f7a88\
             25b650c3cb842b8753032a2f2a",
    );
    let mut server = H2Server::default();

    let (request, consumed, output) = server.accept(&input).unwrap();

    assert_eq!(consumed, input.len());
    let request = request.unwrap();
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, "/hmac/index.html");
    assert!(!output.is_empty());
}

#[test]
fn h2_response_data_is_split_by_peer_frame_size() {
    let mut server = H2Server::default();
    let body = vec![b'a'; 40 * 1024];
    let response = response_frames_bytes(&mut server, 1, 200, &[], &body, true);
    let (_headers, mut offset) = H2Frame::decode(&response).unwrap();
    let mut data_frames = 0usize;
    while offset < response.len() {
        let (frame, used) = H2Frame::decode(&response[offset..]).unwrap();
        assert_eq!(frame.frame_type, H2FrameType::Data);
        assert!(frame.payload.len() <= H2Settings::default().max_frame_size);
        data_frames += 1;
        offset += used;
        if offset == response.len() {
            assert_eq!(frame.flags, 0x1);
        } else {
            assert_eq!(frame.flags, 0);
        }
    }
    assert!(data_frames > 1);
}

#[test]
fn h2_accept_ignores_unknown_extension_frames_after_preface() {
    let mut server = h2_server_after_preface();
    let mut frame = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Unknown(0x21),
        flags: 0,
        stream_id: 0,
        payload: b"ext".to_vec(),
    }
    .encode(&mut frame);

    assert_eq!(
        server.accept_event(&frame).unwrap(),
        (None, frame.len(), Vec::new())
    );
}

#[test]
fn h2_accept_frame_typed_reports_scope_and_ignored_frames() {
    let mut server = h2_server_after_preface();
    let invalid_data = H2Frame {
        frame_type: H2FrameType::Data,
        flags: 0,
        stream_id: 0,
        payload: Vec::new(),
    };

    assert_eq!(
        server.accept_frame_typed(invalid_data).0,
        H2FrameOutcome::Error(H2ProtocolError::connection(
            H2ErrorCode::ProtocolError,
            "invalid HTTP/2 frame",
        ))
    );

    let unknown = H2Frame {
        frame_type: H2FrameType::Unknown(0x21),
        flags: 0,
        stream_id: 1,
        payload: b"ext".to_vec(),
    };

    assert_eq!(
        server.accept_frame_typed(unknown).0,
        H2FrameOutcome::Ignored
    );
}

#[test]
fn h2_accept_frame_typed_reports_connection_scope_for_control_stream_id_errors() {
    for frame_type in [
        H2FrameType::Settings,
        H2FrameType::Ping,
        H2FrameType::Goaway,
    ] {
        let mut server = h2_server_after_preface();
        let payload = match frame_type {
            H2FrameType::Settings => Vec::new(),
            H2FrameType::Ping => [0; 8].to_vec(),
            H2FrameType::Goaway => [0_u32.to_be_bytes(), 0_u32.to_be_bytes()].concat(),
            _ => unreachable!(),
        };
        let frame = H2Frame {
            frame_type,
            flags: 0,
            stream_id: 1,
            payload,
        };

        let H2FrameOutcome::Error(error) = server.accept_frame_typed(frame).0 else {
            panic!("expected typed error");
        };

        assert_eq!(error.scope, H2ErrorScope::Connection);
        assert_eq!(error.code, H2ErrorCode::ProtocolError);
    }
}

#[test]
fn h2_accept_frame_typed_rejects_priority_self_dependency() {
    let priority = H2Frame {
        frame_type: H2FrameType::Priority,
        flags: 0,
        stream_id: 1,
        payload: [1_u32.to_be_bytes().as_slice(), &[0]].concat(),
    };

    let mut server = h2_server_after_preface();
    let H2FrameOutcome::Error(server_error) = server.accept_frame_typed(priority.clone()).0 else {
        panic!("expected server priority error");
    };
    assert_eq!(server_error.scope, H2ErrorScope::Stream(1));
    assert_eq!(server_error.code, H2ErrorCode::ProtocolError);

    let mut client = h2_client_after_server_settings();
    let H2FrameOutcome::Error(client_error) = client.accept_frame_typed(priority).0 else {
        panic!("expected client priority error");
    };
    assert_eq!(client_error.scope, H2ErrorScope::Stream(1));
    assert_eq!(client_error.code, H2ErrorCode::ProtocolError);
}

#[test]
fn h2_accept_parses_padded_data_payload() {
    let mut server = h2_server_after_preface();
    let mut client = H2Client::default();
    let headers = open_stream_bytes(
        &mut client,
        "POST",
        "https",
        "example.com",
        "/svc",
        &[],
        false,
    )
    .unwrap()
    .1;
    assert!(matches!(
        server.accept_event(&headers).unwrap().0,
        Some(H2StreamEvent::RequestHeaders { .. })
    ));
    let mut data = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Data,
        flags: 0x8,
        stream_id: 1,
        payload: [vec![2], b"hello".to_vec(), vec![0, 0]].concat(),
    }
    .encode(&mut data);

    let (event, consumed, _) = server.accept_event(&data).unwrap();

    assert_eq!(consumed, data.len());
    assert_eq!(
        event,
        Some(H2StreamEvent::Data {
            stream_id: 1,
            payload: b"hello".to_vec(),
            flow_control_len: 8,
            end_stream: false,
        })
    );
}

#[test]
fn h2_accept_strips_headers_priority_metadata() {
    let mut server = h2_server_after_preface();
    let payload = encode_hpack_request_headers("GET", "https", "example.com", "/", &[]);
    let mut frame = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Headers,
        flags: 0x24,
        stream_id: 1,
        payload: [[0; 5].as_slice(), payload.as_slice()].concat(),
    }
    .encode(&mut frame);

    let (event, consumed, _) = server.accept_event(&frame).unwrap();

    assert_eq!(consumed, frame.len());
    assert!(matches!(event, Some(H2StreamEvent::RequestHeaders { .. })));

    let payload = encode_hpack_request_headers("GET", "https", "example.com", "/", &[]);
    let mut self_dependency = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Headers,
        flags: 0x24,
        stream_id: 1,
        payload: [1_u32.to_be_bytes().as_slice(), &[0], payload.as_slice()].concat(),
    }
    .encode(&mut self_dependency);

    let mut server = h2_server_after_preface();
    assert_eq!(
        server.accept_event(&self_dependency),
        Err(ServerError::InvalidFrame)
    );
}

#[test]
fn h2_accept_assembles_headers_continuation() {
    let mut server = h2_server_after_preface();
    let block = encode_hpack_request_headers("GET", "https", "example.com", "/", &[]);
    let split = block.len() / 2;
    let mut headers = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Headers,
        flags: 0,
        stream_id: 1,
        payload: block[..split].to_vec(),
    }
    .encode(&mut headers);
    let mut continuation = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Continuation,
        flags: 0x4,
        stream_id: 1,
        payload: block[split..].to_vec(),
    }
    .encode(&mut continuation);

    assert_eq!(server.accept_event(&headers).unwrap().0, None);
    let (event, consumed, _) = server.accept_event(&continuation).unwrap();

    assert_eq!(consumed, continuation.len());
    assert!(matches!(event, Some(H2StreamEvent::RequestHeaders { .. })));
}

#[test]
fn h2_accept_rejects_interleaved_or_mismatched_continuation() {
    let block = encode_hpack_request_headers("GET", "https", "example.com", "/", &[]);
    let mut headers = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Headers,
        flags: 0,
        stream_id: 1,
        payload: block[..1].to_vec(),
    }
    .encode(&mut headers);

    let mut server = h2_server_after_preface();
    server.accept_event(&headers).unwrap();
    assert_eq!(
        server.accept_event(&h2_ping_frame()),
        Err(ServerError::InvalidFrame)
    );

    let mut continuation = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Continuation,
        flags: 0x4,
        stream_id: 3,
        payload: block[1..].to_vec(),
    }
    .encode(&mut continuation);
    let mut server = h2_server_after_preface();
    server.accept_event(&headers).unwrap();
    assert_eq!(
        server.accept_event(&continuation),
        Err(ServerError::InvalidFrame)
    );
}

#[test]
fn h2_typed_continuation_ordering_errors_are_connection_scoped() {
    let block = encode_hpack_request_headers("GET", "https", "example.com", "/", &[]);
    let headers = H2Frame {
        frame_type: H2FrameType::Headers,
        flags: 0,
        stream_id: 1,
        payload: block[..1].to_vec(),
    };
    let mut server = h2_server_after_preface();
    assert_eq!(
        server.accept_frame_typed(headers.clone()).0,
        H2FrameOutcome::Ignored
    );
    let H2FrameOutcome::Error(error) = server
        .accept_frame_typed(H2Frame {
            frame_type: H2FrameType::Data,
            flags: 0,
            stream_id: 1,
            payload: Vec::new(),
        })
        .0
    else {
        panic!("expected server ordering error");
    };
    assert_eq!(error.scope, H2ErrorScope::Connection);

    let mut server = h2_server_after_preface();
    assert_eq!(
        server.accept_frame_typed(headers).0,
        H2FrameOutcome::Ignored
    );
    let H2FrameOutcome::Error(error) = server
        .accept_frame_typed(H2Frame {
            frame_type: H2FrameType::Continuation,
            flags: 0x4,
            stream_id: 3,
            payload: block[1..].to_vec(),
        })
        .0
    else {
        panic!("expected server continuation mismatch error");
    };
    assert_eq!(error.scope, H2ErrorScope::Connection);

    let block = encode_hpack_response_headers(200, 0, &[]);
    let headers = H2Frame {
        frame_type: H2FrameType::Headers,
        flags: 0,
        stream_id: 1,
        payload: block[..1].to_vec(),
    };
    let mut client = h2_client_after_server_settings();
    assert_eq!(
        client.accept_frame_typed(headers).0,
        H2FrameOutcome::Ignored
    );
    let H2FrameOutcome::Error(error) = client
        .accept_frame_typed(H2Frame {
            frame_type: H2FrameType::Unknown(0x21),
            flags: 0,
            stream_id: 1,
            payload: Vec::new(),
        })
        .0
    else {
        panic!("expected client ordering error");
    };
    assert_eq!(error.scope, H2ErrorScope::Connection);
}

#[test]
fn h2_typed_classify_continuation_ordering_errors_are_connection_scoped() {
    let block = encode_hpack_request_headers("GET", "https", "example.com", "/", &[]);
    let mut server = h2_server_after_preface();
    assert_eq!(
        server.classify_frame_typed(H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0,
            stream_id: 1,
            payload: block[..1].to_vec(),
        }),
        H2FrameOutcome::Ignored
    );

    let H2FrameOutcome::Error(error) = server.classify_frame_typed(H2Frame {
        frame_type: H2FrameType::Data,
        flags: 0,
        stream_id: 1,
        payload: Vec::new(),
    }) else {
        panic!("expected classify ordering error");
    };

    assert_eq!(error.scope, H2ErrorScope::Connection);
}

#[test]
fn h2_accept_rejects_continuation_count_and_header_list_limits() {
    let mut server = H2Server::with_limits(H2Limits {
        max_continuation_frames: 0,
        ..H2Limits::default()
    })
    .unwrap();
    let mut client = H2Client::default();
    server.accept_event(&client.connection_preface()).unwrap();
    let block = encode_hpack_request_headers("GET", "https", "example.com", "/", &[]);
    let mut headers = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Headers,
        flags: 0,
        stream_id: 1,
        payload: block[..1].to_vec(),
    }
    .encode(&mut headers);
    let mut continuation = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Continuation,
        flags: 0x4,
        stream_id: 1,
        payload: block[1..].to_vec(),
    }
    .encode(&mut continuation);
    server.accept_event(&headers).unwrap();
    assert_eq!(
        server.accept_event(&continuation),
        Err(ServerError::InvalidFrame)
    );

    let mut server = H2Server::with_limits(H2Limits {
        max_header_list_size: 1,
        ..H2Limits::default()
    })
    .unwrap();
    let mut client = H2Client::default();
    server.accept_event(&client.connection_preface()).unwrap();
    let mut headers = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Headers,
        flags: 0x4,
        stream_id: 1,
        payload: block,
    }
    .encode(&mut headers);
    assert_eq!(
        server.accept_event(&headers),
        Err(ServerError::HeaderTooLarge {
            limit: 1,
            actual: 177
        })
    );
}

#[test]
fn h2_control_budget_refills_for_spaced_idle_ping() {
    let mut budget = H2ControlFrameBudget::default();
    let mut now = Duration::ZERO;
    for _ in 0..200 {
        budget.record_control_at(H2FrameType::Ping, now).unwrap();
        now += H2_CONTROL_BUDGET_REFILL_INTERVAL;
    }
}

#[test]
fn h2_control_budget_rejects_rapid_ping_flood() {
    let mut budget = H2ControlFrameBudget::default();
    let now = Duration::ZERO;
    for _ in 0..H2_CONTROL_FRAME_BUDGET_LIMITS.ping {
        budget.record_control_at(H2FrameType::Ping, now).unwrap();
    }

    assert_eq!(
        budget.record_control_at(H2FrameType::Ping, now),
        Err(ServerError::InvalidFrame)
    );
}

#[test]
fn h2_settings_events_report_initial_window_size_changes() {
    let mut server = H2Server::default();
    let mut client = H2Client::default();
    let preface = client.connection_preface();
    let (_event, _used, _output) = server.accept_event(&preface).unwrap();
    let mut settings = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Settings,
        flags: 0,
        stream_id: 0,
        payload: {
            let mut payload = Vec::new();
            H2Settings::encode_payload(
                &[H2Setting::new(H2SettingId::InitialWindowSize, 1024)],
                &mut payload,
            );
            payload
        },
    }
    .encode(&mut settings);

    let (event, _used, output) = server.accept_event(&settings).unwrap();

    assert!(!output.is_empty());
    assert_eq!(
        event,
        Some(H2StreamEvent::Settings {
            initial_window_size: Some(H2InitialWindowSizeChange {
                previous: 65_535,
                current: 1024,
            })
        })
    );
}

#[test]
fn h2_control_frame_budget_rejects_ping_flood_and_does_not_reset_after_headers() {
    let mut server = h2_server_after_preface();
    server.endpoint.control_budget.ping = 2;
    let ping = h2_ping_frame();

    server.accept_event(&ping).unwrap();
    server.accept_event(&ping).unwrap();
    assert_eq!(server.accept_event(&ping), Err(ServerError::InvalidFrame));
    let diagnostics = server.control_diagnostics();
    assert_eq!(diagnostics.control_rejections, 1);
    assert_eq!(diagnostics.ping_rejections, 1);

    let mut client = H2Client::default();
    let (_stream_id, headers) = open_stream_bytes(
        &mut client,
        "POST",
        "http",
        "localhost",
        "/svc/Call",
        &[],
        true,
    )
    .unwrap();
    server.accept_event(&headers).unwrap();
    assert_eq!(server.accept_event(&ping), Err(ServerError::InvalidFrame));
}

#[test]
fn h2_control_frame_budget_is_not_reset_by_immediate_external_progress() {
    let mut server = h2_server_after_preface();
    server.endpoint.control_budget.ping = 1;
    let ping = h2_ping_frame();

    server.accept_event(&ping).unwrap();
    assert_eq!(server.accept_event(&ping), Err(ServerError::InvalidFrame));

    server.record_progress_frame();
    assert_eq!(server.accept_event(&ping), Err(ServerError::InvalidFrame));
}

#[test]
fn h2_progress_replenishes_only_bounded_window_update_credits() {
    let mut client = h2_client_after_server_settings();
    client.endpoint.control_budget.window_update = 0;
    client.endpoint.control_budget.ping = 0;

    client.record_progress_frame();

    assert_eq!(
        client.endpoint.control_budget.window_update,
        H2_WINDOW_UPDATE_CREDITS_PER_PROGRESS_FRAME
    );
    assert_eq!(client.endpoint.control_budget.ping, 0);

    let update = h2_window_update_frame(0, 1);
    for _ in 0..H2_WINDOW_UPDATE_CREDITS_PER_PROGRESS_FRAME {
        client.accept(&update).unwrap();
    }
    assert_eq!(client.accept(&update), Err(ServerError::InvalidFrame));

    client.endpoint.control_budget.window_update = H2_CONTROL_FRAME_BUDGET_LIMITS.window_update - 1;
    client.record_progress_frame();
    assert_eq!(
        client.endpoint.control_budget.window_update,
        H2_CONTROL_FRAME_BUDGET_LIMITS.window_update
    );
}

#[test]
fn h2_client_control_frame_budget_rejects_ping_flood_and_does_not_reset_after_headers() {
    let mut client = h2_client_after_server_settings();
    client.endpoint.control_budget.ping = 2;
    let ping = h2_ping_frame();

    client.accept(&ping).unwrap();
    client.accept(&ping).unwrap();
    assert_eq!(client.accept(&ping), Err(ServerError::InvalidFrame));

    open_stream_bytes(&mut client, "GET", "https", "example.com", "/", &[], true).unwrap();
    let headers = h2_response_headers_frame(1);
    client.accept(&headers).unwrap();
    assert_eq!(client.accept(&ping), Err(ServerError::InvalidFrame));
}

#[test]
fn h2_client_rejects_control_churn_and_priority() {
    let mut client = H2Client::default();
    client.endpoint.control_budget.settings = 1;
    let settings = h2_settings_frame(&[]);
    client.accept(&settings).unwrap();
    assert_eq!(client.accept(&settings), Err(ServerError::InvalidFrame));

    let mut client = h2_client_after_server_settings();
    client.endpoint.control_budget.window_update = 1;
    let update = h2_window_update_frame(0, 1);
    client.accept(&update).unwrap();
    assert_eq!(client.accept(&update), Err(ServerError::InvalidFrame));

    let mut client = h2_client_after_server_settings();
    client.endpoint.control_budget.priority = 1;
    let priority = h2_priority_frame(1);
    assert_eq!(
        client.accept(&priority),
        Ok((None, priority.len(), Vec::new()))
    );
    assert_eq!(client.accept(&priority), Err(ServerError::InvalidFrame));
}

#[test]
fn h2_control_frame_budget_rejects_settings_window_reset_and_goaway_churn() {
    let mut server = h2_server_after_preface();
    server.endpoint.control_budget.settings = 1;
    let settings = h2_settings_frame(&[]);
    server.accept_event(&settings).unwrap();
    assert_eq!(
        server.accept_event(&settings),
        Err(ServerError::InvalidFrame)
    );

    let mut server = h2_server_after_preface();
    server.endpoint.control_budget.window_update = 1;
    let update = h2_window_update_frame(0, 1);
    server.accept_event(&update).unwrap();
    assert_eq!(server.accept_event(&update), Err(ServerError::InvalidFrame));

    let mut server = h2_server_after_preface();
    let mut client = H2Client::default();
    let (_, headers) = open_stream_bytes(
        &mut client,
        "POST",
        "https",
        "example.com",
        "/svc",
        &[],
        false,
    )
    .unwrap();
    server.accept_event(&headers).unwrap();
    let (_, headers_2) = open_stream_bytes(
        &mut client,
        "POST",
        "https",
        "example.com",
        "/svc2",
        &[],
        false,
    )
    .unwrap();
    server.accept_event(&headers_2).unwrap();
    server.endpoint.control_budget.reset = 1;
    let reset = h2_reset_frame(1);
    server.accept_event(&reset).unwrap();
    assert_eq!(
        server.accept_event(&h2_reset_frame(3)),
        Err(ServerError::InvalidFrame)
    );

    let mut server = h2_server_after_preface();
    server.endpoint.control_budget.goaway = 1;
    let goaway = h2_goaway_frame(1);
    server.accept_event(&goaway).unwrap();
    assert_eq!(server.accept_event(&goaway), Err(ServerError::InvalidFrame));
}

#[test]
fn h2_control_settings_ack_state_and_timer_intent() {
    let mut client = H2Client::default();
    assert_eq!(client.timer_intent(), None);
    let _preface = client.connection_preface();
    assert_eq!(
        client.timer_intent(),
        Some(H2TimerIntent::SettingsAckTimeout)
    );

    let mut ack = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Settings,
        flags: 0x1,
        stream_id: 0,
        payload: Vec::new(),
    }
    .encode(&mut ack);

    client.accept(&h2_settings_frame(&[])).unwrap();
    client.accept(&ack).unwrap();
    assert_eq!(client.timer_intent(), None);
    assert_eq!(client.control_diagnostics().settings_acks, 1);
    assert_eq!(client.accept(&ack), Err(ServerError::InvalidFrame));
}

#[test]
fn h2_settings_ack_debt_counts_each_local_settings_frame() {
    let mut server = h2_server_after_preface();
    assert_eq!(
        server.local_settings_state(),
        H2SettingsSyncState::WaitingAck
    );
    server.mark_local_settings_sent();
    assert!(server.timer_obligations().settings_ack());

    server.accept_event(&h2_settings_ack_frame()).unwrap();
    assert_eq!(server.control_diagnostics().settings_acks, 1);
    assert!(server.timer_obligations().settings_ack());
    assert_eq!(
        server.local_settings_state(),
        H2SettingsSyncState::WaitingAck
    );

    server.accept_event(&h2_settings_ack_frame()).unwrap();
    assert_eq!(server.control_diagnostics().settings_acks, 2);
    assert!(!server.timer_obligations().settings_ack());
    assert_eq!(server.local_settings_state(), H2SettingsSyncState::Synced);
    assert_eq!(
        server.accept_event(&h2_settings_ack_frame()),
        Err(ServerError::InvalidFrame)
    );

    let mut client = H2Client::default();
    let _preface = client.connection_preface();
    client.mark_local_settings_sent();
    assert!(client.timer_obligations().settings_ack());
    client.accept(&h2_settings_frame(&[])).unwrap();
    client.accept(&h2_settings_ack_frame()).unwrap();
    assert_eq!(client.control_diagnostics().settings_acks, 1);
    assert!(client.timer_obligations().settings_ack());
    client.accept(&h2_settings_ack_frame()).unwrap();
    assert!(!client.timer_obligations().settings_ack());
    assert_eq!(
        client.accept(&h2_settings_ack_frame()),
        Err(ServerError::InvalidFrame)
    );
}

#[test]
fn h2_settings_and_graceful_timers_are_independent() {
    let mut server = h2_server_after_preface();
    assert!(server.timer_obligations().settings_ack());
    assert!(!server.timer_obligations().graceful_shutdown_ping());
    assert_eq!(
        server.timer_intent(),
        Some(H2TimerIntent::SettingsAckTimeout)
    );

    let _first_stage = server.begin_graceful_shutdown().unwrap();
    let both = server.timer_obligations();
    assert!(
        both.settings_ack(),
        "graceful start must not drop SETTINGS ACK debt {:?}",
        server.local_settings_state()
    );
    assert!(both.graceful_shutdown_ping());
    assert_eq!(
        server.timer_intent(),
        Some(H2TimerIntent::GracefulShutdownPing)
    );

    server.accept_event(&h2_settings_ack_frame()).unwrap();
    assert!(!server.timer_obligations().settings_ack());
    assert!(server.timer_obligations().graceful_shutdown_ping());
    assert_eq!(
        server.timer_intent(),
        Some(H2TimerIntent::GracefulShutdownPing)
    );

    let mut server = h2_server_after_preface();
    let first_stage = decode_h2_frames(&server.begin_graceful_shutdown().unwrap());
    let ping_ack = h2_ping_ack_frame(&first_stage[1].payload);
    server.accept_event(&ping_ack).unwrap();
    assert!(server.timer_obligations().settings_ack());
    assert!(!server.timer_obligations().graceful_shutdown_ping());
    assert_eq!(
        server.timer_intent(),
        Some(H2TimerIntent::SettingsAckTimeout)
    );
}

#[test]
fn h2_settings_ack_timeout_emits_settings_timeout_goaway() {
    let mut server = h2_server_after_preface();
    let frames = decode_h2_frames(&server.settings_ack_timeout_elapsed().unwrap());
    assert_eq!(frames.len(), 1);
    assert_goaway(&frames[0], 0, H2ErrorCode::SettingsTimeout);
    assert!(server.settings_ack_timeout_elapsed().unwrap().is_empty());

    let mut client = H2Client::default();
    let _preface = client.connection_preface();
    let frames = decode_h2_frames(&client.settings_ack_timeout_elapsed().unwrap());
    assert_eq!(frames.len(), 1);
    assert_goaway(&frames[0], 0, H2ErrorCode::SettingsTimeout);
    assert!(client.settings_ack_timeout_elapsed().unwrap().is_empty());
    assert!(
        H2Client::default()
            .settings_ack_timeout_elapsed()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn h2_graceful_shutdown_emits_two_goaways_around_round_trip_ping() {
    let mut server = h2_server_after_preface();
    server.accept_event(&h2_settings_ack_frame()).unwrap();
    let mut client = H2Client::default();
    let (stream_id, request) =
        open_stream_bytes(&mut client, "GET", "http", "example.com", "/", &[], true).unwrap();
    server.accept_event(&request).unwrap();

    let first_stage = decode_h2_frames(&server.begin_graceful_shutdown().unwrap());
    assert_eq!(first_stage.len(), 2);
    assert_goaway(&first_stage[0], 0x7fff_ffff, H2ErrorCode::NoError);
    assert_eq!(first_stage[1].frame_type, H2FrameType::Ping);
    assert_eq!(first_stage[1].flags, 0);
    assert_eq!(first_stage[1].stream_id, 0);
    assert_eq!(first_stage[1].payload.len(), 8);

    let ping_ack = h2_ping_ack_frame(&first_stage[1].payload);
    let (_, consumed, second_stage) = server.accept_event(&ping_ack).unwrap();
    assert_eq!(consumed, ping_ack.len());
    let second_stage = decode_h2_frames(&second_stage);
    assert_eq!(second_stage.len(), 1);
    assert_goaway(&second_stage[0], stream_id, H2ErrorCode::NoError);
}

#[test]
fn h2_graceful_shutdown_deadline_emits_final_goaway_without_ping_ack() {
    let mut server = h2_server_after_preface();
    server.accept_event(&h2_settings_ack_frame()).unwrap();
    let mut client = H2Client::default();
    let (stream_id, request) =
        open_stream_bytes(&mut client, "GET", "http", "example.com", "/", &[], true).unwrap();
    server.accept_event(&request).unwrap();
    let _first_stage = server.begin_graceful_shutdown().unwrap();

    let second_stage = decode_h2_frames(&server.graceful_shutdown_ping_elapsed().unwrap());
    assert_eq!(second_stage.len(), 1);
    assert_goaway(&second_stage[0], stream_id, H2ErrorCode::NoError);
    assert!(
        server.graceful_shutdown_ping_elapsed().unwrap().is_empty(),
        "a repeated virtual-clock expiry must not emit or widen the boundary"
    );
}

#[test]
fn h2_graceful_shutdown_timer_intent_tracks_only_the_ping_wait() {
    let mut server = h2_server_after_preface();
    server.accept_event(&h2_settings_ack_frame()).unwrap();
    assert_eq!(server.timer_intent(), None);

    let _first_stage = server.begin_graceful_shutdown().unwrap();
    assert_eq!(
        server.timer_intent(),
        Some(H2TimerIntent::GracefulShutdownPing)
    );

    let _second_stage = server.graceful_shutdown_ping_elapsed().unwrap();
    assert_eq!(server.timer_intent(), None);
}

#[test]
fn h2_graceful_shutdown_final_boundary_includes_stream_opened_during_stage_one() {
    let mut server = h2_server_after_preface();
    server.accept_event(&h2_settings_ack_frame()).unwrap();
    let mut client = H2Client::default();
    let (first_stream, first_request) =
        open_stream_bytes(&mut client, "GET", "http", "example.com", "/one", &[], true).unwrap();
    server.accept_event(&first_request).unwrap();

    let first_stage = decode_h2_frames(&server.begin_graceful_shutdown().unwrap());
    assert_goaway(&first_stage[0], 0x7fff_ffff, H2ErrorCode::NoError);

    let (concurrent_stream, concurrent_request) =
        open_stream_bytes(&mut client, "GET", "http", "example.com", "/two", &[], true).unwrap();
    assert!(concurrent_stream > first_stream);
    server.accept_event(&concurrent_request).unwrap();

    let ping_ack = h2_ping_ack_frame(&first_stage[1].payload);
    let (_, _, second_stage) = server.accept_event(&ping_ack).unwrap();
    let second_stage = decode_h2_frames(&second_stage);
    assert_eq!(second_stage.len(), 1);
    assert_goaway(&second_stage[0], concurrent_stream, H2ErrorCode::NoError);
    assert_eq!(
        server.shutdown_intent(),
        H2ShutdownIntent::Drain {
            last_stream_id: concurrent_stream
        }
    );
}

#[test]
fn h2_goaway_frame_enforces_non_increasing_last_stream_id_and_reports_intent() {
    let mut server = H2Server::default();
    let first = server.goaway_frame(u32::MAX >> 1, 0).unwrap();
    assert!(!first.is_empty());
    assert_eq!(server.shutdown_intent(), H2ShutdownIntent::Close);
    assert!(server.goaway_frame(u32::MAX >> 1, 0).is_ok());
    assert_eq!(
        server.goaway_frame(u32::MAX, 0),
        Err(ServerError::InvalidFrame)
    );
    assert_eq!(server.control_diagnostics().goaways, 2);

    let mut server = h2_server_after_preface();
    let mut client = H2Client::default();
    let (_, headers) = open_stream_bytes(
        &mut client,
        "POST",
        "https",
        "example.com",
        "/svc",
        &[],
        false,
    )
    .unwrap();
    server.accept_event(&headers).unwrap();
    server.goaway_frame(1, 0).unwrap();
    assert_eq!(
        server.shutdown_intent(),
        H2ShutdownIntent::Drain { last_stream_id: 1 }
    );

    let mut server = h2_server_after_preface();
    let mut client = H2Client::default();
    let (_, headers) = open_stream_bytes(
        &mut client,
        "GET",
        "https",
        "example.com",
        "/svc",
        &[],
        true,
    )
    .unwrap();
    server.accept_event(&headers).unwrap();
    server.goaway_frame(1, 0).unwrap();
    assert_eq!(
        server.shutdown_intent(),
        H2ShutdownIntent::Drain { last_stream_id: 1 }
    );
    server.finish_response_stream(1);
    assert_eq!(server.shutdown_intent(), H2ShutdownIntent::Close);
}

#[test]
fn h2_client_inbound_goaway_only_narrows_last_stream_id() {
    let mut client = h2_client_after_server_settings();
    for (received, expected) in [(5_u32, 5_u32), (1, 1), (5, 1)] {
        let (event, output) = client
            .accept_frame_bytes(H2Frame {
                frame_type: H2FrameType::Goaway,
                flags: 0,
                stream_id: 0,
                payload: [received.to_be_bytes(), 0_u32.to_be_bytes()].concat(),
            })
            .unwrap();
        assert!(output.is_empty());
        assert!(matches!(
            event,
            H2ByteClientEvent::Goaway {
                last_stream_id,
                error_code: 0,
            } if last_stream_id == expected
        ));
        assert_eq!(client.received_goaway_last_stream_id(), Some(expected));
    }
}

#[test]
fn h2_explicit_goaway_supersedes_graceful_ping_wait() {
    let mut server = h2_server_after_preface();
    server.accept_event(&h2_settings_ack_frame()).unwrap();
    let mut client = H2Client::default();
    let (stream_id, request) =
        open_stream_bytes(&mut client, "GET", "http", "example.com", "/", &[], true).unwrap();
    server.accept_event(&request).unwrap();

    let first_stage = decode_h2_frames(&server.begin_graceful_shutdown().unwrap());
    assert_goaway(&first_stage[0], 0x7fff_ffff, H2ErrorCode::NoError);
    assert!(server.timer_obligations().graceful_shutdown_ping());

    let explicit = decode_h2_frames(&server.goaway_frame(stream_id, 0).unwrap());
    assert_eq!(explicit.len(), 1);
    assert_goaway(&explicit[0], stream_id, H2ErrorCode::NoError);
    assert!(!server.timer_obligations().graceful_shutdown_ping());
    assert_eq!(
        server.shutdown_intent(),
        H2ShutdownIntent::Drain {
            last_stream_id: stream_id
        }
    );

    let ping_ack = h2_ping_ack_frame(&first_stage[1].payload);
    let (_, _, late_ack) = server.accept_event(&ping_ack).unwrap();
    assert!(
        late_ack.is_empty(),
        "a late graceful PING ACK must not restart the final GOAWAY"
    );
    assert!(
        server.graceful_shutdown_ping_elapsed().unwrap().is_empty(),
        "a deadline after explicit GOAWAY must not emit another boundary"
    );
    assert_eq!(
        server.goaway_frame(stream_id + 2, 0),
        Err(ServerError::InvalidFrame)
    );
    let narrower = decode_h2_frames(&server.goaway_frame(0, 0).unwrap());
    assert_eq!(narrower.len(), 1);
    assert_goaway(&narrower[0], 0, H2ErrorCode::NoError);
}

#[test]
fn h2_control_diagnostics_count_ping_and_goaway() {
    let mut server = h2_server_after_preface();
    let ping = h2_ping_frame();
    server.accept_event(&ping).unwrap();
    server.accept_event(&h2_goaway_frame(1)).unwrap();

    let diagnostics = server.control_diagnostics();
    assert_eq!(diagnostics.pings, 1);
    assert_eq!(diagnostics.goaways, 1);
}

#[test]
fn h2_rejects_invalid_window_update_payloads() {
    let mut server = h2_server_after_preface();
    let zero = h2_window_update_frame(0, 0);
    assert_eq!(server.accept_event(&zero), Err(ServerError::InvalidFrame));

    let mut short = Vec::new();
    H2Frame {
        frame_type: H2FrameType::WindowUpdate,
        flags: 0,
        stream_id: 0,
        payload: vec![0, 0, 1],
    }
    .encode(&mut short);
    assert_eq!(server.accept_event(&short), Err(ServerError::InvalidFrame));
}

#[test]
fn h2_server_stream_events_cover_headers_data_trailers_and_reset() {
    let mut client = H2Client::default();
    let mut input = client.connection_preface();
    let (stream_id, headers) = open_stream_bytes(
        &mut client,
        "POST",
        "http",
        "localhost",
        "/pkg.Service/Call",
        &[Header::new("content-type", "application/grpc")],
        false,
    )
    .unwrap();
    input.extend_from_slice(&headers);
    input.extend_from_slice(&client.data_frame(stream_id, b"abc", false));
    input.extend_from_slice(&trailers_bytes(
        &mut client,
        stream_id,
        &[Header::new("grpc-status", "0")],
    ));
    H2Frame {
        frame_type: H2FrameType::RstStream,
        flags: 0,
        stream_id,
        payload: 8_u32.to_be_bytes().to_vec(),
    }
    .encode(&mut input);

    let mut server = H2Server::default();
    let (event, consumed, output) = server.accept_event(&input).unwrap();
    assert!(matches!(
        event,
        Some(H2StreamEvent::RequestHeaders {
            request: H2Request {
                ref method,
                ref path,
                ..
            },
            end_stream: false,
            ..
        }) if method == "POST" && path == "/pkg.Service/Call"
    ));
    assert!(!output.is_empty());

    let (event, used, _) = server.accept_event(&input[consumed..]).unwrap();
    assert_eq!(
        event,
        Some(H2StreamEvent::Data {
            stream_id,
            payload: b"abc".to_vec(),
            flow_control_len: 3,
            end_stream: false,
        })
    );

    let (event, reset_offset, _) = server.accept_event(&input[consumed + used..]).unwrap();
    assert!(matches!(
        event,
        Some(H2StreamEvent::Trailers {
            stream_id: id,
            ref headers
        }) if id == stream_id
            && headers.iter().any(|h| h.name == "grpc-status" && h.value == "0")
    ));

    let (event, _, _) = server
        .accept_event(&input[consumed + used + reset_offset..])
        .unwrap();
    assert_eq!(
        event,
        Some(H2StreamEvent::Reset {
            stream_id,
            error_code: 8,
        })
    );
}

#[test]
fn h2_client_streams_request_and_accepts_response_events() {
    let mut client = H2Client::default();
    let preface = client.connection_preface();
    assert!(preface.starts_with(CLIENT_PREFACE));
    assert!(client.connection_preface().is_empty());

    let (stream_id, request) = open_stream_bytes(
        &mut client,
        "POST",
        "https",
        "localhost",
        "/svc/Unary",
        &[],
        true,
    )
    .unwrap();
    let (frame, _) = H2Frame::decode(&request).unwrap();
    assert_eq!(frame.frame_type, H2FrameType::Headers);
    assert_eq!(frame.flags, 0x5);

    let mut response = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Settings,
        flags: 0,
        stream_id: 0,
        payload: Vec::new(),
    }
    .encode(&mut response);
    let mut server = H2Server::default();
    response.extend_from_slice(&response_frames_bytes(
        &mut server,
        stream_id,
        200,
        &[Header::new("content-type", "application/grpc")],
        b"hello",
        false,
    ));
    H2Frame {
        frame_type: H2FrameType::Headers,
        flags: 0x5,
        stream_id,
        payload: encode_hpack_header_block(&[Header::new("grpc-status", "0")]),
    }
    .encode(&mut response);

    let (event, consumed, ack) = client.accept(&response).unwrap();
    assert_eq!(
        event,
        Some(H2ClientEvent::Settings {
            initial_window_size: None
        })
    );
    assert!(!ack.is_empty());

    let (event, used, _) = client.accept(&response[consumed..]).unwrap();
    assert!(matches!(
        event,
        Some(H2ClientEvent::ResponseHeaders {
            stream_id: id,
            status: 200,
            end_stream: false,
            ..
        }) if id == stream_id
    ));

    let (event, trailer_offset, _) = client.accept(&response[consumed + used..]).unwrap();
    assert_eq!(
        event,
        Some(H2ClientEvent::Data {
            stream_id,
            payload: b"hello".to_vec(),
            flow_control_len: 5,
            end_stream: false,
        })
    );

    let (event, _, _) = client
        .accept(&response[consumed + used + trailer_offset..])
        .unwrap();
    assert!(matches!(
        event,
        Some(H2ClientEvent::Trailers {
            stream_id: id,
            ref headers
        }) if id == stream_id
            && headers.iter().any(|h| h.name == "grpc-status" && h.value == "0")
    ));
}

#[test]
fn h2_client_rejects_response_frames_before_server_settings() {
    let mut client = H2Client::default();
    open_stream_bytes(&mut client, "GET", "https", "example.com", "/", &[], true).unwrap();

    assert_eq!(
        client.accept(&h2_response_headers_frame(1)),
        Err(ServerError::InvalidFrame)
    );
    assert_eq!(
        H2Client::default().accept(&h2_settings_ack_frame()),
        Err(ServerError::InvalidFrame)
    );
}

#[test]
fn h2_client_accepts_response_frames_after_server_settings() {
    let mut client = h2_client_after_server_settings();
    open_stream_bytes(&mut client, "GET", "https", "example.com", "/", &[], true).unwrap();

    let (event, _, _) = client.accept(&h2_response_headers_frame(1)).unwrap();

    assert!(matches!(
        event,
        Some(H2ClientEvent::ResponseHeaders {
            stream_id: 1,
            status: 200,
            ..
        })
    ));
}

#[test]
fn h2_stream_events_cover_split_request_data_window_update_and_goaway() {
    let mut client = H2Client::default();
    let mut input = client.connection_preface();
    let (stream_id, headers) = open_stream_bytes(
        &mut client,
        "POST",
        "http",
        "localhost",
        "/svc/Upload",
        &[],
        false,
    )
    .unwrap();
    input.extend_from_slice(&headers);
    input.extend_from_slice(&client.data_frame(stream_id, b"one", false));
    input.extend_from_slice(&client.data_frame(stream_id, b"two", true));
    H2Frame {
        frame_type: H2FrameType::WindowUpdate,
        flags: 0,
        stream_id,
        payload: 1024_u32.to_be_bytes().to_vec(),
    }
    .encode(&mut input);
    H2Frame {
        frame_type: H2FrameType::Goaway,
        flags: 0,
        stream_id: 0,
        payload: [stream_id.to_be_bytes(), 0_u32.to_be_bytes()].concat(),
    }
    .encode(&mut input);

    let mut server = H2Server::default();
    let (event, mut offset, _) = server.accept_event(&input).unwrap();
    assert!(matches!(event, Some(H2StreamEvent::RequestHeaders { .. })));

    let (event, used, _) = server.accept_event(&input[offset..]).unwrap();
    assert_eq!(
        event,
        Some(H2StreamEvent::Data {
            stream_id,
            payload: b"one".to_vec(),
            flow_control_len: 3,
            end_stream: false,
        })
    );
    offset += used;

    let (event, used, _) = server.accept_event(&input[offset..]).unwrap();
    assert_eq!(
        event,
        Some(H2StreamEvent::Data {
            stream_id,
            payload: b"two".to_vec(),
            flow_control_len: 3,
            end_stream: true,
        })
    );
    offset += used;

    let (event, used, _) = server.accept_event(&input[offset..]).unwrap();
    assert_eq!(
        event,
        Some(H2StreamEvent::WindowUpdate {
            stream_id,
            increment: 1024,
        })
    );
    offset += used;

    let (event, _, _) = server.accept_event(&input[offset..]).unwrap();
    assert_eq!(
        event,
        Some(H2StreamEvent::Goaway {
            last_stream_id: stream_id,
            error_code: 0,
        })
    );
}

#[test]
fn h2_rejects_invalid_settings_max_frame_size() {
    let mut input = Vec::new();
    input.extend_from_slice(CLIENT_PREFACE);
    H2Frame {
        frame_type: H2FrameType::Settings,
        flags: 0,
        stream_id: 0,
        payload: [
            5_u16.to_be_bytes().as_slice(),
            0_u32.to_be_bytes().as_slice(),
        ]
        .concat(),
    }
    .encode(&mut input);

    let mut server = H2Server::default();
    assert_eq!(server.accept_event(&input), Err(ServerError::InvalidFrame));

    let mut client = H2Client::default();
    let mut server_settings = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Settings,
        flags: 0,
        stream_id: 0,
        payload: [
            5_u16.to_be_bytes().as_slice(),
            (H2_MAX_MAX_FRAME_SIZE as u32 + 1).to_be_bytes().as_slice(),
        ]
        .concat(),
    }
    .encode(&mut server_settings);
    assert_eq!(
        client.accept(&server_settings),
        Err(ServerError::InvalidFrame)
    );
}

#[test]
fn h2_stream_tracking_releases_completed_stream_ids() {
    let mut client = H2Client::default();
    let mut server_input = client.connection_preface();
    let (stream_id, headers) = open_stream_bytes(
        &mut client,
        "POST",
        "http",
        "localhost",
        "/svc/One",
        &[],
        false,
    )
    .unwrap();
    server_input.extend_from_slice(&headers);
    server_input.extend_from_slice(&client.data_frame(stream_id, b"done", true));
    let (next_stream_id, next_headers) = open_stream_bytes(
        &mut client,
        "POST",
        "http",
        "localhost",
        "/svc/Two",
        &[],
        false,
    )
    .unwrap();
    server_input.extend_from_slice(&next_headers);
    H2Frame {
        frame_type: H2FrameType::Headers,
        flags: 0x5,
        stream_id: next_stream_id,
        payload: encode_hpack_header_block(&[Header::new("grpc-status", "0")]),
    }
    .encode(&mut server_input);

    let mut server = H2Server::default();
    let (_event, mut offset, _) = server.accept_event(&server_input).unwrap();
    assert!(server.has_inbound(stream_id));
    let (_event, used, _) = server.accept_event(&server_input[offset..]).unwrap();
    offset += used;
    assert!(!server.has_inbound(stream_id));
    let (_event, used, _) = server.accept_event(&server_input[offset..]).unwrap();
    offset += used;
    assert!(server.has_inbound(next_stream_id));
    let (_event, _used, _) = server.accept_event(&server_input[offset..]).unwrap();
    assert!(!server.has_inbound(next_stream_id));

    let mut h2_client = h2_client_after_server_settings();
    let (_first_stream_id, _) = open_stream_bytes(
        &mut h2_client,
        "POST",
        "http",
        "localhost",
        "/svc/One",
        &[],
        true,
    )
    .unwrap();
    let (client_next_stream_id, _) = open_stream_bytes(
        &mut h2_client,
        "POST",
        "http",
        "localhost",
        "/svc/Two",
        &[],
        true,
    )
    .unwrap();
    assert_eq!(client_next_stream_id, next_stream_id);
    let mut response = Vec::new();
    let mut response_server = H2Server::default();
    response.extend_from_slice(&response_frames_bytes(
        &mut response_server,
        next_stream_id,
        200,
        &[],
        b"ok",
        true,
    ));
    let (_event, consumed, _) = h2_client.accept(&response).unwrap();
    assert!(h2_client.has_receive_body(next_stream_id));
    let (_event, _used, _) = h2_client.accept(&response[consumed..]).unwrap();
    assert!(!h2_client.has_receive_body(next_stream_id));
}

#[test]
fn h2_client_allows_informational_response_before_final_headers() {
    let mut client = h2_client_after_server_settings();
    let (stream_id, _) =
        open_stream_bytes(&mut client, "GET", "https", "example.com", "/", &[], true).unwrap();
    let mut h2 = H2Server::default();
    let mut response = response_headers_bytes(&mut h2, stream_id, 100, &[], false);
    response.extend_from_slice(&response_headers_bytes(&mut h2, stream_id, 200, &[], true));

    let (event, offset, _) = client.accept(&response).unwrap();
    assert!(matches!(
        event,
        Some(H2ClientEvent::ResponseHeaders { status: 100, .. })
    ));
    let (event, _, _) = client.accept(&response[offset..]).unwrap();
    assert!(matches!(
        event,
        Some(H2ClientEvent::ResponseHeaders {
            status: 200,
            end_stream: true,
            ..
        })
    ));
}

#[test]
fn h2_client_enforces_stream_id_and_concurrency_limits() {
    let mut client = H2Client::default();
    client.next_stream_id = 0x8000_0001;
    assert_eq!(
        open_stream_bytes(&mut client, "GET", "https", "example.com", "/", &[], true,),
        Err(ServerError::InvalidFrame)
    );

    let mut client = H2Client::default();
    client.endpoint.settings.max_concurrent_streams = 1;
    open_stream_bytes(
        &mut client,
        "GET",
        "https",
        "example.com",
        "/one",
        &[],
        true,
    )
    .unwrap();
    assert_eq!(
        open_stream_bytes(
            &mut client,
            "GET",
            "https",
            "example.com",
            "/two",
            &[],
            true,
        ),
        Err(ServerError::InvalidFrame)
    );
}

#[test]
fn h2_rejects_rst_stream_for_idle_stream() {
    let mut server = h2_server_after_preface();
    assert_eq!(
        server.accept_event(&h2_reset_frame(1)),
        Err(ServerError::InvalidFrame)
    );
}

#[test]
fn h2_rejects_data_after_normal_stream_close() {
    let mut client = H2Client::default();
    let mut input = client.connection_preface();
    let (stream_id, headers) = open_stream_bytes(
        &mut client,
        "POST",
        "https",
        "example.com",
        "/upload",
        &[],
        false,
    )
    .unwrap();
    input.extend_from_slice(&headers);
    input.extend_from_slice(&client.data_frame(stream_id, b"done", true));
    input.extend_from_slice(&client.data_frame(stream_id, b"late", false));
    let mut server = H2Server::default();
    let (_event, offset, _) = server.accept_event(&input).unwrap();
    let (_event, used, _) = server.accept_event(&input[offset..]).unwrap();

    assert_eq!(
        server.accept_event(&input[offset + used..]),
        Err(ServerError::InvalidFrame)
    );
}

#[test]
fn h2_server_discarded_data_preserves_borrowed_owned_and_wrapper_contracts() {
    let mut server = h2_server_after_preface();
    let mut peer = H2Client::default();
    let (stream_id, headers) = open_stream_bytes(
        &mut peer,
        "POST",
        "https",
        "example.com",
        "/upload",
        &[],
        false,
    )
    .unwrap();
    server.accept_event(&headers).unwrap();
    server.close_stream(stream_id);

    for (frame, flow_control_len) in h2_discarded_data_inputs(stream_id) {
        let (event, consumed, output) = server.accept_event_ref(&frame).unwrap();
        assert_eq!(consumed, frame.len());
        assert!(output.is_empty());
        let event = event.expect("discarded DATA event");
        assert!(matches!(
            event,
            H2StreamEvent::DiscardedData {
                stream_id: id,
                flow_control_len: len,
            } if id == stream_id && len == flow_control_len
        ));
        assert_eq!(
            event.into_owned(),
            H2StreamEvent::DiscardedData {
                stream_id,
                flow_control_len,
            }
        );

        let (event, consumed, output) = server.accept_event(&frame).unwrap();
        assert_eq!(consumed, frame.len());
        assert!(output.is_empty());
        assert_eq!(
            event,
            Some(H2StreamEvent::DiscardedData {
                stream_id,
                flow_control_len,
            })
        );

        let (request, consumed, output) = server.accept(&frame).unwrap();
        assert!(request.is_none());
        assert_eq!(consumed, frame.len());
        assert!(output.is_empty());
    }
}

#[test]
fn h2_client_discarded_data_preserves_borrowed_and_owned_contracts() {
    let mut client = h2_client_after_server_settings();
    let (stream_id, _) = open_stream_bytes(
        &mut client,
        "GET",
        "https",
        "example.com",
        "/download",
        &[],
        false,
    )
    .unwrap();
    let mut peer = H2Server::default();
    let headers = response_headers_bytes(&mut peer, stream_id, 200, &[], false);
    client.accept(&headers).unwrap();
    client.close_stream(stream_id);

    for (frame, flow_control_len) in h2_discarded_data_inputs(stream_id) {
        let (event, consumed, output) = client.accept_ref(&frame).unwrap();
        assert_eq!(consumed, frame.len());
        assert!(output.is_empty());
        let event = event.expect("discarded DATA event");
        assert!(matches!(
            event,
            H2ClientEvent::DiscardedData {
                stream_id: id,
                flow_control_len: len,
            } if id == stream_id && len == flow_control_len
        ));
        assert_eq!(
            event.into_owned(),
            H2ClientEvent::DiscardedData {
                stream_id,
                flow_control_len,
            }
        );

        let (event, consumed, output) = client.accept(&frame).unwrap();
        assert_eq!(consumed, frame.len());
        assert!(output.is_empty());
        assert_eq!(
            event,
            Some(H2ClientEvent::DiscardedData {
                stream_id,
                flow_control_len,
            })
        );
    }
}

#[test]
fn h2_client_ignores_rst_stream_for_known_closed_stream() {
    let mut client = h2_client_after_server_settings();
    let (stream_id, _) =
        open_stream_bytes(&mut client, "GET", "https", "example.com", "/", &[], true).unwrap();
    let mut h2 = H2Server::default();
    let response = response_headers_bytes(&mut h2, stream_id, 200, &[], true);
    client.accept(&response).unwrap();
    let mut reset = Vec::new();
    H2Frame {
        frame_type: H2FrameType::RstStream,
        flags: 0,
        stream_id,
        payload: 8_u32.to_be_bytes().to_vec(),
    }
    .encode(&mut reset);

    assert_eq!(client.accept(&reset), Ok((None, reset.len(), Vec::new())));
}

#[test]
fn h2_closed_stream_tombstones_are_bounded() {
    let mut server = H2Server::with_limits(H2Limits {
        max_closed_stream_tombstones: 1,
        ..H2Limits::default()
    })
    .unwrap();
    let mut client = H2Client::default();
    server.accept_event(&client.connection_preface()).unwrap();
    for path in ["/one", "/two"] {
        let (_stream_id, headers) =
            open_stream_bytes(&mut client, "GET", "https", "example.com", path, &[], true).unwrap();
        server.accept_event(&headers).unwrap();
    }

    server.finish_response_stream(1);
    server.finish_response_stream(3);
    server.assert_stream_invariants();
    assert_eq!(server.endpoint.tombstones.len(), 1);
    assert!(!server.endpoint.tombstones.contains_key(&1));
    assert!(server.endpoint.tombstones.contains_key(&3));
}

#[test]
fn h2_zero_tombstone_capacity_does_not_retain_reset_tolerance() {
    let limits = H2Limits {
        max_closed_stream_tombstones: 0,
        ..H2Limits::default()
    };

    let mut server = H2Server::with_limits(limits).unwrap();
    let mut peer = H2Client::default();
    server.accept_event(&peer.connection_preface()).unwrap();
    let (server_stream, headers) =
        open_stream_bytes(&mut peer, "POST", "https", "example.com", "/", &[], false).unwrap();
    server.accept_event(&headers).unwrap();
    server.accept_event(&h2_reset_frame(server_stream)).unwrap();
    server.close_stream(server_stream.saturating_add(2));
    server.assert_stream_invariants();
    assert!(server.endpoint.tombstones.is_empty());
    assert!(server.endpoint.closed_stream_order.is_empty());

    let mut client = H2Client::with_limits(limits).unwrap();
    client.accept(&h2_settings_frame(&[])).unwrap();
    let (reset_stream, _) =
        open_stream_bytes(&mut client, "POST", "https", "example.com", "/", &[], false).unwrap();
    client.accept(&h2_reset_frame(reset_stream)).unwrap();
    let (closed_stream, _) =
        open_stream_bytes(&mut client, "POST", "https", "example.com", "/", &[], false).unwrap();
    client.close_stream(closed_stream);
    assert!(client.endpoint.tombstones.is_empty());
    assert!(client.endpoint.closed_stream_order.is_empty());
}

#[test]
fn h2_active_stream_limit_counts_end_stream_requests_until_response_finishes() {
    let mut server = H2Server::with_limits(H2Limits {
        max_active_streams: 1,
        ..H2Limits::default()
    })
    .unwrap();
    let mut client = H2Client::default();
    server.accept_event(&client.connection_preface()).unwrap();

    let (_first, headers) = open_stream_bytes(
        &mut client,
        "GET",
        "https",
        "example.com",
        "/one",
        &[],
        true,
    )
    .unwrap();
    server.accept_event(&headers).unwrap();
    let (_second, headers) = open_stream_bytes(
        &mut client,
        "GET",
        "https",
        "example.com",
        "/two",
        &[],
        true,
    )
    .unwrap();

    let (event, _, output) = server.accept_event(&headers).unwrap();
    assert_eq!(event, None);
    let (reset, consumed) = H2Frame::decode(&output).unwrap();
    assert_eq!(consumed, output.len());
    assert_eq!(reset.frame_type, H2FrameType::RstStream);
    assert_eq!(reset.stream_id, 3);
    assert_eq!(
        reset.payload,
        H2ErrorCode::RefusedStream.as_u32().to_be_bytes()
    );

    server.finish_response_stream(1);
    let (_third, headers) = open_stream_bytes(
        &mut client,
        "GET",
        "https",
        "example.com",
        "/three",
        &[],
        true,
    )
    .unwrap();
    assert!(server.accept_event(&headers).is_ok());
}

#[test]
fn h2_network_owners_accept_explicit_active_stream_limits_above_default() {
    let limits = H2Limits {
        max_active_streams: H2_DEFAULT_MAX_ACTIVE_STREAMS + 1,
        ..H2Limits::default()
    };

    assert!(H2Server::with_limits(limits).is_ok());
    assert!(H2Client::with_limits(limits).is_ok());
}

#[test]
fn h2_default_and_explicit_default_construction_match_startup_state() {
    let window = H2Settings::default().initial_window_size;
    let mut default_client = H2Client::default();
    let mut explicit_client = H2Client::with_local_flow_control_and_http_limits(
        window,
        window,
        H2Limits::default(),
        HttpLimits::default(),
    )
    .unwrap();

    assert_eq!(default_client.settings(), explicit_client.settings());
    assert_eq!(
        default_client.local_settings_state(),
        explicit_client.local_settings_state()
    );
    assert_eq!(
        default_client.connection_preface(),
        explicit_client.connection_preface()
    );
    assert!(default_client.connection_preface().is_empty());
    assert!(explicit_client.connection_preface().is_empty());

    let mut peer = H2Client::default();
    let preface = peer.connection_preface();
    let mut default_server = H2Server::default();
    let mut explicit_server = H2Server::with_local_flow_control_and_http_limits(
        window,
        window,
        H2Limits::default(),
        HttpLimits::default(),
    )
    .unwrap();

    assert_eq!(default_server.settings(), explicit_server.settings());
    assert_eq!(
        default_server.accept_event(&preface),
        explicit_server.accept_event(&preface)
    );
}

#[test]
fn h2_client_enforces_shared_header_and_body_limits() {
    let count_limits = HttpLimits::new()
        .set_max_header_bytes(usize::MAX)
        .set_max_headers(0);
    let mut client = H2Client::with_local_flow_control_and_http_limits(
        65_535,
        65_535,
        H2Limits::from_http_limits(count_limits),
        count_limits,
    )
    .unwrap();
    let count_error = client
        .open_stream_with_raw_headers(
            "GET",
            "http",
            "x",
            "/",
            &[H2HeaderField::new(b"x", b"y")],
            true,
        )
        .unwrap_err();
    assert_eq!(count_error.classify().kind(), HttpErrorKind::TooManyHeaders);
    assert_eq!(count_error.classify().limit().unwrap().limit(), 0);
    assert_eq!(count_error.classify().limit().unwrap().actual(), Some(1));

    let byte_limits = HttpLimits::new()
        .set_max_header_bytes(165)
        .set_max_headers(usize::MAX);
    let mut client = H2Client::with_local_flow_control_and_http_limits(
        65_535,
        65_535,
        H2Limits::from_http_limits(byte_limits),
        byte_limits,
    )
    .unwrap();
    let byte_error = client
        .open_stream_with_raw_headers("GET", "http", "x", "/", &[], true)
        .unwrap_err();
    assert_eq!(byte_error.classify().kind(), HttpErrorKind::HeadersTooLarge);
    assert_eq!(byte_error.classify().limit().unwrap().limit(), 165);
    assert_eq!(byte_error.classify().limit().unwrap().actual(), Some(166));

    let body_limits = HttpLimits::new()
        .set_max_header_bytes(usize::MAX)
        .set_max_body_bytes(3);
    let mut client = H2Client::with_local_flow_control_and_http_limits(
        65_535,
        65_535,
        H2Limits::from_http_limits(body_limits),
        body_limits,
    )
    .unwrap();
    let body_error = client
        .open_stream_with_raw_headers(
            "POST",
            "http",
            "x",
            "/",
            &[H2HeaderField::new(b"content-length", b"4")],
            false,
        )
        .unwrap_err();
    assert_eq!(body_error.classify().kind(), HttpErrorKind::BodyTooLarge);
    assert_eq!(body_error.classify().limit().unwrap().limit(), 3);
    assert_eq!(body_error.classify().limit().unwrap().actual(), Some(4));
}

#[test]
fn h2_server_enforces_explicit_active_stream_limit_above_default() {
    let mut server = H2Server::with_limits(H2Limits {
        max_active_streams: H2_DEFAULT_MAX_ACTIVE_STREAMS + 1,
        ..H2Limits::default()
    })
    .unwrap();
    let mut client = H2Client::with_limits(H2Limits {
        max_active_streams: H2_DEFAULT_MAX_ACTIVE_STREAMS + 2,
        ..H2Limits::default()
    })
    .unwrap();
    server.accept_event(&client.connection_preface()).unwrap();

    for index in 0..=H2_DEFAULT_MAX_ACTIVE_STREAMS {
        let (_stream_id, headers) = open_stream_bytes(
            &mut client,
            "GET",
            "https",
            "example.com",
            &format!("/stream/{index}"),
            &[],
            true,
        )
        .unwrap();
        assert!(server.accept_event(&headers).is_ok());
    }
    let (_stream_id, headers) = open_stream_bytes(
        &mut client,
        "GET",
        "https",
        "example.com",
        "/over-limit",
        &[],
        true,
    )
    .unwrap();

    let (event, _, output) = server.accept_event(&headers).unwrap();
    assert_eq!(event, None);
    let (reset, consumed) = H2Frame::decode(&output).unwrap();
    assert_eq!(consumed, output.len());
    assert_eq!(reset.frame_type, H2FrameType::RstStream);
    assert_eq!(
        reset.stream_id,
        2 * H2_DEFAULT_MAX_ACTIVE_STREAMS as u32 + 3
    );
    assert_eq!(
        reset.payload,
        H2ErrorCode::RefusedStream.as_u32().to_be_bytes()
    );
}

#[test]
fn h2_client_enforces_its_local_active_stream_limit() {
    let mut client = H2Client::with_limits(H2Limits {
        max_active_streams: 1,
        ..H2Limits::default()
    })
    .unwrap();

    assert!(
        open_stream_bytes(
            &mut client,
            "POST",
            "https",
            "example.test",
            "/one",
            &[],
            false,
        )
        .is_ok()
    );
    assert_eq!(
        open_stream_bytes(
            &mut client,
            "POST",
            "https",
            "example.test",
            "/two",
            &[],
            false,
        )
        .unwrap_err(),
        ServerError::InvalidFrame
    );
}

#[test]
fn h2_rst_stream_cancels_response_active_stream_after_tombstone_eviction() {
    let mut server = H2Server::with_limits(H2Limits {
        max_closed_stream_tombstones: 1,
        ..H2Limits::default()
    })
    .unwrap();
    let mut client = H2Client::default();
    server.accept_event(&client.connection_preface()).unwrap();
    for path in ["/one", "/two"] {
        let (_stream_id, headers) =
            open_stream_bytes(&mut client, "GET", "https", "example.com", path, &[], true).unwrap();
        server.accept_event(&headers).unwrap();
    }
    server.assert_stream_invariants();
    assert!(!server.endpoint.tombstones.contains_key(&1));
    assert!(server.has_outbound(1));

    let (event, _, _) = server.accept_event(&h2_reset_frame(1)).unwrap();

    assert_eq!(
        event,
        Some(H2StreamEvent::Reset {
            stream_id: 1,
            error_code: 8,
        })
    );
    server.assert_stream_invariants();
    assert!(!server.has_outbound(1));
    assert!(!server.endpoint.streams.contains_key(&1));
}

#[test]
fn h2_server_stream_record_keeps_halves_and_tombstones_disjoint() {
    let mut server = H2Server::with_limits(H2Limits {
        max_closed_stream_tombstones: 1,
        ..H2Limits::default()
    })
    .unwrap();
    let mut client = H2Client::default();
    server.accept_event(&client.connection_preface()).unwrap();
    server.assert_stream_invariants();

    let (stream_id, headers) = open_stream_bytes(
        &mut client,
        "POST",
        "https",
        "example.com",
        "/body",
        &[],
        false,
    )
    .unwrap();
    server.accept_event(&headers).unwrap();
    server.assert_stream_invariants();
    assert!(server.has_inbound(stream_id));
    assert!(server.has_outbound(stream_id));
    assert!(server.endpoint.tombstones.is_empty());

    server
        .accept_event(&client.data_frame(stream_id, b"ab", false))
        .unwrap();
    server.assert_stream_invariants();
    assert!(server.has_inbound(stream_id));

    server
        .accept_event(&trailers_bytes(
            &mut client,
            stream_id,
            &[Header::new("x-trailer", "1")],
        ))
        .unwrap();
    server.assert_stream_invariants();
    assert!(!server.has_inbound(stream_id));
    assert!(server.has_outbound(stream_id));

    server.finish_response_stream(stream_id);
    server.assert_stream_invariants();
    assert!(!server.endpoint.streams.contains_key(&stream_id));
    assert!(server.endpoint.tombstones.contains_key(&stream_id));

    let (peer_reset, headers) = open_stream_bytes(
        &mut client,
        "POST",
        "https",
        "example.com",
        "/peer-reset",
        &[],
        false,
    )
    .unwrap();
    server.accept_event(&headers).unwrap();
    server.accept_event(&h2_reset_frame(peer_reset)).unwrap();
    server.assert_stream_invariants();
    assert!(!server.endpoint.streams.contains_key(&peer_reset));
    assert!(!server.is_reset_tolerant(peer_reset));

    let (local_reset, headers) = open_stream_bytes(
        &mut client,
        "POST",
        "https",
        "example.com",
        "/local-reset",
        &[],
        false,
    )
    .unwrap();
    server.accept_event(&headers).unwrap();
    server.close_stream(local_reset);
    server.assert_stream_invariants();
    assert!(!server.endpoint.streams.contains_key(&local_reset));
    assert!(server.is_reset_tolerant(local_reset) || server.endpoint.tombstones.is_empty());

    let (windowed, headers) = open_stream_bytes(
        &mut client,
        "GET",
        "https",
        "example.com",
        "/window",
        &[],
        true,
    )
    .unwrap();
    server.accept_event(&headers).unwrap();
    let before = server.outbound(windowed).unwrap().window.available();
    let mut settings = Vec::new();
    let mut payload = Vec::new();
    H2Settings::encode_payload(
        &[H2Setting::new(H2SettingId::InitialWindowSize, 1024)],
        &mut payload,
    );
    H2Frame {
        frame_type: H2FrameType::Settings,
        flags: 0,
        stream_id: 0,
        payload,
    }
    .encode(&mut settings);
    server.accept_event(&settings).unwrap();
    server.assert_stream_invariants();
    let after = server.outbound(windowed).unwrap().window.available();
    assert_ne!(before, after);

    server.finish_response_stream(windowed);
    server.assert_stream_invariants();
    assert_eq!(server.endpoint.tombstones.len(), 1);
    assert!(server.endpoint.tombstones.contains_key(&windowed));

    let mut malformed = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Headers,
        flags: 0x5,
        stream_id: windowed + 2,
        payload: encode_hpack_raw_header_block(&[
            H2RawHeader::new(":method", "GET"),
            H2RawHeader::new(":scheme", "https"),
            H2RawHeader::new(":authority", "example.com"),
        ]),
    }
    .encode(&mut malformed);
    let _ = server.accept_event(&malformed);
    server.assert_stream_invariants();
}

#[test]
fn h2_client_stream_record_keeps_halves_and_tombstones_disjoint() {
    let mut client = H2Client::with_limits(H2Limits {
        max_closed_stream_tombstones: 1,
        ..H2Limits::default()
    })
    .unwrap();
    client.accept(&h2_settings_frame(&[])).unwrap();
    client.assert_stream_invariants();

    let (ended, _) = open_stream_bytes(
        &mut client,
        "POST",
        "https",
        "example.com",
        "/end-first",
        &[],
        false,
    )
    .unwrap();
    client.assert_stream_invariants();
    assert!(client.has_send(ended));
    assert!(client.is_awaiting_head(ended));

    let plan = client
        .prepare_data_frame(ended, 0, true)
        .unwrap()
        .expect("empty END_STREAM DATA");
    client.commit_data_frame(plan).unwrap();
    client.assert_stream_invariants();
    assert!(!client.has_send(ended));
    assert!(client.is_awaiting_head(ended));
    assert!(client.send_capacity(ended, 1).is_err());

    client
        .accept(&client_response_headers(ended, 100, false))
        .unwrap();
    client.assert_stream_invariants();
    assert!(client.is_awaiting_head(ended));

    client
        .accept(&client_response_headers(ended, 200, false))
        .unwrap();
    client.assert_stream_invariants();
    assert!(client.has_receive_body(ended));
    assert!(!client.is_awaiting_head(ended));

    client
        .accept(&client_data_bytes(ended, b"ok", false))
        .unwrap();
    client.assert_stream_invariants();
    assert!(client.has_receive_body(ended));

    client.accept(&client_data_bytes(ended, b"", true)).unwrap();
    client.assert_stream_invariants();
    assert!(!client.endpoint.streams.contains_key(&ended));
    assert_eq!(
        client.endpoint.tombstones.get(&ended),
        Some(&H2StreamTombstone::Closed)
    );

    let (early, _) = open_stream_bytes(
        &mut client,
        "POST",
        "https",
        "example.com",
        "/early",
        &[],
        false,
    )
    .unwrap();
    client.assert_stream_invariants();
    assert!(client.has_send(early));
    client
        .accept(&client_response_headers(early, 200, true))
        .unwrap();
    client.assert_stream_invariants();
    assert!(!client.endpoint.streams.contains_key(&early));
    assert_eq!(
        client.endpoint.tombstones.get(&early),
        Some(&H2StreamTombstone::Closed)
    );
    assert_eq!(client.endpoint.tombstones.len(), 1);

    let (awaiting_reset, _) = open_stream_bytes(
        &mut client,
        "POST",
        "https",
        "example.com",
        "/awaiting-reset",
        &[],
        false,
    )
    .unwrap();
    client.accept(&h2_reset_frame(awaiting_reset)).unwrap();
    client.assert_stream_invariants();
    assert!(!client.endpoint.streams.contains_key(&awaiting_reset));
    assert!(client.is_reset_tolerant(awaiting_reset));

    let (body_reset, _) = open_stream_bytes(
        &mut client,
        "POST",
        "https",
        "example.com",
        "/body-reset",
        &[],
        true,
    )
    .unwrap();
    client
        .accept(&client_response_headers(body_reset, 200, false))
        .unwrap();
    client.assert_stream_invariants();
    assert!(client.has_receive_body(body_reset));
    client.accept(&h2_reset_frame(body_reset)).unwrap();
    client.assert_stream_invariants();
    assert!(!client.endpoint.streams.contains_key(&body_reset));
    assert!(client.is_reset_tolerant(body_reset));

    let (local, _) = open_stream_bytes(
        &mut client,
        "POST",
        "https",
        "example.com",
        "/local",
        &[],
        false,
    )
    .unwrap();
    client.close_stream(local);
    client.assert_stream_invariants();
    assert!(!client.endpoint.streams.contains_key(&local));
    assert!(client.is_reset_tolerant(local) || client.endpoint.tombstones.is_empty());

    let (windowed, _) = open_stream_bytes(
        &mut client,
        "POST",
        "https",
        "example.com",
        "/window",
        &[],
        false,
    )
    .unwrap();
    let before = client.send_window_available(windowed).unwrap();
    client
        .accept(&h2_settings_frame(&[H2Setting::new(
            H2SettingId::InitialWindowSize,
            1024,
        )]))
        .unwrap();
    client.assert_stream_invariants();
    let after = client.send_window_available(windowed).unwrap();
    assert_ne!(before, after);

    client.close_stream(windowed);
    client.assert_stream_invariants();
    assert_eq!(client.endpoint.tombstones.len(), 1);
    assert!(client.endpoint.tombstones.contains_key(&windowed));
}

#[test]
fn h2_server_accept_and_classify_share_one_dispatcher() {
    let mut emit = h2_server_after_preface();
    let mut suppress = h2_server_after_preface();
    assert_server_protocol_eq(&emit, &suppress);

    let settings = decode_single_frame(&h2_settings_frame(&[]));
    let (accept_event, classify_event, accept_output) =
        dispatch_pair(&mut emit, &mut suppress, settings);
    assert_eq!(accept_event, classify_event);
    assert_single_control_frame(&accept_output, H2FrameType::Settings, 0x1);
    assert_server_protocol_eq(&emit, &suppress);

    let settings_ack = decode_single_frame(&h2_settings_ack_frame());
    let (accept_event, classify_event, accept_output) =
        dispatch_pair(&mut emit, &mut suppress, settings_ack);
    assert_eq!(accept_event, classify_event);
    assert!(accept_output.is_empty());
    assert_server_protocol_eq(&emit, &suppress);

    let ping = decode_single_frame(&h2_ping_frame());
    let (accept_event, classify_event, accept_output) =
        dispatch_pair(&mut emit, &mut suppress, ping);
    assert_eq!(accept_event, classify_event);
    assert_single_control_frame(&accept_output, H2FrameType::Ping, 0x1);
    assert_server_protocol_eq(&emit, &suppress);

    let ping_ack = decode_single_frame(&h2_ping_ack_frame(&[0; 8]));
    let (accept_event, classify_event, accept_output) =
        dispatch_pair(&mut emit, &mut suppress, ping_ack);
    assert_eq!(accept_event, classify_event);
    assert!(accept_output.is_empty());
    assert_server_protocol_eq(&emit, &suppress);

    let window = decode_single_frame(&h2_window_update_frame(0, 1));
    let (accept_event, classify_event, accept_output) =
        dispatch_pair(&mut emit, &mut suppress, window);
    assert_eq!(accept_event, classify_event);
    assert!(accept_output.is_empty());
    assert_server_protocol_eq(&emit, &suppress);

    let goaway = decode_single_frame(&h2_goaway_frame(0));
    let (accept_event, classify_event, accept_output) =
        dispatch_pair(&mut emit, &mut suppress, goaway);
    assert_eq!(accept_event, classify_event);
    assert!(accept_output.is_empty());
    assert_server_protocol_eq(&emit, &suppress);

    let unknown = H2Frame {
        frame_type: H2FrameType::Unknown(0xfe),
        flags: 0,
        stream_id: 0,
        payload: Vec::new(),
    };
    let (accept_event, classify_event, accept_output) =
        dispatch_pair(&mut emit, &mut suppress, unknown);
    assert_eq!(accept_event, classify_event);
    assert!(accept_output.is_empty());
    assert_server_protocol_eq(&emit, &suppress);

    let mut emit = h2_server_after_preface();
    let mut suppress = h2_server_after_preface();
    let mut client = H2Client::default();
    let (_, headers) = open_stream_bytes(
        &mut client,
        "POST",
        "https",
        "example.com",
        "/parity",
        &[],
        false,
    )
    .unwrap();
    let headers = decode_single_frame(&headers);
    let (accept_event, classify_event, accept_output) =
        dispatch_pair(&mut emit, &mut suppress, headers);
    assert_eq!(accept_event, classify_event);
    assert!(accept_output.is_empty());
    assert_server_protocol_eq(&emit, &suppress);

    let data = H2Frame {
        frame_type: H2FrameType::Data,
        flags: 0,
        stream_id: 1,
        payload: b"ab".to_vec(),
    };
    let (accept_event, classify_event, accept_output) =
        dispatch_pair(&mut emit, &mut suppress, data);
    assert_eq!(accept_event, classify_event);
    assert!(accept_output.is_empty());
    assert_server_protocol_eq(&emit, &suppress);

    let stream_window = decode_single_frame(&h2_window_update_frame(1, 1));
    let (accept_event, classify_event, accept_output) =
        dispatch_pair(&mut emit, &mut suppress, stream_window);
    assert_eq!(accept_event, classify_event);
    assert!(accept_output.is_empty());
    assert_server_protocol_eq(&emit, &suppress);

    let priority = decode_single_frame(&h2_priority_frame(1));
    let (accept_event, classify_event, accept_output) =
        dispatch_pair(&mut emit, &mut suppress, priority);
    assert_eq!(accept_event, classify_event);
    assert!(accept_output.is_empty());
    assert_server_protocol_eq(&emit, &suppress);

    let reset = decode_single_frame(&h2_reset_frame(1));
    let (accept_event, classify_event, accept_output) =
        dispatch_pair(&mut emit, &mut suppress, reset);
    assert_eq!(accept_event, classify_event);
    assert!(accept_output.is_empty());
    assert_server_protocol_eq(&emit, &suppress);

    let mut emit = h2_server_after_preface();
    let mut suppress = h2_server_after_preface();
    let block = encode_hpack_request_headers("GET", "https", "example.com", "/", &[]);
    let first = H2Frame {
        frame_type: H2FrameType::Headers,
        flags: 0,
        stream_id: 1,
        payload: block[..1].to_vec(),
    };
    let (accept_event, classify_event, accept_output) =
        dispatch_pair(&mut emit, &mut suppress, first);
    assert_eq!(accept_event, classify_event);
    assert!(accept_output.is_empty());
    assert_server_protocol_eq(&emit, &suppress);

    let rest = H2Frame {
        frame_type: H2FrameType::Continuation,
        flags: 0x4,
        stream_id: 1,
        payload: block[1..].to_vec(),
    };
    let (accept_event, classify_event, accept_output) =
        dispatch_pair(&mut emit, &mut suppress, rest);
    assert_eq!(accept_event, classify_event);
    assert!(accept_output.is_empty());
    assert_server_protocol_eq(&emit, &suppress);

    let mut emit = h2_server_after_preface();
    let mut suppress = h2_server_after_preface();
    let push = H2Frame {
        frame_type: H2FrameType::PushPromise,
        flags: 0x4,
        stream_id: 1,
        payload: vec![0, 0, 0, 2],
    };
    let (accept_event, classify_event, accept_output) =
        dispatch_pair(&mut emit, &mut suppress, push);
    assert!(matches!(accept_event, H2FrameOutcome::Error(_)));
    assert_eq!(accept_event, classify_event);
    assert!(accept_output.is_empty());
    assert_server_protocol_eq(&emit, &suppress);

    let mut emit = h2_server_after_preface();
    let mut suppress = h2_server_after_preface();
    assert!(!emit.begin_graceful_shutdown().unwrap().is_empty());
    assert!(!suppress.begin_graceful_shutdown().unwrap().is_empty());
    assert_server_protocol_eq(&emit, &suppress);
    let graceful = decode_single_frame(&h2_ping_ack_frame(&H2_GRACEFUL_SHUTDOWN_PING_PAYLOAD));
    let (accept_event, classify_event, accept_output) =
        dispatch_pair(&mut emit, &mut suppress, graceful);
    assert_eq!(accept_event, classify_event);
    assert_single_control_frame(&accept_output, H2FrameType::Goaway, 0);
    assert!(matches!(
        emit.endpoint.outbound_shutdown,
        H2OutboundShutdown::GoawaySent { .. }
    ));
    assert_eq!(
        suppress.endpoint.outbound_shutdown,
        H2OutboundShutdown::GracefulPingPending
    );
    assert_eq!(
        emit.control_diagnostics().goaways,
        suppress.control_diagnostics().goaways + 1
    );
}

#[test]
fn h2_rejects_bad_preface_and_frame() {
    let mut server = H2Server::default();
    assert_eq!(server.accept(b"bad"), Err(ServerError::InvalidPreface));
    let mut server = H2Server::default();
    assert_eq!(
        server.accept(&CLIENT_PREFACE[..3]),
        Ok((None, 0, Vec::new()))
    );
    let mut input = b"bad bad bad bad bad bad bad!".to_vec();
    input.truncate(CLIENT_PREFACE.len());
    assert_eq!(server.accept(&input), Err(ServerError::InvalidPreface));
}

#[test]
fn h2_rejects_unexpected_frame_after_settings() {
    let mut input = Vec::new();
    input.extend_from_slice(CLIENT_PREFACE);
    H2Frame {
        frame_type: H2FrameType::Settings,
        flags: 0,
        stream_id: 0,
        payload: Vec::new(),
    }
    .encode(&mut input);
    H2Frame {
        frame_type: H2FrameType::Data,
        flags: 0,
        stream_id: 1,
        payload: b"unexpected".to_vec(),
    }
    .encode(&mut input);

    let mut server = H2Server::default();
    assert_eq!(server.accept(&input), Err(ServerError::InvalidFrame));
}

#[test]
fn h2_server_decoded_frame_paths_require_initial_settings() {
    let mut server = H2Server::default();
    let headers = H2Frame {
        frame_type: H2FrameType::Headers,
        flags: 0x5,
        stream_id: 1,
        payload: encode_hpack_request_headers("GET", "https", "example.com", "/", &[]),
    };

    let H2FrameOutcome::Error(error) = server.accept_frame_typed(headers).0 else {
        panic!("expected decoded HEADERS before SETTINGS to fail");
    };
    assert_eq!(error.scope, H2ErrorScope::Connection);
    assert_eq!(error.code, H2ErrorCode::ProtocolError);

    let mut server = H2Server::default();
    assert_eq!(
        server.accept_complete_header_block(
            1,
            0x5,
            &encode_hpack_request_headers("GET", "https", "example.com", "/", &[])
        ),
        Err(ServerError::InvalidFrame)
    );

    let mut server = H2Server::default();
    assert!(matches!(
        server.classify_frame_typed(H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0x5,
            stream_id: 1,
            payload: encode_hpack_request_headers("GET", "https", "example.com", "/", &[]),
        }),
        H2FrameOutcome::Error(H2ProtocolError {
            scope: H2ErrorScope::Connection,
            code: H2ErrorCode::ProtocolError,
            ..
        })
    ));
}

#[test]
fn h2_client_data_plans_follow_stream_settings_and_window_updates() {
    let mut client = H2Client::default();
    client
        .accept(&h2_settings_frame(&[H2Setting::new(
            H2SettingId::InitialWindowSize,
            5,
        )]))
        .unwrap();
    let (stream_id, _) =
        open_stream_bytes(&mut client, "POST", "http", "example.com", "/", &[], false).unwrap();

    let plan = client
        .prepare_data_frame(stream_id, 10, true)
        .unwrap()
        .unwrap();
    assert_eq!(plan.payload_len(), 5);
    assert!(!plan.end_stream());
    assert_eq!(plan.header(), &[0, 0, 5, 0, 0, 0, 0, 0, 1]);
    client.commit_data_frame(plan).unwrap();
    assert_eq!(client.prepare_data_frame(stream_id, 5, true).unwrap(), None);

    let update = H2Frame {
        frame_type: H2FrameType::WindowUpdate,
        flags: 0,
        stream_id,
        payload: 3_u32.to_be_bytes().to_vec(),
    };
    client.accept_frame_bytes(update).unwrap();
    assert_eq!(
        client
            .prepare_data_frame(stream_id, 5, true)
            .unwrap()
            .unwrap()
            .payload_len(),
        3
    );

    let settings = H2Frame {
        frame_type: H2FrameType::Settings,
        flags: 0,
        stream_id: 0,
        payload: {
            let mut payload = Vec::new();
            H2Settings::encode_payload(
                &[H2Setting::new(H2SettingId::InitialWindowSize, 2)],
                &mut payload,
            );
            payload
        },
    };
    client.accept_frame_bytes(settings).unwrap();
    assert_eq!(
        client.send_capacity(stream_id, 5).unwrap().sendable_bytes,
        0
    );
}

#[test]
fn h2_client_data_plans_enforce_connection_window_and_zero_length_end_stream() {
    let mut client = H2Client::default();
    client
        .accept(&h2_settings_frame(&[H2Setting::new(
            H2SettingId::InitialWindowSize,
            100_000,
        )]))
        .unwrap();
    let (stream_id, _) =
        open_stream_bytes(&mut client, "POST", "http", "example.com", "/", &[], false).unwrap();
    let mut sent = 0;
    while let Some(plan) = client
        .prepare_data_frame(stream_id, 100_000 - sent, false)
        .unwrap()
    {
        sent += plan.payload_len();
        client.commit_data_frame(plan).unwrap();
    }
    assert_eq!(sent, 65_535);
    assert!(
        client
            .send_capacity(stream_id, 1)
            .unwrap()
            .connection_window_blocked
    );

    client
        .accept_frame_bytes(H2Frame {
            frame_type: H2FrameType::WindowUpdate,
            flags: 0,
            stream_id: 0,
            payload: 1_u32.to_be_bytes().to_vec(),
        })
        .unwrap();
    assert_eq!(
        client.send_capacity(stream_id, 1).unwrap().sendable_bytes,
        1
    );

    let mut empty_client = h2_client_after_server_settings();
    let (empty_stream, _) = open_stream_bytes(
        &mut empty_client,
        "POST",
        "http",
        "example.com",
        "/",
        &[],
        false,
    )
    .unwrap();
    let plan = empty_client
        .prepare_data_frame(empty_stream, 0, true)
        .unwrap()
        .unwrap();
    assert_eq!(plan.header(), &[0, 0, 0, 0, 1, 0, 0, 0, 1]);
    empty_client.commit_data_frame(plan).unwrap();
    assert_eq!(
        empty_client.send_capacity(empty_stream, 1),
        Err(ServerError::FlowControlViolation)
    );
}

#[test]
fn h2_window_update_overflow_is_a_scoped_flow_control_error() {
    let mut client = h2_client_after_server_settings();
    let outcome = client
        .accept_frame_bytes_typed(H2Frame {
            frame_type: H2FrameType::WindowUpdate,
            flags: 0,
            stream_id: 0,
            payload: 0x7fff_ffff_u32.to_be_bytes().to_vec(),
        })
        .0;
    assert!(matches!(
        outcome,
        H2FrameOutcome::Error(H2ProtocolError {
            scope: H2ErrorScope::Connection,
            code: H2ErrorCode::FlowControlError,
            ..
        })
    ));
}

#[test]
fn h2_window_update_distinguishes_unknown_and_known_without_send_half() {
    let window_update = |stream_id| H2Frame {
        frame_type: H2FrameType::WindowUpdate,
        flags: 0,
        stream_id,
        payload: 1_u32.to_be_bytes().to_vec(),
    };

    let mut server = h2_server_after_preface();
    assert!(matches!(
        server.accept_frame_bytes_typed(window_update(1)).0,
        H2FrameOutcome::Error(H2ProtocolError {
            scope: H2ErrorScope::Connection,
            code: H2ErrorCode::ProtocolError,
            ..
        })
    ));

    let mut client = h2_client_after_server_settings();
    assert_eq!(
        client.accept_frame_bytes(window_update(1)),
        Err(ServerError::InvalidFrame)
    );
    assert!(client.endpoint.last_protocol_error.is_none());

    let mut peer = H2Client::default();
    let mut server = H2Server::default();
    server.accept_event(&peer.connection_preface()).unwrap();
    let (server_stream, request) =
        open_stream_bytes(&mut peer, "POST", "http", "example.com", "/", &[], false).unwrap();
    server.accept_event(&request).unwrap();
    server.finish_response_stream(server_stream);
    assert!(!server.has_outbound(server_stream));
    assert!(matches!(
        server
            .accept_frame_bytes(window_update(server_stream))
            .unwrap()
            .0,
        Some(H2ByteStreamEvent::WindowUpdate {
            stream_id,
            increment: 1,
        }) if stream_id == server_stream
    ));

    let mut client = h2_client_after_server_settings();
    let (client_stream, _) =
        open_stream_bytes(&mut client, "POST", "http", "example.com", "/", &[], true).unwrap();
    assert!(!client.has_send(client_stream));
    assert!(matches!(
        client
            .accept_frame_bytes(window_update(client_stream))
            .unwrap()
            .0,
        H2ByteClientEvent::WindowUpdate {
            stream_id,
            increment: 1,
        } if stream_id == client_stream
    ));
}

#[test]
fn h2_settings_send_window_overflow_is_a_connection_flow_control_error() {
    let mut client = H2Client::default();
    client
        .accept(&h2_settings_frame(&[H2Setting::new(
            H2SettingId::InitialWindowSize,
            5,
        )]))
        .unwrap();
    let (stream_id, _) =
        open_stream_bytes(&mut client, "POST", "http", "example.com", "/", &[], false).unwrap();
    client
        .accept_frame_bytes(H2Frame {
            frame_type: H2FrameType::WindowUpdate,
            flags: 0,
            stream_id,
            payload: (H2_MAX_WINDOW_SIZE - 5).to_be_bytes().to_vec(),
        })
        .unwrap();

    let mut payload = Vec::new();
    H2Settings::encode_payload(
        &[H2Setting::new(H2SettingId::InitialWindowSize, 6)],
        &mut payload,
    );
    assert!(matches!(
        client
            .accept_frame_bytes_typed(H2Frame {
                frame_type: H2FrameType::Settings,
                flags: 0,
                stream_id: 0,
                payload,
            })
            .0,
        H2FrameOutcome::Error(H2ProtocolError {
            scope: H2ErrorScope::Connection,
            code: H2ErrorCode::FlowControlError,
            ..
        })
    ));
}

#[cfg(feature = "hpack-test-support")]
#[test]
fn h2_mixed_settings_overflow_preserves_all_state_for_both_roles() {
    const TABLE_SIZE: u32 = 1024;
    let mixed_settings_payload = || {
        let mut payload = Vec::new();
        H2Settings::encode_payload(
            &[
                H2Setting::new(H2SettingId::HeaderTableSize, TABLE_SIZE),
                H2Setting::new(H2SettingId::InitialWindowSize, 6),
            ],
            &mut payload,
        );
        payload
    };

    let mut client = H2Client::default();
    client
        .accept(&h2_settings_frame(&[H2Setting::new(
            H2SettingId::InitialWindowSize,
            5,
        )]))
        .unwrap();
    let (client_stream, _) =
        open_stream_bytes(&mut client, "POST", "http", "example.com", "/", &[], false).unwrap();
    client
        .accept_frame_bytes(H2Frame {
            frame_type: H2FrameType::WindowUpdate,
            flags: 0,
            stream_id: client_stream,
            payload: (H2_MAX_WINDOW_SIZE - 5).to_be_bytes().to_vec(),
        })
        .unwrap();
    let client_window = client.send(client_stream).unwrap().window.available();
    let client_settings = *client.settings();
    assert!(matches!(
        client
            .accept_frame_bytes_typed(H2Frame {
                frame_type: H2FrameType::Settings,
                flags: 0,
                stream_id: 0,
                payload: mixed_settings_payload(),
            })
            .0,
        H2FrameOutcome::Error(H2ProtocolError {
            scope: H2ErrorScope::Connection,
            code: H2ErrorCode::FlowControlError,
            ..
        })
    ));
    assert_eq!(*client.settings(), client_settings);
    assert_eq!(
        client.send(client_stream).unwrap().window.available(),
        client_window
    );
    assert_eq!(
        client
            .endpoint
            .header_codecs
            .as_ref()
            .unwrap()
            .outbound
            .test_configured_max_size(),
        client_settings.header_table_size as usize
    );

    let mut peer = H2Client::with_local_flow_control(5, 65_535).unwrap();
    let mut server = H2Server::default();
    server.accept_event(&peer.connection_preface()).unwrap();
    let (server_stream, request) =
        open_stream_bytes(&mut peer, "POST", "http", "example.com", "/", &[], false).unwrap();
    server.accept_event(&request).unwrap();
    server
        .accept_frame_bytes(H2Frame {
            frame_type: H2FrameType::WindowUpdate,
            flags: 0,
            stream_id: server_stream,
            payload: (H2_MAX_WINDOW_SIZE - 5).to_be_bytes().to_vec(),
        })
        .unwrap();
    let server_window = server.outbound(server_stream).unwrap().window.available();
    let server_settings = *server.settings();
    assert!(matches!(
        server
            .accept_frame_bytes_typed(H2Frame {
                frame_type: H2FrameType::Settings,
                flags: 0,
                stream_id: 0,
                payload: mixed_settings_payload(),
            })
            .0,
        H2FrameOutcome::Error(H2ProtocolError {
            scope: H2ErrorScope::Connection,
            code: H2ErrorCode::FlowControlError,
            ..
        })
    ));
    assert_eq!(*server.settings(), server_settings);
    assert_eq!(
        server.outbound(server_stream).unwrap().window.available(),
        server_window
    );
    assert_eq!(
        server
            .endpoint
            .header_codecs
            .as_ref()
            .unwrap()
            .outbound
            .test_configured_max_size(),
        server_settings.header_table_size as usize
    );
}

#[test]
fn h2_server_data_plans_share_response_stream_lifecycle() {
    let mut peer = H2Client::with_local_flow_control(4, 65_535).unwrap();
    let mut server = H2Server::default();
    server.accept_event(&peer.connection_preface()).unwrap();
    let (stream_id, request) =
        open_stream_bytes(&mut peer, "GET", "http", "example.com", "/", &[], true).unwrap();
    server.accept_event(&request).unwrap();

    let plan = server
        .prepare_data_frame(stream_id, 10, true)
        .unwrap()
        .unwrap();
    assert_eq!(plan.payload_len(), 4);
    assert!(!plan.end_stream());
    server.commit_data_frame(plan).unwrap();
    assert_eq!(server.prepare_data_frame(stream_id, 6, true).unwrap(), None);

    server
        .accept_frame_bytes(H2Frame {
            frame_type: H2FrameType::WindowUpdate,
            flags: 0,
            stream_id,
            payload: 6_u32.to_be_bytes().to_vec(),
        })
        .unwrap();
    let plan = server
        .prepare_data_frame(stream_id, 6, true)
        .unwrap()
        .unwrap();
    assert!(plan.end_stream());
    server.commit_data_frame(plan).unwrap();
    assert_eq!(
        server.send_capacity(stream_id, 1),
        Err(ServerError::FlowControlViolation)
    );
}

#[test]
fn h2_server_data_plans_allow_zero_length_end_stream() {
    let mut peer = H2Client::default();
    let mut server = H2Server::default();
    server.accept_event(&peer.connection_preface()).unwrap();
    let (stream_id, request) =
        open_stream_bytes(&mut peer, "GET", "http", "example.com", "/", &[], true).unwrap();
    server.accept_event(&request).unwrap();

    let plan = server
        .prepare_data_frame(stream_id, 0, true)
        .unwrap()
        .unwrap();
    assert_eq!(plan.payload_len(), 0);
    assert!(plan.end_stream());

    let frame = decode_single_frame(&server.data_frame(stream_id, &[], true));
    assert_eq!(frame.frame_type, H2FrameType::Data);
    assert_eq!(frame.flags, 0x1);
    assert!(frame.payload.is_empty());

    server.commit_data_frame(plan).unwrap();
    assert_eq!(
        server.send_capacity(stream_id, 1),
        Err(ServerError::FlowControlViolation)
    );
}

#[test]
fn h2_data_commit_errors_preserve_role_specific_side_effect_order() {
    let mut peer = H2Client::default();
    let mut server = H2Server::default();
    server.accept_event(&peer.connection_preface()).unwrap();
    let (server_stream, request) =
        open_stream_bytes(&mut peer, "GET", "http", "example.com", "/", &[], true).unwrap();
    server.accept_event(&request).unwrap();
    let server_plan = server
        .prepare_data_frame(server_stream, 1, false)
        .unwrap()
        .unwrap();
    server.outbound_mut(server_stream).unwrap().sent.limit = Some(0);
    let server_stream_window = server.outbound(server_stream).unwrap().window.available();
    let server_connection_window = server.endpoint.send_connection_window.available();
    assert!(server.commit_data_frame(server_plan).is_err());
    let server_half = server.outbound(server_stream).unwrap();
    assert_eq!(server_half.window.available(), server_stream_window - 1);
    assert_eq!(
        server.endpoint.send_connection_window.available(),
        server_connection_window - 1
    );
    assert_eq!(server_half.sent.sent, 1);

    let mut client = h2_client_after_server_settings();
    let (client_stream, _) =
        open_stream_bytes(&mut client, "POST", "http", "example.com", "/", &[], false).unwrap();
    let client_plan = client
        .prepare_data_frame(client_stream, 1, false)
        .unwrap()
        .unwrap();
    client.send_mut(client_stream).unwrap().sent.limit = Some(0);
    let client_stream_window = client.send(client_stream).unwrap().window.available();
    let client_connection_window = client.endpoint.send_connection_window.available();
    assert!(client.commit_data_frame(client_plan).is_err());
    let client_half = client.send(client_stream).unwrap();
    assert_eq!(client_half.window.available(), client_stream_window - 1);
    assert_eq!(client_half.sent.sent, 1);
    assert_eq!(
        client.endpoint.send_connection_window.available(),
        client_connection_window
    );
}

#[test]
fn h2_empty_end_stream_commit_rejects_negative_stream_windows() {
    let mut peer = H2Client::default();
    let mut server = H2Server::default();
    server.accept_event(&peer.connection_preface()).unwrap();
    let (server_stream, request) =
        open_stream_bytes(&mut peer, "GET", "http", "example.com", "/", &[], true).unwrap();
    server.accept_event(&request).unwrap();
    server
        .outbound_mut(server_stream)
        .unwrap()
        .window
        .adjust(-65_536)
        .unwrap();
    let server_plan = server
        .prepare_data_frame(server_stream, 0, true)
        .unwrap()
        .unwrap();
    assert_eq!(
        server.commit_data_frame(server_plan),
        Err(ServerError::FlowControlViolation)
    );
    assert!(server.has_outbound(server_stream));

    let mut client = h2_client_after_server_settings();
    let (client_stream, _) =
        open_stream_bytes(&mut client, "POST", "http", "example.com", "/", &[], false).unwrap();
    client
        .send_mut(client_stream)
        .unwrap()
        .window
        .adjust(-65_536)
        .unwrap();
    let client_plan = client
        .prepare_data_frame(client_stream, 0, true)
        .unwrap()
        .unwrap();
    assert_eq!(
        client.commit_data_frame(client_plan),
        Err(ServerError::FlowControlViolation)
    );
    assert!(client.has_send(client_stream));
}
