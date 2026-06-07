// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Unary-first gRPC foundations for `kimojio-stack`.

pub mod client;
pub mod codec;
pub mod error;
pub mod metadata;
pub mod server;
pub mod status;

pub use client::{ClientConfig, UnaryClient, UnaryResponse};
pub use codec::{decode_message, encode_message};
pub use error::{Error, ErrorKind};
pub use metadata::Metadata;
pub use server::{ServerConfig, UnaryReply, UnaryServer};
pub use status::{Status, StatusCode};

#[cfg(test)]
#[global_allocator]
static TEST_ALLOCATOR: allocation_tracking::CountingAllocator =
    allocation_tracking::CountingAllocator;

#[cfg(test)]
mod allocation_tracking {
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        cell::Cell,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    thread_local! {
        static ACTIVE: Cell<bool> = const { Cell::new(false) };
    }

    static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
    static ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);
    static REALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
    static REALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);
    static MEASUREMENT_LOCK: Mutex<()> = Mutex::new(());

    pub struct CountingAllocator;

    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct AllocationCounts {
        pub allocations: usize,
        pub allocated_bytes: usize,
        pub reallocations: usize,
        pub reallocated_bytes: usize,
    }

    impl AllocationCounts {
        pub fn allocating_operations(self) -> usize {
            self.allocations + self.reallocations
        }

        pub fn allocated_or_reallocated_bytes(self) -> usize {
            self.allocated_bytes + self.reallocated_bytes
        }
    }

    pub fn measure<T>(f: impl FnOnce() -> T) -> (T, AllocationCounts) {
        let _measurement = MEASUREMENT_LOCK.lock().unwrap();
        ACTIVE.with(|active| assert!(!active.get(), "nested allocation measurement"));
        ALLOCATIONS.store(0, Ordering::Relaxed);
        ALLOCATED_BYTES.store(0, Ordering::Relaxed);
        REALLOCATIONS.store(0, Ordering::Relaxed);
        REALLOCATED_BYTES.store(0, Ordering::Relaxed);

        ACTIVE.with(|active| active.set(true));
        let output = f();
        ACTIVE.with(|active| active.set(false));

        (
            output,
            AllocationCounts {
                allocations: ALLOCATIONS.load(Ordering::Relaxed),
                allocated_bytes: ALLOCATED_BYTES.load(Ordering::Relaxed),
                reallocations: REALLOCATIONS.load(Ordering::Relaxed),
                reallocated_bytes: REALLOCATED_BYTES.load(Ordering::Relaxed),
            },
        )
    }

    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            ACTIVE.with(|active| {
                if active.get() {
                    ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
                    ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
                }
            });
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            ACTIVE.with(|active| {
                if active.get() {
                    ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
                    ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
                }
            });
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            ACTIVE.with(|active| {
                if active.get() {
                    REALLOCATIONS.fetch_add(1, Ordering::Relaxed);
                    REALLOCATED_BYTES.fetch_add(new_size, Ordering::Relaxed);
                }
            });
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::hint::black_box;

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
        UnaryServer, allocation_tracking, client::ClientConfig, codec, decode_message,
        encode_message, server::ServerConfig,
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
                    http.shutdown_write_and_close_after_peer(cx).unwrap();
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
                    http.shutdown_write_and_close_after_peer(cx).unwrap();
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

    #[test]
    fn allocation_unary_grpc_warmed_local_loop_records_current_hot_path_allocations() {
        const WARMUP_ROUNDS: u64 = 2;
        const MEASURED_ROUNDS: u64 = 4;

        let counts = Runtime::new().block_on(|cx| {
            let (client_transport, server_transport) = socket_transport_pair();
            let request = TestMessage {
                value: "allocation".to_owned(),
            };

            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut http =
                        h2::ServerConnection::new(server_transport, HttpConfig::default());
                    let mut grpc = UnaryServer::new(ServerConfig::default());
                    grpc.add_unary::<TestMessage, TestMessage, _>(
                        "/test.Echo/Allocation",
                        |_cx, _metadata, request| Ok(UnaryReply::new(request)),
                    );
                    for _ in 0..WARMUP_ROUNDS + MEASURED_ROUNDS {
                        grpc.serve_one(cx, &mut http).unwrap();
                    }
                    http.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                let http = h2::ClientConnection::new(client_transport, HttpConfig::default());
                let mut client = UnaryClient::new(http, ClientConfig::default());
                for _ in 0..WARMUP_ROUNDS {
                    black_box(
                        client
                            .call::<_, TestMessage>(
                                cx,
                                "/test.Echo/Allocation",
                                Metadata::new(),
                                &request,
                            )
                            .unwrap(),
                    );
                }

                let (_, counts) = allocation_tracking::measure(|| {
                    for _ in 0..MEASURED_ROUNDS {
                        black_box(
                            client
                                .call::<_, TestMessage>(
                                    cx,
                                    "/test.Echo/Allocation",
                                    Metadata::new(),
                                    &request,
                                )
                                .unwrap(),
                        );
                    }
                });

                client.close(cx).unwrap();
                server.join(cx).unwrap();
                counts
            })
        });

        assert!(counts.allocating_operations() <= 768, "{counts:?}");
        assert!(
            counts.allocated_or_reallocated_bytes() <= 768 * 1024,
            "{counts:?}"
        );
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
