// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{Request, Response};
use kimojio_stack::{IoFd, RuntimeSocket};
use std::time::Instant;

use crate::{Body, Error, HttpConfig, HttpRuntime, RuntimeStackTransport, h2, http1};

/// Shared configuration for stackful HTTP clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClientConfig {
    /// Protocol limits and buffer sizes used by the selected HTTP implementation.
    pub http: HttpConfig,
}

/// Protocol-neutral client connection entry point.
///
/// This enum keeps the concrete HTTP/1.1 or HTTP/2 connection inline so callers
/// can choose a protocol without paying for dynamic dispatch or heap allocation.
#[allow(clippy::large_enum_variant)] // Keep concrete connections inline to avoid heap allocation.
pub enum RuntimeClientConnection<S> {
    /// HTTP/1.1 connection.
    Http1(http1::RuntimeClientConnection<S>),
    /// HTTP/2 connection.
    Http2(h2::RuntimeClientConnection<S>),
}

/// Stack-core protocol-neutral HTTP client compatibility alias.
pub type ClientConnection = RuntimeClientConnection<IoFd>;

impl<S> RuntimeClientConnection<S> {
    /// Wraps a transport as an HTTP/1.1 client connection.
    pub fn http1(transport: RuntimeStackTransport<S>, config: HttpConfig) -> Self {
        Self::Http1(http1::RuntimeClientConnection::new(transport, config))
    }

    /// Wraps a transport as an HTTP/2 client connection.
    pub fn http2(transport: RuntimeStackTransport<S>, config: HttpConfig) -> Self {
        Self::Http2(h2::RuntimeClientConnection::new(transport, config))
    }

    /// Sends one request and buffers the complete response body.
    ///
    /// Use [`send_with_body_chunks`](Self::send_with_body_chunks) when response
    /// bodies can be large or should be processed incrementally.
    pub fn send(
        &mut self,
        cx: &impl HttpRuntime<S>,
        request: &Request<Body>,
    ) -> Result<Response<Body>, Error>
    where
        S: RuntimeSocket,
    {
        match self {
            Self::Http1(connection) => connection.send(cx, request),
            Self::Http2(connection) => connection.send(cx, request),
        }
    }

    /// Sets an I/O deadline on the underlying transport.
    ///
    /// The previous deadline is returned so callers can restore it after a
    /// request-specific timeout.
    pub fn set_io_deadline(&mut self, deadline: Option<Instant>) -> Option<Instant> {
        match self {
            Self::Http1(connection) => connection.set_io_deadline(deadline),
            Self::Http2(connection) => connection.set_io_deadline(deadline),
        }
    }

    /// Sends one request and delivers response body chunks incrementally.
    ///
    /// The returned response has an empty body; body bytes are delivered to
    /// `on_chunk`. If `on_chunk` returns an error, the request fails immediately.
    pub fn send_with_body_chunks<F>(
        &mut self,
        cx: &impl HttpRuntime<S>,
        request: &Request<Body>,
        on_chunk: F,
    ) -> Result<Response<Body>, Error>
    where
        S: RuntimeSocket,
        F: FnMut(bytes::Bytes) -> Result<(), Error>,
    {
        match self {
            Self::Http1(connection) => connection.send_with_body_chunks(cx, request, on_chunk),
            Self::Http2(connection) => {
                let stream_id = connection.send_request(cx, request)?;
                connection
                    .read_response_with_body_chunks(cx, stream_id, on_chunk)
                    .map(|response| response.response)
            }
        }
    }

    /// Sends one request, exposes response headers, then optionally streams body chunks.
    ///
    /// `on_headers` receives the response head with an empty body. Return `true`
    /// to stream the body to `on_chunk`, or `false` to drain/discard the body
    /// while still completing the response.
    pub fn send_with_body_chunks_after_headers<H, F>(
        &mut self,
        cx: &impl HttpRuntime<S>,
        request: &Request<Body>,
        on_headers: H,
        on_chunk: F,
    ) -> Result<Response<Body>, Error>
    where
        S: RuntimeSocket,
        H: FnMut(&Response<Body>) -> Result<bool, Error>,
        F: FnMut(bytes::Bytes) -> Result<(), Error>,
    {
        match self {
            Self::Http1(connection) => {
                connection.send_with_body_chunks_after_headers(cx, request, on_headers, on_chunk)
            }
            Self::Http2(connection) => {
                let stream_id = connection.send_request(cx, request)?;
                connection
                    .read_response_with_body_chunks_after_headers(
                        cx, stream_id, on_headers, on_chunk,
                    )
                    .map(|response| response.response)
            }
        }
    }

    /// Closes the underlying transport through the stack runtime.
    pub fn close(self, cx: &impl HttpRuntime<S>) -> Result<(), Error>
    where
        S: RuntimeSocket,
    {
        match self {
            Self::Http1(connection) => connection.close(cx),
            Self::Http2(connection) => connection.close(cx),
        }
    }
}
