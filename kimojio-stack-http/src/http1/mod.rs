// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub mod body;
pub mod client;
pub mod codec;
pub mod server;

pub use client::{ClientConnection, RuntimeClientConnection};
pub use server::{RuntimeServerConnection, ServerConnection};

/// HTTP/1.1 connection configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Http1Config {
    pub keep_alive: bool,
}

impl Default for Http1Config {
    fn default() -> Self {
        Self { keep_alive: true }
    }
}

#[cfg(test)]
mod tests {
    use http::{
        Request, Response, StatusCode,
        header::{TRANSFER_ENCODING, UPGRADE},
    };
    use kimojio_stack::Runtime;
    use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

    use crate::{
        Body, BodyLimits, ErrorKind, HttpConfig, StackTransport,
        http1::{ClientConnection, ServerConnection, body as http1_body, codec},
    };

    fn body(bytes: &[u8]) -> Body {
        Body::copy_from_slice(bytes, BodyLimits::new(1024)).unwrap()
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
    fn http1_body_chunks_deliver_content_length_chunked_and_eof() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            for (wire, kind, expected) in [
                (
                    b"abcdef".as_slice(),
                    http1_body::BodyKind::ContentLength(6),
                    b"abcdef".as_slice(),
                ),
                (
                    b"3\r\nabc\r\n3\r\ndef\r\n0\r\n\r\n".as_slice(),
                    http1_body::BodyKind::Chunked,
                    b"abcdef".as_slice(),
                ),
                (
                    b"abcdef".as_slice(),
                    http1_body::BodyKind::Eof,
                    b"abcdef".as_slice(),
                ),
            ] {
                let (mut reader, mut writer) = socket_transport_pair();
                cx.scope(|scope| {
                    let write = scope.spawn(move |cx| {
                        writer.write_all(cx, wire).unwrap();
                        writer.shutdown_write(cx).unwrap();
                    });
                    let mut read_buf = Vec::new();
                    let mut output = Vec::new();
                    http1_body::read_body_chunks(
                        cx,
                        &mut reader,
                        &mut read_buf,
                        kind,
                        BodyLimits::new(1024),
                        |chunk| {
                            output.extend_from_slice(&chunk);
                            Ok(())
                        },
                    )
                    .unwrap();
                    write.join(cx);
                    assert_eq!(output, expected);
                });
            }
        });
    }

    #[test]
    fn http1_request_head_and_body_writes_payload_separately() {
        let (mut client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let writer = scope.spawn(move |cx| {
                    let request = Request::builder()
                        .method("PUT")
                        .uri("/object")
                        .body(body(b"payload"))
                        .unwrap();
                    codec::write_request_head_and_body(cx, &mut client_transport, &request)
                        .unwrap();
                    client_transport.shutdown_write(cx).unwrap();
                });

                let mut server = ServerConnection::new(server_transport, HttpConfig::default());
                let incoming = server
                    .serve_one(cx, |request| {
                        assert_eq!(request.method(), "PUT");
                        assert_eq!(request.uri(), "/object");
                        assert_eq!(request.body().as_bytes(), b"payload");
                        Ok(Response::builder()
                            .status(StatusCode::OK)
                            .body(Body::empty())
                            .unwrap())
                    })
                    .unwrap();
                assert!(incoming);
                server.close(cx).unwrap();
                writer.join(cx);
            });
        });
    }

    #[test]
    fn http1_fixed_length_request_response_over_socketpair() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut server = ServerConnection::new(server_transport, HttpConfig::default());
                    server
                        .serve_one(cx, |request| {
                            assert_eq!(request.method(), "POST");
                            assert_eq!(request.uri(), "/echo");
                            assert_eq!(request.body().as_bytes(), b"ping");
                            Ok(Response::builder()
                                .status(StatusCode::OK)
                                .body(body(b"pong"))
                                .unwrap())
                        })
                        .unwrap();
                    server.close(cx).unwrap();
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
    fn http1_head_response_ignores_content_length_body() {
        let (client_transport, mut server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut request = [0_u8; 512];
                    let amount = server_transport.read(cx, &mut request).unwrap();
                    assert!(request[..amount].starts_with(b"HEAD /object HTTP/1.1\r\n"));
                    server_transport
                        .write_all(cx, b"HTTP/1.1 200 OK\r\ncontent-length: 123\r\n\r\n")
                        .unwrap();
                    server_transport.close(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let mut client = ClientConnection::new(client_transport, HttpConfig::default());
                    let request = Request::builder()
                        .method("HEAD")
                        .uri("/object")
                        .body(Body::empty())
                        .unwrap();
                    let response = client.send(cx, &request).unwrap();
                    client.close(cx).unwrap();
                    response
                });

                server.join(cx);
                let response = client.join(cx);
                assert_eq!(response.status(), StatusCode::OK);
                assert!(response.body().is_empty());
            });
        });
    }

    #[test]
    fn http1_chunked_request_response_over_socketpair() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut server = ServerConnection::new(server_transport, HttpConfig::default());
                    server
                        .serve_one(cx, |request| {
                            assert_eq!(request.body().as_bytes(), b"chunked request");
                            Ok(Response::builder()
                                .status(StatusCode::OK)
                                .header(TRANSFER_ENCODING, "chunked")
                                .body(body(b"chunked response"))
                                .unwrap())
                        })
                        .unwrap();
                    server.close(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let mut client = ClientConnection::new(client_transport, HttpConfig::default());
                    let request = Request::builder()
                        .method("POST")
                        .uri("/chunked")
                        .header(TRANSFER_ENCODING, "chunked")
                        .body(body(b"chunked request"))
                        .unwrap();
                    let response = client.send(cx, &request).unwrap();
                    client.close(cx).unwrap();
                    response
                });

                server.join(cx);
                let response = client.join(cx);
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(response.body().as_bytes(), b"chunked response");
            });
        });
    }

    #[test]
    fn http1_keep_alive_sequential_requests() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut server = ServerConnection::new(server_transport, HttpConfig::default());
                    for expected in [b"one".as_slice(), b"two".as_slice()] {
                        let keep_alive = server
                            .serve_one(cx, |request| {
                                assert_eq!(request.body().as_bytes(), expected);
                                Ok(Response::builder()
                                    .status(StatusCode::OK)
                                    .body(Body::copy_from_slice(
                                        request.body().as_bytes(),
                                        BodyLimits::new(1024),
                                    )?)
                                    .unwrap())
                            })
                            .unwrap();
                        assert!(keep_alive);
                    }
                    server.close(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let mut client = ClientConnection::new(client_transport, HttpConfig::default());
                    let first = Request::builder()
                        .method("POST")
                        .uri("/seq")
                        .body(body(b"one"))
                        .unwrap();
                    let first_response = client.send(cx, &first).unwrap();
                    assert_eq!(first_response.body().as_bytes(), b"one");

                    let second = Request::builder()
                        .method("POST")
                        .uri("/seq")
                        .body(body(b"two"))
                        .unwrap();
                    let second_response = client.send(cx, &second).unwrap();
                    client.close(cx).unwrap();
                    second_response
                });

                server.join(cx);
                let response = client.join(cx);
                assert_eq!(response.body().as_bytes(), b"two");
            });
        });
    }

    #[test]
    fn http1_response_body_can_be_delimited_by_eof() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut transport = server_transport;
                    let mut read_buf = Vec::new();
                    let request = codec::read_request(
                        cx,
                        &mut transport,
                        &mut read_buf,
                        HttpConfig::default(),
                    )
                    .unwrap()
                    .unwrap();
                    assert_eq!(request.request.uri(), "/eof");
                    transport
                        .write_all(cx, b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\neof-body")
                        .unwrap();
                    transport.shutdown_write(cx).unwrap();
                    transport.close(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let mut client = ClientConnection::new(client_transport, HttpConfig::default());
                    let request = Request::builder()
                        .method("GET")
                        .uri("/eof")
                        .body(Body::empty())
                        .unwrap();
                    let response = client.send(cx, &request).unwrap();
                    client.close(cx).unwrap();
                    response
                });

                server.join(cx);
                let response = client.join(cx);
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(response.body().as_bytes(), b"eof-body");
            });
        });
    }

    #[test]
    fn http1_rejects_malformed_header() {
        let error = raw_request_error(b"GET / HTTP/1.1\r\nbad header\r\n\r\n");
        assert_eq!(error.kind(), ErrorKind::Parse);
    }

    #[test]
    fn http1_rejects_invalid_content_length() {
        let error = raw_request_error(b"POST / HTTP/1.1\r\ncontent-length: nope\r\n\r\nhello");
        assert_eq!(error.kind(), ErrorKind::Parse);
    }

    #[test]
    fn http1_rejects_body_over_configured_limit() {
        let config = HttpConfig {
            max_body_bytes: 2,
            ..HttpConfig::default()
        };
        let error = raw_request_error_with_config(
            b"POST / HTTP/1.1\r\ncontent-length: 5\r\n\r\nhello",
            config,
        );
        assert_eq!(error.kind(), ErrorKind::SizeLimit);
    }

    #[test]
    fn http1_rejects_chunked_body_over_configured_limit_before_payload() {
        let config = HttpConfig {
            max_body_bytes: 2,
            ..HttpConfig::default()
        };
        let error = raw_request_error_with_config(
            b"POST / HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
            config,
        );
        assert_eq!(error.kind(), ErrorKind::SizeLimit);
    }

    #[test]
    fn http1_rejects_start_line_over_configured_limit() {
        let config = HttpConfig {
            max_start_line_len: 8,
            ..HttpConfig::default()
        };
        let error = raw_request_error_with_config(b"GET /long HTTP/1.1\r\n\r\n", config);
        assert_eq!(error.kind(), ErrorKind::SizeLimit);
    }

    #[test]
    fn http1_rejects_header_bytes_over_configured_limit() {
        let config = HttpConfig {
            max_header_bytes: 8,
            ..HttpConfig::default()
        };
        let error =
            raw_request_error_with_config(b"GET / HTTP/1.1\r\nx-long: value\r\n\r\n", config);
        assert_eq!(error.kind(), ErrorKind::SizeLimit);
    }

    #[test]
    fn http1_rejects_upgrade_requests() {
        let request = Request::builder()
            .method("GET")
            .uri("/")
            .header(UPGRADE, "websocket")
            .body(Body::empty())
            .unwrap();
        let mut bytes = Vec::new();
        codec::write_request_to_vec(&mut bytes, &request).unwrap();
        let error = raw_request_error(&bytes);
        assert_eq!(error.kind(), ErrorKind::Unsupported);
    }

    #[test]
    fn http1_rejects_unsupported_transfer_encoding() {
        let error = raw_request_error(b"POST / HTTP/1.1\r\ntransfer-encoding: gzip\r\n\r\n");
        assert_eq!(error.kind(), ErrorKind::Unsupported);
    }

    #[test]
    fn http1_rejects_pipelined_requests() {
        let error = raw_request_error(b"GET /one HTTP/1.1\r\n\r\nGET /two HTTP/1.1\r\n\r\n");
        assert_eq!(error.kind(), ErrorKind::Unsupported);
    }

    fn raw_request_error(bytes: &[u8]) -> crate::Error {
        raw_request_error_with_config(bytes, HttpConfig::default())
    }

    fn raw_request_error_with_config(bytes: &[u8], config: HttpConfig) -> crate::Error {
        let (client_transport, server_transport) = socket_transport_pair();
        let bytes = bytes.to_vec();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut transport = server_transport;
                    let mut read_buf = Vec::new();
                    codec::read_request(cx, &mut transport, &mut read_buf, config).unwrap_err()
                });

                let client = scope.spawn(move |cx| {
                    let mut transport = client_transport;
                    transport.write_all(cx, &bytes).unwrap();
                    transport.shutdown_write(cx).unwrap();
                    transport.close(cx).unwrap();
                });

                client.join(cx);
                server.join(cx)
            })
        })
    }
}
