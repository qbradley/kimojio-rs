// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::error;
use std::fmt;
use std::rc::Rc;

use kimojio_fsm_http::HttpProtocol;
use openssl::error::ErrorStack;
use openssl::ssl::{AlpnError, SslContextBuilder};

use super::{Error, Protocol, Result};
use crate::tlscontext::TlsContext;

/// An HTTP application protocol advertised or selected with TLS ALPN.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AlpnProtocol {
    /// HTTP/1.1 (`http/1.1`).
    Http11,
    /// HTTP/2 (`h2`).
    ///
    /// TLS selects HTTP/2 through ALPN. Internally this maps to
    /// [`Protocol::Http2PriorKnowledge`] because the same HTTP/2 state machine
    /// drives the connection after negotiation.
    H2,
}

impl AlpnProtocol {
    /// Returns the protocol's ALPN wire name without its length prefix.
    pub const fn wire_name(self) -> &'static [u8] {
        match self {
            Self::Http11 => HttpProtocol::Http1.alpn_identifier(),
            Self::H2 => HttpProtocol::Http2.alpn_identifier(),
        }
    }
}

impl From<AlpnProtocol> for Protocol {
    fn from(protocol: AlpnProtocol) -> Self {
        match protocol {
            AlpnProtocol::Http11 => Self::Http1,
            AlpnProtocol::H2 => Self::Http2PriorKnowledge,
        }
    }
}

/// An error constructing an HTTP TLS configuration.
#[derive(Debug)]
#[non_exhaustive]
pub enum TlsConfigError {
    /// At least one ALPN protocol must be configured.
    EmptyProtocols,
    /// OpenSSL rejected the ALPN configuration.
    OpenSsl(ErrorStack),
}

impl fmt::Display for TlsConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyProtocols => {
                formatter.write_str("at least one HTTP ALPN protocol is required")
            }
            Self::OpenSsl(error) => write!(formatter, "failed to configure HTTP ALPN: {error}"),
        }
    }
}

impl error::Error for TlsConfigError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Self::OpenSsl(error) => Some(error),
            Self::EmptyProtocols => None,
        }
    }
}

impl From<ErrorStack> for TlsConfigError {
    fn from(error: ErrorStack) -> Self {
        Self::OpenSsl(error)
    }
}

/// Client-side TLS and ordered ALPN protocol configuration.
#[derive(Clone)]
pub struct TlsClientConfig {
    context: Rc<TlsContext>,
    protocols: Box<[AlpnProtocol]>,
}

impl TlsClientConfig {
    /// Configures ALPN on `builder`, builds it, and wraps the resulting context.
    ///
    /// Protocols are offered in the supplied order.
    pub fn new(
        mut builder: SslContextBuilder,
        protocols: &[AlpnProtocol],
    ) -> std::result::Result<Self, TlsConfigError> {
        let wire = encode_protocols(protocols)?;
        builder.set_alpn_protos(&wire)?;
        Ok(Self {
            context: Rc::new(TlsContext::from_openssl(builder.build())),
            protocols: protocols.into(),
        })
    }

    /// Wraps a context whose client ALPN configuration was installed externally.
    ///
    /// The caller owns ALPN correctness. [`Self::protocols`] is empty because
    /// a built OpenSSL context does not expose its configured offer list.
    pub fn from_context(context: TlsContext) -> Self {
        Self {
            context: Rc::new(context),
            protocols: Box::new([]),
        }
    }

    /// Returns the configured offer order, or an empty slice for an externally
    /// configured context.
    pub fn protocols(&self) -> &[AlpnProtocol] {
        &self.protocols
    }

    pub(super) fn context(&self) -> &TlsContext {
        &self.context
    }
}

impl fmt::Debug for TlsClientConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TlsClientConfig")
            .field("protocols", &self.protocols)
            .finish_non_exhaustive()
    }
}

/// Server-side TLS and server-preference ALPN configuration.
#[derive(Clone)]
pub struct TlsServerConfig {
    context: Rc<TlsContext>,
    protocols: Box<[AlpnProtocol]>,
    require_alpn: bool,
}

impl TlsServerConfig {
    /// Configures server-preference ALPN and requires a negotiated protocol.
    ///
    /// The first server protocol also offered by the client is selected. A
    /// client with no matching protocol receives a fatal TLS alert.
    pub fn new(
        builder: SslContextBuilder,
        protocols: &[AlpnProtocol],
    ) -> std::result::Result<Self, TlsConfigError> {
        Self::new_with_alpn_requirement(builder, protocols, true)
    }

    /// Configures server-preference ALPN with an explicit ALPN requirement.
    ///
    /// If `require_alpn` is false, no overlap completes the handshake without
    /// ALPN and the HTTP server falls back to HTTP/1.1.
    pub fn new_with_alpn_requirement(
        mut builder: SslContextBuilder,
        protocols: &[AlpnProtocol],
        require_alpn: bool,
    ) -> std::result::Result<Self, TlsConfigError> {
        let wire = encode_protocols(protocols)?;
        builder.set_alpn_select_callback(move |_, client| {
            select_server_protocol(&wire, client).ok_or(if require_alpn {
                AlpnError::ALERT_FATAL
            } else {
                AlpnError::NOACK
            })
        });
        Ok(Self {
            context: Rc::new(TlsContext::from_openssl(builder.build())),
            protocols: protocols.into(),
            require_alpn,
        })
    }

    /// Wraps a context whose server ALPN callback was installed externally.
    ///
    /// The caller owns ALPN correctness. No negotiated ALPN falls back to
    /// HTTP/1.1, and [`Self::protocols`] is empty because OpenSSL does not
    /// expose callback preferences from a built context.
    pub fn from_context(context: TlsContext) -> Self {
        Self {
            context: Rc::new(context),
            protocols: Box::new([]),
            require_alpn: false,
        }
    }

    /// Returns the server preference order, or an empty slice for an externally
    /// configured context.
    pub fn protocols(&self) -> &[AlpnProtocol] {
        &self.protocols
    }

    /// Returns whether the constructor-installed callback requires ALPN.
    pub const fn require_alpn(&self) -> bool {
        self.require_alpn
    }

    pub(super) fn context(&self) -> &TlsContext {
        &self.context
    }
}

impl fmt::Debug for TlsServerConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TlsServerConfig")
            .field("protocols", &self.protocols)
            .field("require_alpn", &self.require_alpn)
            .finish_non_exhaustive()
    }
}

fn encode_protocols(protocols: &[AlpnProtocol]) -> std::result::Result<Vec<u8>, TlsConfigError> {
    if protocols.is_empty() {
        return Err(TlsConfigError::EmptyProtocols);
    }
    let capacity = protocols
        .iter()
        .map(|protocol| 1 + protocol.wire_name().len())
        .sum();
    let mut wire = Vec::with_capacity(capacity);
    for protocol in protocols {
        let name = protocol.wire_name();
        wire.push(u8::try_from(name.len()).expect("HTTP ALPN names fit in one byte"));
        wire.extend_from_slice(name);
    }
    Ok(wire)
}

fn select_server_protocol<'client>(server: &[u8], client: &'client [u8]) -> Option<&'client [u8]> {
    let mut server = ProtocolIter::new(server);
    server.find_map(|preferred| ProtocolIter::new(client).find(|offered| *offered == preferred))
}

struct ProtocolIter<'a> {
    remaining: &'a [u8],
}

impl<'a> ProtocolIter<'a> {
    const fn new(protocols: &'a [u8]) -> Self {
        Self {
            remaining: protocols,
        }
    }
}

impl<'a> Iterator for ProtocolIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        let (&length, remaining) = self.remaining.split_first()?;
        let length = usize::from(length);
        if length == 0 || remaining.len() < length {
            self.remaining = &[];
            return None;
        }
        let (protocol, remaining) = remaining.split_at(length);
        self.remaining = remaining;
        Some(protocol)
    }
}

pub(super) fn negotiated_protocol(selected: Option<&[u8]>) -> Result<AlpnProtocol> {
    let unsupported = || Error::UnsupportedAlpnProtocol(selected.unwrap_or_default().to_vec());
    match HttpProtocol::from_alpn(selected).map_err(|_| unsupported())? {
        HttpProtocol::Http1 => Ok(AlpnProtocol::Http11),
        HttpProtocol::Http2 => Ok(AlpnProtocol::H2),
        _ => Err(unsupported()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpn_wire_encoding_is_length_prefixed() {
        assert_eq!(
            encode_protocols(&[AlpnProtocol::H2, AlpnProtocol::Http11]).unwrap(),
            b"\x02h2\x08http/1.1"
        );
        assert!(matches!(
            encode_protocols(&[]),
            Err(TlsConfigError::EmptyProtocols)
        ));
    }

    #[test]
    fn alpn_selection_uses_server_preference_and_reports_no_overlap() {
        let server = encode_protocols(&[AlpnProtocol::H2, AlpnProtocol::Http11]).unwrap();
        let client = b"\x08http/1.1\x02h2";
        assert_eq!(
            select_server_protocol(&server, client),
            Some(HttpProtocol::Http2.alpn_identifier())
        );
        assert_eq!(
            select_server_protocol(&server, b"\x08http/1.1"),
            Some(HttpProtocol::Http1.alpn_identifier())
        );
        assert_eq!(select_server_protocol(&server, b"\x06spdy/3"), None);
    }
}
