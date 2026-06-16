// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! HTTP/2 connection, stream, frame, and settings primitives.
//!
//! The public client and server connection types expose a low-level, stackful
//! HTTP/2 surface. Unary request/response helpers buffer bodies into
//! [`Body`](crate::Body), while streaming helpers expose response headers,
//! individual DATA chunks, and terminal trailers so higher-level protocols such
//! as gRPC can avoid whole-body buffering.
//!
//! ```
//! use kimojio_stack_http::h2::{Settings, Setting, SettingId};
//!
//! let mut settings = Settings::default();
//! settings.apply(Setting::new(SettingId::MaxConcurrentStreams, 32)).unwrap();
//! assert_eq!(settings.max_concurrent_streams, 32);
//! ```

pub mod client;
mod codec;
pub mod connection;
pub mod frame;
mod hpack;
pub mod server;
pub mod settings;
pub mod stream;

pub use client::{
    ClientConnection, ResponseStreamEvent, ResponseWithTrailers, RuntimeClientConnection,
};
pub use connection::ConnectionState;
pub use frame::{Frame, FrameFlags, FramePayload, FrameType};
pub use hpack::Header;
pub use server::{IncomingRequest, RuntimeServerConnection, ServerConnection};
pub use settings::{Setting, SettingId, Settings};
pub use stream::{FlowControlWindow, Stream, StreamId, StreamState};

/// HTTP/2 error code for stream cancellation.
pub const ERROR_CODE_CANCEL: u32 = 8;

/// HTTP/2 client connection preface.
pub const CLIENT_PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// HTTP/2 connection settings used by the stackful connection core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct H2Config {
    /// Initial per-stream inbound flow-control window.
    pub initial_stream_window: u32,
    /// Initial connection-level inbound flow-control window.
    pub initial_connection_window: u32,
    /// Maximum frame payload size to advertise and accept.
    pub max_frame_size: u32,
    /// Maximum concurrent streams to advertise to the peer.
    pub max_concurrent_streams: u32,
}

impl Default for H2Config {
    fn default() -> Self {
        Self {
            initial_stream_window: 65_535,
            initial_connection_window: 65_535,
            max_frame_size: 16_384,
            max_concurrent_streams: 100,
        }
    }
}

/// Validates the exact HTTP/2 client connection preface.
///
/// Servers call this while initializing an HTTP/2 connection. It is exposed for
/// tests and custom handshake code that needs the same protocol check.
pub fn validate_client_preface(bytes: &[u8]) -> Result<(), crate::Error> {
    if bytes == CLIENT_PREFACE {
        Ok(())
    } else {
        Err(crate::Error::Protocol("invalid HTTP/2 client preface"))
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use http::{
        HeaderName, HeaderValue, Request, Response, StatusCode,
        header::{CONTENT_TYPE, TE},
    };
    use kimojio_stack::{Runtime, SocketIoRuntime, channel};
    use kimojio_stack_steal::{
        Runtime as StealRuntime, RuntimeConfig as StealRuntimeConfig, StealPolicy,
    };
    use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};
    use std::num::NonZeroUsize;

    use crate::{
        Body, BodyLimits, ErrorKind, HttpConfig, RuntimeStackTransport, StackTransport, Trailers,
        client::{
            ClientConnection as AnyClientConnection,
            RuntimeClientConnection as AnyRuntimeClientConnection,
        },
        server::{
            RuntimeServerConnection as AnyRuntimeServerConnection,
            ServerConnection as AnyServerConnection,
        },
    };

    use super::{
        ClientConnection, ConnectionState, ERROR_CODE_CANCEL, Frame, Header, ResponseStreamEvent,
        ServerConnection, Setting, SettingId, Settings, StreamId, codec,
    };

    fn body(bytes: &[u8]) -> Body {
        Body::copy_from_slice(bytes, BodyLimits::new(64 * 1024)).unwrap()
    }

    fn socket_transport_pair() -> (StackTransport, StackTransport) {
        let (client_fd, server_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .unwrap();
        (
            StackTransport::plaintext(client_fd),
            StackTransport::plaintext(server_fd),
        )
    }

    #[test]
    fn h2_client_server_request_response_over_socketpair() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut server = ServerConnection::new(server_transport, HttpConfig::default());
                    let incoming = server.accept(cx).unwrap().unwrap();
                    assert_eq!(incoming.request.method(), "POST");
                    assert_eq!(incoming.request.uri(), "/echo");
                    assert_eq!(incoming.request.body().as_bytes(), b"ping");
                    let response = Response::builder()
                        .status(StatusCode::OK)
                        .header(CONTENT_TYPE, "text/plain")
                        .body(body(b"pong"))
                        .unwrap();
                    server
                        .send_response(cx, incoming.stream_id, &response)
                        .unwrap();
                    server.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let mut client = ClientConnection::new(client_transport, HttpConfig::default());
                    let request = Request::builder()
                        .method("POST")
                        .uri("/echo")
                        .body(body(b"ping"))
                        .unwrap();
                    let response = client.send(cx, &request).unwrap();
                    client.close(cx).unwrap();
                    response
                });

                server.join(cx);
                let response = client.join(cx);
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(response.body().as_bytes(), b"pong");
            });
        });
    }

    #[test]
    fn h2_response_trailers_over_socketpair() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut server = ServerConnection::new(server_transport, HttpConfig::default());
                    let incoming = server.accept(cx).unwrap().unwrap();
                    let response = Response::builder()
                        .status(StatusCode::OK)
                        .body(body(b"reply"))
                        .unwrap();
                    let mut trailers = Trailers::new();
                    trailers.insert(
                        HeaderName::from_static("grpc-status"),
                        HeaderValue::from_static("0"),
                    );
                    server
                        .send_response_with_trailers(
                            cx,
                            incoming.stream_id,
                            &response,
                            Some(&trailers),
                        )
                        .unwrap();
                    server.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let mut client = ClientConnection::new(client_transport, HttpConfig::default());
                    let request = Request::builder()
                        .method("POST")
                        .uri("/grpc.Service/Method")
                        .header(TE, "trailers")
                        .body(body(b"request"))
                        .unwrap();
                    let stream_id = client.send_request(cx, &request).unwrap();
                    let response = client.read_response_with_trailers(cx, stream_id).unwrap();
                    client.close(cx).unwrap();
                    response
                });

                server.join(cx);
                let response = client.join(cx);
                assert_eq!(response.response.body().as_bytes(), b"reply");
                assert_eq!(
                    response
                        .trailers
                        .get(&HeaderName::from_static("grpc-status")),
                    Some(&HeaderValue::from_static("0"))
                );
            });
        });
    }

    #[test]
    fn h2_incremental_response_stream_delivers_data_and_trailers() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut server = ServerConnection::new(server_transport, HttpConfig::default());
                    let incoming = server.accept(cx).unwrap().unwrap();
                    let response = Response::builder()
                        .status(StatusCode::OK)
                        .header(CONTENT_TYPE, "application/octet-stream")
                        .header("x-stream", "yes")
                        .body(())
                        .unwrap();
                    server
                        .send_response_headers(cx, incoming.stream_id, &response)
                        .unwrap();
                    server
                        .send_response_data(cx, incoming.stream_id, b"one")
                        .unwrap();
                    server
                        .send_response_data(cx, incoming.stream_id, b"two")
                        .unwrap();
                    let mut trailers = Trailers::new();
                    trailers.insert(
                        HeaderName::from_static("x-terminal"),
                        HeaderValue::from_static("done"),
                    );
                    server
                        .finish_response_stream(cx, incoming.stream_id, Some(&trailers))
                        .unwrap();
                    server.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let mut client = ClientConnection::new(client_transport, HttpConfig::default());
                    let request = Request::builder()
                        .method("POST")
                        .uri("/stream")
                        .body(Body::empty())
                        .unwrap();
                    let stream_id = client.send_request(cx, &request).unwrap();
                    let response = client.read_response_headers(cx, stream_id).unwrap();
                    assert_eq!(response.status(), StatusCode::OK);
                    assert_eq!(response.headers()["x-stream"], "yes");

                    let first = client.read_response_event(cx, stream_id).unwrap();
                    let second = client.read_response_event(cx, stream_id).unwrap();
                    let terminal = client.read_response_event(cx, stream_id).unwrap();
                    client.close(cx).unwrap();
                    (first, second, terminal)
                });

                server.join(cx);
                let (first, second, terminal) = client.join(cx);
                assert!(matches!(first, ResponseStreamEvent::Data(data) if data == b"one"[..]));
                assert!(matches!(second, ResponseStreamEvent::Data(data) if data == b"two"[..]));
                assert!(matches!(terminal, ResponseStreamEvent::Trailers(trailers)
                        if trailers.get(&HeaderName::from_static("x-terminal"))
                            == Some(&HeaderValue::from_static("done"))));
            });
        });
    }

    #[test]
    fn h2_response_body_chunks_are_delivered_incrementally() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut server = ServerConnection::new(server_transport, HttpConfig::default());
                    let incoming = server.accept(cx).unwrap().unwrap();
                    let response = Response::builder()
                        .status(StatusCode::OK)
                        .body(body(b"chunk-one-and-two"))
                        .unwrap();
                    server
                        .send_response(cx, incoming.stream_id, &response)
                        .unwrap();
                    server.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let mut client = ClientConnection::new(client_transport, HttpConfig::default());
                    let request = Request::builder()
                        .method("GET")
                        .uri("/stream")
                        .body(Body::empty())
                        .unwrap();
                    let stream_id = client.send_request(cx, &request).unwrap();
                    let mut chunks = Vec::new();
                    let response = client
                        .read_response_with_body_chunks(cx, stream_id, |chunk| {
                            chunks.push(chunk);
                            Ok(())
                        })
                        .unwrap();
                    client.close(cx).unwrap();
                    (response, chunks)
                });

                server.join(cx);
                let (response, chunks) = client.join(cx);
                assert_eq!(response.response.status(), StatusCode::OK);
                assert!(response.response.body().is_empty());
                assert_eq!(
                    chunks
                        .iter()
                        .flat_map(|chunk| chunk.iter().copied())
                        .collect::<Vec<_>>(),
                    b"chunk-one-and-two"
                );
            });
        });
    }

    #[test]
    fn h2_response_stream_can_finish_without_trailers() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut server = ServerConnection::new(server_transport, HttpConfig::default());
                    let incoming = server.accept(cx).unwrap().unwrap();
                    let response = Response::builder()
                        .status(StatusCode::NO_CONTENT)
                        .body(())
                        .unwrap();
                    server
                        .send_response_headers(cx, incoming.stream_id, &response)
                        .unwrap();
                    server
                        .finish_response_stream(cx, incoming.stream_id, None)
                        .unwrap();
                    server.close(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let mut client = ClientConnection::new(client_transport, HttpConfig::default());
                    let request = Request::builder()
                        .method("GET")
                        .uri("/empty-stream")
                        .body(Body::empty())
                        .unwrap();
                    let stream_id = client.send_request(cx, &request).unwrap();
                    let response = client.read_response_headers(cx, stream_id).unwrap();
                    let terminal = client.read_response_event(cx, stream_id).unwrap();
                    client.close(cx).unwrap();
                    (response, terminal)
                });

                server.join(cx);
                let (response, terminal) = client.join(cx);
                assert_eq!(response.status(), StatusCode::NO_CONTENT);
                assert!(matches!(terminal, ResponseStreamEvent::Trailers(trailers) if trailers.is_empty()));
            });
        });
    }

    #[test]
    fn h2_concurrent_response_streams_preserve_association_out_of_order() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut server = ServerConnection::new(server_transport, HttpConfig::default());
                    let first = server.accept(cx).unwrap().unwrap();
                    let second = server.accept(cx).unwrap().unwrap();
                    assert_eq!(first.request.uri(), "/first-stream");
                    assert_eq!(second.request.uri(), "/second-stream");

                    let second_response = Response::builder()
                        .status(StatusCode::OK)
                        .header("x-stream", "second")
                        .body(())
                        .unwrap();
                    server
                        .send_response_headers(cx, second.stream_id, &second_response)
                        .unwrap();
                    server
                        .send_response_data(cx, second.stream_id, b"second")
                        .unwrap();
                    server
                        .finish_response_stream(cx, second.stream_id, None)
                        .unwrap();

                    let first_response = Response::builder()
                        .status(StatusCode::OK)
                        .header("x-stream", "first")
                        .body(())
                        .unwrap();
                    server
                        .send_response_headers(cx, first.stream_id, &first_response)
                        .unwrap();
                    server
                        .send_response_data(cx, first.stream_id, b"first")
                        .unwrap();
                    server
                        .finish_response_stream(cx, first.stream_id, None)
                        .unwrap();
                    server.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let mut client = ClientConnection::new(client_transport, HttpConfig::default());
                    let first = Request::builder()
                        .method("GET")
                        .uri("/first-stream")
                        .body(Body::empty())
                        .unwrap();
                    let second = Request::builder()
                        .method("GET")
                        .uri("/second-stream")
                        .body(Body::empty())
                        .unwrap();
                    let first_id = client.send_request(cx, &first).unwrap();
                    let second_id = client.send_request(cx, &second).unwrap();

                    let second_head = client.read_response_headers(cx, second_id).unwrap();
                    let first_head = client.read_response_headers(cx, first_id).unwrap();
                    let second_data = client.read_response_event(cx, second_id).unwrap();
                    let first_data = client.read_response_event(cx, first_id).unwrap();
                    let second_end = client.read_response_event(cx, second_id).unwrap();
                    let first_end = client.read_response_event(cx, first_id).unwrap();
                    client.close(cx).unwrap();
                    (
                        first_head,
                        first_data,
                        first_end,
                        second_head,
                        second_data,
                        second_end,
                    )
                });

                server.join(cx);
                let (first_head, first_data, first_end, second_head, second_data, second_end) =
                    client.join(cx);
                assert_eq!(first_head.headers()["x-stream"], "first");
                assert!(matches!(first_data, ResponseStreamEvent::Data(data) if data == b"first"[..]));
                assert!(matches!(first_end, ResponseStreamEvent::Trailers(trailers) if trailers.is_empty()));
                assert_eq!(second_head.headers()["x-stream"], "second");
                assert!(
                    matches!(second_data, ResponseStreamEvent::Data(data) if data == b"second"[..])
                );
                assert!(
                    matches!(second_end, ResponseStreamEvent::Trailers(trailers) if trailers.is_empty())
                );
            });
        });
    }

    #[test]
    fn h2_response_stream_can_be_read_incrementally_after_completing_off_target() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut server = ServerConnection::new(server_transport, HttpConfig::default());
                    let first = server.accept(cx).unwrap().unwrap();
                    let second = server.accept(cx).unwrap().unwrap();

                    let first_response = Response::builder()
                        .status(StatusCode::OK)
                        .header("x-stream", "first")
                        .body(())
                        .unwrap();
                    server
                        .send_response_headers(cx, first.stream_id, &first_response)
                        .unwrap();
                    server
                        .send_response_data(cx, first.stream_id, b"first")
                        .unwrap();
                    server
                        .finish_response_stream(cx, first.stream_id, None)
                        .unwrap();

                    let second_response = Response::builder()
                        .status(StatusCode::OK)
                        .header("x-stream", "second")
                        .body(())
                        .unwrap();
                    server
                        .send_response_headers(cx, second.stream_id, &second_response)
                        .unwrap();
                    server
                        .send_response_data(cx, second.stream_id, b"second")
                        .unwrap();
                    server
                        .finish_response_stream(cx, second.stream_id, None)
                        .unwrap();
                    server.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let mut client = ClientConnection::new(client_transport, HttpConfig::default());
                    let first = Request::builder()
                        .method("GET")
                        .uri("/first-stream")
                        .body(Body::empty())
                        .unwrap();
                    let second = Request::builder()
                        .method("GET")
                        .uri("/second-stream")
                        .body(Body::empty())
                        .unwrap();
                    let first_id = client.send_request(cx, &first).unwrap();
                    let second_id = client.send_request(cx, &second).unwrap();

                    let second_head = client.read_response_headers(cx, second_id).unwrap();
                    let first_head = client.read_response_headers(cx, first_id).unwrap();
                    let first_data = client.read_response_event(cx, first_id).unwrap();
                    let first_end = client.read_response_event(cx, first_id).unwrap();
                    let second_data = client.read_response_event(cx, second_id).unwrap();
                    let second_end = client.read_response_event(cx, second_id).unwrap();
                    client.close(cx).unwrap();
                    (
                        first_head,
                        first_data,
                        first_end,
                        second_head,
                        second_data,
                        second_end,
                    )
                });

                server.join(cx);
                let (first_head, first_data, first_end, second_head, second_data, second_end) =
                    client.join(cx);
                assert_eq!(first_head.headers()["x-stream"], "first");
                assert!(matches!(first_data, ResponseStreamEvent::Data(data) if data == b"first"[..]));
                assert!(matches!(first_end, ResponseStreamEvent::Trailers(trailers) if trailers.is_empty()));
                assert_eq!(second_head.headers()["x-stream"], "second");
                assert!(
                    matches!(second_data, ResponseStreamEvent::Data(data) if data == b"second"[..])
                );
                assert!(
                    matches!(second_end, ResponseStreamEvent::Trailers(trailers) if trailers.is_empty())
                );
            });
        });
    }

    #[test]
    fn h2_response_stream_cancel_sends_reset_observed_by_blocked_server_send() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();
        let client_settings = Settings {
            initial_window_size: 0,
            ..Settings::default()
        };
        let (headers_sent_tx, headers_sent_rx) = channel::bounded::<()>(1);
        let (cancelled_tx, cancelled_rx) = channel::bounded::<()>(1);

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut server = ServerConnection::new(server_transport, HttpConfig::default());
                    let incoming = server.accept(cx).unwrap().unwrap();
                    let response = Response::builder().status(StatusCode::OK).body(()).unwrap();
                    server
                        .send_response_headers(cx, incoming.stream_id, &response)
                        .unwrap();
                    headers_sent_tx.send(cx, ()).unwrap();
                    cancelled_rx.recv(cx).unwrap();
                    server
                        .send_response_data(cx, incoming.stream_id, b"blocked")
                        .unwrap_err()
                        .kind()
                });

                let client = scope.spawn(move |cx| {
                    let mut client = ClientConnection::new_with_settings(
                        client_transport,
                        HttpConfig::default(),
                        client_settings,
                    );
                    let request = Request::builder()
                        .method("GET")
                        .uri("/cancel")
                        .body(Body::empty())
                        .unwrap();
                    let stream_id = client.send_request(cx, &request).unwrap();
                    headers_sent_rx.recv(cx).unwrap();
                    client.read_response_headers(cx, stream_id).unwrap();
                    client
                        .cancel_response_stream(cx, stream_id, ERROR_CODE_CANCEL)
                        .unwrap();
                    cancelled_tx.send(cx, ()).unwrap();
                    client.close(cx).unwrap();
                });

                client.join(cx);
                assert_eq!(server.join(cx), ErrorKind::PeerReset);
            });
        });
    }
    #[test]
    fn h2_concurrent_streams_preserve_association_out_of_order() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut server = ServerConnection::new(server_transport, HttpConfig::default());
                    let first = server.accept(cx).unwrap().unwrap();
                    let second = server.accept(cx).unwrap().unwrap();
                    assert_eq!(first.request.body().as_bytes(), b"one");
                    assert_eq!(second.request.body().as_bytes(), b"two");

                    let second_response = Response::builder()
                        .status(StatusCode::OK)
                        .body(body(b"second"))
                        .unwrap();
                    let first_response = Response::builder()
                        .status(StatusCode::OK)
                        .body(body(b"first"))
                        .unwrap();
                    server
                        .send_response(cx, second.stream_id, &second_response)
                        .unwrap();
                    server
                        .send_response(cx, first.stream_id, &first_response)
                        .unwrap();
                    server.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let mut client = ClientConnection::new(client_transport, HttpConfig::default());
                    let first = Request::builder()
                        .method("POST")
                        .uri("/one")
                        .body(body(b"one"))
                        .unwrap();
                    let second = Request::builder()
                        .method("POST")
                        .uri("/two")
                        .body(body(b"two"))
                        .unwrap();
                    let first_id = client.send_request(cx, &first).unwrap();
                    let second_id = client.send_request(cx, &second).unwrap();
                    let second_response = client.read_response(cx, second_id).unwrap();
                    let first_response = client.read_response(cx, first_id).unwrap();
                    client.close(cx).unwrap();
                    (first_response, second_response)
                });

                server.join(cx);
                let (first_response, second_response) = client.join(cx);
                assert_eq!(first_response.body().as_bytes(), b"first");
                assert_eq!(second_response.body().as_bytes(), b"second");
            });
        });
    }

    #[test]
    fn h2_flow_control_blocks_and_unblocks_with_window_updates() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();
        let server_settings = Settings {
            initial_window_size: 5,
            ..Settings::default()
        };

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut server = ServerConnection::new_with_settings(
                        server_transport,
                        HttpConfig::default(),
                        server_settings,
                    );
                    let incoming = server.accept(cx).unwrap().unwrap();
                    assert_eq!(incoming.request.body().as_bytes(), b"abcdefghijkl");
                    let response = Response::builder()
                        .status(StatusCode::OK)
                        .body(body(b"ok"))
                        .unwrap();
                    server
                        .send_response(cx, incoming.stream_id, &response)
                        .unwrap();
                    server.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let mut client = ClientConnection::new(client_transport, HttpConfig::default());
                    let request = Request::builder()
                        .method("POST")
                        .uri("/flow")
                        .body(body(b"abcdefghijkl"))
                        .unwrap();
                    let response = client.send(cx, &request).unwrap();
                    client.close(cx).unwrap();
                    response
                });

                server.join(cx);
                let response = client.join(cx);
                assert_eq!(response.body().as_bytes(), b"ok");
            });
        });
    }

    #[test]
    fn h2_large_header_block_uses_continuation_frames() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();
        let large_header = "x".repeat(20 * 1024);

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server_header = large_header.clone();
                let server = scope.spawn(move |cx| {
                    let mut server = ServerConnection::new(server_transport, HttpConfig::default());
                    let incoming = server.accept(cx).unwrap().unwrap();
                    let response = Response::builder()
                        .status(StatusCode::OK)
                        .header("x-large", server_header)
                        .body(body(b"ok"))
                        .unwrap();
                    server
                        .send_response(cx, incoming.stream_id, &response)
                        .unwrap();
                    server.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let mut client = ClientConnection::new(client_transport, HttpConfig::default());
                    let request = Request::builder()
                        .method("GET")
                        .uri("/large-headers")
                        .body(Body::empty())
                        .unwrap();
                    let response = client.send(cx, &request).unwrap();
                    client.close(cx).unwrap();
                    response
                });

                server.join(cx);
                let response = client.join(cx);
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(response.headers()["x-large"], large_header);
                assert_eq!(response.body().as_bytes(), b"ok");
            });
        });
    }

    #[test]
    fn h2_server_rejects_malformed_pseudo_headers() {
        let (mut client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut server = ServerConnection::new(server_transport, HttpConfig::default());
                    server.accept(cx).unwrap_err().kind()
                });

                let client = scope.spawn(move |cx| {
                    let mut encoder = ConnectionState::new(Settings::default()).unwrap();
                    codec::write_client_preface(cx, &mut client_transport).unwrap();
                    codec::write_frame(
                        cx,
                        &mut client_transport,
                        &codec::settings_frame(Settings::default()),
                    )
                    .unwrap();
                    let block = encoder.encode_header_block(&[Header::new(":path", "/bad")]);
                    codec::write_frame(
                        cx,
                        &mut client_transport,
                        &Frame::headers(StreamId::new(1).unwrap(), Bytes::from(block), true),
                    )
                    .unwrap();
                    codec::read_frame(
                        cx,
                        &mut client_transport,
                        Settings::default().max_frame_size,
                    )
                    .unwrap();
                    codec::read_frame(
                        cx,
                        &mut client_transport,
                        Settings::default().max_frame_size,
                    )
                    .unwrap();
                    client_transport.close(cx).unwrap();
                });

                client.join(cx);
                assert_eq!(server.join(cx), ErrorKind::Protocol);
            });
        });
    }

    #[test]
    fn h2_stream_reset_and_goaway_surface_peer_reset() {
        for goaway in [false, true] {
            let (client_transport, server_transport) = socket_transport_pair();
            let mut runtime = Runtime::new();

            runtime.block_on(|cx| {
                cx.scope(|scope| {
                    let server = scope.spawn(move |cx| {
                        let mut server =
                            ServerConnection::new(server_transport, HttpConfig::default());
                        let incoming = server.accept(cx).unwrap().unwrap();
                        if goaway {
                            server.goaway(cx, 0, ERROR_CODE_CANCEL).unwrap();
                        } else {
                            server
                                .reset_stream(cx, incoming.stream_id, ERROR_CODE_CANCEL)
                                .unwrap();
                        }
                        server.close(cx).unwrap();
                    });

                    let client = scope.spawn(move |cx| {
                        let mut client =
                            ClientConnection::new(client_transport, HttpConfig::default());
                        let request = Request::builder()
                            .method("POST")
                            .uri("/reset")
                            .body(body(b"request"))
                            .unwrap();
                        let stream_id = client.send_request(cx, &request).unwrap();
                        let error = client.read_response(cx, stream_id).unwrap_err();
                        client.close(cx).unwrap();
                        error.kind()
                    });

                    server.join(cx);
                    assert_eq!(client.join(cx), ErrorKind::PeerReset);
                });
            });
        }
    }

    #[test]
    fn h2_protocol_neutral_dispatch_uses_http2() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut server =
                        AnyServerConnection::http2(server_transport, HttpConfig::default());
                    server
                        .serve_one(cx, |request| {
                            assert_eq!(request.uri(), "/neutral");
                            Ok(Response::builder()
                                .status(StatusCode::OK)
                                .body(body(request.body().as_bytes()))
                                .unwrap())
                        })
                        .unwrap();
                    server.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let mut client =
                        AnyClientConnection::http2(client_transport, HttpConfig::default());
                    let request = Request::builder()
                        .method("POST")
                        .uri("/neutral")
                        .body(body(b"dispatch"))
                        .unwrap();
                    let response = client.send(cx, &request).unwrap();
                    client.close(cx).unwrap();
                    response
                });

                server.join(cx);
                let response = client.join(cx);
                assert_eq!(response.body().as_bytes(), b"dispatch");
            });
        });
    }

    #[test]
    fn h2_protocol_neutral_dispatch_runs_on_stealing_runtime() {
        let (client_fd, server_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .unwrap();
        let mut runtime = StealRuntime::with_config(StealRuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..StealRuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn_stealable(move |cx| {
                    let server_fd = cx.socket_from_owned_fd(server_fd).unwrap();
                    let server_transport = RuntimeStackTransport::plaintext_socket(server_fd);
                    let mut server =
                        AnyRuntimeServerConnection::http2(server_transport, HttpConfig::default());
                    server
                        .serve_one(cx, |request| {
                            assert_eq!(request.uri(), "/steal-neutral");
                            Ok(Response::builder()
                                .status(StatusCode::OK)
                                .body(body(request.body().as_bytes()))
                                .unwrap())
                        })
                        .unwrap();
                    server.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                let client = scope.spawn_stealable(move |cx| {
                    let client_fd = cx.socket_from_owned_fd(client_fd).unwrap();
                    let client_transport = RuntimeStackTransport::plaintext_socket(client_fd);
                    let mut client =
                        AnyRuntimeClientConnection::http2(client_transport, HttpConfig::default());
                    let request = Request::builder()
                        .method("POST")
                        .uri("/steal-neutral")
                        .body(body(b"dispatch-steal"))
                        .unwrap();
                    let response = client.send(cx, &request).unwrap();
                    client.close(cx).unwrap();
                    response
                });

                server.join(cx);
                let response = client.join(cx);
                assert_eq!(response.body().as_bytes(), b"dispatch-steal");
            });
        });
    }

    #[test]
    fn h2_settings_ack_is_processed_before_request_frames() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut server = ServerConnection::new(server_transport, HttpConfig::default());
                    let incoming = server.accept(cx).unwrap().unwrap();
                    assert_eq!(incoming.stream_id, StreamId::new(1).unwrap());
                    let response = Response::builder()
                        .status(StatusCode::NO_CONTENT)
                        .body(Body::empty())
                        .unwrap();
                    server
                        .send_response(cx, incoming.stream_id, &response)
                        .unwrap();
                    server.close(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let mut client = ClientConnection::new(client_transport, HttpConfig::default());
                    let request = Request::builder()
                        .method("GET")
                        .uri("/ack")
                        .body(Body::empty())
                        .unwrap();
                    let response = client.send(cx, &request).unwrap();
                    client.close(cx).unwrap();
                    response
                });

                server.join(cx);
                let response = client.join(cx);
                assert_eq!(response.status(), StatusCode::NO_CONTENT);
            });
        });
    }

    #[test]
    fn h2_request_headers_include_common_static_table_entries() {
        let request = Request::builder()
            .method("POST")
            .uri("/static")
            .body(Body::empty())
            .unwrap();
        let headers = codec::request_headers(&request).unwrap();

        assert!(
            headers
                .iter()
                .any(|header| header.name.as_ref() == b":method")
        );
        assert!(
            headers
                .iter()
                .any(|header| header.name.as_ref() == b":path")
        );
    }

    #[test]
    fn h2_raw_settings_frame_roundtrip_for_configured_limits() {
        let frame = codec::settings_frame(Settings {
            max_header_list_size: 4096,
            initial_window_size: 32,
            ..Settings::default()
        });
        let encoded = frame.encode().unwrap();
        let (decoded, consumed) =
            Frame::decode(&encoded, Settings::default().max_frame_size).unwrap();

        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded, frame);
        assert!(
            matches!(decoded.payload, super::FramePayload::Settings(settings) if settings.contains(&Setting::new(SettingId::InitialWindowSize, 32)))
        );
    }
}
