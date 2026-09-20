//! HTTP/2 HPACK and header-validation tests.

use super::*;
use crate::Header;
use crate::server::*;

#[test]
fn hpack_encoder_emits_table_size_update_once() {
    let headers = [H2RawHeader::new("custom-key", "custom-value")];
    let mut encoder = H2HeaderBlockEncoder::new();
    encoder.set_max_table_size(0);

    let first = encoder.encode(&headers);
    let second = encoder.encode(&headers);

    assert_eq!(first.first().copied(), Some(0x20));
    assert_ne!(second.first().copied(), Some(0x20));
}

#[test]
fn hpack_typed_error_preserves_decoder_category() {
    let mut server = h2_server_after_preface();
    let frame = H2Frame {
        frame_type: H2FrameType::Headers,
        flags: 0x4,
        stream_id: 1,
        payload: vec![0x80],
    };

    let H2FrameOutcome::Error(error) = server.accept_frame_typed(frame).0 else {
        panic!("expected HPACK error");
    };

    assert_eq!(error.code, H2ErrorCode::CompressionError);
    assert_eq!(error.scope, H2ErrorScope::Connection);
    assert_eq!(
        error.hpack_error,
        Some(H2HpackError::HeaderIndexOutOfBounds)
    );
}

#[test]
fn hpack_discard_block_preserves_decoder_dynamic_table_state() {
    let mut server = h2_server_after_preface();
    let block = hpack_literal_with_indexing("custom-key", "custom-value");

    server.discard_hpack_block(&block).unwrap();
    let headers = server
        .endpoint
        .header_codecs
        .as_mut()
        .unwrap()
        .inbound
        .decode(&[0xbe], usize::MAX)
        .unwrap();

    assert_eq!(
        headers,
        vec![H2HeaderField::new("custom-key", "custom-value")]
    );
}

#[test]
fn h2_pseudo_header_validation_rejects_invalid_requests() {
    let invalid_cases: Vec<Vec<H2RawHeader>> = vec![
        vec![
            H2RawHeader::new(":method", "GET"),
            H2RawHeader::new(":path", "/"),
        ],
        vec![
            H2RawHeader::new(":method", "GET"),
            H2RawHeader::new(":method", "POST"),
            H2RawHeader::new(":scheme", "https"),
            H2RawHeader::new(":path", "/"),
        ],
        vec![
            H2RawHeader::new(":method", "GET"),
            H2RawHeader::new(":scheme", "https"),
            H2RawHeader::new(":authority", "one.example"),
            H2RawHeader::new(":authority", "two.example"),
            H2RawHeader::new(":path", "/"),
        ],
        vec![
            H2RawHeader::new("x-test", "1"),
            H2RawHeader::new(":method", "GET"),
            H2RawHeader::new(":scheme", "https"),
            H2RawHeader::new(":path", "/"),
        ],
        vec![
            H2RawHeader::new(":method", "GET"),
            H2RawHeader::new(":scheme", "https"),
            H2RawHeader::new(":path", "/"),
            H2RawHeader::new("Host", "example.com"),
        ],
        vec![
            H2RawHeader::new(":method", "GET"),
            H2RawHeader::new(":scheme", "https"),
            H2RawHeader::new(":path", "/"),
            H2RawHeader::new("te", "gzip"),
        ],
    ];

    for headers in invalid_cases {
        let mut encoder = H2HeaderBlockEncoder::new();
        let mut frame = Vec::new();
        H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0x5,
            stream_id: 1,
            payload: encoder.encode(&headers),
        }
        .encode(&mut frame);
        let mut server = h2_server_after_preface();

        assert_eq!(server.accept_event(&frame), Err(ServerError::InvalidFrame));
    }
}

#[test]
fn h2_typed_malformed_header_errors_are_stream_protocol_errors() {
    let mut encoder = H2HeaderBlockEncoder::new();
    let frame = H2Frame {
        frame_type: H2FrameType::Headers,
        flags: 0x5,
        stream_id: 1,
        payload: encoder.encode(&[
            H2RawHeader::new(":method", "GET"),
            H2RawHeader::new(":scheme", "https"),
            H2RawHeader::new(":path", "/"),
            H2RawHeader::new("Host", "example.com"),
        ]),
    };
    let mut server = h2_server_after_preface();

    let H2FrameOutcome::Error(error) = server.accept_frame_typed(frame).0 else {
        panic!("expected malformed header typed error");
    };

    assert_eq!(error.scope, H2ErrorScope::Stream(1));
    assert_eq!(error.code, H2ErrorCode::ProtocolError);
    assert_eq!(error.hpack_error, None);
}

#[test]
fn h2_pseudo_header_validation_rejects_invalid_responses() {
    let invalid_cases: Vec<Vec<H2RawHeader>> = vec![
        vec![H2RawHeader::new("content-type", "application/grpc")],
        vec![
            H2RawHeader::new(":status", "200"),
            H2RawHeader::new(":status", "204"),
        ],
        vec![
            H2RawHeader::new("content-type", "application/grpc"),
            H2RawHeader::new(":status", "200"),
        ],
        vec![H2RawHeader::new(":status", "101")],
        vec![H2RawHeader::new(":status", "99")],
        vec![H2RawHeader::new(":status", "1000")],
    ];

    for headers in invalid_cases {
        let mut client = h2_client_after_server_settings();
        let (stream_id, _) =
            open_stream_bytes(&mut client, "GET", "https", "example.com", "/", &[], true).unwrap();
        let mut encoder = H2HeaderBlockEncoder::new();
        let mut frame = Vec::new();
        H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0x5,
            stream_id,
            payload: encoder.encode(&headers),
        }
        .encode(&mut frame);

        assert_eq!(client.accept(&frame), Err(ServerError::InvalidFrame));
    }
}

#[test]
fn h2_content_length_mismatch_is_rejected() {
    let mut client = h2_client_after_server_settings();
    let mut server_input = client.connection_preface();
    let (stream_id, headers) = open_stream_bytes(
        &mut client,
        "POST",
        "https",
        "example.com",
        "/upload",
        &[Header::new("content-length", "4")],
        false,
    )
    .unwrap();
    server_input.extend_from_slice(&headers);
    server_input.extend_from_slice(&client.data_frame(stream_id, b"abc", true));
    let mut server = H2Server::default();
    let (_event, offset, _) = server.accept_event(&server_input).unwrap();

    assert_eq!(
        server.accept_event(&server_input[offset..]),
        Err(ServerError::InvalidContentLength)
    );

    let mut client = h2_client_after_server_settings();
    let (stream_id, _) = open_stream_bytes(
        &mut client,
        "GET",
        "https",
        "example.com",
        "/download",
        &[],
        true,
    )
    .unwrap();
    let mut h2 = H2Server::default();
    let mut response = response_headers_bytes(
        &mut h2,
        stream_id,
        200,
        &[Header::new("content-length", "4")],
        false,
    );
    response.extend_from_slice(&h2.data_frame(stream_id, b"abc", true));
    let (_event, offset, _) = client.accept(&response).unwrap();

    assert_eq!(
        client.accept(&response[offset..]),
        Err(ServerError::InvalidContentLength)
    );
}

#[test]
fn h2_request_content_length_accepts_exact_and_duplicate_equivalent_values() {
    for headers in [
        vec![Header::new("content-length", "3")],
        vec![
            Header::new("content-length", "3"),
            Header::new("content-length", "3"),
        ],
        vec![Header::new("content-length", "3, 3")],
    ] {
        let mut client = h2_client_after_server_settings();
        let mut input = client.connection_preface();
        let (stream_id, request) = open_stream_bytes(
            &mut client,
            "POST",
            "https",
            "example.com",
            "/upload",
            &headers,
            false,
        )
        .unwrap();
        input.extend_from_slice(&request);
        input.extend_from_slice(&client.data_frame(stream_id, b"abc", true));
        let mut server = H2Server::default();
        let (_, consumed, _) = server.accept_event(&input).unwrap();

        assert!(server.accept_event(&input[consumed..]).is_ok());
    }
}

#[test]
fn h2_request_content_length_rejects_too_long_and_invalid_declarations() {
    for (headers, body) in [
        (vec![Header::new("content-length", "2")], b"abc".as_slice()),
        (
            vec![
                Header::new("content-length", "2"),
                Header::new("content-length", "3"),
            ],
            b"".as_slice(),
        ),
        (vec![Header::new("content-length", "2, 3")], b"".as_slice()),
        (
            vec![Header::new("content-length", "184467440737095516160")],
            b"".as_slice(),
        ),
    ] {
        let mut client = h2_client_after_server_settings();
        let mut input = client.connection_preface();
        let (stream_id, request) = open_stream_bytes(
            &mut client,
            "POST",
            "https",
            "example.com",
            "/upload",
            &headers,
            body.is_empty(),
        )
        .unwrap();
        input.extend_from_slice(&request);
        if !body.is_empty() {
            input.extend_from_slice(&client.data_frame(stream_id, body, false));
        }
        let mut server = H2Server::default();

        if body.is_empty() {
            assert!(server.accept_event(&input).is_err());
        } else {
            let (_, consumed, _) = server.accept_event(&input).unwrap();
            assert!(server.accept_event(&input[consumed..]).is_err());
        }
    }
}

#[test]
fn h2_response_content_length_honors_no_body_semantics() {
    for (method, status) in [("HEAD", 200), ("GET", 304)] {
        let mut client = h2_client_after_server_settings();
        let (stream_id, _) = open_stream_bytes(
            &mut client,
            method,
            "https",
            "example.com",
            "/resource",
            &[],
            true,
        )
        .unwrap();
        let mut server = H2Server::default();
        let response = response_headers_bytes(
            &mut server,
            stream_id,
            status,
            &[Header::new("content-length", "123")],
            true,
        );

        assert!(client.accept(&response).is_ok());
    }

    let mut client = h2_client_after_server_settings();
    let (stream_id, _) = open_stream_bytes(
        &mut client,
        "GET",
        "https",
        "example.com",
        "/resource",
        &[],
        true,
    )
    .unwrap();
    let mut server = H2Server::default();
    let response = response_headers_bytes(
        &mut server,
        stream_id,
        204,
        &[Header::new("content-length", "0")],
        true,
    );
    assert!(client.accept(&response).is_err());

    let mut client = h2_client_after_server_settings();
    let (stream_id, _) = open_stream_bytes(
        &mut client,
        "HEAD",
        "https",
        "example.com",
        "/resource",
        &[],
        true,
    )
    .unwrap();
    let mut server = H2Server::default();
    let mut response = response_headers_bytes(
        &mut server,
        stream_id,
        200,
        &[Header::new("content-length", "3")],
        false,
    );
    response.extend_from_slice(&server.data_frame(stream_id, b"abc", true));
    let (_, consumed, _) = client.accept(&response).unwrap();
    assert!(client.accept(&response[consumed..]).is_err());
}

#[test]
fn h2_content_length_finishes_on_trailers_but_not_reset_or_goaway() {
    for (declared, trailers_are_valid) in [("3", true), ("4", false)] {
        let mut client = h2_client_after_server_settings();
        let mut input = client.connection_preface();
        let (stream_id, request) = open_stream_bytes(
            &mut client,
            "POST",
            "https",
            "example.com",
            "/upload",
            &[Header::new("content-length", declared)],
            false,
        )
        .unwrap();
        input.extend_from_slice(&request);
        input.extend_from_slice(&client.data_frame(stream_id, b"abc", false));
        input.extend_from_slice(&trailers_bytes(
            &mut client,
            stream_id,
            &[Header::new("x-checksum", "ok")],
        ));
        let mut server = H2Server::default();
        let (_, first, _) = server.accept_event(&input).unwrap();
        let (_, second, _) = server.accept_event(&input[first..]).unwrap();
        let trailers = server.accept_event(&input[first + second..]);

        assert_eq!(trailers.is_ok(), trailers_are_valid);
    }

    for terminal in [h2_reset_frame(1), h2_goaway_frame(1)] {
        let mut client = h2_client_after_server_settings();
        let mut input = client.connection_preface();
        let (_, request) = open_stream_bytes(
            &mut client,
            "POST",
            "https",
            "example.com",
            "/upload",
            &[Header::new("content-length", "4")],
            false,
        )
        .unwrap();
        input.extend_from_slice(&request);
        input.extend_from_slice(&terminal);
        let mut server = H2Server::default();
        let (_, consumed, _) = server.accept_event(&input).unwrap();

        assert!(server.accept_event(&input[consumed..]).is_ok());
    }
}

#[test]
fn h2_trailers_reject_pseudo_headers() {
    let mut client = h2_client_after_server_settings();
    let mut input = client.connection_preface();
    let (stream_id, headers) = open_stream_bytes(
        &mut client,
        "POST",
        "https",
        "example.com",
        "/svc",
        &[],
        false,
    )
    .unwrap();
    input.extend_from_slice(&headers);
    H2Frame {
        frame_type: H2FrameType::Headers,
        flags: 0x5,
        stream_id,
        payload: encode_hpack_header_block(&[Header::new(":status", "200")]),
    }
    .encode(&mut input);
    let mut server = H2Server::default();
    let (_event, offset, _) = server.accept_event(&input).unwrap();

    assert_eq!(
        server.accept_event(&input[offset..]),
        Err(ServerError::InvalidFrame)
    );
}

#[test]
fn corpus_hpack_errors_preserve_distinct_categories() {
    let invalid_cases = [
        (vec![0x80], H2HpackError::HeaderIndexOutOfBounds),
        (vec![0xff], H2HpackError::IntegerDecoding),
    ];

    for (payload, expected) in invalid_cases {
        let mut server = h2_server_after_preface();
        let frame = H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0x4,
            stream_id: 1,
            payload,
        };
        let H2FrameOutcome::Error(error) = server.accept_frame_typed(frame).0 else {
            panic!("expected HPACK error");
        };
        assert_eq!(error.hpack_error, Some(expected));
    }
}
