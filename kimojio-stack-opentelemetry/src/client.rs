// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use kimojio_stack_grpc::ClientConfig;
use kimojio_stack_grpc::{Metadata, UnaryClient};
use kimojio_stack_http::{HttpConfig, StackTransport, h2};
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

pub(crate) struct UnaryExportClient {
    grpc: UnaryClient,
    config: ExportClientConfig,
}

impl UnaryExportClient {
    pub(crate) fn new(http: h2::ClientConnection, config: ExportClientConfig) -> Self {
        Self {
            grpc: UnaryClient::new(http, config.grpc()),
            config,
        }
    }

    pub(crate) fn from_transport(
        transport: StackTransport,
        http_config: HttpConfig,
        config: ExportClientConfig,
    ) -> Self {
        Self::new(h2::ClientConnection::new(transport, http_config), config)
    }

    pub(crate) fn plaintext(
        socket: kimojio_stack::OwnedFd,
        http_config: HttpConfig,
        config: ExportClientConfig,
    ) -> Self {
        Self::from_transport(StackTransport::plaintext(socket), http_config, config)
    }

    #[cfg(feature = "tls")]
    pub(crate) fn tls(
        stream: kimojio_stack_tls::TlsStream,
        http_config: HttpConfig,
        config: ExportClientConfig,
    ) -> Self {
        Self::from_transport(StackTransport::tls(stream), http_config, config)
    }

    pub(crate) fn export<Req, Resp>(
        &mut self,
        cx: &kimojio_stack::RuntimeContext<'_>,
        path: &str,
        request: &Req,
    ) -> Result<Resp, Error>
    where
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

    pub(crate) fn finish(self, cx: &kimojio_stack::RuntimeContext<'_>) -> Result<(), Error> {
        self.grpc.close(cx).map_err(Error::from)
    }
}
