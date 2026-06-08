// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{Request, Response};
use kimojio_stack::RuntimeContext;
use std::time::Instant;

use crate::{Body, Error, HttpConfig, StackTransport};

use super::codec;

/// Low-level HTTP/1.1 client connection over a stackful transport.
pub struct ClientConnection {
    transport: StackTransport,
    config: HttpConfig,
    read_buf: Vec<u8>,
}

impl ClientConnection {
    pub fn new(transport: StackTransport, config: HttpConfig) -> Self {
        Self {
            transport,
            config,
            read_buf: Vec::with_capacity(config.read_buffer_size),
        }
    }

    pub fn set_io_deadline(&mut self, deadline: Option<Instant>) -> Option<Instant> {
        self.transport.set_io_deadline(deadline)
    }

    pub fn send(
        &mut self,
        cx: &RuntimeContext<'_>,
        request: &Request<Body>,
    ) -> Result<Response<Body>, Error> {
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

    pub fn send_with_body_chunks<F>(
        &mut self,
        cx: &RuntimeContext<'_>,
        request: &http::Request<crate::Body>,
        on_chunk: F,
    ) -> Result<http::Response<crate::Body>, crate::Error>
    where
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

    pub fn send_with_body_chunks_after_headers<H, F>(
        &mut self,
        cx: &RuntimeContext<'_>,
        request: &http::Request<crate::Body>,
        on_headers: H,
        on_chunk: F,
    ) -> Result<http::Response<crate::Body>, crate::Error>
    where
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

    pub fn close(self, cx: &RuntimeContext<'_>) -> Result<(), Error> {
        self.transport.close(cx)
    }
}
