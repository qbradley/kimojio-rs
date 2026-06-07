// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{Request, Response};
use kimojio_stack::RuntimeContext;

use crate::{Body, Error, HttpConfig, StackTransport, http1};

/// Shared configuration for stackful HTTP clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClientConfig {
    pub http: HttpConfig,
}

/// Protocol-neutral client connection entry point.
pub enum ClientConnection {
    Http1(http1::ClientConnection),
}

impl ClientConnection {
    pub fn http1(transport: StackTransport, config: HttpConfig) -> Self {
        Self::Http1(http1::ClientConnection::new(transport, config))
    }

    pub fn send(
        &mut self,
        cx: &RuntimeContext<'_>,
        request: &Request<Body>,
    ) -> Result<Response<Body>, Error> {
        match self {
            Self::Http1(connection) => connection.send(cx, request),
        }
    }

    pub fn close(self, cx: &RuntimeContext<'_>) -> Result<(), Error> {
        match self {
            Self::Http1(connection) => connection.close(cx),
        }
    }
}
