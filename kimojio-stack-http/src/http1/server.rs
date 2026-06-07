// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{Request, Response};
use kimojio_stack::RuntimeContext;

use crate::{Body, Error, HttpConfig, StackTransport};

use super::codec;

/// Low-level HTTP/1.1 server connection over a stackful transport.
pub struct ServerConnection {
    transport: StackTransport,
    config: HttpConfig,
    read_buf: Vec<u8>,
}

impl ServerConnection {
    pub fn new(transport: StackTransport, config: HttpConfig) -> Self {
        Self {
            transport,
            config,
            read_buf: Vec::with_capacity(config.read_buffer_size),
        }
    }

    pub fn read_request(
        &mut self,
        cx: &RuntimeContext<'_>,
    ) -> Result<Option<Request<Body>>, Error> {
        codec::read_request(cx, &mut self.transport, &mut self.read_buf, self.config)
            .map(|request| request.map(|request| request.request))
    }

    pub fn write_response(
        &mut self,
        cx: &RuntimeContext<'_>,
        response: &Response<Body>,
    ) -> Result<(), Error> {
        codec::write_response(cx, &mut self.transport, response)
    }

    pub fn serve_one(
        &mut self,
        cx: &RuntimeContext<'_>,
        handler: impl FnOnce(Request<Body>) -> Result<Response<Body>, Error>,
    ) -> Result<bool, Error> {
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

    pub fn close(self, cx: &RuntimeContext<'_>) -> Result<(), Error> {
        self.transport.close(cx)
    }
}
