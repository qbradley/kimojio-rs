// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{Request, Response};
use kimojio_stack::RuntimeContext;
use std::time::Instant;

use crate::{Body, Error, HttpConfig, StackTransport, h2, http1};

/// Shared configuration for stackful HTTP clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClientConfig {
    pub http: HttpConfig,
}

/// Protocol-neutral client connection entry point.
#[allow(clippy::large_enum_variant)] // Keep concrete connections inline to avoid heap allocation.
pub enum ClientConnection {
    Http1(http1::ClientConnection),
    Http2(h2::ClientConnection),
}

impl ClientConnection {
    pub fn http1(transport: StackTransport, config: HttpConfig) -> Self {
        Self::Http1(http1::ClientConnection::new(transport, config))
    }

    pub fn http2(transport: StackTransport, config: HttpConfig) -> Self {
        Self::Http2(h2::ClientConnection::new(transport, config))
    }

    pub fn send(
        &mut self,
        cx: &RuntimeContext<'_>,
        request: &Request<Body>,
    ) -> Result<Response<Body>, Error> {
        match self {
            Self::Http1(connection) => connection.send(cx, request),
            Self::Http2(connection) => connection.send(cx, request),
        }
    }

    pub fn set_io_deadline(&mut self, deadline: Option<Instant>) -> Option<Instant> {
        match self {
            Self::Http1(connection) => connection.set_io_deadline(deadline),
            Self::Http2(connection) => connection.set_io_deadline(deadline),
        }
    }

    pub fn send_with_body_chunks<F>(
        &mut self,
        cx: &RuntimeContext<'_>,
        request: &Request<Body>,
        on_chunk: F,
    ) -> Result<Response<Body>, Error>
    where
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

    pub fn send_with_body_chunks_after_headers<H, F>(
        &mut self,
        cx: &RuntimeContext<'_>,
        request: &Request<Body>,
        on_headers: H,
        on_chunk: F,
    ) -> Result<Response<Body>, Error>
    where
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

    pub fn close(self, cx: &RuntimeContext<'_>) -> Result<(), Error> {
        match self {
            Self::Http1(connection) => connection.close(cx),
            Self::Http2(connection) => connection.close(cx),
        }
    }
}
