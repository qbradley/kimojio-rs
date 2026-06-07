// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{Request, Response};
use kimojio_stack::RuntimeContext;

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

    pub fn send(
        &mut self,
        cx: &RuntimeContext<'_>,
        request: &Request<Body>,
    ) -> Result<Response<Body>, Error> {
        codec::write_request(cx, &mut self.transport, request)?;
        let response =
            codec::read_response(cx, &mut self.transport, &mut self.read_buf, self.config)?;
        if response.close_after_response {
            self.transport.shutdown_write(cx)?;
        }
        Ok(response.response)
    }

    pub fn close(self, cx: &RuntimeContext<'_>) -> Result<(), Error> {
        self.transport.close(cx)
    }
}
