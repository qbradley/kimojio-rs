// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Stackful HTTP foundations for `kimojio-stack`.
//!
//! This crate provides low-level transport, buffering, error, and protocol
//! boundary types used by the HTTP/1.1 and HTTP/2 implementations.
//!
//! It intentionally exposes connection objects instead of a global client,
//! background connection pool, or async runtime integration. Callers decide how
//! sockets are opened, whether TLS is used, how connections are shared, and where
//! response bodies are buffered or streamed.
//!
//! # Choosing a layer
//!
//! - [`client::ClientConnection`] and [`server::ServerConnection`] are
//!   protocol-neutral enums for code that can select HTTP/1.1 or HTTP/2 at
//!   construction time.
//! - [`http1`] exposes low-level HTTP/1.1 request/response handling over one
//!   stack transport.
//! - [`h2`] exposes HTTP/2 stream IDs, trailers, flow-control aware response
//!   streaming, and request multiplexing primitives.
//! - [`transport`] adapts plaintext sockets and, with the `tls` feature, stack
//!   TLS streams to a common read/write surface.
//!
//! # Example
//!
//! ```no_run
//! use http::Request;
//! use kimojio_stack::RuntimeContext;
//! use kimojio_stack_http::{Body, BodyLimits, HttpConfig, StackTransport, client::ClientConnection};
//!
//! # fn connected_transport() -> StackTransport { unimplemented!() }
//! # fn example(cx: &RuntimeContext<'_>) -> Result<(), kimojio_stack_http::Error> {
//! let mut client = ClientConnection::http1(connected_transport(), HttpConfig::default());
//! let request = Request::builder()
//!     .method("GET")
//!     .uri("/")
//!     .header("host", "example.com")
//!     .body(Body::empty())
//!     .expect("valid request");
//!
//! let response = client.send(cx, &request)?;
//! assert!(response.body().len() <= BodyLimits::default().max_len());
//! client.close(cx)?;
//! # Ok(())
//! # }
//! ```
//!
//! # Body and streaming behavior
//!
//! [`Body`] is a bounded in-memory body. Use
//! [`client::ClientConnection::send_with_body_chunks`] or the HTTP/2 streaming
//! APIs when response bodies should be delivered incrementally instead of fully
//! buffered. Body limits are still enforced on buffered responses and request
//! construction.

pub mod body;
pub mod client;
pub mod config;
pub mod error;
pub mod h2;
pub mod headers;
pub mod http1;
pub mod server;
#[cfg(feature = "tls")]
pub mod tls;
pub mod transport;

pub use body::{Body, BodyBuilder, BodyLimits};
pub use config::HttpConfig;
pub use error::{Error, ErrorKind, LimitKind};
pub use headers::{Headers, Trailers};
pub use transport::StackTransport;

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
    use std::{hint::black_box, time::Duration};

    use http::{HeaderName, HeaderValue, header::CONTENT_TYPE};
    use kimojio_stack::{Errno, Runtime};
    use kimojio_stack_tls::TlsContext;
    use openssl::{
        asn1::Asn1Time,
        hash::MessageDigest,
        nid::Nid,
        pkey::PKey,
        rsa::Rsa,
        ssl::{SslAcceptor, SslConnector, SslMethod, SslVerifyMode},
        x509::{X509, X509NameBuilder},
    };
    use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

    use super::{
        Body, BodyLimits, Headers, HttpConfig, StackTransport, Trailers, allocation_tracking, h2,
        http1,
    };

    const TLS_BUFFER_SIZE: usize = 16 * 1024;

    fn make_contexts() -> (TlsContext, TlsContext) {
        let rsa = Rsa::generate(2048).unwrap();
        let key = PKey::from_rsa(rsa).unwrap();

        let mut name = X509NameBuilder::new().unwrap();
        name.append_entry_by_nid(Nid::COMMONNAME, "localhost")
            .unwrap();
        let name = name.build();

        let mut cert = X509::builder().unwrap();
        cert.set_version(2).unwrap();
        cert.set_subject_name(&name).unwrap();
        cert.set_issuer_name(&name).unwrap();
        cert.set_pubkey(&key).unwrap();
        cert.set_not_before(Asn1Time::days_from_now(0).unwrap().as_ref())
            .unwrap();
        cert.set_not_after(Asn1Time::days_from_now(1).unwrap().as_ref())
            .unwrap();
        cert.sign(&key, MessageDigest::sha256()).unwrap();
        let cert = cert.build();

        let mut acceptor = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
        acceptor.set_private_key(&key).unwrap();
        acceptor.set_certificate(&cert).unwrap();
        acceptor.check_private_key().unwrap();

        let mut connector = SslConnector::builder(SslMethod::tls()).unwrap();
        connector.set_verify(SslVerifyMode::NONE);

        (
            TlsContext::from_openssl(acceptor.build().into_context()),
            TlsContext::from_openssl(connector.build().into_context()),
        )
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
    fn body_limits_reject_oversized_payloads() {
        let limits = BodyLimits::new(4);

        let error = Body::copy_from_slice(b"hello", limits).unwrap_err();

        assert_eq!(error.kind(), super::ErrorKind::SizeLimit);
    }

    #[test]
    fn headers_and_trailers_wrap_header_maps() {
        let mut headers = Headers::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/grpc"));
        assert_eq!(
            headers.get(&CONTENT_TYPE),
            Some(&HeaderValue::from_static("application/grpc"))
        );

        let grpc_status = HeaderName::from_static("grpc-status");
        let mut trailers = Trailers::new();
        trailers.insert(grpc_status.clone(), HeaderValue::from_static("0"));
        assert_eq!(
            trailers.get(&grpc_status),
            Some(&HeaderValue::from_static("0"))
        );
    }

    #[test]
    fn plaintext_transport_moves_bytes_over_socketpair() {
        let (client_fd, server_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .unwrap();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut transport = StackTransport::plaintext(server_fd);
                    let mut received = [0_u8; 5];
                    let amount = transport.read_exact_or_eof(cx, &mut received).unwrap();
                    assert_eq!(amount, received.len());
                    assert_eq!(&received, b"hello");
                    transport.write_all(cx, b"world").unwrap();
                    transport.shutdown(cx).unwrap();
                    transport.close(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let mut transport = StackTransport::plaintext(client_fd);
                    transport.write_all(cx, b"hello").unwrap();
                    let mut received = [0_u8; 5];
                    let amount = transport.read_exact_or_eof(cx, &mut received).unwrap();
                    assert_eq!(amount, received.len());
                    transport.shutdown(cx).unwrap();
                    transport.close(cx).unwrap();
                    received
                });

                server.join(cx).unwrap();
                let received = client.join(cx).unwrap();
                assert_eq!(&received, b"world");
            });
        });
    }

    #[test]
    fn plaintext_transport_timeout_cancels_pending_read() {
        let (client_fd, server_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .unwrap();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let mut transport = StackTransport::plaintext(client_fd);
            transport.set_io_timeout(Some(Duration::from_millis(1)));
            let mut received = [0_u8; 1];
            let error = transport.read(cx, &mut received).unwrap_err();

            assert_eq!(error, super::Error::Io(Errno::TIME));
            transport.close(cx).unwrap();
            cx.close(server_fd).unwrap();
        });
    }

    #[test]
    fn plaintext_transport_deadline_is_transport_local_across_tasks() {
        let (timed_fd, timed_peer_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .unwrap();
        let (normal_fd, normal_peer_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .unwrap();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let timed = scope.spawn(move |cx| {
                    let mut transport = StackTransport::plaintext(timed_fd);
                    transport.set_io_timeout(Some(Duration::from_millis(1)));
                    let mut received = [0_u8; 1];

                    assert_eq!(
                        transport.read(cx, &mut received).unwrap_err(),
                        super::Error::Io(Errno::TIME)
                    );
                    transport.close(cx).unwrap();
                    cx.close(timed_peer_fd).unwrap();
                });

                let normal_server = scope.spawn(move |cx| {
                    cx.sleep(Duration::from_millis(5)).unwrap();
                    cx.write(&normal_peer_fd, b"x").unwrap();
                    cx.close(normal_peer_fd).unwrap();
                });

                let normal_client = scope.spawn(move |cx| {
                    let mut transport = StackTransport::plaintext(normal_fd);
                    let mut received = [0_u8; 1];

                    assert_eq!(transport.read(cx, &mut received).unwrap(), 1);
                    transport.close(cx).unwrap();
                    received[0]
                });

                timed.join(cx).unwrap();
                normal_server.join(cx).unwrap();
                assert_eq!(normal_client.join(cx).unwrap(), b'x');
            });
        });
    }

    #[test]
    fn tls_transport_moves_bytes_over_socketpair() {
        let (server_ctx, client_ctx) = make_contexts();
        let (client_fd, server_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .unwrap();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let tls = server_ctx
                        .server(cx, TLS_BUFFER_SIZE, server_fd)
                        .expect("server TLS handshake failed");
                    let mut transport = StackTransport::tls(tls);
                    let mut received = [0_u8; 5];
                    let amount = transport.read_exact_or_eof(cx, &mut received).unwrap();
                    assert_eq!(amount, received.len());
                    assert_eq!(&received, b"hello");
                    transport.write_all(cx, b"world").unwrap();
                    transport.shutdown(cx).unwrap();
                    transport.close(cx).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let tls = client_ctx
                        .client(cx, TLS_BUFFER_SIZE, client_fd, "localhost")
                        .expect("client TLS handshake failed");
                    let mut transport = StackTransport::tls(tls);
                    transport.write_all(cx, b"hello").unwrap();
                    let mut received = [0_u8; 5];
                    let amount = transport.read_exact_or_eof(cx, &mut received).unwrap();
                    assert_eq!(amount, received.len());
                    transport.shutdown(cx).unwrap();
                    transport.close(cx).unwrap();
                    received
                });

                server.join(cx).unwrap();
                let received = client.join(cx).unwrap();
                assert_eq!(&received, b"world");
            });
        });
    }

    #[test]
    fn allocation_http1_warmed_local_loop_records_current_hot_path_allocations() {
        const WARMUP_ROUNDS: u64 = 2;
        const MEASURED_ROUNDS: u64 = 4;

        let counts = Runtime::new().block_on(|cx| {
            let (client_transport, server_transport) = socket_transport_pair();
            let request = request("/allocation-http1", b"ping");
            let response = response(b"pong");

            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut server =
                        http1::ServerConnection::new(server_transport, HttpConfig::default());
                    for _ in 0..WARMUP_ROUNDS + MEASURED_ROUNDS {
                        let request = server.read_request(cx).unwrap().unwrap();
                        black_box(request.body().len());
                        server.write_response(cx, &response).unwrap();
                    }
                    server.close(cx).unwrap();
                });

                let mut client =
                    http1::ClientConnection::new(client_transport, HttpConfig::default());
                for _ in 0..WARMUP_ROUNDS {
                    black_box(client.send(cx, &request).unwrap());
                }

                let (_, counts) = allocation_tracking::measure(|| {
                    for _ in 0..MEASURED_ROUNDS {
                        black_box(client.send(cx, &request).unwrap());
                    }
                });
                client.close(cx).unwrap();
                server.join(cx).unwrap();
                counts
            })
        });

        assert!(counts.allocating_operations() <= 256, "{counts:?}");
        assert!(
            counts.allocated_or_reallocated_bytes() <= 256 * 1024,
            "{counts:?}"
        );
    }

    #[test]
    fn allocation_h2_warmed_local_loop_records_current_hot_path_allocations() {
        const WARMUP_ROUNDS: u64 = 2;
        const MEASURED_ROUNDS: u64 = 4;

        let counts = Runtime::new().block_on(|cx| {
            let (client_transport, server_transport) = socket_transport_pair();
            let request = request("/allocation-h2", b"ping");
            let response = response(b"pong");

            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut server =
                        h2::ServerConnection::new(server_transport, HttpConfig::default());
                    for _ in 0..WARMUP_ROUNDS + MEASURED_ROUNDS {
                        let incoming = server.accept(cx).unwrap().unwrap();
                        black_box(incoming.request.body().len());
                        server
                            .send_response(cx, incoming.stream_id, &response)
                            .unwrap();
                    }
                    server.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                let mut client = h2::ClientConnection::new(client_transport, HttpConfig::default());
                for _ in 0..WARMUP_ROUNDS {
                    black_box(client.send(cx, &request).unwrap());
                }

                let (_, counts) = allocation_tracking::measure(|| {
                    for _ in 0..MEASURED_ROUNDS {
                        black_box(client.send(cx, &request).unwrap());
                    }
                });
                client.close(cx).unwrap();
                server.join(cx).unwrap();
                counts
            })
        });

        assert!(counts.allocating_operations() <= 512, "{counts:?}");
        assert!(
            counts.allocated_or_reallocated_bytes() <= 512 * 1024,
            "{counts:?}"
        );
    }

    fn request(uri: &str, bytes: &[u8]) -> http::Request<Body> {
        http::Request::builder()
            .method("POST")
            .uri(uri)
            .header("x-test", "allocation")
            .body(Body::copy_from_slice(bytes, BodyLimits::new(64 * 1024)).unwrap())
            .unwrap()
    }

    fn response(bytes: &[u8]) -> http::Response<Body> {
        http::Response::builder()
            .status(200)
            .header("x-test", "allocation")
            .body(Body::copy_from_slice(bytes, BodyLimits::new(64 * 1024)).unwrap())
            .unwrap()
    }
}
