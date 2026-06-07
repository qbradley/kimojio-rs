// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{Request, Response};
use kimojio_stack::RuntimeContext;

use crate::{Body, Error, HttpConfig, StackTransport, http1};

/// Shared configuration for stackful HTTP servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ServerConfig {
    pub http: HttpConfig,
}

/// Protocol-neutral server connection entry point.
pub enum ServerConnection {
    Http1(http1::ServerConnection),
}

impl ServerConnection {
    pub fn http1(transport: StackTransport, config: HttpConfig) -> Self {
        Self::Http1(http1::ServerConnection::new(transport, config))
    }

    pub fn serve_one(
        &mut self,
        cx: &RuntimeContext<'_>,
        handler: impl FnOnce(Request<Body>) -> Result<Response<Body>, Error>,
    ) -> Result<bool, Error> {
        match self {
            Self::Http1(connection) => connection.serve_one(cx, handler),
        }
    }

    pub fn close(self, cx: &RuntimeContext<'_>) -> Result<(), Error> {
        match self {
            Self::Http1(connection) => connection.close(cx),
        }
    }
}
