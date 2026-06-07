// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{Request, Response};
use kimojio_stack::RuntimeContext;

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

    pub fn close(self, cx: &RuntimeContext<'_>) -> Result<(), Error> {
        match self {
            Self::Http1(connection) => connection.close(cx),
            Self::Http2(connection) => connection.close(cx),
        }
    }
}
