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
//!   TLS streams to a common read/write surface. [`RuntimeStackTransport`] is
//!   generic over the runtime socket handle; [`StackTransport`] preserves the
//!   existing stack-core `IoFd` API.
//!   Transport deadlines use runtime async socket handles for plaintext and
//!   generic async TLS handles for TLS. They wait on the pending operation and a
//!   runtime-neutral timer waitable, so active read/write timeouts can cancel and
//!   drain before close without zero-duration polling loops.
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
pub use transport::{HttpRuntime, RuntimeStackTransport, StackTransport};

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
    use std::{
        hint::black_box,
        marker::PhantomData,
        num::NonZeroUsize,
        os::fd::{AsFd, BorrowedFd, OwnedFd},
        time::Duration,
    };

    use http::{HeaderName, HeaderValue, header::CONTENT_TYPE};
    use kimojio_stack::{
        Errno, IoReadBuffer, IoRuntime, ReadOutput, Runtime, RuntimeCapabilities,
        RuntimeCapability, RuntimeContext, RuntimeFamily, RuntimeIoError, RuntimeReadResult,
        RuntimeSocket, RuntimeWaitable, RuntimeWriteResult, SocketIoRuntime, StackfulWaitContext,
        StackfulWaitRegistration, StackfulWaiterHandle, UnsupportedCapability, WaitRegistration,
        Waitable, WriteOutput,
    };
    use kimojio_stack_steal::{Runtime as StealRuntime, RuntimeConfig as StealRuntimeConfig};
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
        Body, BodyLimits, Headers, HttpConfig, RuntimeStackTransport, StackTransport, Trailers,
        allocation_tracking, h2, http1,
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

    #[derive(Clone, Copy)]
    enum TlsTransportPath {
        StackCoreWrappers,
        RuntimeAgnostic,
    }

    #[derive(Clone, Copy)]
    struct AllocationBudget {
        max_operations: usize,
        max_bytes: usize,
        rationale: &'static str,
    }

    #[derive(Clone, Copy)]
    struct AllocationDeltaBudget {
        max_extra_operations: usize,
        max_extra_bytes: usize,
        rationale: &'static str,
    }

    #[derive(Clone, Copy)]
    struct SourceSurface {
        name: &'static str,
        source: &'static str,
    }

    #[derive(Clone, Copy)]
    struct ForbiddenSourcePattern {
        pattern: &'static str,
        reason: &'static str,
    }

    const LOCAL_LOOP_WARMUP_ROUNDS: u64 = 2;
    const LOCAL_LOOP_MEASURED_ROUNDS: u64 = 4;
    const TLS_PORTABILITY_WARMUP_ROUNDS: u64 = 1;
    const TLS_PORTABILITY_MEASURED_ROUNDS: u64 = 2;
    const HTTP1_WARMED_LOCAL_LOOP_BUDGET: AllocationBudget = AllocationBudget {
        max_operations: 256,
        max_bytes: 256 * 1024,
        rationale: "HTTP/1 warmed-loop budget tracks the current parser/header/body allocation \
            profile; the runtime-agnostic migration must not add a new hot-path allocation class.",
    };
    const H2_WARMED_LOCAL_LOOP_BUDGET: AllocationBudget = AllocationBudget {
        max_operations: 512,
        max_bytes: 512 * 1024,
        rationale: "HTTP/2 warmed-loop budget includes stream/frame/header bookkeeping and is \
            intentionally higher than HTTP/1 while still catching large portability regressions.",
    };
    const RUNTIME_AGNOSTIC_TLS_DELTA_BUDGET: AllocationDeltaBudget = AllocationDeltaBudget {
        max_extra_operations: 32,
        max_extra_bytes: 32 * 1024,
        rationale: "Runtime-agnostic TLS may pay small wrapper bookkeeping costs, but it must stay \
            within the same allocation class as the stack-core transport wrappers.",
    };
    const PORTABILITY_SOURCE_SCAN_LIMITS: &str = "This is a lexical scan of selected production \
        portability surfaces. It catches obvious heap-erased dynamic dispatch and helper-thread \
        additions, but it can false-positive in comments and cannot prove absence through macros or \
        transitive dependencies.";
    const PORTABILITY_FORBIDDEN_PATTERNS: &[ForbiddenSourcePattern] = &[
        ForbiddenSourcePattern {
            pattern: "Box<dyn",
            reason: "runtime-neutral HTTP portability surfaces should use generic/static dispatch, \
                not heap-erased trait objects",
        },
        ForbiddenSourcePattern {
            pattern: "Arc<dyn",
            reason: "shared trait-object dispatch would hide portability overhead from callers",
        },
        ForbiddenSourcePattern {
            pattern: "Box::new",
            reason: "new heap allocation in these surfaces must be budgeted explicitly",
        },
        ForbiddenSourcePattern {
            pattern: "thread::spawn",
            reason: "portable transports must not hide helper OS threads",
        },
        ForbiddenSourcePattern {
            pattern: "helper thread",
            reason: "helper-thread behavior must not be introduced implicitly",
        },
    ];

    fn server_tls_transport(
        cx: &RuntimeContext<'_>,
        context: &TlsContext,
        fd: OwnedFd,
        path: TlsTransportPath,
    ) -> StackTransport {
        match path {
            TlsTransportPath::StackCoreWrappers => {
                StackTransport::tls(context.server(cx, TLS_BUFFER_SIZE, fd).unwrap())
            }
            TlsTransportPath::RuntimeAgnostic => RuntimeStackTransport::tls_stream(
                context
                    .server_with_runtime(cx, TLS_BUFFER_SIZE, fd)
                    .unwrap(),
            ),
        }
    }

    fn client_tls_transport(
        cx: &RuntimeContext<'_>,
        context: &TlsContext,
        fd: OwnedFd,
        path: TlsTransportPath,
    ) -> StackTransport {
        match path {
            TlsTransportPath::StackCoreWrappers => StackTransport::tls(
                context
                    .client(cx, TLS_BUFFER_SIZE, fd, "localhost")
                    .unwrap(),
            ),
            TlsTransportPath::RuntimeAgnostic => RuntimeStackTransport::tls_stream(
                context
                    .client_with_runtime(cx, TLS_BUFFER_SIZE, fd, "localhost")
                    .unwrap(),
            ),
        }
    }

    struct UnsupportedRuntime;

    struct UnsupportedSocket(OwnedFd);

    struct UnsupportedSleep;

    impl RuntimeWaitable for UnsupportedSleep {
        fn is_ready(&self) -> bool {
            false
        }

        fn add_stackful_waiter(&self, _waiter: StackfulWaiterHandle) -> bool {
            false
        }
    }

    impl AsFd for UnsupportedSocket {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.0.as_fd()
        }
    }

    impl RuntimeSocket for UnsupportedSocket {}

    struct UnsupportedRead<B>(PhantomData<B>);

    impl<B> Waitable for UnsupportedRead<B> {
        fn is_ready(&self) -> bool {
            false
        }

        fn add_waiter(&self, _cx: &RuntimeContext<'_>, _registration: &WaitRegistration) {}
    }

    impl<B> RuntimeWaitable for UnsupportedRead<B> {
        fn is_ready(&self) -> bool {
            Waitable::is_ready(self)
        }

        fn add_stackful_waiter(&self, _waiter: StackfulWaiterHandle) -> bool {
            false
        }
    }

    impl<B> RuntimeReadResult<B> for UnsupportedRead<B>
    where
        B: IoReadBuffer + Send + 'static,
    {
        type Output = ReadOutput<B>;

        fn try_get(&mut self) -> Option<Result<Self::Output, RuntimeIoError>> {
            None
        }

        fn cancel(&mut self) -> Result<(), RuntimeIoError> {
            Err(unsupported_socket_io())
        }
    }

    struct UnsupportedWrite<B>(PhantomData<B>);

    impl<B> Waitable for UnsupportedWrite<B> {
        fn is_ready(&self) -> bool {
            false
        }

        fn add_waiter(&self, _cx: &RuntimeContext<'_>, _registration: &WaitRegistration) {}
    }

    impl<B> RuntimeWaitable for UnsupportedWrite<B> {
        fn is_ready(&self) -> bool {
            Waitable::is_ready(self)
        }

        fn add_stackful_waiter(&self, _waiter: StackfulWaiterHandle) -> bool {
            false
        }
    }

    impl<B> RuntimeWriteResult<B> for UnsupportedWrite<B>
    where
        B: kimojio_stack::IoWriteBuffer + Send + 'static,
    {
        type Output = WriteOutput<B>;

        fn try_get(&mut self) -> Option<Result<Self::Output, RuntimeIoError>> {
            None
        }

        fn cancel(&mut self) -> Result<(), RuntimeIoError> {
            Err(unsupported_socket_io())
        }
    }

    fn unsupported_socket_io() -> RuntimeIoError {
        RuntimeIoError::Unsupported(UnsupportedCapability::new(
            "socket-io",
            RuntimeFamily::Other("unsupported-test"),
        ))
    }

    impl RuntimeCapabilities for UnsupportedRuntime {
        fn runtime_family(&self) -> RuntimeFamily {
            RuntimeFamily::Other("unsupported-test")
        }

        fn supports(&self, _capability: RuntimeCapability) -> bool {
            false
        }
    }

    impl IoRuntime for UnsupportedRuntime {
        type Sleep = UnsupportedSleep;

        fn sleep_async(&self, _duration: Duration) -> Result<Self::Sleep, RuntimeIoError> {
            Err(unsupported_socket_io())
        }

        fn sleep_for(&self, _duration: Duration) -> Result<(), RuntimeIoError> {
            Err(unsupported_socket_io())
        }
    }

    impl StackfulWaitContext for UnsupportedRuntime {
        fn stackful_wait_registration(&self) -> Option<Box<dyn StackfulWaitRegistration + '_>> {
            None
        }

        fn park_stackful(&self) {}
    }

    impl SocketIoRuntime for UnsupportedRuntime {
        type Socket = UnsupportedSocket;
        type ReadResult<B>
            = UnsupportedRead<B>
        where
            B: IoReadBuffer + Send + 'static;
        type WriteResult<B>
            = UnsupportedWrite<B>
        where
            B: kimojio_stack::IoWriteBuffer + Send + 'static;

        fn socket_from_owned_fd(&self, fd: OwnedFd) -> Result<Self::Socket, RuntimeIoError> {
            Ok(UnsupportedSocket(fd))
        }

        fn read(&self, _fd: &Self::Socket, _buf: &mut [u8]) -> Result<usize, RuntimeIoError> {
            Err(unsupported_socket_io())
        }

        fn write(&self, _fd: &Self::Socket, _buf: &[u8]) -> Result<usize, RuntimeIoError> {
            Err(unsupported_socket_io())
        }

        fn read_async<B>(
            &self,
            _fd: &Self::Socket,
            _buffer: B,
        ) -> Result<Self::ReadResult<B>, RuntimeIoError>
        where
            B: IoReadBuffer + Send + 'static,
        {
            Err(unsupported_socket_io())
        }

        fn write_async<B>(
            &self,
            _fd: &Self::Socket,
            _buffer: B,
        ) -> Result<Self::WriteResult<B>, RuntimeIoError>
        where
            B: kimojio_stack::IoWriteBuffer + Send + 'static,
        {
            Err(unsupported_socket_io())
        }

        fn shutdown(
            &self,
            _fd: &Self::Socket,
            _how: rustix::net::Shutdown,
        ) -> Result<(), RuntimeIoError> {
            Err(unsupported_socket_io())
        }

        fn close(&self, _fd: Self::Socket) -> Result<(), RuntimeIoError> {
            Err(unsupported_socket_io())
        }
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

                server.join(cx);
                let received = client.join(cx);
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
            cx.scope(|scope| {
                let timeout = scope.spawn(move |cx| {
                    let mut transport = StackTransport::plaintext(client_fd);
                    transport.set_io_timeout(Some(Duration::from_millis(1)));
                    let mut received = [0_u8; 1];
                    let error = transport.read(cx, &mut received).unwrap_err();

                    assert_eq!(error, super::Error::Io(Errno::TIME));
                    transport.close(cx).unwrap();
                    cx.close(server_fd).unwrap();
                });
                timeout.join(cx);
            });
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

                timed.join(cx);
                normal_server.join(cx);
                assert_eq!(normal_client.join(cx), b'x');
            });
        });
    }

    #[test]
    fn runtime_agnostic_transport_reports_unsupported_socket_io() {
        let (fd, _peer) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .unwrap();
        let runtime = UnsupportedRuntime;
        let mut transport = RuntimeStackTransport::plaintext_socket(
            SocketIoRuntime::socket_from_owned_fd(&runtime, fd).unwrap(),
        );
        let mut received = [0_u8; 1];

        assert_eq!(
            transport.read(&runtime, &mut received).unwrap_err(),
            super::Error::Unsupported("runtime does not support socket I/O")
        );
    }

    #[test]
    fn runtime_agnostic_plaintext_timeout_cancel_and_close_runs_on_stack_runtime() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let timeout = scope.spawn(|cx| {
                    let (read_fd, peer_fd) = socketpair(
                        AddressFamily::UNIX,
                        SocketType::STREAM,
                        SocketFlags::empty(),
                        None,
                    )
                    .unwrap();
                    let read_fd = cx.socket_from_owned_fd(read_fd).unwrap();
                    let peer_fd = cx.socket_from_owned_fd(peer_fd).unwrap();
                    let mut transport = RuntimeStackTransport::plaintext_socket(read_fd);
                    transport.set_io_timeout(Some(Duration::from_millis(1)));
                    let mut received = [0_u8; 1];

                    assert_eq!(
                        transport.read(cx, &mut received).unwrap_err(),
                        super::Error::Io(Errno::TIME)
                    );
                    transport.close(cx).unwrap();
                    SocketIoRuntime::close(cx, peer_fd).unwrap();
                });
                timeout.join(cx);
            });
        });
    }

    #[test]
    fn runtime_agnostic_plaintext_timeout_cancel_and_close_runs_on_stealing_runtime() {
        let mut runtime = StealRuntime::with_config(StealRuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            ..StealRuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let timeout = scope.spawn(|cx| {
                    let (read_fd, peer_fd) = socketpair(
                        AddressFamily::UNIX,
                        SocketType::STREAM,
                        SocketFlags::empty(),
                        None,
                    )
                    .unwrap();
                    let read_fd = cx.socket_from_owned_fd(read_fd).unwrap();
                    let peer_fd = cx.socket_from_owned_fd(peer_fd).unwrap();
                    let mut transport = RuntimeStackTransport::plaintext_socket(read_fd);
                    transport.set_io_timeout(Some(Duration::from_millis(1)));
                    let mut received = [0_u8; 1];

                    assert_eq!(
                        transport.read(cx, &mut received).unwrap_err(),
                        super::Error::Io(Errno::TIME)
                    );
                    transport.close(cx).unwrap();
                    SocketIoRuntime::close(cx, peer_fd).unwrap();
                });
                timeout.join(cx);
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

                server.join(cx);
                let received = client.join(cx);
                assert_eq!(&received, b"world");
            });
        });
    }

    #[test]
    fn allocation_http1_warmed_local_loop_records_current_hot_path_allocations() {
        let counts = Runtime::new().block_on(|cx| {
            let (client_transport, server_transport) = socket_transport_pair();
            let request = request("/allocation-http1", b"ping");
            let response = response(b"pong");

            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut server =
                        http1::ServerConnection::new(server_transport, HttpConfig::default());
                    for _ in 0..LOCAL_LOOP_WARMUP_ROUNDS + LOCAL_LOOP_MEASURED_ROUNDS {
                        let request = server.read_request(cx).unwrap().unwrap();
                        black_box(request.body().len());
                        server.write_response(cx, &response).unwrap();
                    }
                    server.close(cx).unwrap();
                });

                let mut client =
                    http1::ClientConnection::new(client_transport, HttpConfig::default());
                for _ in 0..LOCAL_LOOP_WARMUP_ROUNDS {
                    black_box(client.send(cx, &request).unwrap());
                }

                let (_, counts) = allocation_tracking::measure(|| {
                    for _ in 0..LOCAL_LOOP_MEASURED_ROUNDS {
                        black_box(client.send(cx, &request).unwrap());
                    }
                });
                client.close(cx).unwrap();
                server.join(cx);
                counts
            })
        });

        assert_allocation_budget(
            "HTTP/1 warmed local loop",
            counts,
            HTTP1_WARMED_LOCAL_LOOP_BUDGET,
        );
    }

    #[test]
    fn allocation_h2_warmed_local_loop_records_current_hot_path_allocations() {
        let counts = Runtime::new().block_on(|cx| {
            let (client_transport, server_transport) = socket_transport_pair();
            let request = request("/allocation-h2", b"ping");
            let response = response(b"pong");

            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut server =
                        h2::ServerConnection::new(server_transport, HttpConfig::default());
                    for _ in 0..LOCAL_LOOP_WARMUP_ROUNDS + LOCAL_LOOP_MEASURED_ROUNDS {
                        let incoming = server.accept(cx).unwrap().unwrap();
                        black_box(incoming.request.body().len());
                        server
                            .send_response(cx, incoming.stream_id, &response)
                            .unwrap();
                    }
                    server.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                let mut client = h2::ClientConnection::new(client_transport, HttpConfig::default());
                for _ in 0..LOCAL_LOOP_WARMUP_ROUNDS {
                    black_box(client.send(cx, &request).unwrap());
                }

                let (_, counts) = allocation_tracking::measure(|| {
                    for _ in 0..LOCAL_LOOP_MEASURED_ROUNDS {
                        black_box(client.send(cx, &request).unwrap());
                    }
                });
                client.close(cx).unwrap();
                server.join(cx);
                counts
            })
        });

        assert_allocation_budget(
            "HTTP/2 warmed local loop",
            counts,
            H2_WARMED_LOCAL_LOOP_BUDGET,
        );
    }

    #[test]
    fn allocation_runtime_agnostic_http1_tls_portability_layer_no_extra_allocations() {
        let concrete = measure_http1_tls_allocations(TlsTransportPath::StackCoreWrappers);
        let runtime_agnostic = measure_http1_tls_allocations(TlsTransportPath::RuntimeAgnostic);

        assert_allocation_delta_budget(
            "runtime-agnostic HTTP/1 TLS portability layer",
            concrete,
            runtime_agnostic,
            RUNTIME_AGNOSTIC_TLS_DELTA_BUDGET,
        );
    }

    #[test]
    fn runtime_agnostic_portability_surface_avoids_dyn_heap_and_helper_threads() {
        let surfaces = [
            SourceSurface {
                name: "transport",
                source: include_str!("transport.rs"),
            },
            SourceSurface {
                name: "tls",
                source: include_str!("tls.rs"),
            },
            SourceSurface {
                name: "http1/client",
                source: include_str!("http1/client.rs"),
            },
            SourceSurface {
                name: "http1/server",
                source: include_str!("http1/server.rs"),
            },
        ];

        for surface in surfaces {
            assert_portability_source_surface(surface);
        }
    }

    fn assert_allocation_budget(
        label: &str,
        counts: allocation_tracking::AllocationCounts,
        budget: AllocationBudget,
    ) {
        let operations = counts.allocating_operations();
        assert!(
            operations <= budget.max_operations,
            "{label} exceeded allocation operation budget: observed {operations}, budget {}. \
             counts={counts:?}. Rationale: {}",
            budget.max_operations,
            budget.rationale
        );

        let bytes = counts.allocated_or_reallocated_bytes();
        assert!(
            bytes <= budget.max_bytes,
            "{label} exceeded allocation byte budget: observed {bytes}, budget {}. \
             counts={counts:?}. Rationale: {}",
            budget.max_bytes,
            budget.rationale
        );
    }

    fn assert_allocation_delta_budget(
        label: &str,
        baseline: allocation_tracking::AllocationCounts,
        candidate: allocation_tracking::AllocationCounts,
        budget: AllocationDeltaBudget,
    ) {
        let baseline_operations = baseline.allocating_operations();
        let candidate_operations = candidate.allocating_operations();
        let max_operations = baseline_operations + budget.max_extra_operations;
        assert!(
            candidate_operations <= max_operations,
            "{label} exceeded allocation operation delta budget: baseline {baseline_operations}, \
             observed {candidate_operations}, max allowed {max_operations}. baseline={baseline:?} \
             candidate={candidate:?}. Rationale: {}",
            budget.rationale
        );

        let baseline_bytes = baseline.allocated_or_reallocated_bytes();
        let candidate_bytes = candidate.allocated_or_reallocated_bytes();
        let max_bytes = baseline_bytes + budget.max_extra_bytes;
        assert!(
            candidate_bytes <= max_bytes,
            "{label} exceeded allocation byte delta budget: baseline {baseline_bytes}, observed \
             {candidate_bytes}, max allowed {max_bytes}. baseline={baseline:?} \
             candidate={candidate:?}. Rationale: {}",
            budget.rationale
        );
    }

    fn assert_portability_source_surface(surface: SourceSurface) {
        for forbidden in PORTABILITY_FORBIDDEN_PATTERNS {
            if let Some((line_number, line)) =
                find_source_pattern(surface.source, forbidden.pattern)
            {
                panic!(
                    "{} contains forbidden portability pattern `{}` on line {}: `{}`. Reason: {}. \
                     Scan limits: {}",
                    surface.name,
                    forbidden.pattern,
                    line_number,
                    line.trim(),
                    forbidden.reason,
                    PORTABILITY_SOURCE_SCAN_LIMITS
                );
            }
        }
    }

    fn find_source_pattern<'source>(
        source: &'source str,
        pattern: &str,
    ) -> Option<(usize, &'source str)> {
        source
            .lines()
            .enumerate()
            .find_map(|(index, line)| line.contains(pattern).then_some((index + 1, line)))
    }

    fn measure_http1_tls_allocations(
        path: TlsTransportPath,
    ) -> allocation_tracking::AllocationCounts {
        let (server_ctx, client_ctx) = make_contexts();
        let (client_fd, server_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .unwrap();
        let request = request("/allocation-runtime-agnostic-tls", b"ping");
        let response = response(b"pong");

        Runtime::new().block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let transport = server_tls_transport(cx, &server_ctx, server_fd, path);
                    let mut server =
                        http1::RuntimeServerConnection::new(transport, HttpConfig::default());
                    for _ in 0..TLS_PORTABILITY_WARMUP_ROUNDS + TLS_PORTABILITY_MEASURED_ROUNDS {
                        server
                            .serve_one(cx, |request| {
                                assert_eq!(request.uri(), "/allocation-runtime-agnostic-tls");
                                assert_eq!(request.body().as_bytes(), b"ping");
                                Ok(response.clone())
                            })
                            .unwrap();
                    }
                    server.close(cx).unwrap();
                });

                let transport = client_tls_transport(cx, &client_ctx, client_fd, path);
                let mut client =
                    http1::RuntimeClientConnection::new(transport, HttpConfig::default());
                for _ in 0..TLS_PORTABILITY_WARMUP_ROUNDS {
                    black_box(client.send(cx, &request).unwrap());
                }

                let (_, counts) = allocation_tracking::measure(|| {
                    for _ in 0..TLS_PORTABILITY_MEASURED_ROUNDS {
                        black_box(client.send(cx, &request).unwrap());
                    }
                });
                client.close(cx).unwrap();
                server.join(cx);
                counts
            })
        })
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
