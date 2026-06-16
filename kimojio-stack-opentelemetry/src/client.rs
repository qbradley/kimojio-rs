// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared OTLP unary export client.
//!
//! Runtime I/O is inherited from the contained gRPC unary client and its generic
//! HTTP/2 connection. This module intentionally adds no runtime instrumentation
//! hooks or hidden background export machinery; future hooks should be optional
//! and attach below this layer at the shared runtime socket contract.

use kimojio_stack::{IoFd, RuntimeSocket};
use kimojio_stack_grpc::{ClientConfig, Metadata, RuntimeUnaryClient};
use kimojio_stack_http::{HttpConfig, HttpRuntime, RuntimeStackTransport, StackTransport, h2};
use prost::Message;

use crate::{Error, ExportLimits};

/// Shared configuration for low-level OpenTelemetry export clients.
///
/// Both limits are checked by the client path before writing to the transport:
/// `export_limits` constrains the encoded OTLP protobuf request, and
/// `max_message_len` configures the underlying gRPC frame/message limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExportClientConfig {
    /// Maximum accepted gRPC message length for requests and responses.
    pub max_message_len: usize,
    /// Maximum encoded OTLP request bytes before gRPC framing.
    pub export_limits: ExportLimits,
}

impl Default for ExportClientConfig {
    fn default() -> Self {
        Self {
            max_message_len: 4 * 1024 * 1024,
            export_limits: ExportLimits::default(),
        }
    }
}

impl ExportClientConfig {
    pub(crate) fn grpc(self) -> ClientConfig {
        ClientConfig {
            max_message_len: self.max_message_len,
        }
    }
}

pub(crate) struct RuntimeUnaryExportClient<S> {
    grpc: RuntimeUnaryClient<S>,
    config: ExportClientConfig,
}

pub(crate) type UnaryExportClient = RuntimeUnaryExportClient<IoFd>;

impl<S> RuntimeUnaryExportClient<S> {
    pub(crate) fn new(http: h2::RuntimeClientConnection<S>, config: ExportClientConfig) -> Self {
        Self {
            grpc: RuntimeUnaryClient::new(http, config.grpc()),
            config,
        }
    }

    pub(crate) fn from_transport(
        transport: RuntimeStackTransport<S>,
        http_config: HttpConfig,
        config: ExportClientConfig,
    ) -> Self {
        Self::new(
            h2::RuntimeClientConnection::new(transport, http_config),
            config,
        )
    }

    pub(crate) fn export<Req, Resp>(
        &mut self,
        cx: &impl HttpRuntime<S>,
        path: &str,
        request: &Req,
    ) -> Result<Resp, Error>
    where
        S: RuntimeSocket,
        Req: Message,
        Resp: Message + Default,
    {
        Ok(self
            .grpc
            .call::<_, Resp>(cx, path, Metadata::new(), request)?
            .message)
    }

    pub(crate) fn export_limits(&self) -> ExportLimits {
        self.config.export_limits
    }

    pub(crate) fn finish(self, cx: &impl HttpRuntime<S>) -> Result<(), Error>
    where
        S: RuntimeSocket,
    {
        self.grpc.close(cx).map_err(Error::from)
    }
}

impl UnaryExportClient {
    pub(crate) fn from_stack_transport(
        transport: StackTransport,
        http_config: HttpConfig,
        config: ExportClientConfig,
    ) -> Self {
        Self::from_transport(transport, http_config, config)
    }

    pub(crate) fn plaintext(
        socket: kimojio_stack::OwnedFd,
        http_config: HttpConfig,
        config: ExportClientConfig,
    ) -> Self {
        Self::from_stack_transport(StackTransport::plaintext(socket), http_config, config)
    }

    #[cfg(feature = "tls")]
    pub(crate) fn tls(
        stream: kimojio_stack_tls::TlsStream,
        http_config: HttpConfig,
        config: ExportClientConfig,
    ) -> Self {
        Self::from_stack_transport(StackTransport::tls(stream), http_config, config)
    }
}
