// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use kimojio_stack::{RuntimeContext, SocketIoRuntime};
use kimojio_stack_tls::{RuntimeTlsStream, TlsContext, TlsStream};
use rustix::fd::OwnedFd;

use crate::{Error, RuntimeStackTransport, StackTransport};

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

/// Converts an established runtime TLS stream into a runtime HTTP transport.
pub fn transport_with_runtime<S>(stream: RuntimeTlsStream<S>) -> RuntimeStackTransport<S> {
    RuntimeStackTransport::tls_stream(stream)
}

/// Returns the negotiated HTTP protocol from an established TLS stream.
pub fn selected_protocol<S>(stream: &RuntimeTlsStream<S>) -> Option<HttpProtocol> {
    match stream.ssl().selected_alpn_protocol()? {
        b"http/1.1" => Some(HttpProtocol::Http1),
        b"h2" => Some(HttpProtocol::Http2),
        _ => None,
    }
}

/// Validates that a TLS stream negotiated the expected HTTP ALPN protocol.
pub fn validate_protocol<S>(
    stream: &RuntimeTlsStream<S>,
    expected: HttpProtocol,
) -> Result<(), Error> {
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
    server_name: &str,
    expected: Option<HttpProtocol>,
) -> Result<StackTransport, Error> {
    let stream = context
        .client_with_runtime(cx, buffer_size, socket, server_name)
        .map_err(Error::tls)?;
    if let Some(expected) = expected {
        validate_protocol(&stream, expected)?;
    }
    Ok(transport(stream))
}

/// Performs a client-side TLS handshake through a runtime adapter and returns an HTTP transport.
pub fn client_transport_with_runtime<R>(
    cx: &R,
    context: &TlsContext,
    buffer_size: usize,
    socket: OwnedFd,
    server_name: &str,
    expected: Option<HttpProtocol>,
) -> Result<RuntimeStackTransport<R::Socket>, Error>
where
    R: SocketIoRuntime,
{
    let stream = context
        .client_with_runtime(cx, buffer_size, socket, server_name)
        .map_err(Error::tls)?;
    if let Some(expected) = expected {
        validate_protocol(&stream, expected)?;
    }
    Ok(transport_with_runtime(stream))
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
        .server_with_runtime(cx, buffer_size, socket)
        .map_err(Error::tls)?;
    if let Some(expected) = expected {
        validate_protocol(&stream, expected)?;
    }
    Ok(transport(stream))
}

/// Performs a server-side TLS handshake through a runtime adapter and returns an HTTP transport.
pub fn server_transport_with_runtime<R>(
    cx: &R,
    context: &TlsContext,
    buffer_size: usize,
    socket: OwnedFd,
    expected: Option<HttpProtocol>,
) -> Result<RuntimeStackTransport<R::Socket>, Error>
where
    R: SocketIoRuntime,
{
    let stream = context
        .server_with_runtime(cx, buffer_size, socket)
        .map_err(Error::tls)?;
    if let Some(expected) = expected {
        validate_protocol(&stream, expected)?;
    }
    Ok(transport_with_runtime(stream))
}
