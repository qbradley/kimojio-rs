// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{Request, Response};
use kimojio_stack::{IoFd, RuntimeSocket};

use crate::{Body, Error, HttpConfig, HttpRuntime, RuntimeStackTransport};

use super::codec;

/// Low-level HTTP/1.1 server connection over a stackful transport.
pub struct RuntimeServerConnection<S> {
    transport: RuntimeStackTransport<S>,
    config: HttpConfig,
    read_buf: Vec<u8>,
}

/// Stack-core HTTP/1.1 server compatibility alias.
pub type ServerConnection = RuntimeServerConnection<IoFd>;

impl<S> RuntimeServerConnection<S> {
    /// Creates an HTTP/1.1 server over an already-connected transport.
    pub fn new(transport: RuntimeStackTransport<S>, config: HttpConfig) -> Self {
        Self {
            transport,
            config,
            read_buf: Vec::with_capacity(config.read_buffer_size),
        }
    }

    /// Reads the next request, returning `Ok(None)` after clean peer EOF.
    pub fn read_request<R>(&mut self, cx: &R) -> Result<Option<Request<Body>>, Error>
    where
        R: HttpRuntime<S>,
        S: RuntimeSocket,
    {
        codec::read_request(cx, &mut self.transport, &mut self.read_buf, self.config)
            .map(|request| request.map(|request| request.request))
    }

    /// Writes one response on the connection.
    pub fn write_response<R>(&mut self, cx: &R, response: &Response<Body>) -> Result<(), Error>
    where
        R: HttpRuntime<S>,
        S: RuntimeSocket,
    {
        codec::write_response(cx, &mut self.transport, response)
    }

    /// Reads one request, calls `handler`, and writes the returned response.
    ///
    /// Returns `Ok(false)` when the client closed before another request. Returns
    /// `Ok(true)` when the connection can be kept alive for another request.
    pub fn serve_one<R>(
        &mut self,
        cx: &R,
        handler: impl FnOnce(Request<Body>) -> Result<Response<Body>, Error>,
    ) -> Result<bool, Error>
    where
        R: HttpRuntime<S>,
        S: RuntimeSocket,
    {
        let Some(incoming) =
            codec::read_request(cx, &mut self.transport, &mut self.read_buf, self.config)?
        else {
            return Ok(false);
        };
        let close_after_response = incoming.close_after_response;
        let response = handler(incoming.request)?;
        codec::write_response(cx, &mut self.transport, &response)?;
        if close_after_response {
            self.transport.shutdown_write(cx)?;
        }
        Ok(!close_after_response)
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
