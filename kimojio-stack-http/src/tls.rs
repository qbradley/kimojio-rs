// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use kimojio_stack::RuntimeContext;
use kimojio_stack_tls::{TlsContext, TlsStream};
use rustix::fd::OwnedFd;

use crate::{Error, StackTransport};

/// HTTP protocol identifiers recognized from TLS ALPN negotiation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpProtocol {
    /// HTTP/1.1 using the `http/1.1` ALPN identifier.
    Http1,
    /// HTTP/2 using the `h2` ALPN identifier.
    Http2,
}

impl HttpProtocol {
    /// Returns the raw, non-length-prefixed ALPN identifier for this protocol.
    pub const fn alpn(self) -> &'static [u8] {
        match self {
            Self::Http1 => b"http/1.1",
            Self::Http2 => b"h2",
        }
    }
}

/// Converts an established stack TLS stream into an HTTP transport.
pub fn transport(stream: TlsStream) -> StackTransport {
    StackTransport::tls(stream)
}

/// Returns the negotiated HTTP protocol from an established TLS stream.
pub fn selected_protocol(stream: &TlsStream) -> Option<HttpProtocol> {
    match stream.ssl().selected_alpn_protocol()? {
        b"http/1.1" => Some(HttpProtocol::Http1),
        b"h2" => Some(HttpProtocol::Http2),
        _ => None,
    }
}

/// Validates that a TLS stream negotiated the expected HTTP ALPN protocol.
pub fn validate_protocol(stream: &TlsStream, expected: HttpProtocol) -> Result<(), Error> {
    if selected_protocol(stream) == Some(expected) {
        Ok(())
    } else {
        Err(Error::Protocol("unexpected TLS ALPN protocol"))
    }
}

/// Performs a client-side TLS handshake and returns an HTTP transport.
pub fn client_transport(
    cx: &RuntimeContext<'_>,
    context: &TlsContext,
    buffer_size: usize,
    socket: OwnedFd,
    expected: Option<HttpProtocol>,
) -> Result<StackTransport, Error> {
    let stream = context
        .client(cx, buffer_size, socket)
        .map_err(Error::tls)?;
    if let Some(expected) = expected {
        validate_protocol(&stream, expected)?;
    }
    Ok(transport(stream))
}

/// Performs a server-side TLS handshake and returns an HTTP transport.
pub fn server_transport(
    cx: &RuntimeContext<'_>,
    context: &TlsContext,
    buffer_size: usize,
    socket: OwnedFd,
    expected: Option<HttpProtocol>,
) -> Result<StackTransport, Error> {
    let stream = context
        .server(cx, buffer_size, socket)
        .map_err(Error::tls)?;
    if let Some(expected) = expected {
        validate_protocol(&stream, expected)?;
    }
    Ok(transport(stream))
}
