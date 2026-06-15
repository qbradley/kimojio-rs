// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{Request, Response};
use kimojio_stack::{IoFd, RuntimeSocket};
use std::time::Instant;

use crate::{Body, Error, HttpConfig, HttpRuntime, RuntimeStackTransport};

use super::codec;

/// Low-level HTTP/1.1 client connection over a stackful transport.
pub struct RuntimeClientConnection<S> {
    transport: RuntimeStackTransport<S>,
    config: HttpConfig,
    read_buf: Vec<u8>,
}

/// Stack-core HTTP/1.1 client compatibility alias.
pub type ClientConnection = RuntimeClientConnection<IoFd>;

impl<S> RuntimeClientConnection<S> {
    /// Creates an HTTP/1.1 client over an already-connected transport.
    pub fn new(transport: RuntimeStackTransport<S>, config: HttpConfig) -> Self {
        Self {
            transport,
            config,
            read_buf: Vec::with_capacity(config.read_buffer_size),
        }
    }

    /// Sets an I/O deadline on the underlying transport and returns the previous value.
    pub fn set_io_deadline(&mut self, deadline: Option<Instant>) -> Option<Instant> {
        self.transport.set_io_deadline(deadline)
    }

    /// Sends one request and buffers the complete response body.
    ///
    /// Connection-close semantics are derived from the request and response
    /// headers. If the peer indicates close-after-response, the write side is
    /// shut down before returning.
    pub fn send<R>(&mut self, cx: &R, request: &Request<Body>) -> Result<Response<Body>, Error>
    where
        R: HttpRuntime<S>,
        S: RuntimeSocket,
    {
        codec::write_request_head_and_body(cx, &mut self.transport, request)?;
        let response = codec::read_response(
            cx,
            &mut self.transport,
            &mut self.read_buf,
            self.config,
            request.method(),
        )?;
        if response.close_after_response {
            self.transport.shutdown_write(cx)?;
        }
        Ok(response.response)
    }

    /// Sends one request and delivers response body chunks incrementally.
    ///
    /// The returned response has an empty body.
    pub fn send_with_body_chunks<R, F>(
        &mut self,
        cx: &R,
        request: &http::Request<crate::Body>,
        on_chunk: F,
    ) -> Result<http::Response<crate::Body>, crate::Error>
    where
        R: HttpRuntime<S>,
        S: RuntimeSocket,
        F: FnMut(bytes::Bytes) -> Result<(), crate::Error>,
    {
        codec::write_request_head_and_body(cx, &mut self.transport, request)?;
        let response = codec::read_response_with_body_chunks(
            cx,
            &mut self.transport,
            &mut self.read_buf,
            self.config,
            request.method(),
            on_chunk,
        )?;
        if response.close_after_response {
            self.transport.shutdown_write(cx)?;
        }
        Ok(response.response)
    }

    /// Sends one request, exposes the response headers, then optionally streams body chunks.
    ///
    /// Return `false` from `on_headers` to drain the response body without
    /// invoking `on_chunk`.
    pub fn send_with_body_chunks_after_headers<R, H, F>(
        &mut self,
        cx: &R,
        request: &http::Request<crate::Body>,
        on_headers: H,
        on_chunk: F,
    ) -> Result<http::Response<crate::Body>, crate::Error>
    where
        R: HttpRuntime<S>,
        S: RuntimeSocket,
        H: FnMut(&http::Response<crate::Body>) -> Result<bool, crate::Error>,
        F: FnMut(bytes::Bytes) -> Result<(), crate::Error>,
    {
        codec::write_request_head_and_body(cx, &mut self.transport, request)?;
        let response = codec::read_response_with_body_chunks_after_headers(
            cx,
            &mut self.transport,
            &mut self.read_buf,
            self.config,
            request.method(),
            on_headers,
            on_chunk,
        )?;
        if response.close_after_response {
            self.transport.shutdown_write(cx)?;
        }
        Ok(response.response)
    }

    /// Closes the underlying transport.
    pub fn close<R>(self, cx: &R) -> Result<(), Error>
    where
        R: HttpRuntime<S>,
        S: RuntimeSocket,
    {
        self.transport.close(cx)
    }
}
