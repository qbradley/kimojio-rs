// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{Request, Response};
use kimojio_stack::Runtime;
use kimojio_stack_http::{
    Body, BodyLimits, ErrorKind, HttpConfig, LimitKind, StackTransport, h2, http1,
};
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

#[test]
fn protocol_limits_http1_rejects_oversized_headers() {
    let (client_transport, server_transport) = socket_transport_pair();
    let mut runtime = Runtime::new();

    let error = runtime.block_on(|cx| {
        cx.scope(|scope| {
            let client = scope.spawn(move |cx| {
                let mut transport = client_transport;
                transport
                    .write_all(cx, b"GET / HTTP/1.1\r\nx-too-large: 123456789\r\n\r\n")
                    .unwrap();
                transport.shutdown_write(cx).unwrap();
                transport.close(cx).unwrap();
            });

            let config = HttpConfig {
                max_header_bytes: 4,
                ..HttpConfig::default()
            };
            let mut server = http1::ServerConnection::new(server_transport, config);
            let error = server.read_request(cx).unwrap_err();
            server.close(cx).unwrap();
            client.join(cx);
            error
        })
    });

    assert_eq!(error.kind(), ErrorKind::SizeLimit);
    assert!(
        matches!(
            error,
            kimojio_stack_http::Error::SizeLimit {
                kind: LimitKind::Headers,
                ..
            }
        ),
        "unexpected error: {error:?}"
    );
}

#[test]
fn protocol_limits_http1_rejects_oversized_body() {
    let (client_transport, server_transport) = socket_transport_pair();
    let mut runtime = Runtime::new();

    let error = runtime.block_on(|cx| {
        cx.scope(|scope| {
            let client = scope.spawn(move |cx| {
                let mut transport = client_transport;
                transport
                    .write_all(cx, b"POST / HTTP/1.1\r\nContent-Length: 5\r\n\r\n12345")
                    .unwrap();
                transport.shutdown_write(cx).unwrap();
                transport.close(cx).unwrap();
            });

            let config = HttpConfig {
                max_body_bytes: 4,
                ..HttpConfig::default()
            };
            let mut server = http1::ServerConnection::new(server_transport, config);
            let error = server.read_request(cx).unwrap_err();
            server.close(cx).unwrap();
            client.join(cx);
            error
        })
    });

    assert_eq!(error.kind(), ErrorKind::SizeLimit);
    assert!(matches!(
        error,
        kimojio_stack_http::Error::SizeLimit {
            kind: LimitKind::Body,
            ..
        }
    ));
}

#[test]
fn protocol_limits_h2_rejects_invalid_frame_type() {
    let (client_transport, server_transport) = socket_transport_pair();
    let mut runtime = Runtime::new();

    let error = runtime.block_on(|cx| {
        cx.scope(|scope| {
            let client = scope.spawn(move |cx| {
                let mut transport = client_transport;
                transport.write_all(cx, h2::CLIENT_PREFACE).unwrap();
                transport
                    .write_all(cx, &h2::Frame::settings(Vec::new()).encode().unwrap())
                    .unwrap();
                transport
                    .write_all(cx, &[0, 0, 0, 0xff, 0, 0, 0, 0, 0])
                    .unwrap();
                transport.shutdown_write(cx).unwrap();
                transport.close(cx).unwrap();
            });

            let mut server = h2::ServerConnection::new(server_transport, HttpConfig::default());
            let error = server.accept(cx).unwrap_err();
            server.close(cx).unwrap();
            client.join(cx);
            error
        })
    });

    assert_eq!(error.kind(), ErrorKind::Unsupported);
}

#[test]
fn protocol_limits_h2_rejects_oversized_frame_before_payload_read() {
    let (client_transport, server_transport) = socket_transport_pair();
    let mut runtime = Runtime::new();

    let error = runtime.block_on(|cx| {
        cx.scope(|scope| {
            let client = scope.spawn(move |cx| {
                let mut transport = client_transport;
                transport.write_all(cx, h2::CLIENT_PREFACE).unwrap();
                transport
                    .write_all(cx, &h2::Frame::settings(Vec::new()).encode().unwrap())
                    .unwrap();
                transport
                    .write_all(cx, &[0x00, 0x40, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01])
                    .unwrap();
                transport.shutdown_write(cx).unwrap();
                transport.close(cx).unwrap();
            });

            let mut server = h2::ServerConnection::new(server_transport, HttpConfig::default());
            let error = server.accept(cx).unwrap_err();
            server.close(cx).unwrap();
            client.join(cx);
            error
        })
    });

    assert!(matches!(
        error,
        kimojio_stack_http::Error::SizeLimit {
            kind: LimitKind::Frame,
            ..
        }
    ));
}

#[test]
fn protocol_limits_h2_rejects_encoded_header_block_over_limit() {
    let (client_transport, server_transport) = socket_transport_pair();
    let mut runtime = Runtime::new();

    let error = runtime.block_on(|cx| {
        cx.scope(|scope| {
            let client = scope.spawn(move |cx| {
                let mut transport = client_transport;
                transport.write_all(cx, h2::CLIENT_PREFACE).unwrap();
                transport
                    .write_all(cx, &h2::Frame::settings(Vec::new()).encode().unwrap())
                    .unwrap();
                transport
                    .write_all(
                        cx,
                        &h2::Frame::headers(
                            h2::StreamId::new(1).unwrap(),
                            bytes::Bytes::from_static(b"12345"),
                            true,
                        )
                        .encode()
                        .unwrap(),
                    )
                    .unwrap();
                transport.shutdown_write(cx).unwrap();
                transport.close(cx).unwrap();
            });

            let config = HttpConfig {
                max_header_bytes: 4,
                ..HttpConfig::default()
            };
            let mut server = h2::ServerConnection::new(server_transport, config);
            let error = server.accept(cx).unwrap_err();
            server.close(cx).unwrap();
            client.join(cx);
            error
        })
    });

    assert!(
        matches!(
            error,
            kimojio_stack_http::Error::SizeLimit {
                kind: LimitKind::Headers,
                ..
            }
        ),
        "unexpected error: {error:?}"
    );
}

#[test]
fn protocol_limits_h2_rejects_oversized_body() {
    let (client_transport, server_transport) = socket_transport_pair();
    let mut runtime = Runtime::new();

    let error = runtime.block_on(|cx| {
        cx.scope(|scope| {
            let client = scope.spawn(move |cx| {
                let mut client = h2::ClientConnection::new(client_transport, HttpConfig::default());
                let request = Request::builder()
                    .method("POST")
                    .uri("/too-large")
                    .body(Body::copy_from_slice(b"12345", BodyLimits::new(16)).unwrap())
                    .unwrap();
                client.send_request(cx, &request).unwrap();
                client.close(cx).unwrap();
            });

            let config = HttpConfig {
                max_body_bytes: 4,
                ..HttpConfig::default()
            };
            let mut server = h2::ServerConnection::new(server_transport, config);
            let error = server.accept(cx).unwrap_err();
            server.close(cx).unwrap();
            client.join(cx);
            error
        })
    });

    assert_eq!(error.kind(), ErrorKind::SizeLimit);
    assert!(matches!(
        error,
        kimojio_stack_http::Error::SizeLimit {
            kind: LimitKind::Body,
            ..
        }
    ));
}

#[test]
fn protocol_limits_h2_enforces_max_concurrent_streams() {
    let (client_transport, server_transport) = socket_transport_pair();
    let mut runtime = Runtime::new();

    let error = runtime.block_on(|cx| {
        cx.scope(|scope| {
            let client = scope.spawn(move |cx| {
                let mut transport = client_transport;
                let mut encoder = h2::ConnectionState::new(h2::Settings::default()).unwrap();
                let request_headers = [
                    h2::Header::new(":method", "GET"),
                    h2::Header::new(":scheme", "http"),
                    h2::Header::new(":path", "/"),
                ];
                transport.write_all(cx, h2::CLIENT_PREFACE).unwrap();
                transport
                    .write_all(cx, &h2::Frame::settings(Vec::new()).encode().unwrap())
                    .unwrap();
                for id in [1, 3] {
                    let block = encoder.encode_header_block(&request_headers);
                    transport
                        .write_all(
                            cx,
                            &h2::Frame::headers(
                                h2::StreamId::new(id).unwrap(),
                                bytes::Bytes::from(block),
                                false,
                            )
                            .encode()
                            .unwrap(),
                        )
                        .unwrap();
                }
                transport.shutdown_write(cx).unwrap();
                transport.close(cx).unwrap();
            });

            let settings = h2::Settings {
                max_concurrent_streams: 1,
                ..h2::Settings::default()
            };
            let mut server = h2::ServerConnection::new_with_settings(
                server_transport,
                HttpConfig::default(),
                settings,
            );
            let error = server.accept(cx).unwrap_err();
            server.close(cx).unwrap();
            client.join(cx);
            error
        })
    });

    assert_eq!(error.kind(), ErrorKind::Protocol);
}

#[test]
fn protocol_limits_h2_peer_close_during_in_flight_exchange_is_explicit() {
    let (client_transport, server_transport) = socket_transport_pair();
    let mut runtime = Runtime::new();

    let error_kind = runtime.block_on(|cx| {
        cx.scope(|scope| {
            let server = scope.spawn(move |cx| {
                let mut server = h2::ServerConnection::new(server_transport, HttpConfig::default());
                let incoming = server.accept(cx).unwrap().unwrap();
                assert_eq!(incoming.request.uri(), "/close-before-response");
                server.close(cx).unwrap();
            });

            let mut client = h2::ClientConnection::new(client_transport, HttpConfig::default());
            let request = Request::builder()
                .method("POST")
                .uri("/close-before-response")
                .body(Body::copy_from_slice(b"ping", BodyLimits::new(16)).unwrap())
                .unwrap();
            let stream_id = client.send_request(cx, &request).unwrap();
            let error = client.read_response(cx, stream_id).unwrap_err();
            client.close(cx).unwrap();
            server.join(cx);
            error.kind()
        })
    });

    assert!(matches!(error_kind, ErrorKind::Eof | ErrorKind::PeerReset));
}

#[test]
fn protocol_limits_h2_stalled_stream_window_unblocks_after_window_update() {
    let (client_transport, server_transport) = socket_transport_pair();
    let mut runtime = Runtime::new();

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let server = scope.spawn(move |cx| {
                let mut transport = server_transport;
                read_preface(cx, &mut transport);
                let _client_settings = read_frame(cx, &mut transport);
                transport
                    .write_all(
                        cx,
                        &h2::Frame::settings(vec![h2::Setting::new(
                            h2::SettingId::InitialWindowSize,
                            0,
                        )])
                        .encode()
                        .unwrap(),
                    )
                    .unwrap();

                let mut saw_headers = false;
                loop {
                    let frame = read_frame(cx, &mut transport);
                    match frame.frame_type {
                        h2::FrameType::Settings if frame.flags.contains(h2::FrameFlags::ACK) => {}
                        h2::FrameType::Headers => {
                            saw_headers = true;
                            transport
                                .write_all(cx, &h2::Frame::window_update(1, 4).encode().unwrap())
                                .unwrap();
                        }
                        h2::FrameType::Data => {
                            assert!(saw_headers);
                            let h2::FramePayload::Data(data) = frame.payload else {
                                unreachable!();
                            };
                            assert_eq!(data.as_ref(), b"ping");
                            assert!(frame.flags.contains(h2::FrameFlags::END_STREAM));
                            transport.close(cx).unwrap();
                            break;
                        }
                        _ => {}
                    }
                }
            });

            let mut client = h2::ClientConnection::new(client_transport, HttpConfig::default());
            let request = Request::builder()
                .method("POST")
                .uri("/stalled")
                .body(Body::copy_from_slice(b"ping", BodyLimits::new(16)).unwrap())
                .unwrap();
            let stream_id = client.send_request(cx, &request).unwrap();
            assert_eq!(stream_id.get(), 1);
            client.close(cx).unwrap();
            server.join(cx);
        });
    });
}

#[test]
fn protocol_limits_h2_repeated_end_stream_data_replenishes_connection_window() {
    const BODY_LEN: usize = 64;
    const ROUNDS: usize = 1100;

    let (client_transport, server_transport) = socket_transport_pair();
    let mut runtime = Runtime::new();
    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let server = scope.spawn(move |cx| {
                let response = Response::builder()
                    .status(200)
                    .body(Body::copy_from_slice(&[b's'; BODY_LEN], BodyLimits::new(128)).unwrap())
                    .unwrap();
                let mut server = h2::ServerConnection::new(server_transport, HttpConfig::default());
                for _ in 0..ROUNDS {
                    let incoming = server.accept(cx).unwrap().unwrap();
                    server
                        .send_response(cx, incoming.stream_id, &response)
                        .unwrap();
                }
                server.shutdown_write_and_close_after_peer(cx).unwrap();
            });

            let mut client = h2::ClientConnection::new(client_transport, HttpConfig::default());
            for round in 0..ROUNDS {
                let response = client
                    .send(cx, &request_with_body(BODY_LEN))
                    .unwrap_or_else(|error| panic!("round {round} failed: {error:?}"));
                assert_eq!(response.body().as_bytes(), &[b's'; BODY_LEN]);
            }
            client.close(cx).unwrap();
            server.join(cx);
        })
    });
}

fn request_with_body(body_len: usize) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/window")
        .body(Body::copy_from_slice(&vec![b'c'; body_len], BodyLimits::new(128)).unwrap())
        .unwrap()
}

fn socket_transport_pair() -> (StackTransport, StackTransport) {
    let (client, server) = socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::empty(),
        None,
    )
    .unwrap();
    (
        StackTransport::plaintext(client),
        StackTransport::plaintext(server),
    )
}

fn read_preface(cx: &kimojio_stack::RuntimeContext<'_>, transport: &mut StackTransport) {
    let mut preface = [0_u8; h2::CLIENT_PREFACE.len()];
    assert_eq!(
        transport.read_exact_or_eof(cx, &mut preface).unwrap(),
        preface.len()
    );
    assert_eq!(&preface, h2::CLIENT_PREFACE);
}

fn read_frame(cx: &kimojio_stack::RuntimeContext<'_>, transport: &mut StackTransport) -> h2::Frame {
    let mut header = [0_u8; h2::frame::FRAME_HEADER_LEN];
    assert_eq!(
        transport.read_exact_or_eof(cx, &mut header).unwrap(),
        header.len()
    );
    let len = ((header[0] as usize) << 16) | ((header[1] as usize) << 8) | header[2] as usize;
    let mut bytes = Vec::with_capacity(header.len() + len);
    bytes.extend_from_slice(&header);
    bytes.resize(header.len() + len, 0);
    if len > 0 {
        assert_eq!(
            transport
                .read_exact_or_eof(cx, &mut bytes[header.len()..])
                .unwrap(),
            len
        );
    }
    h2::Frame::decode(&bytes, h2::Settings::default().max_frame_size)
        .unwrap()
        .0
}
