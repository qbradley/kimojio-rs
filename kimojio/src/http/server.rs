// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//! Runtime-local HTTP server support.
//!
//! [`Server`] binds an explicit IPv4 or IPv6 socket address, accepts HTTP/1.1
//! and HTTP/2 connections, and invokes an async handler on the current Kimojio
//! runtime. TLS connections select the protocol with ALPN. HTTP/2 requests are
//! multiplexed within each connection, while HTTP/1 requests are served
//! sequentially over persistent connections. Request bodies are buffered by
//! default and may be streamed by opting into a streaming serve method;
//! response bodies may be streamed.
//! [`Server::serve_with_expect_continue`] can reject an expected request body
//! from its parsed head before any body bytes are read.
//! [`ServerConfig`] bounds connection concurrency, I/O idle time, and graceful
//! shutdown. Nonfatal accept and connection failures are reported through
//! [`Server::serve_with_error_handler`].

use std::any::Any;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io::IoSlice;
use std::net::SocketAddr;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::stream::FuturesUnordered;
use futures::{FutureExt, StreamExt, pin_mut, select_biased};
use kimojio_fsm_http::{
    ConnectionResponse, ExchangeId, H2FrameRef, H2FrameType, HeaderBlock, HeaderRef, HttpProtocol,
    HttpVersion, ProtocolSelection, RequestBodyMode, RequestExpectation, ServerConnection,
    ServerEvent, Step,
};

use super::{
    Body, BodyInner, BodyStreamCursor, BodyTrailersFn, Error, HeaderMap, HeaderName, HeaderValue,
    InboundTrailers, IntoHttpResult, Limits, Method, ProtocolError, ProtocolErrorKind, ReadBuffer,
    Request, Response, Result, ServerConfig, Uri, Version, bind_listener, transport::Transport,
};
use crate::{
    AsyncEvent, AsyncStreamRead, AsyncStreamWrite, CancellationToken, Errno, OwnedFd,
    TaskHandleError, operations,
};

const INITIAL_ACCEPT_BACKOFF: Duration = Duration::from_millis(10);
const MAX_ACCEPT_BACKOFF: Duration = Duration::from_secs(1);

/// A decision made from request headers before an expected body is read.
#[derive(Debug)]
#[non_exhaustive]
pub enum ExpectContinueDecision {
    /// Send `100 Continue` when the protocol uses the handshake, then run the
    /// selected handler after accepting the body.
    Continue,
    /// Send this final response without reading or buffering the request body.
    Reject(Response<Body>),
}

type ExpectContinueHandler = dyn Fn(&Request<()>) -> ExpectContinueDecision;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestMode {
    Buffered,
    Streaming,
}

/// A nonfatal error observed while a [`Server`] continues serving.
///
/// Use [`Server::serve_with_error_handler`] to route these reports into the
/// application's logging or telemetry. The default [`Server::serve`] method
/// writes each report to standard error.
#[non_exhaustive]
pub enum ServeError {
    /// An accepted connection failed while parsing, reading, or writing.
    Connection(Error),
    /// Configuration of an accepted socket failed, so that socket was closed.
    ConnectionSetup(Errno),
    /// A recoverable listener accept failed and will be retried.
    TransientAccept(Errno),
    /// A connection task was canceled independently of graceful shutdown.
    ConnectionTaskCanceled,
    /// A connection task panicked.
    ConnectionTaskPanicked(Box<dyn Any + Send + 'static>),
    /// An HTTP/2 request handler or trailer callback panicked while its
    /// connection kept serving.
    HandlerPanicked(Box<dyn Any + Send + 'static>),
    /// An HTTP/2 response body failed while sibling streams kept serving.
    ResponseBody(Error),
    /// HTTP/2 response trailers were rejected while sibling streams kept
    /// serving.
    ResponseTrailers(Error),
}

impl ServeError {
    /// Returns the original payload for a connection-task or handler panic.
    pub fn panic_payload(&self) -> Option<&(dyn Any + Send + 'static)> {
        match self {
            Self::ConnectionTaskPanicked(payload) | Self::HandlerPanicked(payload) => {
                Some(payload.as_ref())
            }
            _ => None,
        }
    }
}

impl std::fmt::Debug for ServeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connection(error) => formatter.debug_tuple("Connection").field(error).finish(),
            Self::ConnectionSetup(error) => formatter
                .debug_tuple("ConnectionSetup")
                .field(error)
                .finish(),
            Self::TransientAccept(error) => formatter
                .debug_tuple("TransientAccept")
                .field(error)
                .finish(),
            Self::ConnectionTaskCanceled => formatter.write_str("ConnectionTaskCanceled"),
            Self::ConnectionTaskPanicked(payload) => formatter
                .debug_tuple("ConnectionTaskPanicked")
                .field(&panic_message(payload.as_ref()))
                .finish(),
            Self::HandlerPanicked(payload) => formatter
                .debug_tuple("HandlerPanicked")
                .field(&panic_message(payload.as_ref()))
                .finish(),
            Self::ResponseBody(error) => {
                formatter.debug_tuple("ResponseBody").field(error).finish()
            }
            Self::ResponseTrailers(error) => formatter
                .debug_tuple("ResponseTrailers")
                .field(error)
                .finish(),
        }
    }
}

impl std::fmt::Display for ServeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connection(error) => write!(formatter, "HTTP connection failed: {error}"),
            Self::ConnectionSetup(error) => {
                write!(formatter, "accepted HTTP socket setup failed: {error}")
            }
            Self::TransientAccept(error) => {
                write!(
                    formatter,
                    "transient HTTP accept failure; retrying: {error}"
                )
            }
            Self::ConnectionTaskCanceled => formatter.write_str("HTTP connection task canceled"),
            Self::ConnectionTaskPanicked(payload) => write!(
                formatter,
                "HTTP connection task panicked: {}",
                panic_message(payload.as_ref())
            ),
            Self::HandlerPanicked(payload) => write!(
                formatter,
                "HTTP/2 request handler or trailer callback panicked: {}",
                panic_message(payload.as_ref())
            ),
            Self::ResponseBody(error) => write!(formatter, "HTTP/2 response body failed: {error}"),
            Self::ResponseTrailers(error) => {
                write!(formatter, "HTTP/2 response trailers failed: {error}")
            }
        }
    }
}

impl std::error::Error for ServeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Connection(error) | Self::ResponseBody(error) | Self::ResponseTrailers(error) => {
                Some(error)
            }
            _ => None,
        }
    }
}

fn panic_message<'a>(payload: &'a (dyn Any + Send + 'static)) -> &'a str {
    payload
        .downcast_ref::<&'static str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("<non-string panic payload>")
}

/// A builder for an HTTP [`Server`].
#[derive(Clone, Debug)]
pub struct ServerBuilder {
    address: SocketAddr,
    config: ServerConfig,
}

impl ServerBuilder {
    /// Creates a builder for an explicit IPv4 or IPv6 address.
    pub fn new(address: SocketAddr) -> Self {
        Self {
            address,
            config: ServerConfig::new(),
        }
    }

    /// Returns the address that will be bound.
    pub const fn address(&self) -> SocketAddr {
        self.address
    }

    /// Replaces the complete server configuration.
    pub fn config(mut self, config: ServerConfig) -> Self {
        self.config = config;
        self
    }

    /// Sets the request, response, and per-connection limits.
    pub fn limits(mut self, limits: Limits) -> Self {
        self.config = self.config.set_limits(limits);
        self
    }

    /// Sets the kernel listen backlog.
    pub fn listen_backlog(mut self, backlog: u32) -> Self {
        self.config = self.config.set_listen_backlog(backlog);
        self
    }

    /// Sets the maximum time a connection read or write may remain idle.
    pub fn connection_io_timeout(mut self, timeout: Duration) -> Self {
        self.config = self.config.set_connection_io_timeout(timeout);
        self
    }

    /// Sets how long shutdown waits for active handlers to finish.
    pub fn graceful_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.config = self.config.set_graceful_shutdown_timeout(timeout);
        self
    }

    /// Sets the maximum number of concurrently active connections.
    pub fn max_connections(mut self, max_connections: usize) -> Self {
        self.config = self.config.set_max_connections(max_connections);
        self
    }

    /// Enables TLS and ALPN on accepted connections.
    #[cfg(feature = "tls")]
    pub fn tls(mut self, tls: super::TlsServerConfig) -> Self {
        self.config = self.config.set_tls(tls);
        self
    }

    /// Binds the configured address.
    ///
    /// Port zero is supported; use [`Server::local_addr`] to discover the
    /// operating-system-assigned port.
    pub async fn bind(self) -> Result<Server> {
        Server::bind_with_config(self.address, self.config).await
    }
}

/// A bound HTTP/1.1 and HTTP/2 server with optional TLS and ALPN support.
#[derive(Debug)]
pub struct Server {
    listener: OwnedFd,
    local_addr: SocketAddr,
    config: ServerConfig,
}

impl Server {
    /// Creates a builder for an explicit IPv4 or IPv6 address.
    pub fn builder(address: SocketAddr) -> ServerBuilder {
        ServerBuilder::new(address)
    }

    /// Binds an address using [`ServerConfig::default`].
    ///
    /// Port zero is supported; use [`Self::local_addr`] to discover the
    /// operating-system-assigned port.
    pub async fn bind(address: SocketAddr) -> Result<Self> {
        Self::builder(address).bind().await
    }

    /// Binds an address with an explicit configuration.
    pub async fn bind_with_config(address: SocketAddr, config: ServerConfig) -> Result<Self> {
        let (listener, local_addr) = bind_listener(address, &config).await?;
        Ok(Self {
            listener,
            local_addr,
            config,
        })
    }

    /// Returns the resolved local address, including an assigned port.
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Returns this server's configuration.
    pub const fn config(&self) -> &ServerConfig {
        &self.config
    }

    /// Accepts connections until `cancellation` is triggered.
    ///
    /// The handler and its returned future are runtime-local and do not need to
    /// implement `Send`. One Kimojio task is spawned per accepted connection.
    /// HTTP/2 connections may serve concurrent streams; HTTP/1 connections
    /// serve sequential requests until either peer closes or a configured
    /// limit is reached. Cancellation stops new accepts, lets active handlers
    /// finish within the configured grace period, and then cancels remaining
    /// connection tasks. Nonfatal failures are written to standard error; use
    /// [`Self::serve_with_error_handler`] for application-defined reporting.
    ///
    /// The request keeps parsed hop-by-hop headers. If the handler forwards a
    /// request, it must remove `Connection`, each header named by `Connection`,
    /// and other connection-scoped headers before it sends the request.
    pub async fn serve<H, F>(self, handler: H, cancellation: Rc<CancellationToken>) -> Result<()>
    where
        H: Fn(Request<Body>) -> F + 'static,
        F: Future<Output = Response<Body>> + 'static,
    {
        self.serve_inner(
            handler,
            RequestMode::Buffered,
            None,
            cancellation,
            |error| {
                eprintln!("{error}");
            },
        )
        .await
    }

    /// Accepts connections and streams request bodies to the handler.
    ///
    /// The handler is invoked after the parsed head, before the body completes.
    /// Each [`Body::next_chunk`] call pulls at most one chunk from the
    /// connection. Returning before the body ends abandons that request:
    /// HTTP/1 closes the connection, while HTTP/2 retires only that stream.
    pub async fn serve_streaming<H, F>(
        self,
        handler: H,
        cancellation: Rc<CancellationToken>,
    ) -> Result<()>
    where
        H: Fn(Request<Body>) -> F + 'static,
        F: Future<Output = Response<Body>> + 'static,
    {
        self.serve_streaming_with_error_handler(handler, cancellation, |error| {
            eprintln!("{error}");
        })
        .await
    }

    /// Accepts connections with a synchronous `100-continue` decision hook.
    ///
    /// The hook runs only for a supported `Expect: 100-continue` request and
    /// receives an empty-body view of its parsed head. Returning
    /// [`ExpectContinueDecision::Reject`] sends that final response before any
    /// request body is read. Returning [`ExpectContinueDecision::Continue`]
    /// preserves the normal buffered-handler path. Unsupported expectations
    /// are rejected automatically by the protocol driver.
    ///
    /// Hook panics are isolated and converted to `500 Internal Server Error`,
    /// like panicking HTTP/2 request handlers. Nonfatal failures are written to
    /// standard error.
    pub async fn serve_with_expect_continue<H, F, C>(
        self,
        handler: H,
        expect_continue: C,
        cancellation: Rc<CancellationToken>,
    ) -> Result<()>
    where
        H: Fn(Request<Body>) -> F + 'static,
        F: Future<Output = Response<Body>> + 'static,
        C: Fn(&Request<()>) -> ExpectContinueDecision + 'static,
    {
        self.serve_with_expect_continue_and_error_handler(
            handler,
            expect_continue,
            cancellation,
            |error| {
                eprintln!("{error}");
            },
        )
        .await
    }

    /// Accepts connections with streaming request bodies and a `100-continue`
    /// decision hook.
    pub async fn serve_streaming_with_expect_continue<H, F, C>(
        self,
        handler: H,
        expect_continue: C,
        cancellation: Rc<CancellationToken>,
    ) -> Result<()>
    where
        H: Fn(Request<Body>) -> F + 'static,
        F: Future<Output = Response<Body>> + 'static,
        C: Fn(&Request<()>) -> ExpectContinueDecision + 'static,
    {
        self.serve_streaming_with_expect_continue_and_error_handler(
            handler,
            expect_continue,
            cancellation,
            |error| {
                eprintln!("{error}");
            },
        )
        .await
    }

    /// Accepts connections and reports every nonfatal failure to `on_error`.
    ///
    /// Per-connection parse/I/O errors and handler panics do not terminate the
    /// listener. Recoverable accept failures are retried with bounded backoff.
    /// The callback is runtime-local and does not need to implement `Send`.
    /// Fatal listener failures are still returned from this method.
    ///
    /// The request keeps parsed hop-by-hop headers. If the handler forwards a
    /// request, it must remove `Connection`, each header named by `Connection`,
    /// and other connection-scoped headers before it sends the request.
    pub async fn serve_with_error_handler<H, F, E>(
        self,
        handler: H,
        cancellation: Rc<CancellationToken>,
        on_error: E,
    ) -> Result<()>
    where
        H: Fn(Request<Body>) -> F + 'static,
        F: Future<Output = Response<Body>> + 'static,
        E: FnMut(ServeError),
    {
        self.serve_inner(handler, RequestMode::Buffered, None, cancellation, on_error)
            .await
    }

    /// Accepts connections with streaming request bodies and reports every
    /// nonfatal failure to `on_error`.
    pub async fn serve_streaming_with_error_handler<H, F, E>(
        self,
        handler: H,
        cancellation: Rc<CancellationToken>,
        on_error: E,
    ) -> Result<()>
    where
        H: Fn(Request<Body>) -> F + 'static,
        F: Future<Output = Response<Body>> + 'static,
        E: FnMut(ServeError),
    {
        self.serve_inner(
            handler,
            RequestMode::Streaming,
            None,
            cancellation,
            on_error,
        )
        .await
    }

    /// Accepts connections with a `100-continue` hook and error callback.
    ///
    /// This combines [`Self::serve_with_expect_continue`] with
    /// [`Self::serve_with_error_handler`].
    pub async fn serve_with_expect_continue_and_error_handler<H, F, C, E>(
        self,
        handler: H,
        expect_continue: C,
        cancellation: Rc<CancellationToken>,
        on_error: E,
    ) -> Result<()>
    where
        H: Fn(Request<Body>) -> F + 'static,
        F: Future<Output = Response<Body>> + 'static,
        C: Fn(&Request<()>) -> ExpectContinueDecision + 'static,
        E: FnMut(ServeError),
    {
        let expect_continue: Rc<ExpectContinueHandler> = Rc::new(expect_continue);
        self.serve_inner(
            handler,
            RequestMode::Buffered,
            Some(expect_continue),
            cancellation,
            on_error,
        )
        .await
    }

    /// Combines streaming request bodies, a `100-continue` hook, and an error
    /// callback.
    pub async fn serve_streaming_with_expect_continue_and_error_handler<H, F, C, E>(
        self,
        handler: H,
        expect_continue: C,
        cancellation: Rc<CancellationToken>,
        on_error: E,
    ) -> Result<()>
    where
        H: Fn(Request<Body>) -> F + 'static,
        F: Future<Output = Response<Body>> + 'static,
        C: Fn(&Request<()>) -> ExpectContinueDecision + 'static,
        E: FnMut(ServeError),
    {
        let expect_continue: Rc<ExpectContinueHandler> = Rc::new(expect_continue);
        self.serve_inner(
            handler,
            RequestMode::Streaming,
            Some(expect_continue),
            cancellation,
            on_error,
        )
        .await
    }

    async fn serve_inner<H, F, E>(
        self,
        handler: H,
        request_mode: RequestMode,
        expect_continue: Option<Rc<ExpectContinueHandler>>,
        cancellation: Rc<CancellationToken>,
        mut on_error: E,
    ) -> Result<()>
    where
        H: Fn(Request<Body>) -> F + 'static,
        F: Future<Output = Response<Body>> + 'static,
        E: FnMut(ServeError),
    {
        let handler = Rc::new(handler);
        let config = self.config;
        let limits = config.limits();
        let connection_io_timeout = config.connection_io_timeout();
        let mut connections = FuturesUnordered::new();
        let force_shutdown = Rc::new(CancellationToken::new());
        let (reporter, reports) = crate::async_channel_unbounded();
        let mut accept_backoff = INITIAL_ACCEPT_BACKOFF;

        // The accept future outlives each loop iteration. Recreating it per
        // iteration would drop an accept that the kernel had already satisfied,
        // discarding a live connection that the peer believes is established.
        let accepting = operations::accept(&self.listener).fuse();
        pin_mut!(accepting);

        loop {
            if cancellation.is_cancelled() {
                break;
            }

            let event = if connections.len() >= config.max_connections() {
                let cancelled = cancellation.cancelled().fuse();
                let completed = connections.next().fuse();
                let reported = reports.recv().fuse();
                pin_mut!(cancelled, completed, reported);
                select_biased! {
                    _ = cancelled => ServeEvent::Cancelled,
                    reported = reported => ServeEvent::Report(
                        reported.expect("the connection report channel remains open"),
                    ),
                    completed = completed => ServeEvent::Connection(
                        completed.expect("a connection at the configured limit yields a task"),
                    ),
                }
            } else if connections.is_empty() {
                let cancelled = cancellation.cancelled().fuse();
                let reported = reports.recv().fuse();
                pin_mut!(cancelled, reported);
                select_biased! {
                    _ = cancelled => ServeEvent::Cancelled,
                    reported = reported => ServeEvent::Report(
                        reported.expect("the connection report channel remains open"),
                    ),
                    accepted = accepting.as_mut() => ServeEvent::Accepted(accepted),
                }
            } else {
                let cancelled = cancellation.cancelled().fuse();
                let completed = connections.next().fuse();
                let reported = reports.recv().fuse();
                pin_mut!(cancelled, completed, reported);
                select_biased! {
                    _ = cancelled => ServeEvent::Cancelled,
                    reported = reported => ServeEvent::Report(
                        reported.expect("the connection report channel remains open"),
                    ),
                    completed = completed => ServeEvent::Connection(
                        completed.expect("a non-empty connection set yields a task"),
                    ),
                    accepted = accepting.as_mut() => ServeEvent::Accepted(accepted),
                }
            };

            match event {
                ServeEvent::Cancelled => break,
                ServeEvent::Accepted(accepted) => {
                    accepting.set(operations::accept(&self.listener).fuse());
                    let socket = match accepted {
                        Ok(socket) => socket,
                        Err(error) if is_transient_accept_error(error) => {
                            on_error(ServeError::TransientAccept(error));
                            if wait_for_backoff(accept_backoff, &cancellation).await {
                                break;
                            }
                            accept_backoff =
                                accept_backoff.saturating_mul(2).min(MAX_ACCEPT_BACKOFF);
                            continue;
                        }
                        Err(error) => return Err(error.into()),
                    };
                    if let Err(error) = crate::socket_helpers::update_accept_socket(&socket) {
                        on_error(ServeError::ConnectionSetup(error));
                        if is_resource_pressure(error) {
                            if wait_for_backoff(accept_backoff, &cancellation).await {
                                break;
                            }
                            accept_backoff =
                                accept_backoff.saturating_mul(2).min(MAX_ACCEPT_BACKOFF);
                        }
                        continue;
                    }
                    accept_backoff = INITIAL_ACCEPT_BACKOFF;
                    let handler = Rc::clone(&handler);
                    let expect_continue = expect_continue.as_ref().map(Rc::clone);
                    let cancellation = Rc::clone(&cancellation);
                    let force_shutdown = Rc::clone(&force_shutdown);
                    let reporter = reporter.clone();
                    let lifecycle = ConnectionLifecycle {
                        shutdown: cancellation,
                        force_shutdown,
                        io_timeout: connection_io_timeout,
                        reporter,
                    };
                    #[cfg(feature = "tls")]
                    let tls = config.tls().cloned();
                    connections.push(operations::spawn_task(async move {
                        run_connection(
                            socket,
                            limits,
                            handler,
                            request_mode,
                            expect_continue,
                            lifecycle,
                            #[cfg(feature = "tls")]
                            tls,
                        )
                        .await
                    }));
                }
                ServeEvent::Connection(completed) => {
                    report_connection_result(completed, false, &mut on_error)
                }
                ServeEvent::Report(error) => on_error(error),
            }
        }

        let drain_deadline = crate::clock_now()
            .checked_add(config.graceful_shutdown_timeout())
            .expect("validated graceful shutdown timeout");
        while !connections.is_empty() {
            let completed = connections.next().fuse();
            let reported = reports.recv().fuse();
            let deadline = operations::sleep_until(drain_deadline).fuse();
            pin_mut!(completed, reported, deadline);
            let drained = select_biased! {
                _ = deadline => false,
                reported = reported => {
                    on_error(reported.expect("the connection report channel remains open"));
                    true
                },
                completed = completed => {
                    report_connection_result(
                        completed.expect("a non-empty connection set yields a task"),
                        true,
                        &mut on_error,
                    );
                    true
                },
            };
            if !drained {
                force_shutdown.cancel();
                break;
            }
        }
        while !connections.is_empty() {
            let completed = connections.next().fuse();
            let reported = reports.recv().fuse();
            pin_mut!(completed, reported);
            select_biased! {
                reported = reported => {
                    on_error(reported.expect("the connection report channel remains open"));
                },
                completed = completed => {
                    report_connection_result(
                        completed.expect("a non-empty connection set yields a task"),
                        true,
                        &mut on_error,
                    );
                },
            }
        }
        Ok(())
    }
}

enum ServeEvent {
    Cancelled,
    Accepted(std::result::Result<OwnedFd, Errno>),
    Connection(std::result::Result<Result<()>, TaskHandleError>),
    Report(ServeError),
}

fn report_connection_result(
    completed: std::result::Result<Result<()>, TaskHandleError>,
    shutting_down: bool,
    on_error: &mut impl FnMut(ServeError),
) {
    match completed {
        Ok(Ok(())) => {}
        Ok(Err(Error::Canceled)) if shutting_down => {}
        Ok(Err(error)) => on_error(ServeError::Connection(error)),
        Err(TaskHandleError::Canceled) => on_error(ServeError::ConnectionTaskCanceled),
        Err(TaskHandleError::Panic(payload)) => {
            on_error(ServeError::ConnectionTaskPanicked(payload));
        }
    }
}

fn is_transient_accept_error(error: Errno) -> bool {
    matches!(
        error.raw_os_error(),
        libc::EINTR
            | libc::ECONNABORTED
            | libc::EMFILE
            | libc::ENFILE
            | libc::ENOBUFS
            | libc::ENOMEM
            | libc::EAGAIN
    )
}

fn is_resource_pressure(error: Errno) -> bool {
    matches!(
        error.raw_os_error(),
        libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM
    )
}

async fn wait_for_backoff(duration: Duration, cancellation: &CancellationToken) -> bool {
    let cancelled = cancellation.cancelled().fuse();
    let delay = operations::sleep(duration).fuse();
    pin_mut!(cancelled, delay);
    select_biased! {
        _ = cancelled => true,
        _ = delay => false,
    }
}

#[derive(Clone)]
struct ConnectionLifecycle {
    shutdown: Rc<CancellationToken>,
    force_shutdown: Rc<CancellationToken>,
    io_timeout: Duration,
    reporter: crate::SenderUnbounded<ServeError>,
}

impl ConnectionLifecycle {
    fn io_deadline(&self) -> Result<std::time::Instant> {
        crate::clock_now()
            .checked_add(self.io_timeout)
            .ok_or(Error::InvalidConfiguration(
                "connection I/O timeout is too large for the current clock",
            ))
    }

    async fn read(&self, input: &mut ReadBuffer, stream: &mut Transport) -> Result<usize> {
        let read = input
            .read_from_with_deadline(stream, Some(self.io_deadline()?))
            .fuse();
        let shutdown = self.shutdown.cancelled().fuse();
        pin_mut!(read, shutdown);
        select_biased! {
            _ = shutdown => Err(Error::Canceled),
            result = read => result,
        }
    }

    async fn handler<F>(&self, handler: F) -> Result<Response<Body>>
    where
        F: Future<Output = Response<Body>>,
    {
        let handler = handler.fuse();
        let force_shutdown = self.force_shutdown.cancelled().fuse();
        pin_mut!(handler, force_shutdown);
        select_biased! {
            _ = force_shutdown => Err(Error::Canceled),
            response = handler => Ok(response),
        }
    }

    async fn write(&self, stream: &mut Transport, bytes: &[u8]) -> Result<()> {
        let write = stream.write(bytes, Some(self.io_deadline()?)).fuse();
        let force_shutdown = self.force_shutdown.cancelled().fuse();
        pin_mut!(write, force_shutdown);
        select_biased! {
            _ = force_shutdown => Err(Error::Canceled),
            result = write => result.map_err(Error::from),
        }
    }

    async fn writev<'a>(
        &self,
        stream: &'a mut Transport,
        slices: &'a mut [IoSlice<'a>],
    ) -> Result<()> {
        let write = stream.writev(slices, Some(self.io_deadline()?)).fuse();
        let force_shutdown = self.force_shutdown.cancelled().fuse();
        pin_mut!(write, force_shutdown);
        select_biased! {
            _ = force_shutdown => Err(Error::Canceled),
            result = write => result.map_err(Error::from),
        }
    }
}

async fn run_connection<H, F>(
    socket: OwnedFd,
    limits: Limits,
    handler: Rc<H>,
    request_mode: RequestMode,
    expect_continue: Option<Rc<ExpectContinueHandler>>,
    lifecycle: ConnectionLifecycle,
    #[cfg(feature = "tls")] tls: Option<super::TlsServerConfig>,
) -> Result<()>
where
    H: Fn(Request<Body>) -> F + 'static,
    F: Future<Output = Response<Body>> + 'static,
{
    #[cfg(feature = "tls")]
    let mut stream = server_transport(socket, tls, limits, &lifecycle).await?;
    #[cfg(not(feature = "tls"))]
    let mut stream = Transport::Plain(crate::OwnedFdStream::new(socket));
    handle_connection(
        &mut stream,
        limits,
        handler,
        request_mode,
        expect_continue,
        &lifecycle,
    )
    .await
}

async fn handle_connection<H, F>(
    stream: &mut Transport,
    limits: Limits,
    handler: Rc<H>,
    request_mode: RequestMode,
    expect_continue: Option<Rc<ExpectContinueHandler>>,
    lifecycle: &ConnectionLifecycle,
) -> Result<()>
where
    H: Fn(Request<Body>) -> F + 'static,
    F: Future<Output = Response<Body>> + 'static,
{
    let selection = match stream {
        Transport::Plain(_) => ProtocolSelection::Detect,
        #[cfg(feature = "tls")]
        Transport::Tls(stream) => {
            ProtocolSelection::Alpn(stream.get_ssl().selected_alpn_protocol())
        }
    };
    let body_mode = match request_mode {
        RequestMode::Buffered => RequestBodyMode::Buffered,
        RequestMode::Streaming => RequestBodyMode::Streaming,
    };
    let mut connection =
        ServerConnection::with_protocol_selection(selection, limits.protocol(), body_mode)
            .into_http()?;
    let result = serve_exchanges(
        stream,
        limits,
        handler,
        request_mode,
        expect_continue,
        lifecycle,
        &mut connection,
    )
    .await;
    let shutdown_result = async {
        if connection.begin_shutdown().into_http()? {
            let bytes = connection.pending_write().ok_or_else(|| {
                protocol(
                    ProtocolErrorKind::InvalidState,
                    "the connection driver did not retain shutdown output",
                )
            })?;
            lifecycle.write(stream, bytes).await?;
        }
        Ok(())
    }
    .await;

    close_gracefully(stream).await;

    match result {
        Err(error) => Err(error),
        Ok(()) => shutdown_result,
    }
}

/// How long to keep draining a connection after signalling the end of our own
/// output. The peer only has to notice the shutdown, so this stays short.
const LINGER_TIMEOUT: Duration = Duration::from_millis(250);

/// Ends the connection so the peer observes an orderly shutdown.
///
/// Closing a socket that still holds unread data makes the kernel send a reset,
/// and a reset discards whatever the peer has not read yet. That would throw
/// away the GOAWAY frame explaining why the connection ended. Signalling the
/// end of our output first and then draining what remains lets the peer read
/// that frame before it sees end of file.
async fn close_gracefully(stream: &mut Transport) {
    if stream.shutdown().await.is_err() {
        return;
    }

    let Some(deadline) = crate::clock_now().checked_add(LINGER_TIMEOUT) else {
        return;
    };
    let mut discard = [0u8; 512];
    while let Ok(read) = stream.try_read(&mut discard, Some(deadline)).await {
        if read == 0 {
            break;
        }
    }
}

/// Serves requests until the peer stops sending them or the protocol requires
/// the connection to end.
///
/// The read buffer spans exchanges because a peer may pipeline the frames of a
/// later exchange into the same read as the current one.
struct RequestHandling<H> {
    handler: Rc<H>,
    mode: RequestMode,
    expect_continue: Option<Rc<ExpectContinueHandler>>,
}

async fn serve_exchanges<H, F>(
    stream: &mut Transport,
    limits: Limits,
    handler: Rc<H>,
    request_mode: RequestMode,
    expect_continue: Option<Rc<ExpectContinueHandler>>,
    lifecycle: &ConnectionLifecycle,
    connection: &mut ServerConnection,
) -> Result<()>
where
    H: Fn(Request<Body>) -> F + 'static,
    F: Future<Output = Response<Body>> + 'static,
{
    let mut input = ReadBuffer::new(limits.read_buffer_bytes())?;
    let mut exchanges = HashSet::new();
    pump_connection(
        stream,
        &mut input,
        RequestHandling {
            handler,
            mode: request_mode,
            expect_continue,
        },
        lifecycle,
        connection,
        &mut exchanges,
    )
    .await
}

async fn pump_connection<H, F>(
    stream: &mut Transport,
    input: &mut ReadBuffer,
    handling: RequestHandling<H>,
    lifecycle: &ConnectionLifecycle,
    connection: &mut ServerConnection,
    exchanges: &mut HashSet<ExchangeId>,
) -> Result<()>
where
    H: Fn(Request<Body>) -> F + 'static,
    F: Future<Output = Response<Body>> + 'static,
{
    let RequestHandling {
        handler,
        mode: request_mode,
        expect_continue,
    } = handling;
    let mut requests: HashMap<ExchangeId, RequestParts> = HashMap::new();
    let mut request_streams: HashMap<ExchangeId, RequestStreamControl> = HashMap::new();
    let request_body_ready = Rc::new(AsyncEvent::new());
    let (request_demands, requested_bodies) = crate::async_channel_unbounded();
    let mut handler_cancellations = HashMap::new();
    let mut handlers = FuturesUnordered::new();
    let mut responses = HashMap::new();
    let mut header_only_responses = HashSet::new();
    let mut completed_responses = HashSet::new();
    let mut input_state = InputState::Open;
    let mut prefer_body = false;

    loop {
        if !retire_completed_responses(
            &mut completed_responses,
            &mut requests,
            &mut request_streams,
            &mut handler_cancellations,
            exchanges,
            connection,
        )? {
            return Ok(());
        }
        if let Err(failure) = poll_response_body_sources_once(&mut responses).await {
            handle_response_failure(
                failure,
                lifecycle,
                &mut responses,
                &mut completed_responses,
                connection,
            )?;
            prefer_body = true;
            continue;
        }
        if request_body_ready.is_set() {
            request_body_ready.reset();
            while let Some(exchange_id) =
                requested_bodies.try_recv().map_err(|_| Error::Canceled)?
            {
                if let Some(control) = request_streams.get_mut(&exchange_id) {
                    control.demanded = true;
                }
            }
            for control in request_streams.values_mut() {
                if control.chunk_pending && control.sender.is_empty() {
                    control.chunk_pending = false;
                }
            }
        }

        if let Some(completion) = handlers.next().now_or_never().flatten() {
            handle_handler_completion(
                completion,
                &mut request_streams,
                &mut handler_cancellations,
                exchanges,
                &mut responses,
                &mut header_only_responses,
                connection,
            )?;
            continue;
        }

        if request_mode == RequestMode::Buffered
            && connection.protocol() == Some(HttpProtocol::Http1)
            && !handlers.is_empty()
        {
            let completion = handlers
                .next()
                .await
                .expect("a non-empty handler set yields a completion");
            handle_handler_completion(
                completion,
                &mut request_streams,
                &mut handler_cancellations,
                exchanges,
                &mut responses,
                &mut header_only_responses,
                connection,
            )?;
            continue;
        }

        // Alternate protocol progress with one DATA write so neither buffered
        // request frames nor the driver's fair response scheduler can starve.
        let attempted_body = prefer_body && !responses.is_empty();
        if attempted_body {
            let action =
                match write_response_body(stream, lifecycle, connection, &mut responses).await {
                    Ok(action) => action,
                    Err(ResponseWriteFailure::Connection(error)) => return Err(error),
                    Err(ResponseWriteFailure::Exchange(failure)) => {
                        handle_response_failure(
                            failure,
                            lifecycle,
                            &mut responses,
                            &mut completed_responses,
                            connection,
                        )?;
                        prefer_body = true;
                        continue;
                    }
                };
            match action {
                BodyAction::Blocked => {}
                BodyAction::Sent => {
                    prefer_body = connection.protocol() == Some(HttpProtocol::Http1);
                    continue;
                }
                BodyAction::Complete(exchange_id) => {
                    completed_responses.insert(exchange_id);
                    prefer_body = false;
                    continue;
                }
            }
        }

        if attempted_body
            && connection.protocol() == Some(HttpProtocol::Http1)
            && responses.values().any(ResponseBody::needs_chunk)
        {
            let ready = wait_for_response_body_source(&mut responses).fuse();
            let canceled = lifecycle.force_shutdown.cancelled().fuse();
            pin_mut!(ready, canceled);
            select_biased! {
                _ = canceled => return Err(Error::Canceled),
                result = ready => {
                    if let Err(failure) = result {
                        return Err(failure.into_connection_error());
                    }
                },
            }
            prefer_body = true;
            continue;
        }

        let waiting_for_request_demand = match connection.protocol() {
            Some(HttpProtocol::Http1) => {
                !request_streams.is_empty()
                    && !request_streams.values().any(|control| control.demanded)
            }
            Some(HttpProtocol::Http2) => {
                request_streams
                    .values()
                    .any(|control| !control.sender.is_empty())
                    && !request_streams.values().any(|control| control.demanded)
            }
            Some(_) | None => false,
        };
        if waiting_for_request_demand {
            if request_body_ready.is_set() {
                continue;
            }
            let completed = handlers.next().fuse();
            let ready = request_body_ready.wait().fuse();
            let canceled = lifecycle.force_shutdown.cancelled().fuse();
            pin_mut!(completed, ready, canceled);
            select_biased! {
                completion = completed => {
                    handle_handler_completion(
                        completion.expect("a pending request chunk belongs to a handler"),
                        &mut request_streams,
                        &mut handler_cancellations,
                        exchanges,
                        &mut responses,
                        &mut header_only_responses,
                        connection,
                    )?;
                }
                _ = ready => {}
                _ = canceled => return Err(Error::Canceled),
            }
            continue;
        }

        let wire_protocol = connection.protocol();
        let action = match connection
            .step(input.available(), |step| -> Result<PumpAction> {
                match step {
                    Step::NeedInput => Ok(PumpAction::NeedInput),
                    Step::Write(bytes) => Ok(PumpAction::Write(header_only_exchange_for_write(
                        bytes,
                        &header_only_responses,
                    ))),
                    Step::Done => Ok(PumpAction::Done),
                    Step::Event(ServerEvent::RequestHead {
                        exchange_id,
                        method,
                        target,
                        authority,
                        version,
                        headers,
                        content_length,
                        expectation,
                    }) => {
                        if exchanges.contains(&exchange_id) {
                            return Err(protocol(
                                ProtocolErrorKind::UnexpectedEvent,
                                "request head repeated an active exchange",
                            ));
                        }
                        let request = request_parts(
                            method,
                            target,
                            authority,
                            version,
                            headers,
                            content_length,
                        )?;
                        requests.insert(exchange_id, request);
                        Ok(PumpAction::RequestHead {
                            exchange_id,
                            expectation,
                        })
                    }
                    Step::Event(ServerEvent::RequestBody { exchange_id, chunk }) => {
                        let parts = requests.get_mut(&exchange_id).ok_or_else(|| {
                            protocol(
                                ProtocolErrorKind::UnexpectedEvent,
                                "request body preceded its head or followed its completion",
                            )
                        })?;
                        if let Some(control) = request_streams.get_mut(&exchange_id) {
                            if !control.demanded && wire_protocol != Some(HttpProtocol::Http2) {
                                return Err(protocol(
                                    ProtocolErrorKind::InvalidState,
                                    "a request body chunk arrived without consumer demand",
                                ));
                            }
                            match control.sender.try_send(chunk.to_vec()) {
                                Ok(()) => Ok(PumpAction::StreamedBody(exchange_id)),
                                Err(crate::async_channel::SendError::ChannelClosed(_)) => {
                                    Ok(PumpAction::RequestAbandoned(exchange_id))
                                }
                                // The consumer has not taken the previous chunk
                                // and the peer sent more anyway, which it is
                                // entitled to do: our advertised flow-control
                                // window is far larger than this one-slot
                                // handoff, and the adapter cannot withhold a
                                // per-stream WINDOW_UPDATE to say otherwise.
                                //
                                // Retire this exchange only. Failing the
                                // connection would destroy every other stream
                                // multiplexed on it, which a peer can provoke
                                // deliberately by pausing one consumer while a
                                // second stream keeps the pump running. Reading
                                // less is not an option either, since that
                                // head-of-line blocks the healthy streams.
                                // SEC-008 tracks gating the window on demand,
                                // which is the fix that would stop a slow
                                // consumer reaching here at all.
                                Err(crate::async_channel::SendError::ChannelFull(_)) => {
                                    Ok(PumpAction::RequestOverrun(exchange_id))
                                }
                            }
                        } else {
                            parts.body.extend_from_slice(chunk);
                            Ok(PumpAction::Progress)
                        }
                    }
                    Step::Event(ServerEvent::RequestTrailers {
                        exchange_id,
                        headers,
                    }) => {
                        let parts = requests.get_mut(&exchange_id).ok_or_else(|| {
                            protocol(
                                ProtocolErrorKind::UnexpectedEvent,
                                "request trailers preceded the head or followed completion",
                            )
                        })?;
                        if parts.trailers.is_some() {
                            return Err(protocol(
                                ProtocolErrorKind::UnexpectedEvent,
                                "request trailers repeated for one exchange",
                            ));
                        }
                        let headers = convert_headers(headers)?;
                        if let Some(control) = request_streams.get(&exchange_id) {
                            control.trailers.set(headers.clone()).map_err(|_| {
                                protocol(
                                    ProtocolErrorKind::UnexpectedEvent,
                                    "request trailers repeated for one streaming body",
                                )
                            })?;
                        }
                        parts.trailers = Some(headers);
                        Ok(PumpAction::Progress)
                    }
                    Step::Event(ServerEvent::RequestComplete { exchange_id }) => {
                        if !requests.contains_key(&exchange_id) {
                            return Err(protocol(
                                ProtocolErrorKind::UnexpectedEvent,
                                "request completion had no in-progress exchange",
                            ));
                        }
                        Ok(PumpAction::RequestComplete(exchange_id))
                    }
                    _ => Err(protocol(
                        ProtocolErrorKind::UnexpectedEvent,
                        "the connection driver returned an unexpected server step",
                    )),
                }
            })
            .into_http()
        {
            Ok(action) => action?,
            Err(Error::Protocol(error)) if error.kind() == ProtocolErrorKind::PeerReset => {
                let consumed = connection.consumed();
                let Some(exchange_id) = connection.cancel_exchange().into_http()? else {
                    return Err(Error::Protocol(error));
                };
                connection.consume(consumed).into_http()?;
                input.consume(consumed);
                requests.remove(&exchange_id);
                request_streams.remove(&exchange_id);
                responses.remove(&exchange_id);
                header_only_responses.remove(&exchange_id);
                completed_responses.remove(&exchange_id);
                exchanges.remove(&exchange_id);
                if let Some(cancellation) = handler_cancellations.remove(&exchange_id) {
                    cancellation.cancel();
                }
                if !connection.begin_next_exchange(exchange_id).into_http()? {
                    return Ok(());
                }
                prefer_body = true;
                continue;
            }
            Err(error) => return Err(error),
        };
        if let Some(handled) = connection.take_handled_stream_error() {
            let exchange_id = handled.exchange_id();
            requests.remove(&exchange_id);
            request_streams.remove(&exchange_id);
            responses.remove(&exchange_id);
            header_only_responses.remove(&exchange_id);
            completed_responses.remove(&exchange_id);
            exchanges.remove(&exchange_id);
            if let Some(cancellation) = handler_cancellations.remove(&exchange_id) {
                cancellation.cancel();
            }
        }
        let consumed = connection.consumed();

        match action {
            PumpAction::Write(completed_header_only) => {
                let bytes = connection.pending_write().ok_or_else(|| {
                    protocol(
                        ProtocolErrorKind::InvalidState,
                        "the connection driver did not retain pending output",
                    )
                })?;
                lifecycle.write(stream, bytes).await?;
                connection.consume(consumed).into_http()?;
                input.consume(consumed);
                if let Some(exchange_id) = completed_header_only {
                    header_only_responses.remove(&exchange_id);
                    completed_responses.insert(exchange_id);
                }
                prefer_body = true;
            }
            PumpAction::RequestHead {
                exchange_id,
                expectation,
            } => {
                connection.consume(consumed).into_http()?;
                input.consume(consumed);
                exchanges.insert(exchange_id);

                if expectation == RequestExpectation::Continue
                    && let Some(expect_continue) = expect_continue.as_deref()
                {
                    let head = requests
                        .get(&exchange_id)
                        .expect("the request head was just inserted")
                        .head_request();
                    let decision = catch_unwind(AssertUnwindSafe(|| expect_continue(&head)));
                    let decision = match decision {
                        Ok(decision) => decision,
                        Err(payload) => {
                            let _ = lifecycle
                                .reporter
                                .send(ServeError::HandlerPanicked(payload));
                            let mut response = Response::new(Body::empty());
                            *response.status_mut() = ::http::StatusCode::INTERNAL_SERVER_ERROR;
                            ExpectContinueDecision::Reject(response)
                        }
                    };
                    match decision {
                        ExpectContinueDecision::Continue => {
                            connection.prepare_continue(exchange_id).into_http()?;
                        }
                        ExpectContinueDecision::Reject(response) => {
                            if response.status().is_informational() {
                                return Err(protocol(
                                    ProtocolErrorKind::InvalidState,
                                    "the Expect: 100-continue hook returned an informational response",
                                ));
                            }
                            requests.remove(&exchange_id);
                            prepare_application_response(
                                exchange_id,
                                response,
                                &mut responses,
                                &mut header_only_responses,
                                connection,
                            )?;
                        }
                    }
                }
                if request_mode == RequestMode::Streaming
                    && requests.contains_key(&exchange_id)
                    && !responses.contains_key(&exchange_id)
                {
                    let (sender, receiver) = crate::async_channel();
                    let ready = Rc::clone(&request_body_ready);
                    let demands = request_demands.clone();
                    let overrun = Rc::new(Cell::new(false));
                    let reported_overrun = Rc::clone(&overrun);
                    let trailers = InboundTrailers::streaming();
                    let request = requests
                        .get(&exchange_id)
                        .expect("the request head was just inserted")
                        .request_with_body(Body::from_inbound_stream(
                            futures::stream::try_unfold(
                                (receiver, ready, demands, exchange_id, reported_overrun),
                                |(receiver, ready, demands, exchange_id, overrun)| async move {
                                    demands
                                        .send(exchange_id)
                                        .expect("the request demand receiver remains live");
                                    ready.set();
                                    match receiver.recv().await {
                                        Ok(chunk) => Ok(Some((
                                            chunk,
                                            (receiver, ready, demands, exchange_id, overrun),
                                        ))),
                                        // The pump retires an exchange by dropping
                                        // this sender, which is also how a complete
                                        // request ends. Only the overrun flag tells
                                        // the two apart, so report the truncation
                                        // rather than let a partial body pass for a
                                        // whole one.
                                        Err(_) if overrun.get() => Err(protocol(
                                            ProtocolErrorKind::InvalidState,
                                            "the request body outran this consumer and was truncated",
                                        )),
                                        Err(_) => Ok(None),
                                    }
                                },
                            ),
                            trailers.clone(),
                        ));
                    let cancellation = Rc::new(CancellationToken::new());
                    handler_cancellations.insert(exchange_id, Rc::clone(&cancellation));
                    handlers.push(run_exchange_handler(
                        exchange_id,
                        Rc::clone(&handler),
                        request,
                        lifecycle.clone(),
                        cancellation,
                        connection.protocol() == Some(HttpProtocol::Http2),
                    ));
                    request_streams.insert(
                        exchange_id,
                        RequestStreamControl {
                            sender,
                            demanded: false,
                            chunk_pending: false,
                            overrun,
                            trailers,
                        },
                    );
                }
                prefer_body = true;
            }
            PumpAction::Progress => {
                connection.consume(consumed).into_http()?;
                input.consume(consumed);
                prefer_body = true;
            }
            PumpAction::StreamedBody(exchange_id) => {
                connection.consume(consumed).into_http()?;
                input.consume(consumed);
                request_streams
                    .get_mut(&exchange_id)
                    .expect("a streamed request body retains its control")
                    .commit_chunk();
                prefer_body = true;
            }
            PumpAction::RequestAbandoned(exchange_id) => {
                connection.consume(consumed).into_http()?;
                input.consume(consumed);
                request_streams.remove(&exchange_id);
                requests.remove(&exchange_id);
                connection.abandon_request(exchange_id).into_http()?;
                prefer_body = true;
            }
            PumpAction::RequestOverrun(exchange_id) => {
                connection.consume(consumed).into_http()?;
                input.consume(consumed);
                if let Some(control) = request_streams.remove(&exchange_id) {
                    // Raise the flag before the sender drops at the end of this
                    // block, so the consumer reads a truncation error instead of
                    // a clean end of body.
                    control.overrun.set(true);
                }
                // `abandon_request` suppresses this exchange's remaining body
                // and completion events, so nothing will ever remove the
                // retained head. Drop it here or a peer could pin one per
                // stalled stream.
                requests.remove(&exchange_id);
                connection.abandon_request(exchange_id).into_http()?;
                prefer_body = true;
            }
            PumpAction::RequestComplete(exchange_id) => {
                connection.consume(consumed).into_http()?;
                input.consume(consumed);
                let request = requests.remove(&exchange_id).ok_or_else(|| {
                    protocol(
                        ProtocolErrorKind::UnexpectedEvent,
                        "completed request had no assembled exchange",
                    )
                })?;
                if request_mode == RequestMode::Streaming {
                    request_streams.remove(&exchange_id);
                } else {
                    let cancellation = Rc::new(CancellationToken::new());
                    handler_cancellations.insert(exchange_id, Rc::clone(&cancellation));
                    handlers.push(run_exchange_handler(
                        exchange_id,
                        Rc::clone(&handler),
                        request.into_request(),
                        lifecycle.clone(),
                        cancellation,
                        connection.protocol() == Some(HttpProtocol::Http2),
                    ));
                }
                prefer_body = true;
            }
            PumpAction::Done => {
                header_only_responses.clear();
                completed_responses.clear();
                let completed = std::mem::take(exchanges);
                for exchange_id in completed {
                    requests.remove(&exchange_id);
                    request_streams.remove(&exchange_id);
                    responses.remove(&exchange_id);
                    if let Some(cancellation) = handler_cancellations.remove(&exchange_id) {
                        cancellation.cancel();
                    }
                    if !connection.begin_next_exchange(exchange_id).into_http()? {
                        return Ok(());
                    }
                }
                prefer_body = false;
            }
            PumpAction::NeedInput => {
                connection.consume(consumed).into_http()?;
                input.consume(consumed);
                if consumed != 0 {
                    prefer_body = true;
                    continue;
                }
                if !attempted_body && !responses.is_empty() {
                    prefer_body = true;
                    continue;
                }
                if input_state == InputState::PeerClosed
                    && (!requests.is_empty() || !input.available().is_empty())
                {
                    return Err(Error::UnexpectedEof);
                }
                if input_state != InputState::Open {
                    if !handlers.is_empty() {
                        let completion = handlers
                            .next()
                            .await
                            .expect("a non-empty handler set yields a completion");
                        handle_handler_completion(
                            completion,
                            &mut request_streams,
                            &mut handler_cancellations,
                            exchanges,
                            &mut responses,
                            &mut header_only_responses,
                            connection,
                        )?;
                        continue;
                    }
                    if responses.is_empty() {
                        return Ok(());
                    }
                    return Err(match input_state {
                        InputState::PeerClosed => Error::UnexpectedEof,
                        InputState::Shutdown => Error::Canceled,
                        InputState::Open => unreachable!("closed input was already checked"),
                    });
                }

                let handlers_empty = handlers.is_empty();
                let body_source_pending = responses.values().any(ResponseBody::needs_chunk);
                let event = {
                    let read = lifecycle.read(input, stream).fuse();
                    let completed = async {
                        if handlers_empty {
                            futures::future::pending::<HandlerCompletion>().await
                        } else {
                            handlers
                                .next()
                                .await
                                .expect("a non-empty handler set yields a completion")
                        }
                    }
                    .fuse();
                    let body = async {
                        if body_source_pending {
                            wait_for_response_body_source(&mut responses).await
                        } else {
                            futures::future::pending::<std::result::Result<(), ResponseFailure>>()
                                .await
                        }
                    }
                    .fuse();
                    pin_mut!(read, completed, body);
                    select_biased! {
                        completed = completed => PumpEvent::Handler(completed),
                        result = body => PumpEvent::Body(result),
                        input = read => PumpEvent::Input(input),
                    }
                };
                match event {
                    PumpEvent::Input(Ok(0)) => input_state = InputState::PeerClosed,
                    PumpEvent::Input(Ok(_)) => {}
                    PumpEvent::Input(Err(Error::Canceled)) => input_state = InputState::Shutdown,
                    PumpEvent::Input(Err(error)) => return Err(error),
                    PumpEvent::Body(result) => {
                        if let Err(failure) = result {
                            handle_response_failure(
                                failure,
                                lifecycle,
                                &mut responses,
                                &mut completed_responses,
                                connection,
                            )?;
                        }
                        prefer_body = true;
                    }
                    PumpEvent::Handler(completion) => {
                        handle_handler_completion(
                            completion,
                            &mut request_streams,
                            &mut handler_cancellations,
                            exchanges,
                            &mut responses,
                            &mut header_only_responses,
                            connection,
                        )?;
                    }
                }
            }
        }
    }
}

fn retire_completed_responses(
    completed_responses: &mut HashSet<ExchangeId>,
    requests: &mut HashMap<ExchangeId, RequestParts>,
    request_streams: &mut HashMap<ExchangeId, RequestStreamControl>,
    handler_cancellations: &mut HashMap<ExchangeId, Rc<CancellationToken>>,
    exchanges: &mut HashSet<ExchangeId>,
    connection: &mut ServerConnection,
) -> Result<bool> {
    let ready = completed_responses
        .iter()
        .copied()
        .filter(|exchange_id| connection.exchange_is_finished(*exchange_id))
        .collect::<Vec<_>>();
    for exchange_id in ready {
        completed_responses.remove(&exchange_id);
        requests.remove(&exchange_id);
        request_streams.remove(&exchange_id);
        if let Some(cancellation) = handler_cancellations.remove(&exchange_id) {
            cancellation.cancel();
        }
        exchanges.remove(&exchange_id);
        if !connection.begin_next_exchange(exchange_id).into_http()? {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn run_exchange_handler<H, F>(
    exchange_id: ExchangeId,
    handler: Rc<H>,
    request: Request<Body>,
    lifecycle: ConnectionLifecycle,
    cancellation: Rc<CancellationToken>,
    isolate_panics: bool,
) -> HandlerCompletion
where
    H: Fn(Request<Body>) -> F,
    F: Future<Output = Response<Body>>,
{
    if isolate_panics {
        return run_isolated_exchange_handler(
            exchange_id,
            handler,
            request,
            lifecycle,
            cancellation,
        )
        .await;
    }

    let response = lifecycle.handler(handler(request)).fuse();
    let cancelled = cancellation.cancelled().fuse();
    pin_mut!(response, cancelled);
    select_biased! {
        _ = cancelled => HandlerCompletion::Cancelled(exchange_id),
        response = response => HandlerCompletion::Response(exchange_id, response),
    }
}

async fn run_isolated_exchange_handler<H, F>(
    exchange_id: ExchangeId,
    handler: Rc<H>,
    request: Request<Body>,
    lifecycle: ConnectionLifecycle,
    cancellation: Rc<CancellationToken>,
) -> HandlerCompletion
where
    H: Fn(Request<Body>) -> F,
    F: Future<Output = Response<Body>>,
{
    // Server-owned state passed into this caller-supplied handler belongs to
    // one exchange; an unwind drops it before a fresh failure response.
    let reporter = lifecycle.reporter.clone();
    let response = AssertUnwindSafe(async move { lifecycle.handler(handler(request)).await })
        .catch_unwind()
        .fuse();
    let cancelled = cancellation.cancelled().fuse();
    pin_mut!(response, cancelled);
    select_biased! {
        _ = cancelled => HandlerCompletion::Cancelled(exchange_id),
        response = response => match response {
            Ok(response) => HandlerCompletion::Response(exchange_id, response),
            Err(payload) => {
                let _ = reporter.send(ServeError::HandlerPanicked(payload));
                HandlerCompletion::Panicked(exchange_id)
            }
        },
    }
}

fn handle_handler_completion(
    completion: HandlerCompletion,
    request_streams: &mut HashMap<ExchangeId, RequestStreamControl>,
    handler_cancellations: &mut HashMap<ExchangeId, Rc<CancellationToken>>,
    exchanges: &HashSet<ExchangeId>,
    responses: &mut HashMap<ExchangeId, ResponseBody>,
    header_only_responses: &mut HashSet<ExchangeId>,
    connection: &mut ServerConnection,
) -> Result<()> {
    let exchange_id = completion.exchange_id();
    if request_streams.remove(&exchange_id).is_some() {
        connection.abandon_request(exchange_id).into_http()?;
    }
    handler_cancellations.remove(&exchange_id);
    if matches!(completion, HandlerCompletion::Cancelled(_)) {
        return Ok(());
    }
    if !exchanges.contains(&exchange_id) {
        return Ok(());
    }
    let response = match completion {
        HandlerCompletion::Response(_, response) => response?,
        HandlerCompletion::Panicked(_) => {
            let mut response = Response::new(Body::empty());
            *response.status_mut() = ::http::StatusCode::INTERNAL_SERVER_ERROR;
            response
        }
        HandlerCompletion::Cancelled(_) => unreachable!("cancellation returned above"),
    };
    prepare_application_response(
        exchange_id,
        response,
        responses,
        header_only_responses,
        connection,
    )
}

fn prepare_application_response(
    exchange_id: ExchangeId,
    response: Response<Body>,
    responses: &mut HashMap<ExchangeId, ResponseBody>,
    header_only_responses: &mut HashSet<ExchangeId>,
    connection: &mut ServerConnection,
) -> Result<()> {
    let has_trailers = response.body().has_outbound_trailers();
    let send_body = {
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| HeaderRef::new(name.as_str().as_bytes(), value.as_bytes()))
            .collect::<Vec<_>>();
        connection
            .prepare_response(
                exchange_id,
                ConnectionResponse {
                    status: response.status().as_u16(),
                    reason: response.status().canonical_reason().unwrap_or(""),
                    headers: &headers,
                    // RFC 9112 section 7.1.2 permits HTTP/1 trailers only with
                    // chunked framing. Treat even a buffered payload as a
                    // stream when trailers are attached, which also leaves an
                    // explicit terminal write where the callback can run.
                    body_len: if has_trailers {
                        None
                    } else {
                        response.body().known_len()
                    },
                },
            )
            .into_http()?
    };
    if send_body {
        responses.insert(
            exchange_id,
            ResponseBody::new(response.into_body(), has_trailers)?,
        );
    } else if has_trailers {
        return Err(protocol(
            ProtocolErrorKind::InvalidState,
            "response trailers cannot be sent on a bodyless response",
        ));
    } else if connection.protocol() == Some(HttpProtocol::Http2) {
        header_only_responses.insert(exchange_id);
    }
    Ok(())
}

fn header_only_exchange_for_write(
    bytes: &[u8],
    header_only_responses: &HashSet<ExchangeId>,
) -> Option<ExchangeId> {
    // The driver does not expose the exchange attached to a write. Decode only
    // the borrowed frame prefix so the acknowledged HEADERS can be retired
    // without copying its header block.
    let (frame, _) = H2FrameRef::decode(bytes).ok()?;
    if frame.frame_type != H2FrameType::Headers {
        return None;
    }
    header_only_responses
        .iter()
        .copied()
        .find(|exchange_id| exchange_id.as_u64() == u64::from(frame.stream_id))
}

enum ResponseFailure {
    Body {
        exchange_id: ExchangeId,
        error: Error,
    },
    Trailers {
        exchange_id: ExchangeId,
        error: Error,
    },
    TrailerCallbackPanicked {
        exchange_id: ExchangeId,
        payload: Box<dyn Any + Send + 'static>,
    },
}

impl ResponseFailure {
    const fn exchange_id(&self) -> ExchangeId {
        match self {
            Self::Body { exchange_id, .. }
            | Self::Trailers { exchange_id, .. }
            | Self::TrailerCallbackPanicked { exchange_id, .. } => *exchange_id,
        }
    }

    fn into_connection_error(self) -> Error {
        match self {
            Self::Body { error, .. } | Self::Trailers { error, .. } => error,
            Self::TrailerCallbackPanicked { payload, .. } => protocol(
                ProtocolErrorKind::InvalidState,
                format!(
                    "response trailer callback panicked: {}",
                    panic_message(payload.as_ref())
                ),
            ),
        }
    }
}

enum ResponseWriteFailure {
    Connection(Error),
    Exchange(ResponseFailure),
}

fn poll_response_body_sources(
    responses: &mut HashMap<ExchangeId, ResponseBody>,
    context: &mut Context<'_>,
) -> Poll<std::result::Result<(), ResponseFailure>> {
    let mut made_progress = false;
    let mut waiting = false;
    for (&exchange_id, response) in responses.iter_mut() {
        if !response.needs_chunk() {
            continue;
        }
        match response.poll_chunk(context) {
            Poll::Ready(Ok(())) => made_progress = true,
            Poll::Ready(Err(error)) => {
                return Poll::Ready(Err(ResponseFailure::Body { exchange_id, error }));
            }
            Poll::Pending => waiting = true,
        }
    }
    if made_progress || !waiting {
        Poll::Ready(Ok(()))
    } else {
        Poll::Pending
    }
}

async fn poll_response_body_sources_once(
    responses: &mut HashMap<ExchangeId, ResponseBody>,
) -> std::result::Result<(), ResponseFailure> {
    let result = std::future::poll_fn(|context| {
        Poll::Ready(match poll_response_body_sources(responses, context) {
            Poll::Ready(result) => Some(result),
            Poll::Pending => None,
        })
    })
    .await;
    match result {
        Some(result) => result,
        None => Ok(()),
    }
}

async fn wait_for_response_body_source(
    responses: &mut HashMap<ExchangeId, ResponseBody>,
) -> std::result::Result<(), ResponseFailure> {
    std::future::poll_fn(|context| poll_response_body_sources(responses, context)).await
}

fn handle_response_failure(
    failure: ResponseFailure,
    lifecycle: &ConnectionLifecycle,
    responses: &mut HashMap<ExchangeId, ResponseBody>,
    completed_responses: &mut HashSet<ExchangeId>,
    connection: &mut ServerConnection,
) -> Result<()> {
    let exchange_id = failure.exchange_id();
    if connection.protocol() != Some(HttpProtocol::Http2)
        || !connection.abandon_response(exchange_id).into_http()?
    {
        return Err(failure.into_connection_error());
    }
    responses.remove(&exchange_id);
    completed_responses.insert(exchange_id);
    let report = match failure {
        ResponseFailure::Body { error, .. } => ServeError::ResponseBody(error),
        ResponseFailure::Trailers { error, .. } => ServeError::ResponseTrailers(error),
        ResponseFailure::TrailerCallbackPanicked { payload, .. } => {
            ServeError::HandlerPanicked(payload)
        }
    };
    let _ = lifecycle.reporter.send(report);
    Ok(())
}

async fn write_response_body(
    stream: &mut Transport,
    lifecycle: &ConnectionLifecycle,
    connection: &mut ServerConnection,
    responses: &mut HashMap<ExchangeId, ResponseBody>,
) -> std::result::Result<BodyAction, ResponseWriteFailure> {
    let mut selected = None;
    // A false result may mean a sibling owns the fair scheduler turn.
    for (&exchange_id, response) in responses.iter_mut() {
        let Some(available_len) = response.available().map(<[u8]>::len) else {
            continue;
        };
        if available_len == 0
            && let Some(callback) = response.trailers.take()
        {
            let headers = catch_unwind(AssertUnwindSafe(callback)).map_err(|payload| {
                ResponseWriteFailure::Exchange(ResponseFailure::TrailerCallbackPanicked {
                    exchange_id,
                    payload,
                })
            })?;
            let fields = headers
                .iter()
                .map(|(name, value)| HeaderRef::new(name.as_str().as_bytes(), value.as_bytes()))
                .collect::<Vec<_>>();
            connection
                .set_response_trailers(exchange_id, &fields)
                .into_http()
                .map_err(|error| {
                    ResponseWriteFailure::Exchange(ResponseFailure::Trailers { exchange_id, error })
                })?;
        }
        if connection
            .prepare_body_chunk(exchange_id, available_len)
            .into_http()
            .map_err(ResponseWriteFailure::Connection)?
        {
            selected = Some(exchange_id);
            break;
        }
    }
    let Some(exchange_id) = selected else {
        return Ok(BodyAction::Blocked);
    };

    let payload_len = {
        let response = responses
            .get(&exchange_id)
            .expect("a selected response body remains available");
        let available = response
            .available()
            .expect("a selected response has bytes or a termination marker");
        let chunk = connection
            .body_chunk(exchange_id)
            .expect("a prepared body chunk remains available");
        let payload_len = chunk.payload_len();
        let mut slices = [
            IoSlice::new(chunk.header()),
            IoSlice::new(&available[..payload_len]),
            IoSlice::new(chunk.footer()),
        ];
        lifecycle
            .writev(stream, &mut slices)
            .await
            .map_err(ResponseWriteFailure::Connection)?;
        payload_len
    };
    connection
        .commit_body_chunk(exchange_id)
        .into_http()
        .map_err(ResponseWriteFailure::Connection)?;
    let response = responses
        .get_mut(&exchange_id)
        .expect("a committed response body remains available");
    response.commit(payload_len);
    if response.is_complete() {
        responses.remove(&exchange_id);
        Ok(BodyAction::Complete(exchange_id))
    } else {
        Ok(BodyAction::Sent)
    }
}

struct ResponseBody {
    inner: ResponseBodyInner,
    trailers: Option<BodyTrailersFn>,
}

enum ResponseBodyInner {
    Buffered {
        bytes: Vec<u8>,
        sent: usize,
        streaming_framing: bool,
        termination_sent: bool,
    },
    Streaming(BodyStreamCursor),
}

impl ResponseBody {
    fn new(mut body: Body, streaming_framing: bool) -> Result<Self> {
        let trailers = body.take_outbound_trailers().map_err(|()| {
            protocol(
                ProtocolErrorKind::InvalidState,
                "the response trailer callback was already consumed by a cloned body",
            )
        })?;
        let inner = match body.inner {
            BodyInner::Buffered(bytes) => ResponseBodyInner::Buffered {
                bytes,
                sent: 0,
                streaming_framing,
                termination_sent: false,
            },
            BodyInner::Streaming(source) => {
                ResponseBodyInner::Streaming(BodyStreamCursor::new(source))
            }
        };
        Ok(Self { inner, trailers })
    }

    fn available(&self) -> Option<&[u8]> {
        match &self.inner {
            ResponseBodyInner::Buffered {
                bytes,
                sent,
                streaming_framing,
                termination_sent,
            } => {
                if *sent < bytes.len() {
                    Some(&bytes[*sent..])
                } else if *streaming_framing && !*termination_sent {
                    Some(&[])
                } else {
                    None
                }
            }
            ResponseBodyInner::Streaming(cursor) => cursor.available(),
        }
    }

    fn needs_chunk(&self) -> bool {
        matches!(&self.inner, ResponseBodyInner::Streaming(cursor) if cursor.needs_chunk())
    }

    fn poll_chunk(&mut self, context: &mut Context<'_>) -> Poll<Result<()>> {
        match &mut self.inner {
            ResponseBodyInner::Buffered { .. } => Poll::Ready(Ok(())),
            ResponseBodyInner::Streaming(cursor) => match cursor.poll_chunk(context) {
                Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
                Poll::Ready(Err(source)) => Poll::Ready(Err(Error::BodyStream { source })),
                Poll::Pending => Poll::Pending,
            },
        }
    }

    fn commit(&mut self, payload_len: usize) {
        match &mut self.inner {
            ResponseBodyInner::Buffered {
                bytes,
                sent,
                streaming_framing,
                termination_sent,
            } => {
                if payload_len == 0 && *streaming_framing && *sent == bytes.len() {
                    *termination_sent = true;
                } else {
                    assert!(payload_len <= bytes.len() - *sent);
                    *sent += payload_len;
                }
            }
            ResponseBodyInner::Streaming(cursor) => cursor.commit(payload_len),
        }
    }

    fn is_complete(&self) -> bool {
        match &self.inner {
            ResponseBodyInner::Buffered {
                bytes,
                sent,
                streaming_framing,
                termination_sent,
            } => {
                if *streaming_framing {
                    *termination_sent
                } else {
                    *sent == bytes.len()
                }
            }
            ResponseBodyInner::Streaming(cursor) => cursor.is_complete(),
        }
    }
}

enum HandlerCompletion {
    Response(ExchangeId, Result<Response<Body>>),
    Panicked(ExchangeId),
    Cancelled(ExchangeId),
}

impl HandlerCompletion {
    const fn exchange_id(&self) -> ExchangeId {
        match self {
            Self::Response(exchange_id, _)
            | Self::Panicked(exchange_id)
            | Self::Cancelled(exchange_id) => *exchange_id,
        }
    }
}

enum PumpEvent {
    Input(Result<usize>),
    Body(std::result::Result<(), ResponseFailure>),
    Handler(HandlerCompletion),
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum InputState {
    Open,
    PeerClosed,
    Shutdown,
}

enum BodyAction {
    Blocked,
    Sent,
    Complete(ExchangeId),
}

#[cfg(feature = "tls")]
async fn server_transport(
    socket: OwnedFd,
    tls: Option<super::TlsServerConfig>,
    limits: Limits,
    lifecycle: &ConnectionLifecycle,
) -> Result<Transport> {
    let Some(tls) = tls else {
        return Ok(Transport::Plain(crate::OwnedFdStream::new(socket)));
    };

    let handshake = tls
        .context()
        .server(
            limits.read_buffer_bytes(),
            socket,
            Some(lifecycle.io_deadline()?),
        )
        .fuse();
    let shutdown = lifecycle.shutdown.cancelled().fuse();
    pin_mut!(handshake, shutdown);
    let stream = select_biased! {
        _ = shutdown => return Err(Error::Canceled),
        result = handshake => result?,
    };
    Ok(Transport::Tls(stream))
}

struct RequestParts {
    method: Method,
    uri: Uri,
    version: Version,
    headers: HeaderMap,
    body: Vec<u8>,
    trailers: Option<HeaderMap>,
}

impl RequestParts {
    fn head_request(&self) -> Request<()> {
        let mut request = Request::new(());
        *request.method_mut() = self.method.clone();
        *request.uri_mut() = self.uri.clone();
        *request.version_mut() = self.version;
        *request.headers_mut() = self.headers.clone();
        request
    }

    fn into_request(self) -> Request<Body> {
        let mut request = Request::new(Body::from_inbound(self.body, self.trailers));
        *request.method_mut() = self.method;
        *request.uri_mut() = self.uri;
        *request.version_mut() = self.version;
        *request.headers_mut() = self.headers;
        request
    }

    fn request_with_body(&self, body: Body) -> Request<Body> {
        let mut request = Request::new(body);
        *request.method_mut() = self.method.clone();
        *request.uri_mut() = self.uri.clone();
        *request.version_mut() = self.version;
        *request.headers_mut() = self.headers.clone();
        request
    }
}

struct RequestStreamControl {
    sender: crate::Sender<Vec<u8>>,
    demanded: bool,
    chunk_pending: bool,
    /// Set when the exchange is retired with request bytes still outstanding.
    /// Dropping the sender alone ends the consumer's body stream exactly as a
    /// complete request does, so without this the consumer would accept a
    /// truncated body as the whole request.
    overrun: Rc<Cell<bool>>,
    trailers: InboundTrailers,
}

impl RequestStreamControl {
    fn commit_chunk(&mut self) {
        self.demanded = false;
        self.chunk_pending = true;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PumpAction {
    Progress,
    NeedInput,
    Write(Option<ExchangeId>),
    RequestHead {
        exchange_id: ExchangeId,
        expectation: RequestExpectation,
    },
    StreamedBody(ExchangeId),
    RequestAbandoned(ExchangeId),
    RequestOverrun(ExchangeId),
    RequestComplete(ExchangeId),
    Done,
}

fn request_parts(
    method: &[u8],
    target: &[u8],
    authority: Option<&[u8]>,
    version: HttpVersion,
    fields: HeaderBlock<'_>,
    _content_length: Option<usize>,
) -> Result<RequestParts> {
    let method = Method::from_bytes(method).map_err(|error| {
        protocol(
            ProtocolErrorKind::MalformedMessage,
            format!("invalid HTTP method: {error}"),
        )
    })?;
    let target = std::str::from_utf8(target).map_err(|error| {
        protocol(
            ProtocolErrorKind::InvalidHeader,
            format!("invalid request target: {error}"),
        )
    })?;
    let uri = target.parse::<Uri>()?;
    let version = match version {
        HttpVersion::Http10 => Version::HTTP_10,
        HttpVersion::Http11 => Version::HTTP_11,
        HttpVersion::Http2 => Version::HTTP_2,
        _ => {
            return Err(protocol(
                ProtocolErrorKind::InvalidState,
                "the connection driver returned an unknown HTTP version",
            ));
        }
    };
    let mut headers = HeaderMap::with_capacity(
        fields
            .len()
            .saturating_add(usize::from(authority.is_some())),
    );
    for field in fields.iter() {
        let name = HeaderName::from_bytes(field.name())?;
        let value = HeaderValue::from_bytes(field.value())?;
        headers.append(name, value);
    }
    if let Some(authority) = authority {
        headers.insert(::http::header::HOST, HeaderValue::from_bytes(authority)?);
    }
    Ok(RequestParts {
        method,
        uri,
        version,
        headers,
        // A declared length is untrusted; grow only as body bytes arrive.
        body: Vec::new(),
        trailers: None,
    })
}

fn convert_headers(fields: HeaderBlock<'_>) -> Result<HeaderMap> {
    let mut headers = HeaderMap::with_capacity(fields.len());
    for field in fields.iter() {
        let name = HeaderName::from_bytes(field.name())?;
        let value = HeaderValue::from_bytes(field.value())?;
        headers.append(name, value);
    }
    Ok(headers)
}

fn protocol(kind: ProtocolErrorKind, detail: impl Into<String>) -> Error {
    Error::Protocol(ProtocolError::new(kind, detail))
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpStream};
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

    use super::*;
    use crate::AsyncEvent;

    const TEST_WAIT: Duration = Duration::from_secs(5);

    fn send_http1(address: SocketAddr, target: &str) -> Vec<u8> {
        let mut stream = TcpStream::connect(address).unwrap();
        write!(
            stream,
            "GET {target} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n"
        )
        .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        response
    }

    fn http1_response_len(bytes: &[u8]) -> Option<usize> {
        let head_end = bytes.windows(4).position(|window| window == b"\r\n\r\n")? + 4;
        let head = std::str::from_utf8(&bytes[..head_end]).ok()?;
        let content_length = head
            .split("\r\n")
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        let total = head_end.checked_add(content_length)?;
        (bytes.len() >= total).then_some(total)
    }

    fn parse_http1_responses(mut bytes: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut responses = Vec::new();
        while !bytes.is_empty() {
            let length = http1_response_len(bytes).expect("complete HTTP/1 response");
            let response = &bytes[..length];
            let head_end = response
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .unwrap()
                + 4;
            responses.push((response[..head_end].to_vec(), response[head_end..].to_vec()));
            bytes = &bytes[length..];
        }
        responses
    }

    fn read_one_http1_response(stream: &mut TcpStream) -> Vec<u8> {
        let mut response = Vec::new();
        loop {
            if let Some(length) = http1_response_len(&response) {
                response.truncate(length);
                return response;
            }
            let mut chunk = [0u8; 1024];
            let read = stream.read(&mut chunk).unwrap();
            assert_ne!(read, 0, "connection closed before the response completed");
            response.extend_from_slice(&chunk[..read]);
        }
    }

    #[test]
    fn builder_keeps_explicit_address_and_configuration() {
        let address = "127.0.0.1:0".parse().unwrap();
        let limits = Limits::new().set_max_body_bytes(123);
        let builder = Server::builder(address)
            .limits(limits)
            .listen_backlog(7)
            .connection_io_timeout(Duration::from_secs(4))
            .graceful_shutdown_timeout(Duration::from_secs(5))
            .max_connections(6);
        assert_eq!(builder.address(), address);
        assert_eq!(builder.config.limits(), limits);
        assert_eq!(builder.config.listen_backlog(), 7);
        assert_eq!(
            builder.config.connection_io_timeout(),
            Duration::from_secs(4)
        );
        assert_eq!(
            builder.config.graceful_shutdown_timeout(),
            Duration::from_secs(5)
        );
        assert_eq!(builder.config.max_connections(), 6);
    }

    #[test]
    fn accept_error_classification_is_bounded_to_recoverable_errors() {
        for raw in [
            libc::EINTR,
            libc::ECONNABORTED,
            libc::EMFILE,
            libc::ENFILE,
            libc::ENOBUFS,
            libc::ENOMEM,
            libc::EAGAIN,
        ] {
            assert!(is_transient_accept_error(Errno::from_raw_os_error(raw)));
        }
        for raw in [libc::EBADF, libc::EINVAL, libc::ENOTSOCK] {
            assert!(!is_transient_accept_error(Errno::from_raw_os_error(raw)));
        }
    }

    #[test]
    fn declared_content_length_does_not_reserve_request_body_capacity() {
        use kimojio_fsm_http::{H2Client, H2HeaderField};

        let limits = Limits::new();
        let declared = limits.max_body_bytes();
        let declared_text = declared.to_string();
        let headers = [H2HeaderField::new(
            b"content-length",
            declared_text.as_bytes(),
        )];
        let mut client = H2Client::default();
        let mut input = client.connection_preface();
        let (_, commit) = client
            .open_stream_with_raw_headers("POST", "http", "example.test", "/held", &headers, false)
            .unwrap();
        let block = client.next_outbound_block().unwrap();
        assert_eq!(block.commit(), commit);
        input.extend_from_slice(block.bytes());
        client.acknowledge_outbound_block(commit).unwrap();

        let mut connection =
            ServerConnection::new_with_protocol(HttpProtocol::Http2, limits.protocol());
        let mut offset = 0;
        let mut observed_capacity = None;
        for _ in 0..16 {
            let capacity = connection
                .step(&input[offset..], |step| match step {
                    Step::Event(ServerEvent::RequestHead {
                        method,
                        target,
                        authority,
                        version,
                        headers,
                        content_length,
                        ..
                    }) => {
                        assert_eq!(content_length, Some(declared));
                        Some(
                            request_parts(
                                method,
                                target,
                                authority,
                                version,
                                headers,
                                content_length,
                            )
                            .unwrap()
                            .body
                            .capacity(),
                        )
                    }
                    _ => None,
                })
                .unwrap();
            let consumed = connection.consumed();
            connection.consume(consumed).unwrap();
            offset += consumed;
            if capacity.is_some() {
                observed_capacity = capacity;
                break;
            }
        }

        let capacity = observed_capacity.expect("request head was not decoded");
        assert!(
            capacity <= 16 * 1024,
            "declaring {declared} bytes eagerly reserved {capacity} bytes"
        );
    }

    #[crate::test]
    async fn retires_completed_header_only_exchanges_while_sibling_is_open() {
        use kimojio_fsm_http::{H2ByteClientEvent, H2Client, H2OutboundCommit};

        const QUICK_EXCHANGES: usize = 32;

        fn take_block(client: &mut H2Client, commit: H2OutboundCommit) -> Vec<u8> {
            let block = client.next_outbound_block().unwrap();
            assert_eq!(block.commit(), commit);
            let bytes = block.bytes().to_vec();
            client.acknowledge_outbound_block(commit).unwrap();
            bytes
        }

        let (server_fd, client_fd) = crate::pipe::bipipe();
        let mut client = H2Client::default();
        let mut outbound = client.connection_preface();
        let (held_stream, held_commit) = client
            .open_stream("POST", "http", "example.test", "/held", &[], false)
            .unwrap();
        outbound.extend_from_slice(&take_block(&mut client, held_commit));

        let mut quick_streams = HashSet::new();
        for index in 0..QUICK_EXCHANGES {
            let target = format!("/quick/{index}");
            let (stream_id, commit) = client
                .open_stream("HEAD", "http", "example.test", &target, &[], true)
                .unwrap();
            assert!(quick_streams.insert(stream_id));
            outbound.extend_from_slice(&take_block(&mut client, commit));
        }

        let expected_streams = quick_streams.clone();
        let peer = std::thread::spawn(move || {
            let mut stream = UnixStream::from(client_fd);
            stream.set_read_timeout(Some(TEST_WAIT)).unwrap();
            stream.write_all(&outbound).unwrap();

            let mut pending = Vec::new();
            let mut completed = HashSet::new();
            while completed.len() < QUICK_EXCHANGES {
                let mut bytes = [0u8; 4096];
                let read = stream.read(&mut bytes).unwrap();
                assert_ne!(read, 0);
                pending.extend_from_slice(&bytes[..read]);

                let mut consumed_total = 0;
                loop {
                    let (event, consumed, output) =
                        client.accept_bytes(&pending[consumed_total..]).unwrap();
                    if consumed == 0 {
                        break;
                    }
                    consumed_total += consumed;
                    if !output.is_empty() {
                        stream.write_all(&output).unwrap();
                    }
                    if let Some(H2ByteClientEvent::ResponseHeaders {
                        stream_id, headers, ..
                    }) = event
                        && expected_streams.contains(&stream_id)
                    {
                        assert_eq!(
                            headers
                                .iter()
                                .find(|header| header.name == b":status")
                                .map(|header| header.value.as_slice()),
                            Some(b"204".as_slice())
                        );
                        completed.insert(stream_id);
                    }
                }
                pending.drain(..consumed_total);
            }
            completed
        });

        let limits = Limits::new();
        let shutdown = Rc::new(CancellationToken::new());
        let (reporter, _reports) = crate::async_channel_unbounded();
        let lifecycle = ConnectionLifecycle {
            shutdown: Rc::clone(&shutdown),
            force_shutdown: Rc::new(CancellationToken::new()),
            io_timeout: TEST_WAIT,
            reporter,
        };
        let handled = Rc::new(Cell::new(0usize));
        let handler = Rc::new({
            let handled = Rc::clone(&handled);
            let shutdown = Rc::clone(&shutdown);
            move |request: Request<Body>| {
                assert_eq!(request.method(), Method::HEAD);
                assert!(request.uri().path().starts_with("/quick/"));
                let count = handled.get() + 1;
                handled.set(count);
                if count == QUICK_EXCHANGES {
                    shutdown.cancel();
                }
                async {
                    let mut response = Response::new(Body::empty());
                    *response.status_mut() = ::http::StatusCode::NO_CONTENT;
                    response
                }
            }
        });
        let mut stream = Transport::Plain(crate::OwnedFdStream::new(server_fd));
        let mut input = ReadBuffer::new(limits.read_buffer_bytes()).unwrap();
        let mut connection =
            ServerConnection::new_with_protocol(HttpProtocol::Http2, limits.protocol());
        let mut exchanges = HashSet::new();

        operations::timeout_at(
            crate::clock_now() + TEST_WAIT,
            pump_connection(
                &mut stream,
                &mut input,
                RequestHandling {
                    handler,
                    mode: RequestMode::Buffered,
                    expect_continue: None,
                },
                &lifecycle,
                &mut connection,
                &mut exchanges,
            ),
        )
        .await
        .expect("connection pump did not stop after the header-only responses")
        .unwrap();

        assert_eq!(peer.join().unwrap(), quick_streams);
        assert_eq!(handled.get(), QUICK_EXCHANGES);
        assert_eq!(exchanges.len(), 1);
        assert_eq!(
            exchanges.iter().next().map(|exchange| exchange.as_u64()),
            Some(u64::from(held_stream))
        );
    }

    #[crate::test]
    async fn binds_port_zero_and_serves_one_http1_request() {
        let server = Server::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let address = server.local_addr();
        assert_eq!(
            address.ip(),
            "127.0.0.1".parse::<std::net::IpAddr>().unwrap()
        );
        assert_ne!(address.port(), 0);

        let peer = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream
                .write_all(
                    b"POST /echo HTTP/1.1\r\nhost: localhost\r\ncontent-length: 4\r\n\r\nping",
                )
                .unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).unwrap();
            response
        });

        let cancellation = Rc::new(CancellationToken::new());
        let cancel_from_handler = Rc::clone(&cancellation);
        server
            .serve(
                move |request| {
                    let cancellation = Rc::clone(&cancel_from_handler);
                    async move {
                        assert_eq!(request.method(), Method::POST);
                        assert_eq!(request.uri(), "/echo");
                        assert_eq!(request.body().as_bytes(), b"ping");
                        cancellation.cancel();
                        Response::new(Body::from("pong"))
                    }
                },
                cancellation,
            )
            .await
            .unwrap();

        let response = peer.join().unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert!(response.ends_with(b"\r\n\r\npong"));
    }

    #[crate::test]
    async fn serves_pipelined_http1_requests_without_cross_contamination() {
        let server = Server::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let address = server.local_addr();
        let peer = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream.set_read_timeout(Some(TEST_WAIT)).unwrap();
            stream
                .write_all(
                    b"POST /one HTTP/1.1\r\nhost: localhost\r\ncontent-length: 3\r\n\r\none\
POST /two HTTP/1.1\r\nhost: localhost\r\ncontent-length: 7\r\n\r\ntwo-two\
POST /three HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\ncontent-length: 5\r\n\r\nthree",
                )
                .unwrap();
            let mut responses = Vec::new();
            stream.read_to_end(&mut responses).unwrap();
            responses
        });

        let cancellation = Rc::new(CancellationToken::new());
        let seen = Rc::new(RefCell::new(Vec::new()));
        let observed = Rc::clone(&seen);
        let cancel_from_handler = Rc::clone(&cancellation);
        server
            .serve(
                move |request| {
                    let target = request.uri().path().to_owned();
                    let body = request.body().as_bytes().to_vec();
                    observed.borrow_mut().push((target.clone(), body.clone()));
                    let done = observed.borrow().len() == 3;
                    let cancellation = Rc::clone(&cancel_from_handler);
                    async move {
                        if done {
                            cancellation.cancel();
                        }
                        let mut response = format!("{target}:").into_bytes();
                        response.extend_from_slice(&body);
                        Response::new(Body::from(response))
                    }
                },
                cancellation,
            )
            .await
            .unwrap();

        let wire = peer.join().unwrap();
        let responses = parse_http1_responses(&wire);
        assert_eq!(responses.len(), 3);
        assert_eq!(responses[0].1, b"/one:one");
        assert_eq!(responses[1].1, b"/two:two-two");
        assert_eq!(responses[2].1, b"/three:three");
        assert!(
            !responses[0]
                .0
                .windows(b"connection: close\r\n".len())
                .any(|window| window == b"connection: close\r\n")
        );
        assert!(
            !responses[1]
                .0
                .windows(b"connection: close\r\n".len())
                .any(|window| window == b"connection: close\r\n")
        );
        assert!(
            responses[2]
                .0
                .windows(b"connection: close\r\n".len())
                .any(|window| window == b"connection: close\r\n")
        );
        assert_eq!(
            *seen.borrow(),
            [
                ("/one".to_owned(), b"one".to_vec()),
                ("/two".to_owned(), b"two-two".to_vec()),
                ("/three".to_owned(), b"three".to_vec()),
            ]
        );
    }

    #[crate::test]
    async fn maximum_http1_request_count_finishes_then_closes() {
        let config =
            ServerConfig::new().set_limits(Limits::new().set_max_requests_per_connection(2));
        let server = Server::bind_with_config("127.0.0.1:0".parse().unwrap(), config)
            .await
            .unwrap();
        let address = server.local_addr();
        let peer = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream.set_read_timeout(Some(TEST_WAIT)).unwrap();
            stream
                .write_all(
                    b"GET /one HTTP/1.1\r\nhost: localhost\r\n\r\n\
GET /two HTTP/1.1\r\nhost: localhost\r\n\r\n",
                )
                .unwrap();
            let mut responses = Vec::new();
            stream.read_to_end(&mut responses).unwrap();
            responses
        });

        let cancellation = Rc::new(CancellationToken::new());
        let calls = Rc::new(Cell::new(0usize));
        let observed = Rc::clone(&calls);
        let cancel_from_handler = Rc::clone(&cancellation);
        server
            .serve(
                move |request| {
                    let call = observed.get() + 1;
                    observed.set(call);
                    let target = request.uri().path().to_owned();
                    let cancellation = Rc::clone(&cancel_from_handler);
                    async move {
                        if call == 2 {
                            cancellation.cancel();
                        }
                        Response::new(Body::from(target))
                    }
                },
                cancellation,
            )
            .await
            .unwrap();

        let responses = parse_http1_responses(&peer.join().unwrap());
        assert_eq!(calls.get(), 2);
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0].1, b"/one");
        assert_eq!(responses[1].1, b"/two");
        assert!(
            !responses[0]
                .0
                .windows(b"connection: close\r\n".len())
                .any(|window| window == b"connection: close\r\n")
        );
        assert!(
            responses[1]
                .0
                .windows(b"connection: close\r\n".len())
                .any(|window| window == b"connection: close\r\n")
        );
    }

    #[crate::test]
    async fn idle_timeout_reclaims_connection_between_http1_requests() {
        let config = ServerConfig::new().set_connection_io_timeout(Duration::from_millis(40));
        let server = Server::bind_with_config("127.0.0.1:0".parse().unwrap(), config)
            .await
            .unwrap();
        let address = server.local_addr();
        let peer = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream.set_read_timeout(Some(TEST_WAIT)).unwrap();
            stream
                .write_all(b"GET /one HTTP/1.1\r\nhost: localhost\r\n\r\n")
                .unwrap();
            let response = read_one_http1_response(&mut stream);
            std::thread::sleep(Duration::from_millis(150));
            let mut byte = [0u8; 1];
            let eof = stream.read(&mut byte).unwrap();
            (response, eof)
        });

        let cancellation = Rc::new(CancellationToken::new());
        let timed_out = Rc::new(Cell::new(false));
        let observed_timeout = Rc::clone(&timed_out);
        let cancel_from_error = Rc::clone(&cancellation);
        server
            .serve_with_error_handler(
                |_| async { Response::new(Body::from("one")) },
                cancellation,
                move |error| {
                    if let ServeError::Connection(Error::Io(error)) = error
                        && matches!(error, Errno::TIME | Errno::TIMEDOUT)
                    {
                        observed_timeout.set(true);
                        cancel_from_error.cancel();
                    }
                },
            )
            .await
            .unwrap();

        let (response, eof) = peer.join().unwrap();
        assert_eq!(parse_http1_responses(&response)[0].1, b"one");
        assert!(
            !response
                .windows(b"connection: close\r\n".len())
                .any(|window| window == b"connection: close\r\n")
        );
        assert_eq!(eof, 0);
        assert!(timed_out.get());
    }

    #[crate::test]
    async fn serves_one_http2_prior_knowledge_request() {
        use kimojio_fsm_http::{H2ByteClientEvent, H2Client};

        let server = Server::bind("[::1]:0".parse().unwrap()).await.unwrap();
        let address = server.local_addr();
        let peer = std::thread::spawn(move || {
            let mut protocol = H2Client::default();
            let mut outbound = protocol.connection_preface();
            let (stream_id, commit) = protocol
                .open_stream("GET", "http", "[::1]", "/h2", &[], true)
                .unwrap();
            let block = protocol.next_outbound_block().unwrap();
            assert_eq!(block.commit(), commit);
            outbound.extend_from_slice(block.bytes());
            protocol.acknowledge_outbound_block(commit).unwrap();

            let mut stream = TcpStream::connect(address).unwrap();
            stream.write_all(&outbound).unwrap();
            let mut inbound = Vec::new();
            stream.read_to_end(&mut inbound).unwrap();

            let mut offset = 0;
            let mut status = None;
            let mut body = Vec::new();
            while offset < inbound.len() {
                let (event, consumed, _) = protocol.accept_bytes(&inbound[offset..]).unwrap();
                assert_ne!(consumed, 0);
                offset += consumed;
                match event {
                    Some(H2ByteClientEvent::ResponseHeaders {
                        stream_id: response_stream,
                        headers,
                        ..
                    }) => {
                        assert_eq!(response_stream, stream_id);
                        status = headers
                            .iter()
                            .find(|header| header.name == b":status")
                            .map(|header| header.value.clone());
                    }
                    Some(H2ByteClientEvent::Data {
                        stream_id: response_stream,
                        payload,
                        ..
                    }) => {
                        assert_eq!(response_stream, stream_id);
                        body.extend_from_slice(&payload);
                    }
                    _ => {}
                }
            }
            (status, body)
        });

        let cancellation = Rc::new(CancellationToken::new());
        let cancel_from_handler = Rc::clone(&cancellation);
        server
            .serve(
                move |request| {
                    let cancellation = Rc::clone(&cancel_from_handler);
                    async move {
                        assert_eq!(request.version(), Version::HTTP_2);
                        assert_eq!(request.uri(), "/h2");
                        cancellation.cancel();
                        Response::new(Body::from("h2-ok"))
                    }
                },
                cancellation,
            )
            .await
            .unwrap();

        let (status, body) = peer.join().unwrap();
        assert_eq!(status.as_deref(), Some(b"200".as_slice()));
        assert_eq!(body, b"h2-ok");
    }

    #[crate::test]
    async fn multiplexes_http2_handlers_and_sends_ready_response_first() {
        use kimojio_fsm_http::{H2ByteClientEvent, H2Client};

        let server = Server::bind("[::1]:0".parse().unwrap()).await.unwrap();
        let address = server.local_addr();
        let fast_received = Arc::new(AtomicBool::new(false));
        let peer_fast_received = Arc::clone(&fast_received);
        let peer = std::thread::spawn(move || {
            let mut protocol = H2Client::default();
            let mut outbound = protocol.connection_preface();
            let (slow_stream, slow_commit) = protocol
                .open_stream("GET", "http", "[::1]", "/slow", &[], true)
                .unwrap();
            let block = protocol.next_outbound_block().unwrap();
            assert_eq!(block.commit(), slow_commit);
            outbound.extend_from_slice(block.bytes());
            protocol.acknowledge_outbound_block(slow_commit).unwrap();
            let (fast_stream, fast_commit) = protocol
                .open_stream("GET", "http", "[::1]", "/fast", &[], true)
                .unwrap();
            let block = protocol.next_outbound_block().unwrap();
            assert_eq!(block.commit(), fast_commit);
            outbound.extend_from_slice(block.bytes());
            protocol.acknowledge_outbound_block(fast_commit).unwrap();

            let mut stream = TcpStream::connect(address).unwrap();
            stream.set_read_timeout(Some(TEST_WAIT)).unwrap();
            stream.write_all(&outbound).unwrap();
            let mut pending = Vec::new();
            let mut slow_body = Vec::new();
            let mut fast_body = Vec::new();
            while slow_body.is_empty() || fast_body.is_empty() {
                let mut bytes = [0u8; 4096];
                let read = stream.read(&mut bytes).unwrap();
                assert_ne!(read, 0);
                pending.extend_from_slice(&bytes[..read]);

                let mut consumed_total = 0;
                loop {
                    let (event, consumed, output) =
                        protocol.accept_bytes(&pending[consumed_total..]).unwrap();
                    if consumed == 0 {
                        break;
                    }
                    consumed_total += consumed;
                    if !output.is_empty() {
                        stream.write_all(&output).unwrap();
                    }
                    if let Some(H2ByteClientEvent::Data {
                        stream_id, payload, ..
                    }) = event
                    {
                        if stream_id == slow_stream {
                            slow_body.extend_from_slice(&payload);
                        } else if stream_id == fast_stream {
                            fast_body.extend_from_slice(&payload);
                            peer_fast_received.store(true, Ordering::Release);
                        }
                    }
                }
                pending.drain(..consumed_total);
            }
            (slow_body, fast_body)
        });

        let slow_started = Rc::new(AsyncEvent::new());
        let release_slow = Rc::new(AsyncEvent::new());
        let cancellation = Rc::new(CancellationToken::new());
        let started = Rc::clone(&slow_started);
        let release = Rc::clone(&release_slow);
        let cancel_from_handler = Rc::clone(&cancellation);
        let serve_task = operations::spawn_task(server.serve(
            move |request| {
                let target = request.uri().path().to_owned();
                let started = Rc::clone(&started);
                let release = Rc::clone(&release);
                let cancellation = Rc::clone(&cancel_from_handler);
                async move {
                    if target == "/slow" {
                        started.set();
                        release.wait().await.unwrap();
                        cancellation.cancel();
                        Response::new(Body::from("slow"))
                    } else {
                        Response::new(Body::from("fast"))
                    }
                }
            },
            cancellation,
        ));

        slow_started.wait().await.unwrap();
        operations::timeout_at(crate::clock_now() + TEST_WAIT, async {
            while !fast_received.load(Ordering::Acquire) {
                operations::sleep(Duration::from_millis(1)).await.unwrap();
            }
        })
        .await
        .expect("ready HTTP/2 response was blocked by a slow sibling");
        release_slow.set();
        operations::timeout_at(crate::clock_now() + TEST_WAIT, serve_task)
            .await
            .expect("multiplexed server did not shut down")
            .unwrap()
            .unwrap();

        let (slow_body, fast_body) = peer.join().unwrap();
        assert_eq!(slow_body, b"slow");
        assert_eq!(fast_body, b"fast");
    }

    #[crate::test]
    async fn malformed_client_does_not_stop_listener_and_is_reported() {
        let server = Server::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let address = server.local_addr();
        let peer = std::thread::spawn(move || {
            let mut malformed = TcpStream::connect(address).unwrap();
            malformed.write_all(b"GET /partial").unwrap();
            std::thread::sleep(Duration::from_millis(100));
            malformed.shutdown(Shutdown::Both).unwrap();
            drop(malformed);
            std::thread::sleep(Duration::from_millis(50));
            let response = send_http1(address, "/healthy");
            assert!(response.ends_with(b"\r\n\r\nhealthy"));
        });

        let cancellation = Rc::new(CancellationToken::new());
        let errors = Rc::new(RefCell::new(Vec::new()));
        let cancel_from_handler = Rc::clone(&cancellation);
        let reported = Rc::clone(&errors);
        operations::timeout_at(
            crate::clock_now() + TEST_WAIT,
            server.serve_with_error_handler(
                move |_| {
                    let cancellation = Rc::clone(&cancel_from_handler);
                    async move {
                        cancellation.cancel();
                        Response::new(Body::from("healthy"))
                    }
                },
                cancellation,
                move |error| reported.borrow_mut().push(error.to_string()),
            ),
        )
        .await
        .expect("server did not survive an early close")
        .unwrap();

        peer.join().unwrap();
        assert!(
            !errors.borrow().is_empty(),
            "malformed-client failure was not reported"
        );
    }

    #[crate::test]
    async fn handler_panic_is_reported_and_listener_survives() {
        let server = Server::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let address = server.local_addr();
        let peer = std::thread::spawn(move || {
            let mut panicking = TcpStream::connect(address).unwrap();
            panicking
                .write_all(b"GET /panic HTTP/1.1\r\nhost: localhost\r\n\r\n")
                .unwrap();
            std::thread::sleep(Duration::from_millis(100));
            drop(panicking);
            let response = send_http1(address, "/healthy");
            assert!(response.ends_with(b"\r\n\r\nok"));
        });

        let cancellation = Rc::new(CancellationToken::new());
        let panic_payload = Rc::new(RefCell::new(None));
        let cancel_from_handler = Rc::clone(&cancellation);
        let observed_payload = Rc::clone(&panic_payload);
        operations::timeout_at(
            crate::clock_now() + TEST_WAIT,
            server.serve_with_error_handler(
                move |request| {
                    let cancellation = Rc::clone(&cancel_from_handler);
                    async move {
                        if request.uri() == "/panic" {
                            panic!("expected handler panic");
                        }
                        cancellation.cancel();
                        Response::new(Body::from("ok"))
                    }
                },
                cancellation,
                move |error| {
                    if let Some(payload) = error.panic_payload() {
                        observed_payload
                            .borrow_mut()
                            .replace(panic_message(payload).to_owned());
                    }
                },
            ),
        )
        .await
        .expect("server did not survive a handler panic")
        .unwrap();

        peer.join().unwrap();
        assert_eq!(
            panic_payload.borrow().as_deref(),
            Some("expected handler panic")
        );
    }

    #[crate::test]
    async fn stalled_client_times_out_and_is_reported() {
        let config = ServerConfig::new().set_connection_io_timeout(Duration::from_millis(40));
        let server = Server::bind_with_config("127.0.0.1:0".parse().unwrap(), config)
            .await
            .unwrap();
        let address = server.local_addr();
        let peer = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream.write_all(b"P").unwrap();
            std::thread::sleep(Duration::from_millis(150));
        });

        let cancellation = Rc::new(CancellationToken::new());
        let timed_out = Rc::new(Cell::new(false));
        let cancel_from_error = Rc::clone(&cancellation);
        let observed_timeout = Rc::clone(&timed_out);
        let started = Instant::now();
        operations::timeout_at(
            crate::clock_now() + TEST_WAIT,
            server.serve_with_error_handler(
                |_| async { Response::new(Body::empty()) },
                cancellation,
                move |error| {
                    if let ServeError::Connection(Error::Io(error)) = error
                        && matches!(error, Errno::TIME | Errno::TIMEDOUT)
                    {
                        observed_timeout.set(true);
                        cancel_from_error.cancel();
                    }
                },
            ),
        )
        .await
        .expect("stalled connection was not reclaimed")
        .unwrap();

        peer.join().unwrap();
        assert!(timed_out.get());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[crate::test]
    async fn maximum_connection_count_applies_backpressure() {
        let config = ServerConfig::new()
            .set_max_connections(1)
            .set_graceful_shutdown_timeout(Duration::from_secs(1));
        let server = Server::bind_with_config("127.0.0.1:0".parse().unwrap(), config)
            .await
            .unwrap();
        let address = server.local_addr();
        let first_started = Rc::new(AsyncEvent::new());
        let release_first = Rc::new(AsyncEvent::new());
        let handler_calls = Rc::new(Cell::new(0usize));
        let cancellation = Rc::new(CancellationToken::new());

        let calls = Rc::clone(&handler_calls);
        let started = Rc::clone(&first_started);
        let release = Rc::clone(&release_first);
        let cancel_from_handler = Rc::clone(&cancellation);
        let serve_task = operations::spawn_task(server.serve(
            move |_| {
                let call = calls.get() + 1;
                calls.set(call);
                let started = Rc::clone(&started);
                let release = Rc::clone(&release);
                let cancellation = Rc::clone(&cancel_from_handler);
                async move {
                    if call == 1 {
                        started.set();
                        release.wait().await.unwrap();
                    } else {
                        cancellation.cancel();
                    }
                    Response::new(Body::from("ok"))
                }
            },
            cancellation,
        ));

        let first = std::thread::spawn(move || send_http1(address, "/first"));
        first_started.wait().await.unwrap();
        let second = std::thread::spawn(move || send_http1(address, "/second"));
        operations::sleep(Duration::from_millis(75)).await.unwrap();
        assert_eq!(handler_calls.get(), 1);
        release_first.set();

        operations::timeout_at(crate::clock_now() + TEST_WAIT, serve_task)
            .await
            .expect("server did not release capped connection")
            .unwrap()
            .unwrap();
        assert_eq!(handler_calls.get(), 2);
        assert!(first.join().unwrap().ends_with(b"\r\n\r\nok"));
        assert!(second.join().unwrap().ends_with(b"\r\n\r\nok"));
    }

    #[crate::test]
    async fn cancellation_drains_healthy_handler_and_reclaims_stalled_peer() {
        let config = ServerConfig::new()
            .set_connection_io_timeout(Duration::from_secs(5))
            .set_graceful_shutdown_timeout(Duration::from_millis(300))
            .set_max_connections(2);
        let server = Server::bind_with_config("127.0.0.1:0".parse().unwrap(), config)
            .await
            .unwrap();
        let address = server.local_addr();
        let handler_started = Rc::new(AsyncEvent::new());
        let handler_finished = Rc::new(Cell::new(false));
        let cancellation = Rc::new(CancellationToken::new());

        let started = Rc::clone(&handler_started);
        let finished = Rc::clone(&handler_finished);
        let serve_task = operations::spawn_task(server.serve(
            move |_| {
                let started = Rc::clone(&started);
                let finished = Rc::clone(&finished);
                async move {
                    started.set();
                    operations::sleep(Duration::from_millis(80)).await.unwrap();
                    finished.set(true);
                    Response::new(Body::from("healthy"))
                }
            },
            Rc::clone(&cancellation),
        ));

        let stalled = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream.write_all(b"P").unwrap();
            std::thread::sleep(Duration::from_millis(500));
        });
        operations::sleep(Duration::from_millis(50)).await.unwrap();
        let healthy = std::thread::spawn(move || send_http1(address, "/healthy"));
        handler_started.wait().await.unwrap();

        let shutdown_started = Instant::now();
        cancellation.cancel();
        operations::timeout_at(crate::clock_now() + TEST_WAIT, serve_task)
            .await
            .expect("graceful shutdown exceeded its bound")
            .unwrap()
            .unwrap();

        assert!(handler_finished.get());
        assert!(shutdown_started.elapsed() >= Duration::from_millis(50));
        assert!(shutdown_started.elapsed() < Duration::from_millis(300));
        assert!(healthy.join().unwrap().ends_with(b"\r\n\r\nhealthy"));
        stalled.join().unwrap();
    }
}
