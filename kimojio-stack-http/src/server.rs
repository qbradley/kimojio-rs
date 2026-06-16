// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{Request, Response};
use kimojio_stack::{IoFd, RuntimeSocket};

use crate::{Body, Error, HttpConfig, HttpRuntime, RuntimeStackTransport, h2, http1};

/// Shared configuration for stackful HTTP servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ServerConfig {
    /// Protocol limits and buffer sizes used by the selected HTTP implementation.
    pub http: HttpConfig,
}

/// Protocol-neutral server connection entry point.
///
/// This enum mirrors [`crate::client::ClientConnection`], keeping concrete
/// HTTP/1.1 and HTTP/2 server connections inline and avoiding dynamic dispatch.
#[allow(clippy::large_enum_variant)] // Keep concrete connections inline to avoid heap allocation.
pub enum RuntimeServerConnection<S> {
    /// HTTP/1.1 server connection.
    Http1(http1::RuntimeServerConnection<S>),
    /// HTTP/2 server connection.
    Http2(h2::RuntimeServerConnection<S>),
}

/// Stack-core protocol-neutral HTTP server compatibility alias.
pub type ServerConnection = RuntimeServerConnection<IoFd>;

impl<S> RuntimeServerConnection<S> {
    /// Wraps a transport as an HTTP/1.1 server connection.
    pub fn http1(transport: RuntimeStackTransport<S>, config: HttpConfig) -> Self {
        Self::Http1(http1::RuntimeServerConnection::new(transport, config))
    }

    /// Wraps a transport as an HTTP/2 server connection.
    pub fn http2(transport: RuntimeStackTransport<S>, config: HttpConfig) -> Self {
        Self::Http2(h2::RuntimeServerConnection::new(transport, config))
    }

    /// Serves one request if one is available.
    ///
    /// Returns `Ok(false)` after clean peer EOF, otherwise `Ok(true)` after a
    /// request has been handled.
    pub fn serve_one(
        &mut self,
        cx: &impl HttpRuntime<S>,
        handler: impl FnOnce(Request<Body>) -> Result<Response<Body>, Error>,
    ) -> Result<bool, Error>
    where
        S: RuntimeSocket,
    {
        match self {
            Self::Http1(connection) => connection.serve_one(cx, handler),
            Self::Http2(connection) => connection.serve_one(cx, handler),
        }
    }

    /// Closes the underlying transport.
    pub fn close(self, cx: &impl HttpRuntime<S>) -> Result<(), Error>
    where
        S: RuntimeSocket,
    {
        match self {
            Self::Http1(connection) => connection.close(cx),
            Self::Http2(connection) => connection.close(cx),
        }
    }

    /// Gracefully half-closes writes where the protocol supports it, then closes.
    pub fn shutdown_write_and_close_after_peer(self, cx: &impl HttpRuntime<S>) -> Result<(), Error>
    where
        S: RuntimeSocket,
    {
        match self {
            Self::Http1(connection) => connection.close(cx),
            Self::Http2(connection) => connection.shutdown_write_and_close_after_peer(cx),
        }
    }
}
