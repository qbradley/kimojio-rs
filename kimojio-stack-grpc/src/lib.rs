// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Unary-first gRPC foundations for `kimojio-stack`.

pub mod client;
pub mod codec;
pub mod error;
pub mod metadata;
pub mod server;
pub mod status;

pub use client::{UnaryClient, UnaryResponse};
pub use codec::{decode_message, encode_message};
pub use error::{Error, ErrorKind};
pub use metadata::Metadata;
pub use server::{UnaryReply, UnaryServer};
pub use status::{Status, StatusCode};

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use http::{
        HeaderName, HeaderValue, Request, Response, StatusCode as HttpStatusCode,
        header::CONTENT_TYPE,
    };
    use kimojio_stack::Runtime;
    use kimojio_stack_http::{Body, BodyLimits, HttpConfig, StackTransport, h2};
    use prost::Message;
    use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

    use super::{
        Error, ErrorKind, Metadata, Status, StatusCode, UnaryClient, UnaryReply, UnaryResponse,
        UnaryServer, client::ClientConfig, codec, decode_message, encode_message,
        server::ServerConfig,
    };

    #[derive(Clone, PartialEq, Message)]
    struct TestMessage {
        #[prost(string, tag = "1")]
        value: String,
    }

    #[test]
    fn unary_frame_round_trips_prost_message() {
        let message = TestMessage {
            value: "hello".to_string(),
        };

        let frame = encode_message(&message, 1024).unwrap();
        let decoded = decode_message::<TestMessage>(&frame, 1024).unwrap();

        assert_eq!(decoded, message);
    }

    #[test]
    fn status_round_trips_grpc_code_header_value() {
        let status = Status::with_details(
            StatusCode::Unavailable,
            "temporarily unavailable: 50% ☃",
            Bytes::from_static(b"\0binary details"),
        );

        assert_eq!(status.code().as_grpc_code(), 14);
        assert_eq!(
            StatusCode::from_grpc_code(14),
            Some(StatusCode::Unavailable)
        );
        assert_eq!(status.message(), "temporarily unavailable: 50% ☃");

        let trailers = status.to_trailers().unwrap();
        let status = Status::from_trailers(&trailers).unwrap();
        assert_eq!(status.code(), StatusCode::Unavailable);
        assert_eq!(status.message(), "temporarily unavailable: 50% ☃");
        assert_eq!(status.details(), b"\0binary details");
    }

    #[test]
    fn unary_call_succeeds_with_metadata_and_trailers() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut http =
                        h2::ServerConnection::new(server_transport, HttpConfig::default());
                    let mut grpc = UnaryServer::new(ServerConfig::default());
                    grpc.add_unary::<TestMessage, TestMessage, _>(
                        "/test.Echo/Unary",
                        |_cx, metadata, request| {
                            assert_eq!(
                                metadata.get(&HeaderName::from_static("x-request")),
                                Some(&HeaderValue::from_static("seen"))
                            );
                            assert_eq!(
                                metadata
                                    .get_bin(&HeaderName::from_static("trace-bin"))
                                    .unwrap()
                                    .as_deref(),
                                Some(b"abc".as_slice())
                            );
                            let mut reply = UnaryReply::new(TestMessage {
                                value: format!("hello {}", request.value),
                            });
                            reply
                                .metadata
                                .insert(
                                    HeaderName::from_static("x-response"),
                                    HeaderValue::from_static("yes"),
                                )
                                .unwrap();
                            reply
                                .trailers
                                .insert(
                                    HeaderName::from_static("x-trailer"),
                                    HeaderValue::from_static("done"),
                                )
                                .unwrap();
                            Ok(reply)
                        },
                    );
                    grpc.serve_one(cx, &mut http).unwrap();
                    http.close(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let http = h2::ClientConnection::new(client_transport, HttpConfig::default());
                    let mut client = UnaryClient::new(http, ClientConfig::default());
                    let mut metadata = Metadata::new();
                    metadata
                        .insert(
                            HeaderName::from_static("x-request"),
                            HeaderValue::from_static("seen"),
                        )
                        .unwrap();
                    metadata
                        .insert_bin(HeaderName::from_static("trace-bin"), b"abc")
                        .unwrap();
                    let response: UnaryResponse<TestMessage> = client
                        .call(
                            cx,
                            "/test.Echo/Unary",
                            metadata,
                            &TestMessage {
                                value: "world".to_owned(),
                            },
                        )
                        .unwrap();
                    client.close(cx).unwrap();
                    response
                });

                server.join(cx).unwrap();
                let response = client.join(cx).unwrap();
                assert_eq!(response.message.value, "hello world");
                assert_eq!(response.status.code(), StatusCode::Ok);
                assert_eq!(
                    response
                        .metadata
                        .get(&HeaderName::from_static("x-response")),
                    Some(&HeaderValue::from_static("yes"))
                );
                assert_eq!(
                    response.trailers.get(&HeaderName::from_static("x-trailer")),
                    Some(&HeaderValue::from_static("done"))
                );
            });
        });
    }

    #[test]
    fn unary_handler_error_status_reaches_client() {
        let error = unary_call_with_handler(
            |_cx, _metadata, _request: TestMessage| {
                Err(Status::new(StatusCode::Unavailable, "down"))
            },
            TestMessage {
                value: "request".to_owned(),
            },
        )
        .unwrap_err();

        match error {
            Error::Status(status) => {
                assert_eq!(status.code(), StatusCode::Unavailable);
                assert_eq!(status.message(), "down");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn unary_rejects_invalid_content_type() {
        let status = raw_grpc_request_status(
            |body| {
                Request::builder()
                    .method("POST")
                    .uri("/test.Echo/Unary")
                    .header(CONTENT_TYPE, "text/plain")
                    .body(body)
                    .unwrap()
            },
            codec::encode_message(
                &TestMessage {
                    value: "bad".to_owned(),
                },
                1024,
            )
            .unwrap(),
            ServerConfig::default(),
        );

        assert_eq!(status.code(), StatusCode::InvalidArgument);
    }

    #[test]
    fn unary_client_rejects_missing_trailers() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        let error = runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut http =
                        h2::ServerConnection::new(server_transport, HttpConfig::default());
                    let incoming = http.accept(cx).unwrap().unwrap();
                    let response = Response::builder()
                        .status(HttpStatusCode::OK)
                        .header(CONTENT_TYPE, "application/grpc")
                        .body(
                            Body::from_bytes(
                                codec::encode_message(
                                    &TestMessage {
                                        value: "response".to_owned(),
                                    },
                                    1024,
                                )
                                .unwrap(),
                                BodyLimits::new(1024),
                            )
                            .unwrap(),
                        )
                        .unwrap();
                    http.send_response(cx, incoming.stream_id, &response)
                        .unwrap();
                    http.close(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let http = h2::ClientConnection::new(client_transport, HttpConfig::default());
                    let mut client = UnaryClient::new(http, ClientConfig::default());
                    let error = client
                        .call::<_, TestMessage>(
                            cx,
                            "/test.Echo/Unary",
                            Metadata::new(),
                            &TestMessage {
                                value: "request".to_owned(),
                            },
                        )
                        .unwrap_err();
                    client.close(cx).unwrap();
                    error.kind()
                });

                server.join(cx).unwrap();
                client.join(cx).unwrap()
            })
        });

        assert_eq!(error, ErrorKind::Protocol);
    }

    #[test]
    fn unary_rejects_invalid_compressed_flag() {
        let status = raw_grpc_request_status(
            valid_raw_request,
            Bytes::from_static(&[1, 0, 0, 0, 0]),
            ServerConfig::default(),
        );

        assert_eq!(status.code(), StatusCode::InvalidArgument);
    }

    #[test]
    fn unary_rejects_malformed_message_length() {
        let status = raw_grpc_request_status(
            valid_raw_request,
            Bytes::from_static(&[0, 0, 0, 0, 4, 1, 2]),
            ServerConfig::default(),
        );

        assert_eq!(status.code(), StatusCode::InvalidArgument);
    }

    #[test]
    fn unary_enforces_configured_message_size_limits() {
        let client_error = unary_client_size_limit_error();
        assert_eq!(client_error.kind(), ErrorKind::SizeLimit);

        let status = raw_grpc_request_status(
            valid_raw_request,
            codec::encode_bytes(b"abcd", 1024).unwrap(),
            ServerConfig { max_message_len: 1 },
        );
        assert_eq!(status.code(), StatusCode::ResourceExhausted);
    }

    fn unary_call_with_handler<F>(
        handler: F,
        request: TestMessage,
    ) -> Result<UnaryResponse<TestMessage>, Error>
    where
        F: for<'a> Fn(
                &'a kimojio_stack::RuntimeContext<'a>,
                Metadata,
                TestMessage,
            ) -> Result<UnaryReply<TestMessage>, Status>
            + 'static,
    {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut http =
                        h2::ServerConnection::new(server_transport, HttpConfig::default());
                    let mut grpc = UnaryServer::new(ServerConfig::default());
                    grpc.add_unary::<TestMessage, TestMessage, _>("/test.Echo/Unary", handler);
                    grpc.serve_one(cx, &mut http).unwrap();
                    http.close(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let http = h2::ClientConnection::new(client_transport, HttpConfig::default());
                    let mut client = UnaryClient::new(http, ClientConfig::default());
                    let result = client.call(cx, "/test.Echo/Unary", Metadata::new(), &request);
                    client.close(cx).unwrap();
                    result
                });

                server.join(cx).unwrap();
                client.join(cx).unwrap()
            })
        })
    }

    fn raw_grpc_request_status(
        build_request: impl FnOnce(Body) -> Request<Body> + Send + 'static,
        frame: Bytes,
        config: ServerConfig,
    ) -> Status {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut http =
                        h2::ServerConnection::new(server_transport, HttpConfig::default());
                    let mut grpc = UnaryServer::new(config);
                    grpc.add_unary::<TestMessage, TestMessage, _>(
                        "/test.Echo/Unary",
                        |_cx, _metadata, request| {
                            Ok(UnaryReply::new(TestMessage {
                                value: request.value,
                            }))
                        },
                    );
                    grpc.serve_one(cx, &mut http).unwrap();
                    http.close(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let mut http =
                        h2::ClientConnection::new(client_transport, HttpConfig::default());
                    let body = Body::from_bytes(frame, BodyLimits::new(1024)).unwrap();
                    let request = build_request(body);
                    let stream_id = http.send_request(cx, &request).unwrap();
                    let response = http.read_response_with_trailers(cx, stream_id).unwrap();
                    let status = Status::from_trailers(&response.trailers).unwrap();
                    http.close(cx).unwrap();
                    status
                });

                server.join(cx).unwrap();
                client.join(cx).unwrap()
            })
        })
    }

    fn unary_client_size_limit_error() -> Error {
        let (client_transport, _server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let http = h2::ClientConnection::new(client_transport, HttpConfig::default());
            let mut client = UnaryClient::new(http, ClientConfig { max_message_len: 1 });
            client
                .call::<_, TestMessage>(
                    cx,
                    "/test.Echo/Unary",
                    Metadata::new(),
                    &TestMessage {
                        value: "too large".to_owned(),
                    },
                )
                .unwrap_err()
        })
    }

    fn valid_raw_request(body: Body) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/test.Echo/Unary")
            .header(CONTENT_TYPE, "application/grpc")
            .body(body)
            .unwrap()
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
}
