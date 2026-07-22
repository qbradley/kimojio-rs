#![cfg(feature = "hpack-test-support")]

use kimojio_fsm_http::hpack_test_support::{
    client_connection_is_terminal, client_inbound_table, client_outbound_table,
    fail_client_assembly_allocation_after, fail_client_outbound_allocation_after,
    fail_server_assembly_allocation_after, fail_server_inbound_allocation_after,
    server_connection_is_terminal, server_inbound_table, server_outbound_table,
    set_client_pending_encoded_header_block_len, set_server_pending_encoded_header_block_len,
};
use kimojio_fsm_http::{
    H2ByteClientEvent, H2ByteStreamEvent, H2Client, H2ErrorCode, H2ErrorScope, H2Frame,
    H2FrameOutcome, H2FrameType, H2HeaderBlockDecoder, H2HeaderBlockEncoder, H2HeaderField,
    H2HpackError, H2Limits, H2OutboundCommit, H2RawHeaderRef, H2Server, H2Setting, H2SettingId,
    H2Settings,
};

mod support;

use support::hpack_execution_ledger::{CompletionLedger, Runner, RunnerResult};
use support::hpack_manifest::{
    ActualEvidence, CaseInput, ConnectionEndpoint, ConnectionEvidenceV3, ConnectionOccurrence,
    ConnectionOutcome, ConnectionScenarioV3, ConnectionState, ConnectionView, ErrorCategory, Field,
    HeaderRole,
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

fn header_block(frames: &[u8]) -> Vec<u8> {
    let mut cursor = 0;
    let mut block = Vec::new();
    while cursor < frames.len() {
        let (frame, consumed) = H2Frame::decode(&frames[cursor..]).unwrap();
        cursor += consumed;
        if matches!(
            frame.frame_type,
            H2FrameType::Headers | H2FrameType::Continuation
        ) {
            block.extend_from_slice(&frame.payload);
            if frame.flags & 0x4 != 0 {
                return block;
            }
        }
    }
    panic!("no complete header block")
}

fn take_client_outbound(client: &mut H2Client, commit: H2OutboundCommit) -> Vec<u8> {
    let next = client.next_outbound_block().expect("queued client output");
    assert_eq!(next.commit(), commit);
    let bytes = next.bytes().to_vec();
    client.acknowledge_outbound_block(commit).unwrap();
    bytes
}

fn take_server_outbound(server: &mut H2Server, commit: H2OutboundCommit) -> Vec<u8> {
    let next = server.next_outbound_block().expect("queued server output");
    assert_eq!(next.commit(), commit);
    let bytes = next.bytes().to_vec();
    server.acknowledge_outbound_block(commit).unwrap();
    bytes
}

fn encoded_fields(encoder: &mut H2HeaderBlockEncoder, fields: &[H2HeaderField]) -> Vec<u8> {
    encoder.try_encode_fields(fields).unwrap()
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

fn response_fields(extra: H2HeaderField) -> Vec<H2HeaderField> {
    vec![H2HeaderField::new(b":status", b"200"), extra]
}

#[test]
fn stream_rejection_still_advances_connection_hpack_history() {
    let mut server_encoder = H2HeaderBlockEncoder::new();
    let mut server = H2Server::default();
    initialize_server(&mut server);
    let initial = request_fields(H2HeaderField::new(b"x-initial", b"value"));
    assert!(matches!(
        server
            .accept_frame_bytes(H2Frame {
                frame_type: H2FrameType::Headers,
                flags: 0x4,
                stream_id: 1,
                payload: encoded_fields(&mut server_encoder, &initial),
            })
            .unwrap()
            .0,
        Some(H2ByteStreamEvent::RequestHeaders { .. })
    ));
    let rejected = H2HeaderField::new(b"x-rejected", b"value");
    let H2FrameOutcome::Error(error) = server
        .accept_frame_bytes_typed(H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0x4,
            stream_id: 1,
            payload: encoded_fields(&mut server_encoder, std::slice::from_ref(&rejected)),
        })
        .0
    else {
        panic!("non-terminal trailers must be rejected")
    };
    assert_eq!(error.scope, H2ErrorScope::Stream(1));
    assert_eq!(error.code, H2ErrorCode::ProtocolError);
    assert!(matches!(
        server
            .accept_frame_bytes(H2Frame {
                frame_type: H2FrameType::Headers,
                flags: 0x5,
                stream_id: 3,
                payload: encoded_fields(&mut server_encoder, &request_fields(rejected.clone()),),
            })
            .unwrap()
            .0,
        Some(H2ByteStreamEvent::RequestHeaders { .. })
    ));

    let mut client_encoder = H2HeaderBlockEncoder::new();
    let mut client = H2Client::default();
    initialize_client(&mut client);
    let first_stream = client.reserve_stream().unwrap();
    assert!(matches!(
        client
            .accept_frame_bytes(H2Frame {
                frame_type: H2FrameType::Headers,
                flags: 0x4,
                stream_id: first_stream,
                payload: encoded_fields(
                    &mut client_encoder,
                    &response_fields(H2HeaderField::new(b"x-initial", b"value")),
                ),
            })
            .unwrap()
            .0,
        H2ByteClientEvent::ResponseHeaders { .. }
    ));
    let rejected = H2HeaderField::new(b"x-client-rejected", b"value");
    let H2FrameOutcome::Error(error) = client
        .accept_frame_bytes_typed(H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0x4,
            stream_id: first_stream,
            payload: encoded_fields(&mut client_encoder, std::slice::from_ref(&rejected)),
        })
        .0
    else {
        panic!("non-terminal trailers must be rejected")
    };
    assert_eq!(error.scope, H2ErrorScope::Stream(first_stream));
    assert_eq!(error.code, H2ErrorCode::ProtocolError);
    let next_stream = client.reserve_stream().unwrap();
    assert!(matches!(
        client
            .accept_frame_bytes(H2Frame {
                frame_type: H2FrameType::Headers,
                flags: 0x5,
                stream_id: next_stream,
                payload: encoded_fields(&mut client_encoder, &response_fields(rejected),),
            })
            .unwrap()
            .0,
        H2ByteClientEvent::ResponseHeaders { .. }
    ));
}

#[test]
fn canonical_http2_values_reject_nul_and_edge_whitespace_for_every_role() {
    for malformed in [
        b"\0value".as_slice(),
        b" value".as_slice(),
        b"value ".as_slice(),
        b"\tvalue".as_slice(),
        b"value\t".as_slice(),
    ] {
        let mut server_encoder = H2HeaderBlockEncoder::new();
        let mut server = H2Server::default();
        initialize_server(&mut server);
        let H2FrameOutcome::Error(error) = server
            .accept_frame_bytes_typed(H2Frame {
                frame_type: H2FrameType::Headers,
                flags: 0x5,
                stream_id: 1,
                payload: encoded_fields(
                    &mut server_encoder,
                    &request_fields(H2HeaderField::new(b"x-value", malformed)),
                ),
            })
            .0
        else {
            panic!("malformed request value must be rejected")
        };
        assert_eq!(error.scope, H2ErrorScope::Stream(1));
        assert_eq!(error.code, H2ErrorCode::ProtocolError);

        let mut client_encoder = H2HeaderBlockEncoder::new();
        let mut client = H2Client::default();
        initialize_client(&mut client);
        let stream_id = client.reserve_stream().unwrap();
        let H2FrameOutcome::Error(error) = client
            .accept_frame_bytes_typed(H2Frame {
                frame_type: H2FrameType::Headers,
                flags: 0x5,
                stream_id,
                payload: encoded_fields(
                    &mut client_encoder,
                    &response_fields(H2HeaderField::new(b"x-value", malformed)),
                ),
            })
            .0
        else {
            panic!("malformed response value must be rejected")
        };
        assert_eq!(error.scope, H2ErrorScope::Stream(stream_id));
        assert_eq!(error.code, H2ErrorCode::ProtocolError);

        let mut server_encoder = H2HeaderBlockEncoder::new();
        let mut server = H2Server::default();
        initialize_server(&mut server);
        server
            .accept_frame_bytes(H2Frame {
                frame_type: H2FrameType::Headers,
                flags: 0x4,
                stream_id: 1,
                payload: encoded_fields(
                    &mut server_encoder,
                    &request_fields(H2HeaderField::new(b"x-valid", b"value")),
                ),
            })
            .unwrap();
        let H2FrameOutcome::Error(error) = server
            .accept_frame_bytes_typed(H2Frame {
                frame_type: H2FrameType::Headers,
                flags: 0x5,
                stream_id: 1,
                payload: encoded_fields(
                    &mut server_encoder,
                    &[H2HeaderField::new(b"x-value", malformed)],
                ),
            })
            .0
        else {
            panic!("malformed request trailer value must be rejected")
        };
        assert_eq!(error.scope, H2ErrorScope::Stream(1));
        assert_eq!(error.code, H2ErrorCode::ProtocolError);

        let mut client_encoder = H2HeaderBlockEncoder::new();
        let mut client = H2Client::default();
        initialize_client(&mut client);
        let stream_id = client.reserve_stream().unwrap();
        client
            .accept_frame_bytes(H2Frame {
                frame_type: H2FrameType::Headers,
                flags: 0x4,
                stream_id,
                payload: encoded_fields(
                    &mut client_encoder,
                    &response_fields(H2HeaderField::new(b"x-valid", b"value")),
                ),
            })
            .unwrap();
        let H2FrameOutcome::Error(error) = client
            .accept_frame_bytes_typed(H2Frame {
                frame_type: H2FrameType::Headers,
                flags: 0x5,
                stream_id,
                payload: encoded_fields(
                    &mut client_encoder,
                    &[H2HeaderField::new(b"x-value", malformed)],
                ),
            })
            .0
        else {
            panic!("malformed response trailer value must be rejected")
        };
        assert_eq!(error.scope, H2ErrorScope::Stream(stream_id));
        assert_eq!(error.code, H2ErrorCode::ProtocolError);
    }

    let mut server = H2Server::default();
    initialize_server(&mut server);
    assert!(matches!(
        server
            .accept_frame_bytes(H2Frame {
                frame_type: H2FrameType::Headers,
                flags: 0x5,
                stream_id: 1,
                payload: encoded_fields(
                    &mut H2HeaderBlockEncoder::new(),
                    &request_fields(H2HeaderField::new(b"x-value", b"one two\tthree")),
                ),
            })
            .unwrap()
            .0,
        Some(H2ByteStreamEvent::RequestHeaders { .. })
    ));
}

#[test]
fn connection_histories_share_across_streams_and_isolate_fresh_connections() {
    let repeated = H2HeaderField::new(b"x-shared", b"value");

    let mut client = H2Client::default();
    initialize_client(&mut client);
    let first_commit = client
        .trailers_frame_with_raw_headers(1, std::slice::from_ref(&repeated))
        .unwrap();
    let first = take_client_outbound(&mut client, first_commit);
    let second_commit = client
        .trailers_frame_with_raw_headers(3, std::slice::from_ref(&repeated))
        .unwrap();
    let second = take_client_outbound(&mut client, second_commit);
    assert!(header_block(&second).len() < header_block(&first).len());
    assert_eq!(client_outbound_table(&client).entries.len(), 1);
    let mut fresh_client = H2Client::default();
    initialize_client(&mut fresh_client);
    let fresh_commit = fresh_client
        .trailers_frame_with_raw_headers(1, std::slice::from_ref(&repeated))
        .unwrap();
    let fresh = take_client_outbound(&mut fresh_client, fresh_commit);
    assert_eq!(header_block(&fresh), header_block(&first));

    let mut encoder = H2HeaderBlockEncoder::new();
    let first_request = encoded_fields(&mut encoder, &request_fields(repeated.clone()));
    let repeated_request = encoded_fields(&mut encoder, &request_fields(repeated.clone()));
    let mut server = H2Server::default();
    initialize_server(&mut server);
    assert!(matches!(
        server
            .accept_frame_bytes(H2Frame {
                frame_type: H2FrameType::Headers,
                flags: 0x5,
                stream_id: 1,
                payload: first_request,
            })
            .unwrap()
            .0,
        Some(H2ByteStreamEvent::RequestHeaders { .. })
    ));
    assert!(matches!(
        server
            .accept_frame_bytes(H2Frame {
                frame_type: H2FrameType::Headers,
                flags: 0x5,
                stream_id: 3,
                payload: repeated_request.clone(),
            })
            .unwrap()
            .0,
        Some(H2ByteStreamEvent::RequestHeaders { .. })
    ));
    let mut fresh_server = H2Server::default();
    initialize_server(&mut fresh_server);
    let error = fresh_server
        .accept_frame_bytes_typed(H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0x5,
            stream_id: 1,
            payload: repeated_request,
        })
        .0;
    assert!(matches!(
        error,
        H2FrameOutcome::Error(error) if error.code == H2ErrorCode::CompressionError
    ));

    let mut server = H2Server::default();
    initialize_server(&mut server);
    let first_commit = server
        .trailers_frame_with_raw_headers(1, std::slice::from_ref(&repeated))
        .unwrap();
    let first = take_server_outbound(&mut server, first_commit);
    let second_commit = server
        .trailers_frame_with_raw_headers(3, std::slice::from_ref(&repeated))
        .unwrap();
    let second = take_server_outbound(&mut server, second_commit);
    assert!(header_block(&second).len() < header_block(&first).len());

    let mut client = H2Client::default();
    initialize_client(&mut client);
    let first_stream = client.reserve_stream().unwrap();
    let second_stream = client.reserve_stream().unwrap();
    let mut encoder = H2HeaderBlockEncoder::new();
    let first_response = encoded_fields(&mut encoder, &response_fields(repeated.clone()));
    let repeated_response = encoded_fields(&mut encoder, &response_fields(repeated));
    for (stream_id, payload) in [
        (first_stream, first_response),
        (second_stream, repeated_response.clone()),
    ] {
        assert!(matches!(
            client
                .accept_frame_bytes(H2Frame {
                    frame_type: H2FrameType::Headers,
                    flags: 0x5,
                    stream_id,
                    payload,
                })
                .unwrap()
                .0,
            H2ByteClientEvent::ResponseHeaders { .. }
        ));
    }
    let mut fresh_client = H2Client::default();
    initialize_client(&mut fresh_client);
    let stream_id = fresh_client.reserve_stream().unwrap();
    assert!(matches!(
        fresh_client
            .accept_frame_bytes_typed(H2Frame {
                frame_type: H2FrameType::Headers,
                flags: 0x5,
                stream_id,
                payload: repeated_response,
            })
            .0,
        H2FrameOutcome::Error(error) if error.code == H2ErrorCode::CompressionError
    ));
}

#[test]
fn canonical_events_preserve_request_response_trailer_bytes_and_sensitivity() {
    let raw = H2HeaderField::new(b"x-bytes", [0x80, 0xff]).with_sensitive(true);
    let mut encoder = H2HeaderBlockEncoder::new();
    let mut server = H2Server::default();
    initialize_server(&mut server);
    let request = server
        .accept_frame_bytes(H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0x4,
            stream_id: 1,
            payload: encoded_fields(&mut encoder, &request_fields(raw.clone())),
        })
        .unwrap()
        .0
        .unwrap();
    let H2ByteStreamEvent::RequestHeaders { headers, .. } = request else {
        panic!("request event")
    };
    assert_eq!(headers.last(), Some(&raw));
    assert!(
        H2ByteStreamEvent::<Vec<u8>>::RequestHeaders {
            stream_id: 1,
            headers: headers.clone(),
            end_stream: false,
        }
        .try_into_text()
        .is_err()
    );
    let trailers = vec![raw.clone()];
    assert!(matches!(
        server
            .accept_frame_bytes(H2Frame {
                frame_type: H2FrameType::Headers,
                flags: 0x5,
                stream_id: 1,
                payload: encoded_fields(&mut encoder, &trailers),
            })
            .unwrap()
            .0,
        Some(H2ByteStreamEvent::Trailers { headers, .. }) if headers == trailers
    ));

    let mut client = H2Client::default();
    initialize_client(&mut client);
    let stream_id = client.reserve_stream().unwrap();
    let mut encoder = H2HeaderBlockEncoder::new();
    assert!(matches!(
        client
            .accept_frame_bytes(H2Frame {
                frame_type: H2FrameType::Headers,
                flags: 0x4,
                stream_id,
                payload: encoded_fields(&mut encoder, &response_fields(raw.clone())),
            })
            .unwrap()
            .0,
        H2ByteClientEvent::ResponseHeaders { headers, .. } if headers.last() == Some(&raw)
    ));
    assert!(matches!(
        client
            .accept_frame_bytes(H2Frame {
                frame_type: H2FrameType::Headers,
                flags: 0x5,
                stream_id,
                payload: encoded_fields(&mut encoder, std::slice::from_ref(&raw)),
            })
            .unwrap()
            .0,
        H2ByteClientEvent::Trailers { headers, .. } if headers == [raw]
    ));
}

#[test]
fn encoded_limit_is_distinct_exact_and_terminal_on_crossing() {
    let fields = request_fields(H2HeaderField::new(b"x", b"value"));
    let block = encoded_fields(&mut H2HeaderBlockEncoder::new(), &fields);
    let mut exact = H2Server::with_limits(H2Limits {
        max_encoded_header_block_size: block.len(),
        max_header_list_size: usize::MAX,
        ..H2Limits::default()
    })
    .unwrap();
    initialize_server(&mut exact);
    assert!(matches!(
        exact
            .accept_frame_bytes(H2Frame {
                frame_type: H2FrameType::Headers,
                flags: 0x5,
                stream_id: 1,
                payload: block.clone(),
            })
            .unwrap()
            .0,
        Some(H2ByteStreamEvent::RequestHeaders { .. })
    ));

    let mut crossed = H2Server::with_limits(H2Limits {
        max_encoded_header_block_size: block.len() - 1,
        max_header_list_size: usize::MAX,
        ..H2Limits::default()
    })
    .unwrap();
    initialize_server(&mut crossed);
    let first = crossed
        .accept_frame_bytes_typed(H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0x5,
            stream_id: 1,
            payload: block,
        })
        .0;
    let H2FrameOutcome::Error(first) = first else {
        panic!("encoded limit error")
    };
    assert_eq!(first.code, H2ErrorCode::EnhanceYourCalm);
    assert_eq!(
        first.hpack_error,
        Some(H2HpackError::EncodedHeaderBlockTooLarge)
    );
    assert!(server_connection_is_terminal(&crossed));
    let repeated = crossed
        .accept_frame_bytes_typed(H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0x4,
            stream_id: 0,
            payload: vec![0x80],
        })
        .0;
    assert_eq!(repeated, H2FrameOutcome::Error(first));
}

#[test]
fn complete_block_views_enforce_exact_and_stable_encoded_limits() {
    let request = encoded_fields(
        &mut H2HeaderBlockEncoder::new(),
        &request_fields(H2HeaderField::new(b"x", b"value")),
    );
    let mut server = H2Server::with_limits(H2Limits {
        max_encoded_header_block_size: request.len(),
        ..H2Limits::default()
    })
    .unwrap();
    initialize_server(&mut server);
    assert!(matches!(
        server
            .accept_complete_header_block_bytes(1, 0x5, &request)
            .unwrap(),
        H2ByteStreamEvent::RequestHeaders { .. }
    ));

    let response = encoded_fields(
        &mut H2HeaderBlockEncoder::new(),
        &response_fields(H2HeaderField::new(b"x", b"value")),
    );
    let mut client = H2Client::with_limits(H2Limits {
        max_encoded_header_block_size: response.len() - 1,
        ..H2Limits::default()
    })
    .unwrap();
    initialize_client(&mut client);
    let stream_id = client.reserve_stream().unwrap();
    assert!(
        client
            .accept_complete_header_block_bytes(stream_id, 0x5, &response)
            .is_err()
    );
    let diagnostics = client.inbound_hpack_diagnostics();
    assert!(
        client
            .accept_complete_header_block_bytes(stream_id, 0x5, &[])
            .is_err()
    );
    assert_eq!(client.inbound_hpack_diagnostics(), diagnostics);
    assert!(client_connection_is_terminal(&client));
}

#[test]
fn initial_assembly_allocation_failure_is_local_and_terminal_on_both_endpoints() {
    let frame = || H2Frame {
        frame_type: H2FrameType::Headers,
        flags: 0,
        stream_id: 1,
        payload: vec![0x82],
    };

    let mut server = H2Server::default();
    initialize_server(&mut server);
    fail_server_assembly_allocation_after(&mut server, Some(0));
    let H2FrameOutcome::Error(server_error) = server.accept_frame_bytes_typed(frame()).0 else {
        panic!("server assembly allocation failure")
    };
    assert_eq!(server_error.code, H2ErrorCode::InternalError);
    assert_eq!(
        server_error.hpack_error,
        Some(H2HpackError::AllocationFailed)
    );
    assert_eq!(server.inbound_hpack_diagnostics(), Default::default());

    let mut client = H2Client::default();
    initialize_client(&mut client);
    assert_eq!(client.reserve_stream().unwrap(), 1);
    fail_client_assembly_allocation_after(&mut client, Some(0));
    let H2FrameOutcome::Error(client_error) = client.accept_frame_bytes_typed(frame()).0 else {
        panic!("client assembly allocation failure")
    };
    assert_eq!(client_error.code, H2ErrorCode::InternalError);
    assert_eq!(
        client_error.hpack_error,
        Some(H2HpackError::AllocationFailed)
    );
    assert_eq!(client.inbound_hpack_diagnostics(), Default::default());
}

#[test]
fn validation_after_decoded_limit_crossing_precedes_limit_but_not_compression() {
    let invalid = request_fields(H2HeaderField::new(b"Upper", b"value"));
    let mut block = encoded_fields(&mut H2HeaderBlockEncoder::new(), &invalid);
    let mut server = H2Server::with_limits(H2Limits {
        max_header_list_size: 0,
        max_encoded_header_block_size: usize::MAX,
        ..H2Limits::default()
    })
    .unwrap();
    initialize_server(&mut server);
    let H2FrameOutcome::Error(validation) = server
        .accept_frame_bytes_typed(H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0x5,
            stream_id: 1,
            payload: block.clone(),
        })
        .0
    else {
        panic!("validation error")
    };
    assert_eq!(validation.code, H2ErrorCode::ProtocolError);

    block.push(0x80);
    let mut server = H2Server::with_limits(H2Limits {
        max_header_list_size: 0,
        max_encoded_header_block_size: usize::MAX,
        ..H2Limits::default()
    })
    .unwrap();
    initialize_server(&mut server);
    let H2FrameOutcome::Error(compression) = server
        .accept_frame_bytes_typed(H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0x5,
            stream_id: 1,
            payload: block,
        })
        .0
    else {
        panic!("compression error")
    };
    assert_eq!(compression.code, H2ErrorCode::CompressionError);
    assert_eq!(
        compression.hpack_error,
        Some(H2HpackError::HeaderIndexOutOfBounds)
    );
}

#[test]
fn compression_failures_poison_server_and_client_connections_stably() {
    let mut server = H2Server::default();
    initialize_server(&mut server);
    let H2FrameOutcome::Error(first) = server
        .accept_frame_bytes_typed(H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0x5,
            stream_id: 1,
            payload: vec![0x80],
        })
        .0
    else {
        panic!("server compression error")
    };
    assert_eq!(first.code, H2ErrorCode::CompressionError);
    assert_eq!(
        first.hpack_error,
        Some(H2HpackError::HeaderIndexOutOfBounds)
    );
    assert!(server_connection_is_terminal(&server));
    let before = server.inbound_hpack_diagnostics();
    let server_poisoned = server.accept_frame_bytes_typed(settings_frame(&[])).0;
    assert!(matches!(
        server_poisoned,
        H2FrameOutcome::Error(error)
            if error.code == H2ErrorCode::CompressionError
                && error.hpack_error == Some(H2HpackError::DecoderPoisoned)
    ));
    assert_eq!(
        server
            .accept_frame_bytes_typed(H2Frame {
                frame_type: H2FrameType::Headers,
                flags: 0x4,
                stream_id: 0,
                payload: vec![0xff; 16],
            })
            .0,
        server_poisoned
    );
    assert_eq!(server.inbound_hpack_diagnostics(), before);

    let mut client = H2Client::default();
    initialize_client(&mut client);
    let stream_id = client.reserve_stream().unwrap();
    let H2FrameOutcome::Error(first) = client
        .accept_frame_bytes_typed(H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0x5,
            stream_id,
            payload: vec![0x80],
        })
        .0
    else {
        panic!("client compression error")
    };
    assert_eq!(first.code, H2ErrorCode::CompressionError);
    assert_eq!(
        first.hpack_error,
        Some(H2HpackError::HeaderIndexOutOfBounds)
    );
    assert!(client_connection_is_terminal(&client));
    let before = client.inbound_hpack_diagnostics();
    let client_poisoned = client.accept_frame_bytes_typed(settings_frame(&[])).0;
    assert!(matches!(
        client_poisoned,
        H2FrameOutcome::Error(error)
            if error.code == H2ErrorCode::CompressionError
                && error.hpack_error == Some(H2HpackError::DecoderPoisoned)
    ));
    assert_eq!(
        client
            .accept_frame_bytes_typed(H2Frame {
                frame_type: H2FrameType::Headers,
                flags: 0x4,
                stream_id: 0,
                payload: vec![0xff; 16],
            })
            .0,
        client_poisoned
    );
    assert_eq!(client.inbound_hpack_diagnostics(), before);
}

#[test]
fn allocation_failures_are_content_free_terminal_and_pre_handoff_atomic() {
    let fields = request_fields(H2HeaderField::new(b"x", b"value"));
    let block = encoded_fields(&mut H2HeaderBlockEncoder::new(), &fields);
    let mut server = H2Server::default();
    initialize_server(&mut server);
    fail_server_inbound_allocation_after(&mut server, Some(0));
    let H2FrameOutcome::Error(first) = server
        .accept_frame_bytes_typed(H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0x5,
            stream_id: 1,
            payload: block,
        })
        .0
    else {
        panic!("allocation error")
    };
    assert_eq!(first.code, H2ErrorCode::InternalError);
    assert_eq!(first.hpack_error, Some(H2HpackError::AllocationFailed));
    assert_eq!(server.inbound_hpack_diagnostics(), Default::default());
    assert!(server_connection_is_terminal(&server));
    assert_eq!(
        server.accept_frame_bytes_typed(settings_frame(&[])).0,
        H2FrameOutcome::Error(first)
    );

    let field = H2HeaderField::new(b"x", b"value");
    let mut client = H2Client::default();
    initialize_client(&mut client);
    let before = client.outbound_hpack_diagnostics();
    fail_client_outbound_allocation_after(&mut client, Some(0));
    let error = client
        .trailers_frame_with_raw_headers(1, std::slice::from_ref(&field))
        .unwrap_err();
    assert_eq!(error.hpack_error, Some(H2HpackError::AllocationFailed));
    assert_eq!(client.outbound_hpack_diagnostics(), before);
    assert!(client_outbound_table(&client).entries.is_empty());
    assert!(!client_connection_is_terminal(&client));
    fail_client_outbound_allocation_after(&mut client, None);
    let commit = client
        .trailers_frame_with_raw_headers(3, std::slice::from_ref(&field))
        .unwrap();
    assert!(!take_client_outbound(&mut client, commit).is_empty());
}

#[test]
fn outbound_handoff_fragments_complete_blocks_and_applies_peer_settings() {
    let mut client = H2Client::default();
    initialize_client(&mut client);
    client
        .accept_frame_bytes(settings_frame(&[H2Setting::new(
            H2SettingId::HeaderTableSize,
            0,
        )]))
        .unwrap();
    let commit = client.trailers_frame_with_raw_headers(1, &[]).unwrap();
    assert_eq!(
        header_block(&take_client_outbound(&mut client, commit)),
        [0x20]
    );
    client
        .accept_frame_bytes(settings_frame(&[H2Setting::new(
            H2SettingId::HeaderTableSize,
            128,
        )]))
        .unwrap();
    let commit = client.trailers_frame_with_raw_headers(3, &[]).unwrap();
    assert_eq!(
        header_block(&take_client_outbound(&mut client, commit)),
        [0x3f, 0x61]
    );

    let large = H2HeaderField::new(b"x-large", vec![0xff; 20_000]);
    let commit = client.trailers_frame_with_raw_headers(5, &[large]).unwrap();
    let frames = take_client_outbound(&mut client, commit);
    let (first, used) = H2Frame::decode(&frames).unwrap();
    let (second, consumed) = H2Frame::decode(&frames[used..]).unwrap();
    assert_eq!(first.frame_type, H2FrameType::Headers);
    assert_eq!(first.flags & 0x4, 0);
    assert_eq!(second.frame_type, H2FrameType::Continuation);
    assert_ne!(second.flags & 0x4, 0);
    assert_eq!(used + consumed, frames.len());
}

#[test]
fn outbound_queue_rejects_reverse_acknowledgement_and_preserves_peer_history() {
    let field = H2HeaderField::new(b"x-ordered", b"value");
    let mut client = H2Client::default();
    initialize_client(&mut client);
    let first = client
        .trailers_frame_with_raw_headers(1, std::slice::from_ref(&field))
        .unwrap();
    let second = client
        .trailers_frame_with_raw_headers(3, std::slice::from_ref(&field))
        .unwrap();
    assert!(client.acknowledge_outbound_block(second).is_err());
    assert_eq!(client.next_outbound_block().unwrap().commit(), first);
    let first_wire = header_block(&take_client_outbound(&mut client, first));
    assert_eq!(client.next_outbound_block().unwrap().commit(), second);
    let second_wire = header_block(&take_client_outbound(&mut client, second));

    let mut decoder = H2HeaderBlockDecoder::new();
    assert_eq!(
        decoder
            .try_decode_with_limit(&first_wire, usize::MAX)
            .unwrap()
            .as_slice(),
        std::slice::from_ref(&field)
    );
    assert_eq!(
        decoder
            .try_decode_with_limit(&second_wire, usize::MAX)
            .unwrap(),
        [field]
    );
}

fn manifest_field() -> H2HeaderField {
    H2HeaderField::new(b"x-manifest", b"value")
}

fn manifest_fields(fields: &[Field]) -> Vec<H2HeaderField> {
    fields
        .iter()
        .map(|field| {
            H2HeaderField::new(field.name.clone(), field.value.clone())
                .with_sensitive(field.sensitive)
        })
        .collect()
}

fn evidence_fields(fields: &[H2HeaderField]) -> Vec<Field> {
    fields
        .iter()
        .map(|field| Field {
            name: field.name.clone(),
            value: field.value.clone(),
            sensitive: field.sensitive,
        })
        .collect()
}

fn open_evidence(
    outcomes: Vec<ConnectionOutcome>,
    inbound_entries: usize,
    outbound_entries: usize,
    commit_sequences: Vec<u64>,
) -> ConnectionEvidenceV3 {
    ConnectionEvidenceV3 {
        outcomes,
        occurrences: Vec::new(),
        inbound_entries,
        outbound_entries,
        state: ConnectionState::Open,
        reusable: true,
        commit_sequences,
        diagnostics: None,
    }
}

fn connection_block_evidence_v3(
    endpoint: ConnectionEndpoint,
    role: HeaderRole,
    occurrence: ConnectionOccurrence,
    expected_fields: &[Field],
) -> ConnectionEvidenceV3 {
    let fields = manifest_fields(expected_fields);
    let custom = fields.last().unwrap().clone();
    let iterations = if occurrence == ConnectionOccurrence::First {
        1
    } else {
        2
    };
    let mut commits = Vec::new();
    let (wire, occurrences, inbound_entries, outbound_entries) = match (endpoint, role) {
        (ConnectionEndpoint::Client, HeaderRole::Request) => {
            let mut client = H2Client::default();
            initialize_client(&mut client);
            let mut selected = Vec::new();
            for _ in 0..iterations {
                let (_, commit) = client
                    .open_stream_with_raw_headers(
                        "GET",
                        "https",
                        "example.test",
                        "/",
                        std::slice::from_ref(&custom),
                        true,
                    )
                    .unwrap();
                commits.push(commit.sequence());
                selected = header_block(&take_client_outbound(&mut client, commit));
            }
            (
                selected,
                expected_fields.to_vec(),
                0,
                client_outbound_table(&client).entries.len(),
            )
        }
        (ConnectionEndpoint::Client, HeaderRole::Response) => {
            let mut client = H2Client::default();
            initialize_client(&mut client);
            let mut encoder = H2HeaderBlockEncoder::new();
            let mut selected_wire = Vec::new();
            let mut selected_headers = Vec::new();
            for _ in 0..iterations {
                let stream_id = client.reserve_stream().unwrap();
                selected_wire = encoded_fields(&mut encoder, &response_fields(custom.clone()));
                let (event, _) = client
                    .accept_frame_bytes(H2Frame {
                        frame_type: H2FrameType::Headers,
                        flags: 0x5,
                        stream_id,
                        payload: selected_wire.clone(),
                    })
                    .unwrap();
                let H2ByteClientEvent::ResponseHeaders { headers, .. } = event else {
                    panic!("response role event")
                };
                selected_headers = headers;
            }
            (
                selected_wire,
                evidence_fields(&selected_headers),
                client_inbound_table(&client).entries.len(),
                0,
            )
        }
        (ConnectionEndpoint::Client, HeaderRole::Trailers) => {
            let mut client = H2Client::default();
            initialize_client(&mut client);
            let mut selected = Vec::new();
            for _ in 0..iterations {
                let stream_id = client.reserve_stream().unwrap();
                let commit = client
                    .trailers_frame_with_raw_headers(stream_id, std::slice::from_ref(&custom))
                    .unwrap();
                commits.push(commit.sequence());
                selected = header_block(&take_client_outbound(&mut client, commit));
            }
            (
                selected,
                expected_fields.to_vec(),
                0,
                client_outbound_table(&client).entries.len(),
            )
        }
        (ConnectionEndpoint::Server, HeaderRole::Request) => {
            let mut server = H2Server::default();
            initialize_server(&mut server);
            let mut encoder = H2HeaderBlockEncoder::new();
            let mut selected_wire = Vec::new();
            let mut selected_headers = Vec::new();
            for index in 0..iterations {
                selected_wire = encoded_fields(&mut encoder, &request_fields(custom.clone()));
                let event = server
                    .accept_frame_bytes(H2Frame {
                        frame_type: H2FrameType::Headers,
                        flags: 0x5,
                        stream_id: 1 + index as u32 * 2,
                        payload: selected_wire.clone(),
                    })
                    .unwrap()
                    .0
                    .unwrap();
                let H2ByteStreamEvent::RequestHeaders { headers, .. } = event else {
                    panic!("request role event")
                };
                selected_headers = headers;
            }
            (
                selected_wire,
                evidence_fields(&selected_headers),
                server_inbound_table(&server).entries.len(),
                0,
            )
        }
        (ConnectionEndpoint::Server, HeaderRole::Response) => {
            let mut server = H2Server::default();
            initialize_server(&mut server);
            let mut selected = Vec::new();
            for index in 0..iterations {
                let commit = server
                    .response_headers_frame_with_raw_headers(
                        1 + index as u32 * 2,
                        200,
                        std::slice::from_ref(&custom),
                        true,
                    )
                    .unwrap();
                commits.push(commit.sequence());
                selected = header_block(&take_server_outbound(&mut server, commit));
            }
            (
                selected,
                expected_fields.to_vec(),
                0,
                server_outbound_table(&server).entries.len(),
            )
        }
        (ConnectionEndpoint::Server, HeaderRole::Trailers) => {
            let mut server = H2Server::default();
            initialize_server(&mut server);
            let mut selected = Vec::new();
            for index in 0..iterations {
                let commit = server
                    .trailers_frame_with_raw_headers(
                        1 + index as u32 * 2,
                        std::slice::from_ref(&custom),
                    )
                    .unwrap();
                commits.push(commit.sequence());
                selected = header_block(&take_server_outbound(&mut server, commit));
            }
            (
                selected,
                expected_fields.to_vec(),
                0,
                server_outbound_table(&server).entries.len(),
            )
        }
    };
    ConnectionEvidenceV3 {
        outcomes: vec![ConnectionOutcome::Wire(wire)],
        occurrences,
        inbound_entries,
        outbound_entries,
        state: ConnectionState::Open,
        reusable: true,
        commit_sequences: commits,
        diagnostics: None,
    }
}

fn connection_scenario_evidence_v3(scenario: &ConnectionScenarioV3) -> ConnectionEvidenceV3 {
    match scenario {
        ConnectionScenarioV3::Block {
            endpoint,
            role,
            occurrence,
            fields,
        } => connection_block_evidence_v3(*endpoint, *role, *occurrence, fields),
        ConnectionScenarioV3::Settings {
            behavior,
            initial_capacity,
            advertised_capacities,
            fields,
        } => {
            assert!(matches!(
                behavior,
                support::hpack_manifest::ConnectionBehavior::SettingsDecrease
                    | support::hpack_manifest::ConnectionBehavior::SettingsIncrease
            ));
            let fields = manifest_fields(fields);
            let mut client = H2Client::default();
            initialize_client(&mut client);
            assert_eq!(client_outbound_table(&client).max_size, *initial_capacity);
            let mut outcomes = Vec::new();
            let mut commits = Vec::new();
            for (index, capacity) in advertised_capacities.iter().copied().enumerate() {
                client
                    .accept_frame_bytes(settings_frame(&[H2Setting::new(
                        H2SettingId::HeaderTableSize,
                        capacity as u32,
                    )]))
                    .unwrap();
                let commit = client
                    .trailers_frame_with_raw_headers(1 + index as u32 * 2, &fields)
                    .unwrap();
                commits.push(commit.sequence());
                outcomes.push(ConnectionOutcome::Wire(header_block(
                    &take_client_outbound(&mut client, commit),
                )));
            }
            open_evidence(
                outcomes,
                0,
                client_outbound_table(&client).entries.len(),
                commits,
            )
        }
        ConnectionScenarioV3::LocalValidation {
            endpoint,
            role,
            fields,
            decoded_limit,
        } => {
            assert_eq!(*endpoint, ConnectionEndpoint::Server);
            assert_eq!(*role, HeaderRole::Request);
            let invalid = request_fields(manifest_fields(fields).remove(0));
            let block = encoded_fields(&mut H2HeaderBlockEncoder::new(), &invalid);
            let mut server = H2Server::with_limits(H2Limits {
                max_header_list_size: *decoded_limit,
                ..H2Limits::default()
            })
            .unwrap();
            initialize_server(&mut server);
            assert!(matches!(
                server
                    .accept_frame_bytes_typed(H2Frame {
                        frame_type: H2FrameType::Headers,
                        flags: 0x5,
                        stream_id: 1,
                        payload: block,
                    })
                    .0,
                H2FrameOutcome::Error(error) if error.code == H2ErrorCode::ProtocolError
            ));
            open_evidence(
                vec![ConnectionOutcome::Error(ErrorCategory::Protocol)],
                server_inbound_table(&server).entries.len(),
                0,
                Vec::new(),
            )
        }
        ConnectionScenarioV3::Cancellation {
            endpoint,
            pending_table_size,
            fields,
        } => {
            assert_eq!(*endpoint, ConnectionEndpoint::Client);
            let fields = manifest_fields(fields);
            let mut client = H2Client::default();
            initialize_client(&mut client);
            client
                .accept_frame_bytes(settings_frame(&[H2Setting::new(
                    H2SettingId::HeaderTableSize,
                    *pending_table_size as u32,
                )]))
                .unwrap();
            let before_table = client_outbound_table(&client);
            let before_diagnostics = client.outbound_hpack_diagnostics();
            {
                let _abandoned = client
                    .prepare_outbound_header_block(1, &fields, true)
                    .unwrap();
            }
            assert!(client.next_outbound_block().is_none());
            assert_eq!(client_outbound_table(&client), before_table);
            assert_eq!(client.outbound_hpack_diagnostics(), before_diagnostics);
            let commit = client
                .prepare_outbound_header_block(1, &fields, true)
                .unwrap()
                .commit()
                .unwrap();
            let wire = header_block(&take_client_outbound(&mut client, commit));
            open_evidence(
                vec![ConnectionOutcome::Incomplete, ConnectionOutcome::Wire(wire)],
                0,
                client_outbound_table(&client).entries.len(),
                vec![commit.sequence()],
            )
        }
        ConnectionScenarioV3::IncompleteInput {
            endpoint,
            stream_id,
            first_fragment,
        } => {
            assert_eq!(*endpoint, ConnectionEndpoint::Server);
            let mut server = H2Server::default();
            initialize_server(&mut server);
            assert!(matches!(
                server
                    .accept_frame_bytes_typed(H2Frame {
                        frame_type: H2FrameType::Headers,
                        flags: 0,
                        stream_id: *stream_id,
                        payload: first_fragment.clone(),
                    })
                    .0,
                H2FrameOutcome::Ignored
            ));
            assert_eq!(server.inbound_hpack_diagnostics(), Default::default());
            open_evidence(vec![ConnectionOutcome::Incomplete], 0, 0, Vec::new())
        }
        ConnectionScenarioV3::TerminalFailure {
            endpoint,
            first_block,
            repeat_block,
        } => {
            assert_eq!(*endpoint, ConnectionEndpoint::Server);
            let mut server = H2Server::default();
            assert_eq!(
                server.discard_hpack_block(first_block),
                Err(kimojio_fsm_http::ServerError::InvalidHpack)
            );
            assert_eq!(
                server.discard_hpack_block(repeat_block),
                Err(kimojio_fsm_http::ServerError::InvalidHpack)
            );
            ConnectionEvidenceV3 {
                outcomes: vec![
                    ConnectionOutcome::Error(ErrorCategory::InvalidIndex),
                    ConnectionOutcome::Error(ErrorCategory::DecoderPoisoned),
                ],
                occurrences: Vec::new(),
                inbound_entries: 0,
                outbound_entries: 0,
                state: ConnectionState::Terminal,
                reusable: false,
                commit_sequences: Vec::new(),
                diagnostics: None,
            }
        }
        ConnectionScenarioV3::EncodedBoundary {
            endpoint,
            view,
            limit,
            block,
        } => encoded_boundary_evidence(*endpoint, *view, *limit, block),
        ConnectionScenarioV3::EncodedAccountingOverflow {
            endpoint,
            stream_id,
            first_fragment,
            accounted_len,
            continuation,
        } => encoded_accounting_overflow_evidence(
            *endpoint,
            *stream_id,
            first_fragment,
            *accounted_len,
            continuation,
        ),
        ConnectionScenarioV3::DecodedBoundary {
            endpoint,
            limit,
            block,
        } => {
            assert_eq!(*endpoint, ConnectionEndpoint::Server);
            let mut server = H2Server::with_limits(H2Limits {
                max_header_list_size: *limit,
                ..H2Limits::default()
            })
            .unwrap();
            let outcome = match server.discard_hpack_block(block) {
                Ok(()) => ConnectionOutcome::Wire(Vec::new()),
                Err(
                    kimojio_fsm_http::ServerError::MalformedMessage
                    | kimojio_fsm_http::ServerError::HeaderTooLarge { .. },
                ) => ConnectionOutcome::Error(ErrorCategory::HeaderListTooLarge),
                other => panic!("unexpected decoded-boundary outcome: {other:?}"),
            };
            open_evidence(vec![outcome], 0, 0, Vec::new())
        }
        ConnectionScenarioV3::InboundAllocation {
            endpoint,
            block,
            repeat,
        } => {
            assert_eq!(*endpoint, ConnectionEndpoint::Server);
            let mut server = H2Server::default();
            fail_server_inbound_allocation_after(&mut server, Some(0));
            assert_eq!(
                server.discard_hpack_block(block),
                Err(kimojio_fsm_http::ServerError::InvalidFrame)
            );
            let mut outcomes = vec![ConnectionOutcome::Error(ErrorCategory::AllocationFailed)];
            if *repeat {
                assert_eq!(
                    server.discard_hpack_block(block),
                    Err(kimojio_fsm_http::ServerError::InvalidFrame)
                );
                outcomes.push(ConnectionOutcome::Error(ErrorCategory::AllocationFailed));
            }
            ConnectionEvidenceV3 {
                outcomes,
                occurrences: Vec::new(),
                inbound_entries: 0,
                outbound_entries: 0,
                state: ConnectionState::Terminal,
                reusable: false,
                commit_sequences: Vec::new(),
                diagnostics: None,
            }
        }
        ConnectionScenarioV3::OutboundAllocation {
            endpoint,
            pending_table_size,
            fields,
        } => {
            assert_eq!(*endpoint, ConnectionEndpoint::Client);
            let fields = manifest_fields(fields);
            let mut client = H2Client::default();
            initialize_client(&mut client);
            let seed_commit = client.trailers_frame_with_raw_headers(1, &[]).unwrap();
            let seed_wire = header_block(&take_client_outbound(&mut client, seed_commit));
            client
                .accept_frame_bytes(settings_frame(&[H2Setting::new(
                    H2SettingId::HeaderTableSize,
                    *pending_table_size as u32,
                )]))
                .unwrap();
            let before = client.outbound_hpack_diagnostics();
            let before_table = client_outbound_table(&client);
            fail_client_outbound_allocation_after(&mut client, Some(0));
            let error = client
                .trailers_frame_with_raw_headers(3, &fields)
                .unwrap_err();
            assert_eq!(error.hpack_error, Some(H2HpackError::AllocationFailed));
            assert_eq!(client.outbound_hpack_diagnostics(), before);
            assert_eq!(client_outbound_table(&client), before_table);
            assert!(!client_connection_is_terminal(&client));
            fail_client_outbound_allocation_after(&mut client, None);
            let commit = client.trailers_frame_with_raw_headers(3, &fields).unwrap();
            let wire = header_block(&take_client_outbound(&mut client, commit));
            open_evidence(
                vec![
                    ConnectionOutcome::Wire(seed_wire),
                    ConnectionOutcome::Error(ErrorCategory::AllocationFailed),
                    ConnectionOutcome::Wire(wire),
                ],
                0,
                client_outbound_table(&client).entries.len(),
                vec![seed_commit.sequence(), commit.sequence()],
            )
        }
        ConnectionScenarioV3::AssemblyAllocation {
            endpoint,
            stream_id,
            first_fragment,
            continuation,
        } => assembly_allocation_evidence(*endpoint, *stream_id, first_fragment, continuation),
        ConnectionScenarioV3::Diagnostics { .. } => {
            unreachable!("diagnostics rows have a separate authoritative runner")
        }
    }
}

fn encoded_boundary_evidence(
    endpoint: ConnectionEndpoint,
    view: ConnectionView,
    limit: usize,
    block: &[u8],
) -> ConnectionEvidenceV3 {
    let limits = H2Limits {
        max_encoded_header_block_size: limit,
        max_header_list_size: usize::MAX,
        ..H2Limits::default()
    };
    let accepted = block.len() <= limit;
    let mut outcomes = Vec::new();
    match endpoint {
        ConnectionEndpoint::Server => {
            let mut server = H2Server::with_limits(limits).unwrap();
            match view {
                ConnectionView::Discard => {
                    outcomes.push(match server.discard_hpack_block(block) {
                        Ok(()) => ConnectionOutcome::Wire(Vec::new()),
                        Err(_) => {
                            ConnectionOutcome::Error(ErrorCategory::EncodedHeaderBlockTooLarge)
                        }
                    });
                    if !accepted {
                        assert_eq!(
                            server.discard_hpack_block(&[0x82]),
                            Err(kimojio_fsm_http::ServerError::InvalidFrame)
                        );
                        outcomes.push(ConnectionOutcome::Error(
                            ErrorCategory::EncodedHeaderBlockTooLarge,
                        ));
                    }
                }
                ConnectionView::CompleteBlock => {
                    initialize_server(&mut server);
                    outcomes.push(
                        server
                            .accept_complete_header_block_bytes(1, 0x5, block)
                            .map(|_| ConnectionOutcome::Wire(Vec::new()))
                            .unwrap_or(ConnectionOutcome::Error(
                                ErrorCategory::EncodedHeaderBlockTooLarge,
                            )),
                    );
                    if !accepted {
                        assert!(
                            server
                                .accept_complete_header_block_bytes(3, 0x5, &[0x82])
                                .is_err()
                        );
                        outcomes.push(ConnectionOutcome::Error(
                            ErrorCategory::EncodedHeaderBlockTooLarge,
                        ));
                    }
                }
                ConnectionView::FrameAssembly => unreachable!(),
            }
            if accepted {
                open_evidence(
                    outcomes,
                    server_inbound_table(&server).entries.len(),
                    0,
                    Vec::new(),
                )
            } else {
                assert!(server_connection_is_terminal(&server));
                ConnectionEvidenceV3 {
                    outcomes,
                    occurrences: Vec::new(),
                    inbound_entries: 0,
                    outbound_entries: 0,
                    state: ConnectionState::Terminal,
                    reusable: false,
                    commit_sequences: Vec::new(),
                    diagnostics: None,
                }
            }
        }
        ConnectionEndpoint::Client => {
            let mut client = H2Client::with_limits(limits).unwrap();
            match view {
                ConnectionView::Discard => {
                    outcomes.push(match client.discard_hpack_block(block) {
                        Ok(()) => ConnectionOutcome::Wire(Vec::new()),
                        Err(_) => {
                            ConnectionOutcome::Error(ErrorCategory::EncodedHeaderBlockTooLarge)
                        }
                    });
                }
                ConnectionView::CompleteBlock => {
                    initialize_client(&mut client);
                    let stream_id = client.reserve_stream().unwrap();
                    outcomes.push(
                        client
                            .accept_complete_header_block_bytes(stream_id, 0x5, block)
                            .map(|_| ConnectionOutcome::Wire(Vec::new()))
                            .unwrap_or(ConnectionOutcome::Error(
                                ErrorCategory::EncodedHeaderBlockTooLarge,
                            )),
                    );
                }
                ConnectionView::FrameAssembly => unreachable!(),
            }
            if !accepted {
                assert!(
                    client
                        .accept_complete_header_block_bytes(1, 0x5, &[0x88])
                        .is_err()
                );
                outcomes.push(ConnectionOutcome::Error(
                    ErrorCategory::EncodedHeaderBlockTooLarge,
                ));
                assert!(client_connection_is_terminal(&client));
                ConnectionEvidenceV3 {
                    outcomes,
                    occurrences: Vec::new(),
                    inbound_entries: 0,
                    outbound_entries: 0,
                    state: ConnectionState::Terminal,
                    reusable: false,
                    commit_sequences: Vec::new(),
                    diagnostics: None,
                }
            } else {
                open_evidence(
                    outcomes,
                    client_inbound_table(&client).entries.len(),
                    0,
                    Vec::new(),
                )
            }
        }
    }
}

fn encoded_accounting_overflow_evidence(
    endpoint: ConnectionEndpoint,
    stream_id: u32,
    first_fragment: &[u8],
    accounted_len: usize,
    continuation: &[u8],
) -> ConnectionEvidenceV3 {
    let first = H2Frame {
        frame_type: H2FrameType::Headers,
        flags: 0,
        stream_id,
        payload: first_fragment.to_vec(),
    };
    let continuation = H2Frame {
        frame_type: H2FrameType::Continuation,
        flags: 0x4,
        stream_id,
        payload: continuation.to_vec(),
    };
    match endpoint {
        ConnectionEndpoint::Server => {
            let mut server = H2Server::default();
            initialize_server(&mut server);
            assert!(matches!(
                server.accept_frame_bytes_typed(first).0,
                H2FrameOutcome::Ignored
            ));
            set_server_pending_encoded_header_block_len(&mut server, accounted_len);
            let H2FrameOutcome::Error(error) = server.accept_frame_bytes_typed(continuation).0
            else {
                panic!("server encoded accounting overflow")
            };
            assert_eq!(
                error.hpack_error,
                Some(H2HpackError::EncodedHeaderBlockTooLarge)
            );
            assert_eq!(
                server.accept_frame_bytes_typed(settings_frame(&[])).0,
                H2FrameOutcome::Error(error)
            );
            assert_eq!(server.inbound_hpack_diagnostics(), Default::default());
        }
        ConnectionEndpoint::Client => {
            let mut client = H2Client::default();
            initialize_client(&mut client);
            assert_eq!(client.reserve_stream().unwrap(), stream_id);
            assert!(matches!(
                client.accept_frame_bytes_typed(first).0,
                H2FrameOutcome::Ignored
            ));
            set_client_pending_encoded_header_block_len(&mut client, accounted_len);
            let H2FrameOutcome::Error(error) = client.accept_frame_bytes_typed(continuation).0
            else {
                panic!("client encoded accounting overflow")
            };
            assert_eq!(
                error.hpack_error,
                Some(H2HpackError::EncodedHeaderBlockTooLarge)
            );
            assert_eq!(
                client.accept_frame_bytes_typed(settings_frame(&[])).0,
                H2FrameOutcome::Error(error)
            );
            assert_eq!(client.inbound_hpack_diagnostics(), Default::default());
        }
    }
    ConnectionEvidenceV3 {
        outcomes: vec![
            ConnectionOutcome::Incomplete,
            ConnectionOutcome::Error(ErrorCategory::EncodedHeaderBlockTooLarge),
            ConnectionOutcome::Error(ErrorCategory::EncodedHeaderBlockTooLarge),
        ],
        occurrences: Vec::new(),
        inbound_entries: 0,
        outbound_entries: 0,
        state: ConnectionState::Terminal,
        reusable: false,
        commit_sequences: Vec::new(),
        diagnostics: None,
    }
}

fn assembly_allocation_evidence(
    endpoint: ConnectionEndpoint,
    stream_id: u32,
    first_fragment: &[u8],
    continuation: &[u8],
) -> ConnectionEvidenceV3 {
    let first = H2Frame {
        frame_type: H2FrameType::Headers,
        flags: 0,
        stream_id,
        payload: first_fragment.to_vec(),
    };
    let continuation = H2Frame {
        frame_type: H2FrameType::Continuation,
        flags: 0x4,
        stream_id,
        payload: continuation.to_vec(),
    };
    match endpoint {
        ConnectionEndpoint::Server => {
            let mut server = H2Server::default();
            initialize_server(&mut server);
            fail_server_assembly_allocation_after(&mut server, Some(1));
            assert!(matches!(
                server.accept_frame_bytes_typed(first).0,
                H2FrameOutcome::Ignored
            ));
            let H2FrameOutcome::Error(error) = server.accept_frame_bytes_typed(continuation).0
            else {
                panic!("server assembly allocation error")
            };
            assert_eq!(error.hpack_error, Some(H2HpackError::AllocationFailed));
            assert_eq!(
                server.accept_frame_bytes_typed(settings_frame(&[])).0,
                H2FrameOutcome::Error(error)
            );
            assert_eq!(server.inbound_hpack_diagnostics(), Default::default());
        }
        ConnectionEndpoint::Client => {
            let mut client = H2Client::default();
            initialize_client(&mut client);
            assert_eq!(client.reserve_stream().unwrap(), stream_id);
            fail_client_assembly_allocation_after(&mut client, Some(1));
            assert!(matches!(
                client.accept_frame_bytes_typed(first).0,
                H2FrameOutcome::Ignored
            ));
            let H2FrameOutcome::Error(error) = client.accept_frame_bytes_typed(continuation).0
            else {
                panic!("client assembly allocation error")
            };
            assert_eq!(error.hpack_error, Some(H2HpackError::AllocationFailed));
            assert_eq!(
                client.accept_frame_bytes_typed(settings_frame(&[])).0,
                H2FrameOutcome::Error(error)
            );
            assert_eq!(client.inbound_hpack_diagnostics(), Default::default());
        }
    }
    ConnectionEvidenceV3 {
        outcomes: vec![
            ConnectionOutcome::Incomplete,
            ConnectionOutcome::Error(ErrorCategory::AllocationFailed),
            ConnectionOutcome::Error(ErrorCategory::AllocationFailed),
        ],
        occurrences: Vec::new(),
        inbound_entries: 0,
        outbound_entries: 0,
        state: ConnectionState::Terminal,
        reusable: false,
        commit_sequences: Vec::new(),
        diagnostics: None,
    }
}

#[test]
fn authoritative_phase_two_connection_rows_complete_exactly_once() {
    let mut completion = CompletionLedger::phase_two(Runner::HpackConnection);
    let cases = completion.cases().cloned().collect::<Vec<_>>();
    for case in cases {
        let CaseInput::ConnectionV3(scenario) = &case.input else {
            unreachable!("unexpected connection case {}", case.id)
        };
        completion.complete(RunnerResult {
            id: case.id,
            input: case.input.clone(),
            initial_state: case.initial_state,
            actual: ActualEvidence::ConnectionV3(Box::new(connection_scenario_evidence_v3(
                scenario,
            ))),
        });
    }
    completion.finish();
}

#[test]
fn local_header_table_setting_is_enforced_inbound() {
    let mut server = H2Server::with_limits(H2Limits {
        max_header_table_size: 0,
        ..H2Limits::default()
    })
    .unwrap();
    initialize_server(&mut server);
    assert_eq!(
        server.discard_hpack_block(&[0x82]),
        Err(kimojio_fsm_http::ServerError::InvalidHpack)
    );
    let mut peer = H2HeaderBlockEncoder::new();
    peer.set_max_table_size(0);
    let updated = peer
        .try_encode_ref(&[H2RawHeaderRef::new(b":method", b"GET")])
        .unwrap();
    let mut server = H2Server::with_limits(H2Limits {
        max_header_table_size: 0,
        ..H2Limits::default()
    })
    .unwrap();
    assert!(server.discard_hpack_block(&updated).is_ok());
}

#[test]
fn local_settings_advertise_the_effective_header_table_ceiling() {
    let mut client = H2Client::with_limits(H2Limits {
        max_header_table_size: usize::MAX,
        ..H2Limits::default()
    })
    .unwrap();
    let preface = client.connection_preface();
    let (settings, _) = H2Frame::decode(&preface[24..]).unwrap();
    assert_eq!(settings.frame_type, H2FrameType::Settings);
    assert!(
        H2Settings::decode_payload(&settings.payload)
            .unwrap()
            .contains(&H2Setting::new(H2SettingId::HeaderTableSize, 1_048_576))
    );
}

#[test]
fn standalone_decoder_confirms_fresh_dynamic_index_is_invalid() {
    let field = manifest_field();
    let mut encoder = H2HeaderBlockEncoder::new();
    encoded_fields(&mut encoder, std::slice::from_ref(&field));
    let repeated = encoded_fields(&mut encoder, std::slice::from_ref(&field));
    let error = H2HeaderBlockDecoder::new()
        .try_decode_with_limit(&repeated, usize::MAX)
        .unwrap_err();
    assert_eq!(
        error.hpack_error,
        Some(H2HpackError::HeaderIndexOutOfBounds)
    );
}
