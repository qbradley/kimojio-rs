// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//! HTTP client and server support.
//!
//! This module uses the standard [`Request`], [`Response`], [`Method`], [`Uri`],
//! and header types so HTTP values compose with the wider Rust ecosystem.
//! Message bodies are buffered by default. Callers may opt into incremental
//! inbound streaming, and outbound [`Body`] values may be buffered or streamed.
//! [`ServerConfig`] additionally bounds connection I/O, graceful shutdown, and
//! concurrency; [`ServeError`] exposes nonfatal server failures to applications.
//!
//! HTTP/1.1 and HTTP/2 are supported over cleartext connections and, with the
//! `tls` feature, over TLS with ALPN. The server multiplexes concurrent HTTP/2
//! streams within a connection and serves sequential HTTP/1 requests over
//! persistent connections. The client reuses HTTP/1 connections sequentially
//! and multiplexes concurrent requests over pooled HTTP/2 sessions. HTTP/1.1
//! `Expect: 100-continue`
//! requests wait for an interim response before sending their body, with a
//! configurable bounded fallback. Servers can accept them normally or reject
//! them from the parsed request head before reading the body. Protocol upgrades
//! are outside the initial API.

use std::cell::{Cell, OnceCell, RefCell};
use std::error;
use std::fmt;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::ops::Deref;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures::{Stream, StreamExt};
use kimojio_fsm_http::{
    DEFAULT_MAX_REQUESTS_PER_CONNECTION, Error as FsmHttp1Error, H2_DEFAULT_MAX_ACTIVE_STREAMS,
    H2ProtocolError as FsmH2ProtocolError, HttpErrorInfo, HttpErrorKind,
    HttpLimits as FsmHttpLimits, ServerError as FsmServerError,
};

pub use ::http::{
    HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri, Version,
};

use crate::operations::{self, AddressFamily, SocketType, ipproto};
use crate::{AsyncStreamRead, Errno, OwnedFd, OwnedFdStream};

mod client;
mod server;
#[cfg(feature = "tls")]
mod tls;
mod transport;

pub use client::{Client, ClientEvent, RequestBuilder};
pub use server::{ExpectContinueDecision, ServeError, Server, ServerBuilder};
#[cfg(feature = "tls")]
pub use tls::{AlpnProtocol, TlsClientConfig, TlsConfigError, TlsServerConfig};

const DEFAULT_MAX_HEADER_BYTES: usize = 64 * 1024;
const DEFAULT_MAX_HEADERS: usize = 100;
const DEFAULT_MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_READ_BUFFER_BYTES: usize = 64 * 1024;
const DEFAULT_EXPECT_CONTINUE_TIMEOUT: Duration = Duration::from_secs(1);
const DEFAULT_LISTEN_BACKLOG: u32 = 128;
const DEFAULT_CONNECTION_IO_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_CONNECTIONS: usize = 1024;
const DEFAULT_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const DEFAULT_POOL_MAX_IDLE_PER_KEY: usize = 8;
const DEFAULT_POOL_MAX_IDLE_TOTAL: usize = 64;

type BoxBodyStreamError = Box<dyn error::Error + Send + Sync + 'static>;
type BodyStreamItem = std::result::Result<Vec<u8>, BoxBodyStreamError>;
type BodyChunkFuture = Pin<
    Box<dyn Future<Output = std::result::Result<Option<Vec<u8>>, BoxBodyStreamError>> + 'static>,
>;
pub(super) type BodyTrailersFn = Box<dyn FnOnce() -> HeaderMap + 'static>;

#[derive(Clone)]
pub(super) struct InboundTrailers {
    headers: Rc<OnceCell<HeaderMap>>,
    drained: Rc<Cell<bool>>,
}

impl InboundTrailers {
    pub(super) fn streaming() -> Self {
        Self {
            headers: Rc::new(OnceCell::new()),
            drained: Rc::new(Cell::new(false)),
        }
    }

    fn complete(headers: HeaderMap) -> Self {
        let trailers = Self {
            headers: Rc::new(OnceCell::new()),
            drained: Rc::new(Cell::new(true)),
        };
        trailers
            .headers
            .set(headers)
            .expect("a new inbound trailer cell is empty");
        trailers
    }

    pub(super) fn set(&self, headers: HeaderMap) -> std::result::Result<(), HeaderMap> {
        self.headers.set(headers)
    }

    fn mark_drained(&self) {
        self.drained.set(true);
    }

    fn get(&self) -> Option<&HeaderMap> {
        self.drained.get().then(|| self.headers.get()).flatten()
    }
}

#[derive(Clone)]
struct OutboundTrailers {
    callback: Rc<RefCell<Option<BodyTrailersFn>>>,
}

impl OutboundTrailers {
    fn new(callback: BodyTrailersFn) -> Self {
        Self {
            callback: Rc::new(RefCell::new(Some(callback))),
        }
    }

    fn take(&self) -> Option<BodyTrailersFn> {
        self.callback.borrow_mut().take()
    }
}

#[derive(Debug)]
pub(super) struct InboundBodyError(pub(super) Error);

impl fmt::Display for InboundBodyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl error::Error for InboundBodyError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        self.0.source()
    }
}

#[derive(Clone)]
pub(super) struct BodyStreamSource {
    stream: Rc<RefCell<Pin<Box<dyn Stream<Item = BodyStreamItem> + 'static>>>>,
}

impl BodyStreamSource {
    fn new(stream: impl Stream<Item = BodyStreamItem> + 'static) -> Self {
        Self {
            stream: Rc::new(RefCell::new(Box::pin(stream))),
        }
    }

    fn next_chunk(&self) -> BodyChunkFuture {
        let stream = Rc::clone(&self.stream);
        Box::pin(async move {
            loop {
                let item = std::future::poll_fn(|context| {
                    let mut stream = stream.borrow_mut();
                    stream.as_mut().poll_next(context)
                })
                .await;
                match item {
                    Some(Ok(chunk)) if chunk.is_empty() => {}
                    Some(Ok(chunk)) => return Ok(Some(chunk)),
                    Some(Err(error)) => return Err(error),
                    None => return Ok(None),
                }
            }
        })
    }

    fn ptr_eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.stream, &other.stream)
    }
}

pub(super) struct BodyStreamCursor {
    source: BodyStreamSource,
    chunk: Vec<u8>,
    offset: usize,
    ended: bool,
    termination_sent: bool,
    pending: Option<BodyChunkFuture>,
}

impl BodyStreamCursor {
    pub(super) fn new(source: BodyStreamSource) -> Self {
        Self {
            source,
            chunk: Vec::new(),
            offset: 0,
            ended: false,
            termination_sent: false,
            pending: None,
        }
    }

    pub(super) fn available(&self) -> Option<&[u8]> {
        if self.offset < self.chunk.len() {
            Some(&self.chunk[self.offset..])
        } else if self.ended && !self.termination_sent {
            Some(&[])
        } else {
            None
        }
    }

    pub(super) fn needs_chunk(&self) -> bool {
        self.offset == self.chunk.len() && !self.ended
    }

    pub(super) fn poll_chunk(
        &mut self,
        context: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), BoxBodyStreamError>> {
        if !self.needs_chunk() {
            return Poll::Ready(Ok(()));
        }
        let pending = self.pending.get_or_insert_with(|| self.source.next_chunk());
        match pending.as_mut().poll(context) {
            Poll::Ready(Ok(Some(chunk))) => {
                self.pending = None;
                self.chunk = chunk;
                self.offset = 0;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Ok(None)) => {
                self.pending = None;
                self.ended = true;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(error)) => {
                self.pending = None;
                Poll::Ready(Err(error))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    pub(super) fn commit(&mut self, payload_len: usize) {
        let available = self
            .available()
            .expect("a streamed body commit follows a prepared chunk");
        assert!(payload_len <= available.len());
        if payload_len == 0 {
            assert!(self.ended);
            self.termination_sent = true;
        } else {
            self.offset += payload_len;
            if self.offset == self.chunk.len() {
                self.chunk.clear();
                self.offset = 0;
            }
        }
    }

    pub(super) const fn is_complete(&self) -> bool {
        self.termination_sent
    }
}

pub(super) enum BodyInner {
    Buffered(Vec<u8>),
    Streaming(BodyStreamSource),
}

/// An owned HTTP message body.
///
/// Buffered bodies preserve the original byte-slice accessors. Streaming
/// bodies are pulled one chunk at a time by the HTTP transport and do not
/// require the complete body to reside in memory.
pub struct Body {
    pub(super) inner: BodyInner,
    inbound_trailers: Option<InboundTrailers>,
    outbound_trailers: Option<OutboundTrailers>,
}

impl Clone for Body {
    fn clone(&self) -> Self {
        let inner = match &self.inner {
            BodyInner::Buffered(bytes) => BodyInner::Buffered(bytes.clone()),
            BodyInner::Streaming(source) => BodyInner::Streaming(source.clone()),
        };
        Self {
            inner,
            inbound_trailers: self.inbound_trailers.clone(),
            outbound_trailers: self.outbound_trailers.clone(),
        }
    }
}

impl fmt::Debug for Body {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            BodyInner::Buffered(bytes) => formatter
                .debug_struct("Body")
                .field("bytes", bytes)
                .finish(),
            BodyInner::Streaming(_) => formatter
                .debug_struct("Body")
                .field("streaming", &true)
                .finish(),
        }
    }
}

impl Default for Body {
    fn default() -> Self {
        Self::empty()
    }
}

impl PartialEq for Body {
    fn eq(&self, other: &Self) -> bool {
        match (&self.inner, &other.inner) {
            (BodyInner::Buffered(left), BodyInner::Buffered(right)) => left == right,
            (BodyInner::Streaming(left), BodyInner::Streaming(right)) => left.ptr_eq(right),
            (BodyInner::Buffered(_), BodyInner::Streaming(_))
            | (BodyInner::Streaming(_), BodyInner::Buffered(_)) => false,
        }
    }
}

impl Eq for Body {}

impl Body {
    /// Creates an empty body.
    pub const fn empty() -> Self {
        Self {
            inner: BodyInner::Buffered(Vec::new()),
            inbound_trailers: None,
            outbound_trailers: None,
        }
    }

    /// Creates a body from owned bytes without copying them.
    pub const fn new(bytes: Vec<u8>) -> Self {
        Self {
            inner: BodyInner::Buffered(bytes),
            inbound_trailers: None,
            outbound_trailers: None,
        }
    }

    /// Creates a streaming body from fallible chunks.
    ///
    /// Empty chunks are ignored. The stream's end emits the protocol-specific
    /// body terminator. Cloning this body shares the single underlying stream.
    pub fn from_stream<S, B, E>(stream: S) -> Self
    where
        S: Stream<Item = std::result::Result<B, E>> + 'static,
        B: Into<Vec<u8>>,
        E: error::Error + Send + Sync + 'static,
    {
        let stream = stream.map(|item| {
            item.map(Into::into)
                .map_err(|error| Box::new(error) as BoxBodyStreamError)
        });
        Self {
            inner: BodyInner::Streaming(BodyStreamSource::new(stream)),
            inbound_trailers: None,
            outbound_trailers: None,
        }
    }

    /// Creates a streaming body from infallible chunks.
    ///
    /// Empty chunks are ignored. The stream's end emits the protocol-specific
    /// body terminator. Cloning this body shares the single underlying stream.
    pub fn from_chunks<S, B>(stream: S) -> Self
    where
        S: Stream<Item = B> + 'static,
        B: Into<Vec<u8>>,
    {
        let stream = stream.map(|chunk| Ok(chunk.into()));
        Self {
            inner: BodyInner::Streaming(BodyStreamSource::new(stream)),
            inbound_trailers: None,
            outbound_trailers: None,
        }
    }

    /// Pulls the next body chunk.
    ///
    /// A buffered body is returned as one chunk without copying. A streaming
    /// body advances its source only when this method is awaited.
    pub async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        let chunk = match &mut self.inner {
            BodyInner::Buffered(bytes) if bytes.is_empty() => Ok(None),
            BodyInner::Buffered(bytes) => Ok(Some(std::mem::take(bytes))),
            BodyInner::Streaming(source) => source.next_chunk().await.map_err(|source| {
                match source.downcast::<InboundBodyError>() {
                    Ok(error) => error.0,
                    Err(source) => Error::BodyStream { source },
                }
            }),
        };
        if matches!(chunk, Ok(None))
            && let Some(trailers) = &self.inbound_trailers
        {
            trailers.mark_drained();
        }
        chunk
    }

    pub(super) fn from_inbound_stream<S>(stream: S, trailers: InboundTrailers) -> Self
    where
        S: Stream<Item = Result<Vec<u8>>> + 'static,
    {
        let stream = stream.map(|item| {
            item.map_err(|error| Box::new(InboundBodyError(error)) as BoxBodyStreamError)
        });
        Self {
            inner: BodyInner::Streaming(BodyStreamSource::new(stream)),
            inbound_trailers: Some(trailers),
            outbound_trailers: None,
        }
    }

    pub(super) fn from_inbound(bytes: Vec<u8>, trailers: Option<HeaderMap>) -> Self {
        Self {
            inner: BodyInner::Buffered(bytes),
            inbound_trailers: trailers.map(InboundTrailers::complete),
            outbound_trailers: None,
        }
    }

    /// Returns trailers received after the body.
    ///
    /// Buffered inbound bodies expose trailers as soon as the request or
    /// response is returned. For a streaming inbound body, this method returns
    /// `None` until [`Self::next_chunk`] has returned `None`; drain the stream
    /// before inspecting it. `None` also means the peer sent no trailers.
    ///
    /// HTTP/1.1 carries trailers in chunked framing (RFC 9112 section 7.1.2).
    /// HTTP/2 carries them in the terminal HEADERS block (RFC 9113 section 8.1).
    pub fn trailers(&self) -> Option<&HeaderMap> {
        self.inbound_trailers
            .as_ref()
            .and_then(InboundTrailers::get)
    }

    /// Attaches server response trailers known before the body is produced.
    ///
    /// The map is retained until the response body ends. HTTP/1.1 responses
    /// use chunked framing as required by RFC 9112 section 7.1.2, while HTTP/2
    /// sends a terminal HEADERS block as specified by RFC 9113 section 8.1.
    /// Client request trailers are not supported.
    pub fn with_trailers(self, trailers: HeaderMap) -> Self {
        self.with_trailers_fn(move || trailers)
    }

    /// Attaches server response trailers computed when the body ends.
    ///
    /// The server pump invokes this closure exactly once after the body source
    /// finishes and before it prepares the protocol terminator. HTTP/1.1 uses
    /// chunked framing (RFC 9112 section 7.1.2); HTTP/2 uses a terminal HEADERS
    /// block (RFC 9113 section 8.1). Client request trailers are not supported.
    /// Cloned bodies share this one-shot callback, so only one clone can send
    /// it successfully.
    pub fn with_trailers_fn<F>(mut self, trailers: F) -> Self
    where
        F: FnOnce() -> HeaderMap + 'static,
    {
        self.outbound_trailers = Some(OutboundTrailers::new(Box::new(trailers)));
        self
    }

    pub(super) fn has_outbound_trailers(&self) -> bool {
        self.outbound_trailers.is_some()
    }

    pub(super) fn take_outbound_trailers(
        &mut self,
    ) -> std::result::Result<Option<BodyTrailersFn>, ()> {
        self.outbound_trailers
            .as_ref()
            .map_or(Ok(None), |trailers| trailers.take().map(Some).ok_or(()))
    }

    /// Returns the body bytes.
    ///
    /// # Panics
    ///
    /// Panics for a streaming body because its future bytes cannot be borrowed.
    pub fn as_bytes(&self) -> &[u8] {
        match &self.inner {
            BodyInner::Buffered(bytes) => bytes,
            BodyInner::Streaming(_) => {
                panic!(
                    "a streaming body has no bytes in memory to borrow; \
                     read it with Body::next_chunk, or check Body::is_streaming \
                     first. Deref and AsRef reach here too, so `&body[..]` and \
                     a generic AsRef call panic the same way"
                )
            }
        }
    }

    /// Consumes the body and returns its bytes.
    ///
    /// # Panics
    ///
    /// Panics for a streaming body because collecting it requires asynchronous
    /// I/O.
    pub fn into_bytes(self) -> Vec<u8> {
        match self.inner {
            BodyInner::Buffered(bytes) => bytes,
            BodyInner::Streaming(_) => {
                panic!("Body::into_bytes is unavailable for a streaming body")
            }
        }
    }

    /// Returns the payload length when it is known before transmission.
    ///
    /// A server response with attached trailers still uses streaming wire
    /// framing even when this method returns a length.
    pub fn known_len(&self) -> Option<usize> {
        match &self.inner {
            BodyInner::Buffered(bytes) => Some(bytes.len()),
            BodyInner::Streaming(_) => None,
        }
    }

    /// Returns whether this body has a pull-based streaming source.
    ///
    /// A buffered server response with attached trailers uses streaming wire
    /// framing even though this method returns `false`.
    pub fn is_streaming(&self) -> bool {
        matches!(self.inner, BodyInner::Streaming(_))
    }

    /// Returns the body length in bytes.
    ///
    /// # Panics
    ///
    /// Panics for a streaming body. Use [`Self::known_len`] when either body
    /// shape is accepted.
    pub fn len(&self) -> usize {
        self.as_bytes().len()
    }

    /// Returns whether the body is empty.
    ///
    /// # Panics
    ///
    /// Panics for a streaming body because emptiness is unknown until its
    /// stream ends.
    pub fn is_empty(&self) -> bool {
        self.as_bytes().is_empty()
    }
}

impl AsRef<[u8]> for Body {
    /// # Panics
    ///
    /// Panics for a streaming body, whose bytes are not yet in memory. Use
    /// [`Body::next_chunk`] to consume one instead.
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl Deref for Body {
    type Target = [u8];

    /// # Panics
    ///
    /// Panics for a streaming body, so indexing or slicing one - `&body[..]` -
    /// panics even though no method was named. Use [`Body::next_chunk`].
    fn deref(&self) -> &Self::Target {
        self.as_bytes()
    }
}

impl From<Vec<u8>> for Body {
    fn from(bytes: Vec<u8>) -> Self {
        Self::new(bytes)
    }
}

impl From<&[u8]> for Body {
    fn from(bytes: &[u8]) -> Self {
        Self::new(bytes.to_vec())
    }
}

impl From<String> for Body {
    fn from(value: String) -> Self {
        Self::new(value.into_bytes())
    }
}

impl From<&str> for Body {
    fn from(value: &str) -> Self {
        Self::from(value.as_bytes())
    }
}

/// HTTP message and per-connection resource limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    max_header_bytes: usize,
    max_headers: usize,
    max_body_bytes: usize,
    max_active_streams: usize,
    max_requests_per_connection: usize,
    read_buffer_bytes: usize,
}

impl Limits {
    /// Creates the default limits.
    pub const fn new() -> Self {
        Self {
            max_header_bytes: DEFAULT_MAX_HEADER_BYTES,
            max_headers: DEFAULT_MAX_HEADERS,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            max_active_streams: H2_DEFAULT_MAX_ACTIVE_STREAMS,
            max_requests_per_connection: DEFAULT_MAX_REQUESTS_PER_CONNECTION,
            read_buffer_bytes: DEFAULT_READ_BUFFER_BYTES,
        }
    }

    /// Returns the maximum aggregate encoded header size.
    pub const fn max_header_bytes(&self) -> usize {
        self.max_header_bytes
    }

    /// Sets the maximum aggregate encoded header size.
    ///
    /// This value must not exceed [`Self::read_buffer_bytes`]. An inconsistent
    /// limit is rejected when a client or server is constructed.
    pub const fn set_max_header_bytes(mut self, limit: usize) -> Self {
        self.max_header_bytes = limit;
        self
    }

    /// Returns the maximum number of header occurrences.
    pub const fn max_headers(&self) -> usize {
        self.max_headers
    }

    /// Sets the maximum number of header occurrences.
    pub const fn set_max_headers(mut self, limit: usize) -> Self {
        self.max_headers = limit;
        self
    }

    /// Returns the maximum body size that may be accumulated in memory.
    pub const fn max_body_bytes(&self) -> usize {
        self.max_body_bytes
    }

    /// Sets the maximum accumulated body size per active exchange.
    ///
    /// Pull-based bodies have no total-size limit because only a chunk and its
    /// bounded handoff are resident at once. HTTP/2 receive capacity is still
    /// derived from this value, so it continues to bound queued peer input.
    pub const fn set_max_body_bytes(mut self, limit: usize) -> Self {
        self.max_body_bytes = limit;
        self
    }

    /// Returns the maximum number of concurrently active HTTP/2 streams.
    pub const fn max_active_streams(&self) -> usize {
        self.max_active_streams
    }

    /// Sets the maximum number of concurrently active HTTP/2 streams.
    ///
    /// A value of zero is rejected when a client or server is constructed.
    pub const fn set_max_active_streams(mut self, limit: usize) -> Self {
        self.max_active_streams = limit;
        self
    }

    /// Returns the maximum number of requests handled by one connection.
    ///
    /// Clients retire a pooled HTTP/1 or HTTP/2 connection after this many
    /// completed requests. Servers apply this limit to sequential HTTP/1
    /// requests; concurrent HTTP/2 work is bounded by
    /// [`Self::max_active_streams`].
    pub const fn max_requests_per_connection(&self) -> usize {
        self.max_requests_per_connection
    }

    /// Sets the maximum number of requests handled by one connection.
    ///
    /// A value of zero is rejected when a client or server is constructed.
    pub const fn set_max_requests_per_connection(mut self, limit: usize) -> Self {
        self.max_requests_per_connection = limit;
        self
    }

    /// Returns the reusable per-connection input-buffer size.
    pub const fn read_buffer_bytes(&self) -> usize {
        self.read_buffer_bytes
    }

    /// Sets the reusable per-connection input-buffer size.
    ///
    /// A zero-sized buffer, or one smaller than [`Self::max_header_bytes`], is
    /// rejected when a client or server is constructed.
    pub const fn set_read_buffer_bytes(mut self, limit: usize) -> Self {
        self.read_buffer_bytes = limit;
        self
    }

    pub(super) const fn protocol(self) -> FsmHttpLimits {
        FsmHttpLimits::new()
            .set_max_header_bytes(self.max_header_bytes)
            .set_max_headers(self.max_headers)
            .set_max_body_bytes(self.max_body_bytes)
            .set_max_active_streams(self.max_active_streams)
            .set_max_requests_per_connection(self.max_requests_per_connection)
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::new()
    }
}

/// Selects the cleartext HTTP wire protocol used for an outbound request.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Protocol {
    /// HTTP/1.1.
    #[default]
    Http1,
    /// HTTP/2 started directly with the connection preface.
    Http2PriorKnowledge,
}

impl Protocol {
    /// Returns the corresponding standard HTTP version.
    pub const fn version(self) -> Version {
        match self {
            Self::Http1 => Version::HTTP_11,
            Self::Http2PriorKnowledge => Version::HTTP_2,
        }
    }

    pub(super) fn from_version(version: Version) -> Result<Self> {
        match version {
            Version::HTTP_11 => Ok(Self::Http1),
            Version::HTTP_2 => Ok(Self::Http2PriorKnowledge),
            version => Err(Error::UnsupportedVersion(version)),
        }
    }
}

/// Configuration shared by all requests made by a client.
#[derive(Clone, Debug)]
pub struct ClientConfig {
    limits: Limits,
    default_protocol: Protocol,
    expect_continue_timeout: Duration,
    connection_io_timeout: Duration,
    pool_idle_timeout: Duration,
    pool_max_idle_per_key: usize,
    pool_max_idle_total: usize,
    #[cfg(feature = "tls")]
    tls: Option<TlsClientConfig>,
}

impl ClientConfig {
    /// Creates a client configuration with default limits, HTTP/1.1, a
    /// one-second `100 Continue` wait, a 30-second streaming-response I/O
    /// timeout, a 90-second pool idle timeout, eight idle connections per key,
    /// and 64 idle connections in total. Setting any pool bound to zero
    /// explicitly disables idle connection reuse.
    pub const fn new() -> Self {
        Self {
            limits: Limits::new(),
            default_protocol: Protocol::Http1,
            expect_continue_timeout: DEFAULT_EXPECT_CONTINUE_TIMEOUT,
            connection_io_timeout: DEFAULT_CONNECTION_IO_TIMEOUT,
            pool_idle_timeout: DEFAULT_POOL_IDLE_TIMEOUT,
            pool_max_idle_per_key: DEFAULT_POOL_MAX_IDLE_PER_KEY,
            pool_max_idle_total: DEFAULT_POOL_MAX_IDLE_TOTAL,
            #[cfg(feature = "tls")]
            tls: None,
        }
    }

    /// Returns the configured limits.
    pub const fn limits(&self) -> Limits {
        self.limits
    }

    /// Sets the HTTP message and connection limits.
    pub const fn set_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Returns the protocol used when a request does not select a version.
    pub const fn default_protocol(&self) -> Protocol {
        self.default_protocol
    }

    /// Sets the protocol used when a request does not select a version.
    pub const fn set_default_protocol(mut self, protocol: Protocol) -> Self {
        self.default_protocol = protocol;
        self
    }

    /// Returns how long an HTTP/1.1 request waits for `100 Continue`.
    ///
    /// When the wait expires, the client sends the request body without an
    /// interim response. A zero duration sends the body immediately after the
    /// request head.
    pub const fn expect_continue_timeout(&self) -> Duration {
        self.expect_continue_timeout
    }

    /// Sets how long an HTTP/1.1 request waits for `100 Continue`.
    ///
    /// A zero duration disables the wait while preserving the separate request
    /// head write.
    pub const fn set_expect_continue_timeout(mut self, timeout: Duration) -> Self {
        self.expect_continue_timeout = timeout;
        self
    }

    /// Returns the idle timeout for streaming-response transport I/O.
    pub const fn connection_io_timeout(&self) -> Duration {
        self.connection_io_timeout
    }

    /// Sets the idle timeout for each streaming-response transport operation.
    ///
    /// A zero duration is rejected when the client is constructed.
    pub const fn set_connection_io_timeout(mut self, timeout: Duration) -> Self {
        self.connection_io_timeout = timeout;
        self
    }

    /// Returns how long an idle connection remains eligible for reuse.
    ///
    /// A zero duration means idle connection reuse is disabled.
    pub const fn pool_idle_timeout(&self) -> Duration {
        self.pool_idle_timeout
    }

    /// Sets how long an idle connection remains eligible for reuse.
    ///
    /// A zero duration disables idle connection reuse.
    pub const fn set_pool_idle_timeout(mut self, timeout: Duration) -> Self {
        self.pool_idle_timeout = timeout;
        self
    }

    /// Returns the maximum number of idle connections retained per pool key.
    ///
    /// A value of zero means idle connection reuse is disabled.
    pub const fn pool_max_idle_per_key(&self) -> usize {
        self.pool_max_idle_per_key
    }

    /// Sets the maximum number of idle connections retained per pool key.
    ///
    /// A value of zero disables idle connection retention.
    pub const fn set_pool_max_idle_per_key(mut self, limit: usize) -> Self {
        self.pool_max_idle_per_key = limit;
        self
    }

    /// Returns the maximum number of idle connections retained by this client.
    ///
    /// A value of zero means idle connection reuse is disabled.
    pub const fn pool_max_idle_total(&self) -> usize {
        self.pool_max_idle_total
    }

    /// Sets the maximum number of idle connections retained by this client.
    ///
    /// A value of zero disables idle connection retention.
    pub const fn set_pool_max_idle_total(mut self, limit: usize) -> Self {
        self.pool_max_idle_total = limit;
        self
    }

    /// Returns the TLS configuration used for `https` requests.
    #[cfg(feature = "tls")]
    pub const fn tls(&self) -> Option<&TlsClientConfig> {
        self.tls.as_ref()
    }

    /// Sets the TLS context and ALPN protocols used for `https` requests.
    #[cfg(feature = "tls")]
    pub fn set_tls(mut self, tls: TlsClientConfig) -> Self {
        self.tls = Some(tls);
        self
    }

    pub(super) fn validate(&self) -> Result<()> {
        validate_limits(self.limits)?;
        if Instant::now()
            .checked_add(self.expect_continue_timeout)
            .is_none()
        {
            return Err(Error::InvalidConfiguration(
                "Expect: 100-continue timeout is too large",
            ));
        }
        if self.connection_io_timeout.is_zero() {
            return Err(Error::InvalidConfiguration(
                "connection I/O timeout must be greater than zero",
            ));
        }
        if Instant::now()
            .checked_add(self.connection_io_timeout)
            .is_none()
        {
            return Err(Error::InvalidConfiguration(
                "connection I/O timeout is too large",
            ));
        }
        Ok(())
    }
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Configuration for an HTTP server.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    limits: Limits,
    listen_backlog: u32,
    connection_io_timeout: Duration,
    graceful_shutdown_timeout: Duration,
    max_connections: usize,
    #[cfg(feature = "tls")]
    tls: Option<TlsServerConfig>,
}

impl ServerConfig {
    /// Creates a server configuration with default message limits, at most
    /// 1,000 HTTP/1 requests per connection, a 30-second connection I/O
    /// timeout, a 30-second graceful-shutdown timeout, and at most 1,024
    /// concurrent connections.
    pub const fn new() -> Self {
        Self {
            limits: Limits::new(),
            listen_backlog: DEFAULT_LISTEN_BACKLOG,
            connection_io_timeout: DEFAULT_CONNECTION_IO_TIMEOUT,
            graceful_shutdown_timeout: DEFAULT_GRACEFUL_SHUTDOWN_TIMEOUT,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            #[cfg(feature = "tls")]
            tls: None,
        }
    }

    /// Returns the configured limits.
    pub const fn limits(&self) -> Limits {
        self.limits
    }

    /// Sets the HTTP message and connection limits.
    pub const fn set_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Returns the kernel listen backlog.
    pub const fn listen_backlog(&self) -> u32 {
        self.listen_backlog
    }

    /// Sets the kernel listen backlog.
    ///
    /// A value of zero, or one larger than the platform supports, is rejected
    /// when the server is constructed.
    pub const fn set_listen_backlog(mut self, backlog: u32) -> Self {
        self.listen_backlog = backlog;
        self
    }

    /// Returns the maximum time a connection read or write may remain idle.
    pub const fn connection_io_timeout(&self) -> Duration {
        self.connection_io_timeout
    }

    /// Sets the maximum time a connection read or write may remain idle.
    ///
    /// A zero duration is rejected when the server is constructed.
    pub const fn set_connection_io_timeout(mut self, timeout: Duration) -> Self {
        self.connection_io_timeout = timeout;
        self
    }

    /// Returns how long shutdown waits for active handlers to finish.
    pub const fn graceful_shutdown_timeout(&self) -> Duration {
        self.graceful_shutdown_timeout
    }

    /// Sets how long shutdown waits for active handlers to finish.
    ///
    /// After this duration, connection tasks are canceled. A zero duration is
    /// rejected when the server is constructed.
    pub const fn set_graceful_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.graceful_shutdown_timeout = timeout;
        self
    }

    /// Returns the maximum number of concurrently active connections.
    pub const fn max_connections(&self) -> usize {
        self.max_connections
    }

    /// Sets the maximum number of concurrently active connections.
    ///
    /// A value of zero is rejected when the server is constructed.
    pub const fn set_max_connections(mut self, max_connections: usize) -> Self {
        self.max_connections = max_connections;
        self
    }

    /// Returns the maximum number of concurrently active HTTP/2 streams.
    pub const fn max_active_streams(&self) -> usize {
        self.limits.max_active_streams()
    }

    /// Sets the maximum number of concurrently active HTTP/2 streams.
    ///
    /// A value of zero is rejected when the server is constructed.
    pub const fn set_max_active_streams(mut self, max_active_streams: usize) -> Self {
        self.limits = self.limits.set_max_active_streams(max_active_streams);
        self
    }

    /// Returns the maximum number of HTTP/1 requests served by one connection.
    pub const fn max_requests_per_connection(&self) -> usize {
        self.limits.max_requests_per_connection()
    }

    /// Sets the maximum number of HTTP/1 requests served by one connection.
    ///
    /// The final response is completed with `Connection: close`. A value of
    /// zero is rejected when the server is constructed.
    pub const fn set_max_requests_per_connection(
        mut self,
        max_requests_per_connection: usize,
    ) -> Self {
        self.limits = self
            .limits
            .set_max_requests_per_connection(max_requests_per_connection);
        self
    }

    /// Returns the TLS configuration used for accepted connections.
    #[cfg(feature = "tls")]
    pub const fn tls(&self) -> Option<&TlsServerConfig> {
        self.tls.as_ref()
    }

    /// Enables TLS and ALPN on accepted connections.
    #[cfg(feature = "tls")]
    pub fn set_tls(mut self, tls: TlsServerConfig) -> Self {
        self.tls = Some(tls);
        self
    }

    pub(super) fn validate(&self) -> Result<()> {
        validate_limits(self.limits)?;
        if self.listen_backlog == 0 || self.listen_backlog > i32::MAX as u32 {
            return Err(Error::InvalidConfiguration(
                "listen backlog must be between 1 and i32::MAX",
            ));
        }
        if self.connection_io_timeout.is_zero() {
            return Err(Error::InvalidConfiguration(
                "connection I/O timeout must be greater than zero",
            ));
        }
        if Instant::now()
            .checked_add(self.connection_io_timeout)
            .is_none()
        {
            return Err(Error::InvalidConfiguration(
                "connection I/O timeout is too large",
            ));
        }
        if self.graceful_shutdown_timeout.is_zero() {
            return Err(Error::InvalidConfiguration(
                "graceful shutdown timeout must be greater than zero",
            ));
        }
        if Instant::now()
            .checked_add(self.graceful_shutdown_timeout)
            .is_none()
        {
            return Err(Error::InvalidConfiguration(
                "graceful shutdown timeout is too large",
            ));
        }
        if self.max_connections == 0 {
            return Err(Error::InvalidConfiguration(
                "maximum concurrent connections must be greater than zero",
            ));
        }
        Ok(())
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self::new()
    }
}

fn validate_limits(limits: Limits) -> Result<()> {
    if limits.max_active_streams == 0 {
        return Err(Error::InvalidConfiguration(
            "maximum active HTTP/2 streams must be greater than zero",
        ));
    }
    if limits.max_requests_per_connection == 0 {
        return Err(Error::InvalidConfiguration(
            "maximum HTTP/1 requests per connection must be greater than zero",
        ));
    }
    if limits.read_buffer_bytes == 0 {
        return Err(Error::InvalidConfiguration(
            "read buffer size must be greater than zero",
        ));
    }
    if limits.max_header_bytes > limits.read_buffer_bytes {
        return Err(Error::InvalidConfiguration(
            "read buffer size must be at least the maximum header size",
        ));
    }
    Ok(())
}

/// A stable category for an HTTP protocol failure.
///
/// Match this value instead of parsing an error's display text. Transport
/// failures remain [`Error::Io`], configured limits retain their dedicated
/// [`Error`] variants, and server task failures remain structured in
/// [`ServeError`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProtocolErrorKind {
    /// The peer sent a syntactically malformed message.
    MalformedMessage,
    /// Message or frame boundaries were invalid.
    InvalidFraming,
    /// A header or pseudo-header was invalid.
    InvalidHeader,
    /// A content-length value was invalid or inconsistent.
    InvalidContentLength,
    /// HTTP/2 flow-control rules were violated.
    FlowControlViolation,
    /// The peer reset the active HTTP/2 stream.
    PeerReset,
    /// The peer ended the HTTP/2 connection with GOAWAY.
    PeerGoaway,
    /// The message requested a protocol feature this API does not support.
    UnsupportedFeature,
    /// An event was not valid in the current adapter operation.
    UnexpectedEvent,
    /// The protocol adapter or connection state was inconsistent.
    InvalidState,
    /// HTTP/2 header compression failed.
    Compression,
}

impl fmt::Display for ProtocolErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::MalformedMessage => "malformed HTTP message",
            Self::InvalidFraming => "invalid HTTP framing",
            Self::InvalidHeader => "invalid HTTP header",
            Self::InvalidContentLength => "invalid HTTP content length",
            Self::FlowControlViolation => "HTTP/2 flow-control violation",
            Self::PeerReset => "HTTP/2 peer reset",
            Self::PeerGoaway => "HTTP/2 peer GOAWAY",
            Self::UnsupportedFeature => "unsupported HTTP feature",
            Self::UnexpectedEvent => "unexpected HTTP event",
            Self::InvalidState => "invalid HTTP connection state",
            Self::Compression => "HTTP/2 header compression error",
        })
    }
}

/// A structured HTTP protocol failure.
///
/// [`ProtocolError::kind`] is the stable matching contract. Display text is
/// diagnostic and may gain detail without changing the category.
#[derive(Debug)]
pub struct ProtocolError {
    kind: ProtocolErrorKind,
    detail: String,
    source: Option<ProtocolErrorSource>,
}

impl ProtocolError {
    pub(crate) fn new(kind: ProtocolErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            source: None,
        }
    }

    pub(crate) fn with_source(
        kind: ProtocolErrorKind,
        detail: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            detail: detail.into(),
            source: Some(ProtocolErrorSource(source.into())),
        }
    }

    /// Returns the stable category of this failure.
    pub const fn kind(&self) -> ProtocolErrorKind {
        self.kind
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.kind, self.detail)
    }
}

impl error::Error for ProtocolError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| source as &(dyn error::Error + 'static))
    }
}

#[derive(Debug)]
struct ProtocolErrorSource(String);

impl fmt::Display for ProtocolErrorSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl error::Error for ProtocolErrorSource {}

/// An HTTP client or server error.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// A URI string was malformed.
    InvalidUri(::http::uri::InvalidUri),
    /// The URI scheme is not supported.
    UnsupportedScheme(Option<String>),
    /// An absolute URI did not contain an authority.
    MissingAuthority,
    /// Name or address resolution failed.
    AddressResolution {
        /// The authority that could not be resolved.
        authority: String,
        /// The resolver error.
        source: io::Error,
    },
    /// The selected HTTP version is not supported.
    UnsupportedVersion(Version),
    /// An `https` request was made without client TLS configuration.
    TlsNotConfigured,
    /// TLS negotiated an ALPN protocol this HTTP adapter does not support.
    #[cfg(feature = "tls")]
    UnsupportedAlpnProtocol(Vec<u8>),
    /// The requested HTTP/2 protocol was not selected by TLS ALPN.
    AlpnProtocolMismatch {
        /// The version selected on the request.
        requested: Version,
        /// The version selected by ALPN.
        negotiated: Version,
    },
    /// A request or response could not be constructed.
    InvalidMessage(::http::Error),
    /// A header name was invalid.
    InvalidHeaderName(::http::header::InvalidHeaderName),
    /// A header value was invalid.
    InvalidHeaderValue(::http::header::InvalidHeaderValue),
    /// A runtime transport operation failed.
    Io(Errno),
    /// An outbound streaming body failed while producing a chunk.
    BodyStream {
        /// The stream's original error.
        source: Box<dyn error::Error + Send + Sync + 'static>,
    },
    /// Encoded headers exceeded the configured bound.
    HeadersTooLarge {
        /// The configured limit.
        limit: usize,
        /// The observed size, when known.
        actual: Option<usize>,
    },
    /// The number of header occurrences exceeded the configured bound.
    TooManyHeaders {
        /// The configured limit.
        limit: usize,
        /// The observed number of headers, when known.
        actual: Option<usize>,
    },
    /// A buffered body exceeded the configured bound.
    BodyTooLarge {
        /// The configured limit.
        limit: usize,
        /// The observed size, when known.
        actual: Option<usize>,
    },
    /// The bounded connection input buffer filled before progress was possible.
    InputBufferFull {
        /// The configured buffer size.
        limit: usize,
    },
    /// The peer closed the connection before completing the HTTP message.
    UnexpectedEof,
    /// HTTP protocol processing failed.
    Protocol(ProtocolError),
    /// The operation was canceled.
    Canceled,
    /// A configuration value was outside its supported range.
    InvalidConfiguration(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUri(error) => write!(formatter, "invalid URI: {error}"),
            Self::UnsupportedScheme(Some(scheme)) => {
                write!(
                    formatter,
                    "unsupported URI scheme `{scheme}`; expected `http` or `https`"
                )
            }
            Self::UnsupportedScheme(None) => {
                formatter.write_str("URI scheme is required; expected `http` or `https`")
            }
            Self::MissingAuthority => formatter.write_str("URI authority is required"),
            Self::AddressResolution { authority, source } => {
                write!(formatter, "failed to resolve `{authority}`: {source}")
            }
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported HTTP version {version:?}")
            }
            Self::TlsNotConfigured => {
                formatter.write_str("HTTPS requires a configured TLS client context")
            }
            #[cfg(feature = "tls")]
            Self::UnsupportedAlpnProtocol(protocol) => write!(
                formatter,
                "TLS negotiated unsupported ALPN protocol `{}`",
                String::from_utf8_lossy(protocol)
            ),
            Self::AlpnProtocolMismatch {
                requested,
                negotiated,
            } => write!(
                formatter,
                "requested HTTP version {requested:?}, but TLS ALPN selected {negotiated:?}"
            ),
            Self::InvalidMessage(error) => write!(formatter, "invalid HTTP message: {error}"),
            Self::InvalidHeaderName(error) => write!(formatter, "invalid header name: {error}"),
            Self::InvalidHeaderValue(error) => write!(formatter, "invalid header value: {error}"),
            Self::Io(error) => write!(formatter, "HTTP transport error: {error}"),
            Self::BodyStream { source } => write!(formatter, "HTTP body stream failed: {source}"),
            Self::HeadersTooLarge { limit, actual } => {
                display_limit(formatter, "headers", *limit, *actual)
            }
            Self::TooManyHeaders { limit, actual } => {
                display_limit(formatter, "header count", *limit, *actual)
            }
            Self::BodyTooLarge { limit, actual } => {
                display_limit(formatter, "body", *limit, *actual)
            }
            Self::InputBufferFull { limit } => {
                write!(
                    formatter,
                    "connection input exceeds the {limit}-byte buffer"
                )
            }
            Self::UnexpectedEof => {
                formatter.write_str("connection closed before the HTTP message completed")
            }
            Self::Protocol(error) => write!(formatter, "HTTP protocol error: {error}"),
            Self::Canceled => formatter.write_str("HTTP operation canceled"),
            Self::InvalidConfiguration(message) => {
                write!(formatter, "invalid HTTP configuration: {message}")
            }
        }
    }
}

fn display_limit(
    formatter: &mut fmt::Formatter<'_>,
    name: &str,
    limit: usize,
    actual: Option<usize>,
) -> fmt::Result {
    match actual {
        Some(actual) => write!(
            formatter,
            "{name} size {actual} exceeds the configured limit of {limit}"
        ),
        None => write!(formatter, "{name} exceed the configured limit of {limit}"),
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Self::InvalidUri(error) => Some(error),
            Self::AddressResolution { source, .. } => Some(source),
            Self::InvalidMessage(error) => Some(error),
            Self::InvalidHeaderName(error) => Some(error),
            Self::InvalidHeaderValue(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::BodyStream { source } => Some(source.as_ref()),
            Self::Protocol(error) => Some(error),
            _ => None,
        }
    }
}

impl From<::http::uri::InvalidUri> for Error {
    fn from(error: ::http::uri::InvalidUri) -> Self {
        Self::InvalidUri(error)
    }
}

impl From<::http::Error> for Error {
    fn from(error: ::http::Error) -> Self {
        Self::InvalidMessage(error)
    }
}

impl From<::http::header::InvalidHeaderName> for Error {
    fn from(error: ::http::header::InvalidHeaderName) -> Self {
        Self::InvalidHeaderName(error)
    }
}

impl From<::http::header::InvalidHeaderValue> for Error {
    fn from(error: ::http::header::InvalidHeaderValue) -> Self {
        Self::InvalidHeaderValue(error)
    }
}

impl From<Errno> for Error {
    fn from(error: Errno) -> Self {
        Self::Io(error)
    }
}

/// Converts an FSM-reported error into the adapter's public [`Error`].
///
/// This is a crate-private extension trait rather than a set of
/// `From<kimojio_fsm_http::…> for Error` impls so that no
/// `kimojio-fsm-http` type appears in the public API or rustdoc of
/// [`Error`]. The state machine is a hidden implementation detail, and
/// public trait impls would leak it and couple the two crates' semver.
pub(super) trait IntoHttpError {
    fn into_http_error(self) -> Error;
}

impl IntoHttpError for HttpErrorInfo {
    fn into_http_error(self) -> Error {
        let info = self;
        let limit = info.limit();
        match info.kind() {
            HttpErrorKind::HeadersTooLarge => Error::HeadersTooLarge {
                limit: limit.map_or(0, |violation| violation.limit()),
                actual: limit.and_then(|violation| violation.actual()),
            },
            HttpErrorKind::TooManyHeaders => Error::TooManyHeaders {
                limit: limit.map_or(0, |violation| violation.limit()),
                actual: limit.and_then(|violation| violation.actual()),
            },
            HttpErrorKind::BodyTooLarge => Error::BodyTooLarge {
                limit: limit.map_or(0, |violation| violation.limit()),
                actual: limit.and_then(|violation| violation.actual()),
            },
            kind => {
                let kind = match kind {
                    HttpErrorKind::MalformedMessage => ProtocolErrorKind::MalformedMessage,
                    HttpErrorKind::InvalidFraming => ProtocolErrorKind::InvalidFraming,
                    HttpErrorKind::InvalidHeader => ProtocolErrorKind::InvalidHeader,
                    HttpErrorKind::InvalidContentLength => ProtocolErrorKind::InvalidContentLength,
                    HttpErrorKind::UnsupportedFeature => ProtocolErrorKind::UnsupportedFeature,
                    HttpErrorKind::Compression => ProtocolErrorKind::Compression,
                    HttpErrorKind::FlowControlViolation => ProtocolErrorKind::FlowControlViolation,
                    HttpErrorKind::PeerReset => ProtocolErrorKind::PeerReset,
                    HttpErrorKind::PeerGoaway => ProtocolErrorKind::PeerGoaway,
                    HttpErrorKind::InvalidState | HttpErrorKind::NeedMoreInput => {
                        ProtocolErrorKind::InvalidState
                    }
                    _ => ProtocolErrorKind::InvalidState,
                };
                Error::Protocol(ProtocolError::with_source(
                    kind,
                    info.detail(),
                    "HTTP protocol state-machine processing failed",
                ))
            }
        }
    }
}

impl IntoHttpError for FsmHttp1Error {
    fn into_http_error(self) -> Error {
        self.classify().into_http_error()
    }
}

impl IntoHttpError for FsmServerError {
    fn into_http_error(self) -> Error {
        self.classify().into_http_error()
    }
}

impl IntoHttpError for FsmH2ProtocolError {
    fn into_http_error(self) -> Error {
        self.classify().into_http_error()
    }
}

/// Maps a state-machine `Result` into the adapter's public [`Result`].
///
/// Call sites use `.into_http()?` instead of a bare `?` because the
/// `From` impls that would make `?` work are deliberately absent: they
/// would expose `kimojio-fsm-http` types on [`Error`]'s public trait-impl
/// list and tie this crate's semver to the state machine's.
pub(super) trait IntoHttpResult<T> {
    fn into_http(self) -> Result<T>;
}

impl<T, E: IntoHttpError> IntoHttpResult<T> for std::result::Result<T, E> {
    fn into_http(self) -> Result<T> {
        self.map_err(IntoHttpError::into_http_error)
    }
}

#[cfg(test)]
mod protocol_error_tests {
    use std::error::Error as _;

    use super::*;

    #[test]
    fn protocol_error_exposes_stable_kind_and_sanitized_source() {
        let error = ProtocolError::with_source(
            ProtocolErrorKind::InvalidFraming,
            "HTTP/2 frame length is invalid",
            "HTTP protocol state-machine processing failed",
        );

        assert_eq!(error.kind(), ProtocolErrorKind::InvalidFraming);
        assert_eq!(
            error.to_string(),
            "invalid HTTP framing: HTTP/2 frame length is invalid"
        );
        assert_eq!(
            error.source().unwrap().to_string(),
            "HTTP protocol state-machine processing failed"
        );
        assert!(!format!("{error:?}").contains("kimojio_fsm_http"));

        let top = Error::Protocol(error);
        assert!(top.source().is_some());
    }
}

/// The result type used by this module.
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug)]
pub(super) struct UriTarget {
    pub(super) authority: String,
    pub(super) request_target: String,
    pub(super) tls: bool,
    host: String,
    port: u16,
}

impl UriTarget {
    pub(super) fn parse(uri: &Uri) -> Result<Self> {
        let scheme = uri.scheme_str();
        if !matches!(scheme, Some("http" | "https")) {
            return Err(Error::UnsupportedScheme(scheme.map(str::to_owned)));
        }
        let tls = scheme == Some("https");

        let authority = uri.authority().ok_or(Error::MissingAuthority)?;
        let host = authority.host();
        if host.is_empty() {
            return Err(Error::MissingAuthority);
        }

        Ok(Self {
            authority: authority.as_str().to_owned(),
            request_target: uri
                .path_and_query()
                .map_or("/", |path| path.as_str())
                .to_owned(),
            tls,
            host: host.to_owned(),
            port: authority.port_u16().unwrap_or(if tls { 443 } else { 80 }),
        })
    }

    pub(super) const fn scheme(&self) -> &'static str {
        if self.tls { "https" } else { "http" }
    }

    async fn resolve(&self) -> Result<Vec<SocketAddr>> {
        let addresses = crate::resolver::resolve(&self.host, self.port)
            .await
            .map_err(|source| Error::AddressResolution {
                authority: self.authority.clone(),
                source: io::Error::other(source),
            })?;

        if addresses.is_empty() {
            return Err(Error::AddressResolution {
                authority: self.authority.clone(),
                source: io::Error::new(io::ErrorKind::NotFound, "no addresses returned"),
            });
        }
        Ok(addresses)
    }
}

pub(super) async fn connect(target: &UriTarget) -> Result<OwnedFdStream> {
    let mut last_error = None;
    for address in target.resolve().await? {
        match crate::socket_helpers::create_client_socket(&address).await {
            Ok(socket) => return Ok(OwnedFdStream::new(socket)),
            Err(error) => last_error = Some(error),
        }
    }

    Err(Error::Io(
        last_error.expect("URI resolution returned at least one address"),
    ))
}

pub(super) async fn bind_listener(
    address: SocketAddr,
    config: &ServerConfig,
) -> Result<(OwnedFd, SocketAddr)> {
    config.validate()?;
    let family = if address.is_ipv4() {
        AddressFamily::INET
    } else {
        AddressFamily::INET6
    };
    let socket = operations::socket(family, SocketType::STREAM, Some(ipproto::TCP)).await?;
    rustix::net::sockopt::set_socket_reuseaddr(&socket, true)?;
    rustix::net::sockopt::set_tcp_nodelay(&socket, true)?;
    operations::bind(&socket, &address)?;
    operations::listen(&socket, config.listen_backlog as i32)?;
    let local_address = socket_address(&socket)?;
    Ok((socket, local_address))
}

fn socket_address(socket: &OwnedFd) -> Result<SocketAddr> {
    Ok(rustix::net::getsockname(socket)?.try_into()?)
}

pub(super) struct ReadBuffer {
    bytes: Vec<u8>,
    start: usize,
    end: usize,
}

impl ReadBuffer {
    pub(super) fn new(limit: usize) -> Result<Self> {
        if limit == 0 {
            return Err(Error::InvalidConfiguration(
                "read buffer size must be greater than zero",
            ));
        }
        Ok(Self {
            bytes: vec![0; limit],
            start: 0,
            end: 0,
        })
    }

    pub(super) fn available(&self) -> &[u8] {
        &self.bytes[self.start..self.end]
    }

    pub(super) fn consume(&mut self, amount: usize) {
        assert!(amount <= self.end - self.start);
        self.start += amount;
        if self.start == self.end {
            self.start = 0;
            self.end = 0;
        }
    }

    pub(super) fn spare_mut(&mut self) -> Result<&mut [u8]> {
        if self.end == self.bytes.len() && self.start != 0 {
            self.bytes.copy_within(self.start..self.end, 0);
            self.end -= self.start;
            self.start = 0;
        }
        if self.end == self.bytes.len() {
            return Err(Error::InputBufferFull {
                limit: self.bytes.len(),
            });
        }
        Ok(&mut self.bytes[self.end..])
    }

    pub(super) fn commit_read(&mut self, amount: usize) {
        assert!(amount <= self.bytes.len() - self.end);
        self.end += amount;
    }

    pub(super) async fn read_from_with_deadline(
        &mut self,
        stream: &mut impl AsyncStreamRead,
        deadline: Option<Instant>,
    ) -> Result<usize> {
        let amount = stream.try_read(self.spare_mut()?, deadline).await?;
        self.commit_read(amount);
        Ok(amount)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_conversions_preserve_bytes() {
        let body = Body::from("hello");
        assert_eq!(body.as_bytes(), b"hello");
        assert_eq!(body.into_bytes(), b"hello");
    }

    #[crate::test]
    async fn buffered_body_yields_one_owned_chunk() {
        let mut body = Body::from("buffered");
        assert_eq!(body.next_chunk().await.unwrap(), Some(b"buffered".to_vec()));
        assert_eq!(body.next_chunk().await.unwrap(), None);
        assert!(body.as_bytes().is_empty());
    }

    #[test]
    #[should_panic(expected = "a streaming body has no bytes in memory to borrow")]
    fn streaming_body_rejects_borrowed_byte_access() {
        let body = Body::from_chunks(futures::stream::empty::<Vec<u8>>());
        let _ = body.as_bytes();
    }

    /// `Deref` and `AsRef` route through `as_bytes`, so they panic too. A
    /// caller who never named a method still gets a message that explains
    /// which surfaces reach it.
    #[test]
    #[should_panic(expected = "a streaming body has no bytes in memory to borrow")]
    fn streaming_body_rejects_implicit_slicing() {
        let body = Body::from_chunks(futures::stream::empty::<Vec<u8>>());
        let _ = &body[..];
    }

    #[test]
    fn uri_target_parses_http_and_https_absolute_uris() {
        let target = UriTarget::parse(&"http://example.com:8080/a?b=c".parse().unwrap()).unwrap();
        assert_eq!(target.authority, "example.com:8080");
        assert_eq!(target.request_target, "/a?b=c");
        assert_eq!(target.host, "example.com");
        assert_eq!(target.port, 8080);
        assert!(!target.tls);

        let target = UriTarget::parse(&"http://example.com/".parse().unwrap()).unwrap();
        assert_eq!(target.port, 80);
        assert!(!target.tls);

        let target = UriTarget::parse(&"https://example.com/path".parse().unwrap()).unwrap();
        assert_eq!(target.port, 443);
        assert!(target.tls);

        let target = UriTarget::parse(&"https://example.com:8443/".parse().unwrap()).unwrap();
        assert_eq!(target.port, 8443);
        assert!(target.tls);

        assert!(matches!(
            UriTarget::parse(&"/relative".parse().unwrap()),
            Err(Error::UnsupportedScheme(None))
        ));
    }

    #[test]
    fn protocol_versions_are_explicit() {
        assert_eq!(
            Protocol::from_version(Version::HTTP_11).unwrap(),
            Protocol::Http1
        );
        assert_eq!(
            Protocol::from_version(Version::HTTP_2).unwrap(),
            Protocol::Http2PriorKnowledge
        );
        assert!(matches!(
            Protocol::from_version(Version::HTTP_10),
            Err(Error::UnsupportedVersion(Version::HTTP_10))
        ));
    }

    #[test]
    fn client_configuration_has_finite_tunable_timeouts_and_pool_bounds() {
        let config = ClientConfig::new();
        assert_eq!(
            config.expect_continue_timeout(),
            DEFAULT_EXPECT_CONTINUE_TIMEOUT
        );
        assert_eq!(
            config.connection_io_timeout(),
            DEFAULT_CONNECTION_IO_TIMEOUT
        );
        assert_eq!(config.pool_idle_timeout(), DEFAULT_POOL_IDLE_TIMEOUT);
        assert_eq!(
            config.pool_max_idle_per_key(),
            DEFAULT_POOL_MAX_IDLE_PER_KEY
        );
        assert_eq!(config.pool_max_idle_total(), DEFAULT_POOL_MAX_IDLE_TOTAL);
        assert_eq!(
            ClientConfig::default().pool_idle_timeout(),
            DEFAULT_POOL_IDLE_TIMEOUT
        );

        let configured = config
            .set_expect_continue_timeout(Duration::from_millis(250))
            .set_connection_io_timeout(Duration::from_secs(3))
            .set_pool_idle_timeout(Duration::from_secs(7))
            .set_pool_max_idle_per_key(3)
            .set_pool_max_idle_total(11);
        assert_eq!(
            configured.expect_continue_timeout(),
            Duration::from_millis(250)
        );
        assert_eq!(configured.connection_io_timeout(), Duration::from_secs(3));
        assert_eq!(configured.pool_idle_timeout(), Duration::from_secs(7));
        assert_eq!(configured.pool_max_idle_per_key(), 3);
        assert_eq!(configured.pool_max_idle_total(), 11);
        assert!(
            ClientConfig::new()
                .set_expect_continue_timeout(Duration::MAX)
                .validate()
                .is_err()
        );
        assert!(
            ClientConfig::new()
                .set_connection_io_timeout(Duration::ZERO)
                .validate()
                .is_err()
        );
        assert!(
            ClientConfig::new()
                .set_connection_io_timeout(Duration::MAX)
                .validate()
                .is_err()
        );
    }

    #[test]
    fn server_configuration_rejects_unbounded_or_zero_lifecycle_values() {
        let config = ServerConfig::new();
        assert!(!config.connection_io_timeout().is_zero());
        assert!(!config.graceful_shutdown_timeout().is_zero());
        assert!(config.max_connections() > 0);
        assert_eq!(
            config.max_requests_per_connection(),
            DEFAULT_MAX_REQUESTS_PER_CONNECTION
        );

        assert!(
            ServerConfig::new()
                .set_connection_io_timeout(Duration::ZERO)
                .validate()
                .is_err()
        );
        assert!(
            ServerConfig::new()
                .set_graceful_shutdown_timeout(Duration::ZERO)
                .validate()
                .is_err()
        );
        assert!(
            ServerConfig::new()
                .set_max_connections(0)
                .validate()
                .is_err()
        );
        assert!(
            ServerConfig::new()
                .set_max_requests_per_connection(0)
                .validate()
                .is_err()
        );
        assert!(
            ServerConfig::new()
                .set_connection_io_timeout(Duration::MAX)
                .validate()
                .is_err()
        );
    }

    #[test]
    fn configuration_rejects_headers_larger_than_the_read_buffer() {
        let limits = Limits::new()
            .set_read_buffer_bytes(1024)
            .set_max_header_bytes(1025);

        assert!(ClientConfig::new().set_limits(limits).validate().is_err());
        assert!(ServerConfig::new().set_limits(limits).validate().is_err());
    }

    #[test]
    fn request_limit_is_forwarded_to_the_protocol_driver() {
        let limits = Limits::new().set_max_requests_per_connection(17);
        assert_eq!(limits.max_requests_per_connection(), 17);
        assert_eq!(limits.protocol().max_requests_per_connection(), 17);
        assert_eq!(
            ServerConfig::new()
                .set_max_requests_per_connection(23)
                .max_requests_per_connection(),
            23
        );
    }
}
