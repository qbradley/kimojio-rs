// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Low-level gRPC foundations for `kimojio-stack`.
//!
//! The crate implements unary, client-streaming, and server-streaming gRPC over
//! the stackful HTTP/2 client/server primitives. It does not generate service
//! code, start a runtime, manage connection pools, or hide background tasks.
//! Callers provide Prost message types, explicit metadata, and stackful HTTP/2
//! connections.
//!
//! # Client sketch
//!
//! ```no_run
//! use kimojio_stack::RuntimeContext;
//! use kimojio_stack_grpc::{Metadata, UnaryClient, UnaryResponse};
//! use kimojio_stack_http::{HttpConfig, StackTransport, h2};
//! use prost::Message;
//!
//! #[derive(Clone, PartialEq, Message)]
//! struct Echo {
//!     #[prost(string, tag = "1")]
//!     value: String,
//! }
//!
//! # fn transport() -> StackTransport { unimplemented!() }
//! # fn example(cx: &RuntimeContext<'_>) -> Result<(), kimojio_stack_grpc::Error> {
//! let http = h2::ClientConnection::new(transport(), HttpConfig::default());
//! let mut client = UnaryClient::new(http, Default::default());
//! let response: UnaryResponse<Echo> = client.call(
//!     cx,
//!     "/example.Echo/Unary",
//!     Metadata::new(),
//!     &Echo { value: "hello".into() },
//! )?;
//! assert_eq!(response.status.code(), kimojio_stack_grpc::StatusCode::Ok);
//! client.close(cx)?;
//! # Ok(())
//! # }
//! ```
//!
//! # Server sketch
//!
//! ```no_run
//! use kimojio_stack_grpc::{Status, StatusCode, UnaryReply, UnaryServer};
//! use prost::Message;
//!
//! #[derive(Clone, PartialEq, Message)]
//! struct Echo {
//!     #[prost(string, tag = "1")]
//!     value: String,
//! }
//!
//! let mut server = UnaryServer::new(Default::default());
//! server.add_unary::<Echo, Echo, _>("/example.Echo/Unary", |_cx, _metadata, request| {
//!     if request.value.is_empty() {
//!         return Err(Status::new(StatusCode::InvalidArgument, "empty value"));
//!     }
//!     Ok(UnaryReply::new(Echo { value: request.value }))
//! });
//! ```
//!
//! # Caveats
//!
//! Compression is intentionally not implemented yet. Message limits are enforced
//! before encode/decode and oversized messages return size-limit errors. A
//! [`client::ServerStreamingResponse`] borrows its [`UnaryClient`] mutably for
//! the life of the stream; use separate HTTP/2 connections when concurrent
//! streaming RPCs are needed.
//!
//! # Runtime migration boundary
//!
//! The runtime boundary for this crate is intentionally the HTTP/2 connection:
//! [`UnaryClient`] owns a `kimojio-stack-http` client connection and
//! [`UnaryServer`] serves a `kimojio-stack-http` server connection supplied by
//! the caller. When HTTP/2 grows generic runtime connection types, gRPC can
//! parameterize over those same types without adding a second runtime abstraction
//! or a hidden dispatch layer.

pub mod client;
pub mod codec;
pub mod error;
pub mod metadata;
pub mod server;
pub mod status;

pub use client::{
    ClientConfig, RuntimeServerStreamingResponse, RuntimeUnaryClient, ServerStreamingResponse,
    UnaryClient, UnaryResponse,
};
pub use codec::{decode_message, encode_message};
pub use error::{Error, ErrorKind};
pub use metadata::Metadata;
pub use server::{
    ClientStreamingRequest, GrpcRuntime, ReceiverStream, RuntimeUnaryServer, ServerConfig,
    ServerStream, ServerStreamingReply, StackGrpcRuntime, UnaryReply, UnaryServer,
};
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
    use std::{hint::black_box, num::NonZeroUsize};

    use bytes::{Bytes, BytesMut};
    use http::{
        HeaderName, HeaderValue, Request, Response, StatusCode as HttpStatusCode,
        header::{CONTENT_TYPE, TE},
    };
    use kimojio_stack::{Runtime, SocketIoRuntime, channel};
    use kimojio_stack_http::{
        Body, BodyLimits, HttpConfig, RuntimeStackTransport, StackTransport, h2,
    };
    use kimojio_stack_steal::{
        Runtime as StealRuntime, RuntimeConfig as StealRuntimeConfig, StealPolicy,
    };
    use prost::Message;
    use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

    use super::{
        Error, ErrorKind, Metadata, RuntimeUnaryClient, Status, StatusCode, UnaryClient,
        UnaryReply, UnaryResponse, UnaryServer, allocation_tracking,
        client::ClientConfig,
        codec, decode_message, encode_message,
        server::{
            GrpcRuntime, ReceiverStream, RuntimeUnaryServer, ServerConfig, ServerStreamingReply,
        },
    };

    #[derive(Clone, PartialEq, Message)]
    struct TestMessage {
        #[prost(string, tag = "1")]
        value: String,
    }

    struct StealGrpcRuntime;

    impl GrpcRuntime for StealGrpcRuntime {
        type Context<'cx> = kimojio_stack_steal::RuntimeContext<'cx>;
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

                server.join(cx);
                let response = client.join(cx);
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
    fn client_streaming_handler_decodes_ordered_messages_metadata_and_trailers() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut http =
                        h2::ServerConnection::new(server_transport, HttpConfig::default());
                    let mut grpc = UnaryServer::new(ServerConfig::default());
                    grpc.add_client_streaming::<TestMessage, TestMessage, _>(
                        "/test.Echo/ClientStream",
                        |cx, metadata, mut request| {
                            assert_eq!(
                                metadata.get(&HeaderName::from_static("x-request")),
                                Some(&HeaderValue::from_static("seen"))
                            );
                            let mut values = Vec::new();
                            while let Some(message) = request.next(cx)? {
                                values.push(message.value);
                            }
                            let mut reply = UnaryReply::new(TestMessage {
                                value: values.join("+"),
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
                    let response: UnaryResponse<TestMessage> = client
                        .call_client_streaming(
                            cx,
                            "/test.Echo/ClientStream",
                            metadata,
                            [
                                TestMessage {
                                    value: "one".to_owned(),
                                },
                                TestMessage {
                                    value: "two".to_owned(),
                                },
                            ],
                        )
                        .unwrap();
                    client.close(cx).unwrap();
                    response
                });

                server.join(cx);
                let response = client.join(cx);
                assert_eq!(response.message.value, "one+two");
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
    fn client_streaming_handler_rejects_malformed_request_frame() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut http =
                        h2::ServerConnection::new(server_transport, HttpConfig::default());
                    let mut grpc = UnaryServer::new(ServerConfig::default());
                    grpc.add_client_streaming::<TestMessage, TestMessage, _>(
                        "/test.Echo/ClientStream",
                        |cx, _metadata, mut request| {
                            let status = request.next(cx).unwrap_err();
                            Err(status)
                        },
                    );
                    grpc.serve_one(cx, &mut http).unwrap();
                    http.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let mut http =
                        h2::ClientConnection::new(client_transport, HttpConfig::default());
                    let request = Request::builder()
                        .method("POST")
                        .uri("/test.Echo/ClientStream")
                        .header(CONTENT_TYPE, "application/grpc")
                        .header(TE, "trailers")
                        .body(())
                        .unwrap();
                    let stream_id = http.send_request_headers(cx, &request).unwrap();
                    http.send_request_data(cx, stream_id, Bytes::from_static(&[1, 0, 0, 0, 0]))
                        .unwrap();
                    http.finish_request_stream(cx, stream_id).unwrap();
                    let response = http.read_response_with_trailers(cx, stream_id).unwrap();
                    http.close(cx).unwrap();
                    Status::from_trailers(&response.trailers).unwrap()
                });

                server.join(cx);
                let status = client.join(cx);
                assert_eq!(status.code(), StatusCode::InvalidArgument);
                assert_eq!(status.message(), "invalid request");
            });
        });
    }

    #[test]
    fn client_streaming_cancellation_allows_follow_up_unary_request() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();
        let (first_message_tx, first_message_rx) = channel::bounded::<()>(1);

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut http =
                        h2::ServerConnection::new(server_transport, HttpConfig::default());
                    let mut grpc = UnaryServer::new(ServerConfig::default());
                    grpc.add_client_streaming::<TestMessage, TestMessage, _>(
                        "/test.Echo/ClientStream",
                        move |cx, _metadata, mut request| {
                            let first = request.next(cx)?.unwrap();
                            assert_eq!(first.value, "partial");
                            first_message_tx.send(cx, ()).unwrap();
                            let status = request.next(cx).unwrap_err();
                            assert_eq!(status.code(), StatusCode::Unavailable);
                            Err(status)
                        },
                    );
                    grpc.add_unary::<TestMessage, TestMessage, _>(
                        "/test.Echo/Unary",
                        |_cx, _metadata, request| {
                            Ok(UnaryReply::new(TestMessage {
                                value: format!("follow {}", request.value),
                            }))
                        },
                    );
                    assert!(grpc.serve_one(cx, &mut http).is_err());
                    assert!(grpc.serve_one(cx, &mut http).unwrap());
                    http.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let mut http =
                        h2::ClientConnection::new(client_transport, HttpConfig::default());
                    let request = Request::builder()
                        .method("POST")
                        .uri("/test.Echo/ClientStream")
                        .header(CONTENT_TYPE, "application/grpc")
                        .header(TE, "trailers")
                        .body(())
                        .unwrap();
                    let stream_id = http.send_request_headers(cx, &request).unwrap();
                    let frame = encode_message(
                        &TestMessage {
                            value: "partial".to_owned(),
                        },
                        1024,
                    )
                    .unwrap();
                    http.send_request_data(cx, stream_id, frame).unwrap();
                    first_message_rx.recv(cx).unwrap();
                    http.cancel_response_stream(cx, stream_id, h2::ERROR_CODE_CANCEL)
                        .unwrap();

                    let mut client = UnaryClient::new(http, ClientConfig::default());
                    let response: UnaryResponse<TestMessage> = client
                        .call(
                            cx,
                            "/test.Echo/Unary",
                            Metadata::new(),
                            &TestMessage {
                                value: "request".to_owned(),
                            },
                        )
                        .unwrap();
                    client.close(cx).unwrap();
                    response
                });

                server.join(cx);
                let response = client.join(cx);
                assert_eq!(response.message.value, "follow request");
                assert_eq!(response.status.code(), StatusCode::Ok);
            });
        });
    }

    #[test]
    fn server_streaming_client_reads_ordered_messages_metadata_and_trailers() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut http =
                        h2::ServerConnection::new(server_transport, HttpConfig::default());
                    let incoming = http.accept(cx).unwrap().unwrap();
                    let response = Response::builder()
                        .status(HttpStatusCode::OK)
                        .header(CONTENT_TYPE, "application/grpc")
                        .header("x-response", "stream")
                        .body(())
                        .unwrap();
                    http.send_response_headers(cx, incoming.stream_id, &response)
                        .unwrap();
                    http.send_response_data(
                        cx,
                        incoming.stream_id,
                        &codec::encode_message(
                            &TestMessage {
                                value: "one".to_owned(),
                            },
                            1024,
                        )
                        .unwrap(),
                    )
                    .unwrap();
                    http.send_response_data(
                        cx,
                        incoming.stream_id,
                        &codec::encode_message(
                            &TestMessage {
                                value: "two".to_owned(),
                            },
                            1024,
                        )
                        .unwrap(),
                    )
                    .unwrap();
                    let mut trailers = Status::ok().to_trailers().unwrap();
                    trailers.insert(
                        HeaderName::from_static("x-trailer"),
                        HeaderValue::from_static("done"),
                    );
                    http.finish_response_stream(cx, incoming.stream_id, Some(&trailers))
                        .unwrap();
                    http.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let http = h2::ClientConnection::new(client_transport, HttpConfig::default());
                    let mut client = UnaryClient::new(http, ClientConfig::default());
                    let mut stream = client
                        .call_server_streaming::<_, TestMessage>(
                            cx,
                            "/test.Echo/Stream",
                            Metadata::new(),
                            &TestMessage {
                                value: "request".to_owned(),
                            },
                        )
                        .unwrap();
                    assert_eq!(
                        stream.metadata.get(&HeaderName::from_static("x-response")),
                        Some(&HeaderValue::from_static("stream"))
                    );
                    assert_eq!(stream.next(cx).unwrap().unwrap().value, "one");
                    assert_eq!(stream.next(cx).unwrap().unwrap().value, "two");
                    assert!(stream.next(cx).unwrap().is_none());
                    assert_eq!(
                        stream.trailers.get(&HeaderName::from_static("x-trailer")),
                        Some(&HeaderValue::from_static("done"))
                    );
                    drop(stream);
                    client.close(cx).unwrap();
                });

                server.join(cx);
                client.join(cx);
            });
        });
    }

    #[test]
    fn server_streaming_client_decodes_split_and_coalesced_grpc_frames() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut http =
                        h2::ServerConnection::new(server_transport, HttpConfig::default());
                    let incoming = http.accept(cx).unwrap().unwrap();
                    let response = Response::builder()
                        .status(HttpStatusCode::OK)
                        .header(CONTENT_TYPE, "application/grpc")
                        .body(())
                        .unwrap();
                    http.send_response_headers(cx, incoming.stream_id, &response)
                        .unwrap();

                    let split = codec::encode_message(
                        &TestMessage {
                            value: "split".to_owned(),
                        },
                        1024,
                    )
                    .unwrap();
                    let midpoint = split.len() / 2;
                    http.send_response_data(cx, incoming.stream_id, &split[..midpoint])
                        .unwrap();
                    http.send_response_data(cx, incoming.stream_id, &split[midpoint..])
                        .unwrap();

                    let mut coalesced = BytesMut::new();
                    coalesced.extend_from_slice(
                        &codec::encode_message(
                            &TestMessage {
                                value: "coalesced-a".to_owned(),
                            },
                            1024,
                        )
                        .unwrap(),
                    );
                    coalesced.extend_from_slice(
                        &codec::encode_message(
                            &TestMessage {
                                value: "coalesced-b".to_owned(),
                            },
                            1024,
                        )
                        .unwrap(),
                    );
                    http.send_response_data(cx, incoming.stream_id, &coalesced)
                        .unwrap();
                    let trailers = Status::ok().to_trailers().unwrap();
                    http.finish_response_stream(cx, incoming.stream_id, Some(&trailers))
                        .unwrap();
                    http.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let http = h2::ClientConnection::new(client_transport, HttpConfig::default());
                    let mut client = UnaryClient::new(http, ClientConfig::default());
                    let mut stream = client
                        .call_server_streaming::<_, TestMessage>(
                            cx,
                            "/test.Echo/Stream",
                            Metadata::new(),
                            &TestMessage {
                                value: "request".to_owned(),
                            },
                        )
                        .unwrap();
                    let values = [
                        stream.next(cx).unwrap().unwrap().value,
                        stream.next(cx).unwrap().unwrap().value,
                        stream.next(cx).unwrap().unwrap().value,
                    ];
                    assert!(stream.next(cx).unwrap().is_none());
                    drop(stream);
                    client.close(cx).unwrap();
                    values
                });

                server.join(cx);
                assert_eq!(client.join(cx), ["split", "coalesced-a", "coalesced-b"]);
            });
        });
    }

    #[test]
    fn server_streaming_client_handles_empty_stream_with_initial_metadata() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut http =
                        h2::ServerConnection::new(server_transport, HttpConfig::default());
                    let incoming = http.accept(cx).unwrap().unwrap();
                    let response = Response::builder()
                        .status(HttpStatusCode::OK)
                        .header(CONTENT_TYPE, "application/grpc")
                        .header("x-empty", "yes")
                        .body(())
                        .unwrap();
                    http.send_response_headers(cx, incoming.stream_id, &response)
                        .unwrap();
                    let trailers = Status::ok().to_trailers().unwrap();
                    http.finish_response_stream(cx, incoming.stream_id, Some(&trailers))
                        .unwrap();
                    http.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let http = h2::ClientConnection::new(client_transport, HttpConfig::default());
                    let mut client = UnaryClient::new(http, ClientConfig::default());
                    let mut stream = client
                        .call_server_streaming::<_, TestMessage>(
                            cx,
                            "/test.Echo/Stream",
                            Metadata::new(),
                            &TestMessage {
                                value: "request".to_owned(),
                            },
                        )
                        .unwrap();
                    assert_eq!(
                        stream.metadata.get(&HeaderName::from_static("x-empty")),
                        Some(&HeaderValue::from_static("yes"))
                    );
                    assert!(stream.next(cx).unwrap().is_none());
                    drop(stream);
                    client.close(cx).unwrap();
                });

                server.join(cx);
                client.join(cx);
            });
        });
    }

    #[test]
    fn server_streaming_client_surfaces_terminal_status_after_messages() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut http =
                        h2::ServerConnection::new(server_transport, HttpConfig::default());
                    let incoming = http.accept(cx).unwrap().unwrap();
                    let response = Response::builder()
                        .status(HttpStatusCode::OK)
                        .header(CONTENT_TYPE, "application/grpc")
                        .body(())
                        .unwrap();
                    http.send_response_headers(cx, incoming.stream_id, &response)
                        .unwrap();
                    http.send_response_data(
                        cx,
                        incoming.stream_id,
                        &codec::encode_message(
                            &TestMessage {
                                value: "before-error".to_owned(),
                            },
                            1024,
                        )
                        .unwrap(),
                    )
                    .unwrap();
                    let trailers = Status::new(StatusCode::Unavailable, "stream down")
                        .to_trailers()
                        .unwrap();
                    http.finish_response_stream(cx, incoming.stream_id, Some(&trailers))
                        .unwrap();
                    http.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let http = h2::ClientConnection::new(client_transport, HttpConfig::default());
                    let mut client = UnaryClient::new(http, ClientConfig::default());
                    let mut stream = client
                        .call_server_streaming::<_, TestMessage>(
                            cx,
                            "/test.Echo/Stream",
                            Metadata::new(),
                            &TestMessage {
                                value: "request".to_owned(),
                            },
                        )
                        .unwrap();
                    assert_eq!(stream.next(cx).unwrap().unwrap().value, "before-error");
                    let error = stream.next(cx).unwrap_err();
                    drop(stream);
                    client.close(cx).unwrap();
                    error
                });

                server.join(cx);
                match client.join(cx) {
                    Error::Status(status) => {
                        assert_eq!(status.code(), StatusCode::Unavailable);
                        assert_eq!(status.message(), "stream down");
                    }
                    other => panic!("unexpected error: {other:?}"),
                }
            });
        });
    }

    #[test]
    fn server_streaming_client_handles_header_carried_terminal_status() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut http =
                        h2::ServerConnection::new(server_transport, HttpConfig::default());
                    let incoming = http.accept(cx).unwrap().unwrap();
                    let response = Response::builder()
                        .status(HttpStatusCode::OK)
                        .header(CONTENT_TYPE, "application/grpc")
                        .header("grpc-status", "14")
                        .header("grpc-message", "header-down")
                        .body(Body::empty())
                        .unwrap();
                    http.send_response(cx, incoming.stream_id, &response)
                        .unwrap();
                    http.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let http = h2::ClientConnection::new(client_transport, HttpConfig::default());
                    let mut client = UnaryClient::new(http, ClientConfig::default());
                    let mut stream = client
                        .call_server_streaming::<_, TestMessage>(
                            cx,
                            "/test.Echo/Stream",
                            Metadata::new(),
                            &TestMessage {
                                value: "request".to_owned(),
                            },
                        )
                        .unwrap();
                    let error = stream.next(cx).unwrap_err();
                    drop(stream);
                    client.close(cx).unwrap();
                    error
                });

                server.join(cx);
                match client.join(cx) {
                    Error::Status(status) => {
                        assert_eq!(status.code(), StatusCode::Unavailable);
                        assert_eq!(status.message(), "header-down");
                    }
                    other => panic!("unexpected error: {other:?}"),
                }
            });
        });
    }

    #[test]
    fn server_streaming_client_cancel_sends_reset() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();
        let client_settings = h2::Settings {
            initial_window_size: 0,
            ..h2::Settings::default()
        };
        let (headers_sent_tx, headers_sent_rx) = channel::bounded::<()>(1);
        let (cancelled_tx, cancelled_rx) = channel::bounded::<()>(1);

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut http =
                        h2::ServerConnection::new(server_transport, HttpConfig::default());
                    let incoming = http.accept(cx).unwrap().unwrap();
                    let response = Response::builder()
                        .status(HttpStatusCode::OK)
                        .header(CONTENT_TYPE, "application/grpc")
                        .body(())
                        .unwrap();
                    http.send_response_headers(cx, incoming.stream_id, &response)
                        .unwrap();
                    headers_sent_tx.send(cx, ()).unwrap();
                    cancelled_rx.recv(cx).unwrap();
                    http.send_response_data(
                        cx,
                        incoming.stream_id,
                        &codec::encode_message(
                            &TestMessage {
                                value: "blocked".to_owned(),
                            },
                            1024,
                        )
                        .unwrap(),
                    )
                    .unwrap_err()
                    .kind()
                });

                let client = scope.spawn(move |cx| {
                    let http = h2::ClientConnection::new_with_settings(
                        client_transport,
                        HttpConfig::default(),
                        client_settings,
                    );
                    let mut client = UnaryClient::new(http, ClientConfig::default());
                    let stream = client
                        .call_server_streaming::<_, TestMessage>(
                            cx,
                            "/test.Echo/Stream",
                            Metadata::new(),
                            &TestMessage {
                                value: "request".to_owned(),
                            },
                        )
                        .unwrap();
                    headers_sent_rx.recv(cx).unwrap();
                    stream.cancel(cx).unwrap();
                    cancelled_tx.send(cx, ()).unwrap();
                    client.close(cx).unwrap();
                });

                client.join(cx);
                assert_eq!(server.join(cx), kimojio_stack_http::ErrorKind::PeerReset);
            });
        });
    }

    #[test]
    fn server_streaming_client_cancel_after_message_sends_reset() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();
        let client_settings = h2::Settings {
            initial_window_size: 16,
            ..h2::Settings::default()
        };
        let (message_sent_tx, message_sent_rx) = channel::bounded::<()>(1);
        let (cancelled_tx, cancelled_rx) = channel::bounded::<()>(1);
        let (observed_tx, observed_rx) = channel::bounded::<()>(1);

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut http =
                        h2::ServerConnection::new(server_transport, HttpConfig::default());
                    let incoming = http.accept(cx).unwrap().unwrap();
                    let response = Response::builder()
                        .status(HttpStatusCode::OK)
                        .header(CONTENT_TYPE, "application/grpc")
                        .body(())
                        .unwrap();
                    http.send_response_headers(cx, incoming.stream_id, &response)
                        .unwrap();
                    http.send_response_data(
                        cx,
                        incoming.stream_id,
                        &codec::encode_message(
                            &TestMessage {
                                value: "first".to_owned(),
                            },
                            1024,
                        )
                        .unwrap(),
                    )
                    .unwrap();
                    message_sent_tx.send(cx, ()).unwrap();
                    cancelled_rx.recv(cx).unwrap();
                    let kind = http
                        .send_response_data(
                            cx,
                            incoming.stream_id,
                            &codec::encode_message(
                                &TestMessage {
                                    value: "second-message-that-exceeds-the-small-window"
                                        .to_owned(),
                                },
                                1024,
                            )
                            .unwrap(),
                        )
                        .unwrap_err()
                        .kind();
                    observed_tx.send(cx, ()).unwrap();
                    kind
                });

                let client = scope.spawn(move |cx| {
                    let http = h2::ClientConnection::new_with_settings(
                        client_transport,
                        HttpConfig::default(),
                        client_settings,
                    );
                    let mut client = UnaryClient::new(http, ClientConfig::default());
                    let mut stream = client
                        .call_server_streaming::<_, TestMessage>(
                            cx,
                            "/test.Echo/Stream",
                            Metadata::new(),
                            &TestMessage {
                                value: "request".to_owned(),
                            },
                        )
                        .unwrap();
                    message_sent_rx.recv(cx).unwrap();
                    assert_eq!(stream.next(cx).unwrap().unwrap().value, "first");
                    stream.cancel(cx).unwrap();
                    cancelled_tx.send(cx, ()).unwrap();
                    observed_rx.recv(cx).unwrap();
                    client.close(cx).unwrap();
                });

                client.join(cx);
                assert_eq!(server.join(cx), kimojio_stack_http::ErrorKind::PeerReset);
            });
        });
    }

    #[test]
    fn server_streaming_invalid_initial_headers_cancel_stream() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();
        let client_settings = h2::Settings {
            initial_window_size: 0,
            ..h2::Settings::default()
        };
        let (headers_sent_tx, headers_sent_rx) = channel::bounded::<()>(1);
        let (client_done_tx, client_done_rx) = channel::bounded::<()>(1);

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut http =
                        h2::ServerConnection::new(server_transport, HttpConfig::default());
                    let incoming = http.accept(cx).unwrap().unwrap();
                    let response = Response::builder()
                        .status(HttpStatusCode::OK)
                        .header(CONTENT_TYPE, "text/plain")
                        .body(())
                        .unwrap();
                    http.send_response_headers(cx, incoming.stream_id, &response)
                        .unwrap();
                    headers_sent_tx.send(cx, ()).unwrap();
                    client_done_rx.recv(cx).unwrap();
                    http.send_response_data(cx, incoming.stream_id, b"after-invalid")
                        .unwrap_err()
                        .kind()
                });

                let client = scope.spawn(move |cx| {
                    let http = h2::ClientConnection::new_with_settings(
                        client_transport,
                        HttpConfig::default(),
                        client_settings,
                    );
                    let mut client = UnaryClient::new(http, ClientConfig::default());
                    let error = match client.call_server_streaming::<_, TestMessage>(
                        cx,
                        "/test.Echo/Stream",
                        Metadata::new(),
                        &TestMessage {
                            value: "request".to_owned(),
                        },
                    ) {
                        Ok(_) => panic!("invalid content-type unexpectedly succeeded"),
                        Err(error) => error,
                    };
                    assert_eq!(error.kind(), ErrorKind::Protocol);
                    headers_sent_rx.recv(cx).unwrap();
                    client_done_tx.send(cx, ()).unwrap();
                    client.close(cx).unwrap();
                });

                client.join(cx);
                assert_eq!(server.join(cx), kimojio_stack_http::ErrorKind::PeerReset);
            });
        });
    }

    #[test]
    fn server_streaming_handler_sends_ordered_messages_metadata_and_trailers() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut http =
                        h2::ServerConnection::new(server_transport, HttpConfig::default());
                    let mut grpc = UnaryServer::new(ServerConfig::default());
                    grpc.add_server_streaming::<TestMessage, TestMessage, _, _>(
                        "/test.Echo/Stream",
                        |_cx, metadata, request| {
                            assert_eq!(
                                metadata.get(&HeaderName::from_static("x-request")),
                                Some(&HeaderValue::from_static("seen"))
                            );
                            let messages = [
                                format!("{}-one", request.value),
                                format!("{}-two", request.value),
                            ];
                            let mut index = 0;
                            let stream = move |_cx: &kimojio_stack::RuntimeContext<'_>| {
                                let Some(value) = messages.get(index).cloned() else {
                                    return Ok(None);
                                };
                                index += 1;
                                Ok(Some(TestMessage { value }))
                            };
                            let mut reply = ServerStreamingReply::new(stream);
                            reply
                                .metadata
                                .insert(
                                    HeaderName::from_static("x-response"),
                                    HeaderValue::from_static("stream"),
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
                    let mut stream = client
                        .call_server_streaming::<_, TestMessage>(
                            cx,
                            "/test.Echo/Stream",
                            metadata,
                            &TestMessage {
                                value: "request".to_owned(),
                            },
                        )
                        .unwrap();
                    assert_eq!(
                        stream.metadata.get(&HeaderName::from_static("x-response")),
                        Some(&HeaderValue::from_static("stream"))
                    );
                    assert_eq!(stream.next(cx).unwrap().unwrap().value, "request-one");
                    assert_eq!(stream.next(cx).unwrap().unwrap().value, "request-two");
                    assert!(stream.next(cx).unwrap().is_none());
                    assert_eq!(
                        stream.trailers.get(&HeaderName::from_static("x-trailer")),
                        Some(&HeaderValue::from_static("done"))
                    );
                    drop(stream);
                    client.close(cx).unwrap();
                });

                server.join(cx);
                client.join(cx);
            });
        });
    }

    #[test]
    fn unary_call_runs_on_stealing_runtime() {
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
                    let mut http =
                        h2::RuntimeServerConnection::new(server_transport, HttpConfig::default());
                    let mut grpc =
                        RuntimeUnaryServer::<StealGrpcRuntime>::new(ServerConfig::default());
                    grpc.add_unary::<TestMessage, TestMessage, _>(
                        "/test.Echo/Unary",
                        |_cx, metadata, request| {
                            assert_eq!(
                                metadata.get(&HeaderName::from_static("x-request")),
                                Some(&HeaderValue::from_static("seen"))
                            );
                            Ok(UnaryReply::new(TestMessage {
                                value: format!("steal {}", request.value),
                            }))
                        },
                    );
                    grpc.serve_one(cx, &mut http).unwrap();
                    http.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                let client = scope.spawn_stealable(move |cx| {
                    let client_fd = cx.socket_from_owned_fd(client_fd).unwrap();
                    let client_transport = RuntimeStackTransport::plaintext_socket(client_fd);
                    let http =
                        h2::RuntimeClientConnection::new(client_transport, HttpConfig::default());
                    let mut client = RuntimeUnaryClient::new(http, ClientConfig::default());
                    let mut metadata = Metadata::new();
                    metadata
                        .insert(
                            HeaderName::from_static("x-request"),
                            HeaderValue::from_static("seen"),
                        )
                        .unwrap();
                    let response: UnaryResponse<TestMessage> = client
                        .call(
                            cx,
                            "/test.Echo/Unary",
                            metadata,
                            &TestMessage {
                                value: "runtime".to_owned(),
                            },
                        )
                        .unwrap();
                    client.close(cx).unwrap();
                    response.message.value
                });

                server.join(cx);
                assert_eq!(client.join(cx), "steal runtime");
            });
        });
    }

    #[test]
    fn server_streaming_runs_on_stealing_runtime() {
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
                    let mut http =
                        h2::RuntimeServerConnection::new(server_transport, HttpConfig::default());
                    let mut grpc =
                        RuntimeUnaryServer::<StealGrpcRuntime>::new(ServerConfig::default());
                    grpc.add_server_streaming::<TestMessage, TestMessage, _, _>(
                        "/test.Echo/Stream",
                        |_cx, _metadata, request| {
                            let values = [
                                format!("{}-one", request.value),
                                format!("{}-two", request.value),
                            ];
                            let mut index = 0;
                            let stream = move |_cx: &kimojio_stack_steal::RuntimeContext<'_>| {
                                let Some(value) = values.get(index).cloned() else {
                                    return Ok(None);
                                };
                                index += 1;
                                Ok(Some(TestMessage { value }))
                            };
                            Ok(ServerStreamingReply::new(stream))
                        },
                    );
                    grpc.serve_one(cx, &mut http).unwrap();
                    http.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                let client = scope.spawn_stealable(move |cx| {
                    let client_fd = cx.socket_from_owned_fd(client_fd).unwrap();
                    let client_transport = RuntimeStackTransport::plaintext_socket(client_fd);
                    let http =
                        h2::RuntimeClientConnection::new(client_transport, HttpConfig::default());
                    let mut client = RuntimeUnaryClient::new(http, ClientConfig::default());
                    let mut stream = client
                        .call_server_streaming::<_, TestMessage>(
                            cx,
                            "/test.Echo/Stream",
                            Metadata::new(),
                            &TestMessage {
                                value: "steal".to_owned(),
                            },
                        )
                        .unwrap();
                    let first = stream.next(cx).unwrap().unwrap().value;
                    let second = stream.next(cx).unwrap().unwrap().value;
                    assert!(stream.next(cx).unwrap().is_none());
                    drop(stream);
                    client.close(cx).unwrap();
                    (first, second)
                });

                server.join(cx);
                assert_eq!(
                    client.join(cx),
                    ("steal-one".to_owned(), "steal-two".to_owned())
                );
            });
        });
    }

    #[test]
    fn server_streaming_handler_yielded_status_reaches_client() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut http =
                        h2::ServerConnection::new(server_transport, HttpConfig::default());
                    let mut grpc = UnaryServer::new(ServerConfig::default());
                    grpc.add_server_streaming::<TestMessage, TestMessage, _, _>(
                        "/test.Echo/Stream",
                        |_cx, _metadata, _request| {
                            let mut sent = false;
                            let stream = move |_cx: &kimojio_stack::RuntimeContext<'_>| {
                                if !sent {
                                    sent = true;
                                    return Ok(Some(TestMessage {
                                        value: "before-error".to_owned(),
                                    }));
                                }
                                Err(Status::new(StatusCode::Unavailable, "server stream down"))
                            };
                            Ok(ServerStreamingReply::new(stream))
                        },
                    );
                    grpc.serve_one(cx, &mut http).unwrap();
                    http.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let http = h2::ClientConnection::new(client_transport, HttpConfig::default());
                    let mut client = UnaryClient::new(http, ClientConfig::default());
                    let mut stream = client
                        .call_server_streaming::<_, TestMessage>(
                            cx,
                            "/test.Echo/Stream",
                            Metadata::new(),
                            &TestMessage {
                                value: "request".to_owned(),
                            },
                        )
                        .unwrap();
                    assert_eq!(stream.next(cx).unwrap().unwrap().value, "before-error");
                    let error = stream.next(cx).unwrap_err();
                    assert!(stream.next(cx).unwrap().is_none());
                    drop(stream);
                    client.close(cx).unwrap();
                    error
                });

                server.join(cx);
                match client.join(cx) {
                    Error::Status(status) => {
                        assert_eq!(status.code(), StatusCode::Unavailable);
                        assert_eq!(status.message(), "server stream down");
                    }
                    other => panic!("unexpected error: {other:?}"),
                }
            });
        });
    }

    #[test]
    fn server_streaming_receiver_stream_parks_until_message_and_completes() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();
        let (tx, rx) = channel::bounded::<Result<TestMessage, Status>>(1);

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut http =
                        h2::ServerConnection::new(server_transport, HttpConfig::default());
                    let mut grpc = UnaryServer::new(ServerConfig::default());
                    grpc.add_server_streaming::<TestMessage, TestMessage, _, _>(
                        "/test.Echo/Stream",
                        move |_cx, _metadata, _request| {
                            Ok(ServerStreamingReply::new(ReceiverStream::new(rx.clone())))
                        },
                    );
                    grpc.serve_one(cx, &mut http).unwrap();
                    http.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                let producer = scope.spawn(move |cx| {
                    cx.yield_now();
                    tx.send(
                        cx,
                        Ok(TestMessage {
                            value: "delayed".to_owned(),
                        }),
                    )
                    .unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let http = h2::ClientConnection::new(client_transport, HttpConfig::default());
                    let mut client = UnaryClient::new(http, ClientConfig::default());
                    let mut stream = client
                        .call_server_streaming::<_, TestMessage>(
                            cx,
                            "/test.Echo/Stream",
                            Metadata::new(),
                            &TestMessage {
                                value: "request".to_owned(),
                            },
                        )
                        .unwrap();
                    assert_eq!(stream.next(cx).unwrap().unwrap().value, "delayed");
                    assert!(stream.next(cx).unwrap().is_none());
                    drop(stream);
                    client.close(cx).unwrap();
                });

                producer.join(cx);
                server.join(cx);
                client.join(cx);
            });
        });
    }

    #[test]
    fn server_streaming_client_cancel_surfaces_to_server_serve_one() {
        let (client_transport, server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();
        let client_settings = h2::Settings {
            initial_window_size: 0,
            ..h2::Settings::default()
        };

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut http =
                        h2::ServerConnection::new(server_transport, HttpConfig::default());
                    let mut grpc = UnaryServer::new(ServerConfig::default());
                    grpc.add_server_streaming::<TestMessage, TestMessage, _, _>(
                        "/test.Echo/Stream",
                        |_cx, _metadata, _request| {
                            let mut sent = false;
                            let stream = move |_cx: &kimojio_stack::RuntimeContext<'_>| {
                                if sent {
                                    Ok(None)
                                } else {
                                    sent = true;
                                    Ok(Some(TestMessage {
                                        value: "blocked".to_owned(),
                                    }))
                                }
                            };
                            Ok(ServerStreamingReply::new(stream))
                        },
                    );
                    match grpc.serve_one(cx, &mut http).unwrap_err() {
                        Error::Transport(error) => error.kind(),
                        other => panic!("unexpected error: {other:?}"),
                    }
                });

                let client = scope.spawn(move |cx| {
                    let http = h2::ClientConnection::new_with_settings(
                        client_transport,
                        HttpConfig::default(),
                        client_settings,
                    );
                    let mut client = UnaryClient::new(http, ClientConfig::default());
                    let stream = client
                        .call_server_streaming::<_, TestMessage>(
                            cx,
                            "/test.Echo/Stream",
                            Metadata::new(),
                            &TestMessage {
                                value: "request".to_owned(),
                            },
                        )
                        .unwrap();
                    stream.cancel(cx).unwrap();
                    client.close(cx).unwrap();
                });

                client.join(cx);
                assert_eq!(server.join(cx), kimojio_stack_http::ErrorKind::PeerReset);
            });
        });
    }

    #[test]
    fn server_streaming_independent_client_connections_run_concurrently() {
        let (first_client_transport, first_server_transport) = socket_transport_pair();
        let (second_client_transport, second_server_transport) = socket_transport_pair();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let first_server = scope.spawn(move |cx| {
                    serve_single_value_stream(cx, first_server_transport, "first");
                });
                let second_server = scope.spawn(move |cx| {
                    serve_single_value_stream(cx, second_server_transport, "second");
                });
                let first_client = scope
                    .spawn(move |cx| read_single_value_stream(cx, first_client_transport, "first"));
                let second_client = scope.spawn(move |cx| {
                    read_single_value_stream(cx, second_client_transport, "second")
                });

                assert_eq!(second_client.join(cx), "second");
                assert_eq!(first_client.join(cx), "first");
                first_server.join(cx);
                second_server.join(cx);
            });
        });
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

                server.join(cx);
                client.join(cx)
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
                server.join(cx);
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

                server.join(cx);
                client.join(cx)
            })
        })
    }

    fn serve_single_value_stream(
        cx: &kimojio_stack::RuntimeContext<'_>,
        transport: StackTransport,
        value: &'static str,
    ) {
        let mut http = h2::ServerConnection::new(transport, HttpConfig::default());
        let mut grpc = UnaryServer::new(ServerConfig::default());
        grpc.add_server_streaming::<TestMessage, TestMessage, _, _>(
            "/test.Echo/Stream",
            move |_cx, _metadata, _request| {
                let mut sent = false;
                let stream = move |_cx: &kimojio_stack::RuntimeContext<'_>| {
                    if sent {
                        Ok(None)
                    } else {
                        sent = true;
                        Ok(Some(TestMessage {
                            value: value.to_owned(),
                        }))
                    }
                };
                Ok(ServerStreamingReply::new(stream))
            },
        );
        grpc.serve_one(cx, &mut http).unwrap();
        http.shutdown_write_and_close_after_peer(cx).unwrap();
    }

    fn read_single_value_stream(
        cx: &kimojio_stack::RuntimeContext<'_>,
        transport: StackTransport,
        request_value: &'static str,
    ) -> String {
        let http = h2::ClientConnection::new(transport, HttpConfig::default());
        let mut client = UnaryClient::new(http, ClientConfig::default());
        let mut stream = client
            .call_server_streaming::<_, TestMessage>(
                cx,
                "/test.Echo/Stream",
                Metadata::new(),
                &TestMessage {
                    value: request_value.to_owned(),
                },
            )
            .unwrap();
        let value = stream.next(cx).unwrap().unwrap().value;
        assert!(stream.next(cx).unwrap().is_none());
        drop(stream);
        client.close(cx).unwrap();
        value
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

                server.join(cx);
                client.join(cx)
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

    #[test]
    fn runtime_migration_boundary_is_http2_connection() {
        let (client_transport, server_transport) = socket_transport_pair();
        let http_client = h2::ClientConnection::new(client_transport, HttpConfig::default());
        let _grpc_client = UnaryClient::new(http_client, ClientConfig::default());
        let _http_server = h2::ServerConnection::new(server_transport, HttpConfig::default());
        let _grpc_server = UnaryServer::new(ServerConfig::default());
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
