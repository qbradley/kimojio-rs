//! Compatibility errors for the FSM HTTP server and client helpers.

use core::fmt;

/// Compatibility error surface for low-level FSM HTTP server/client helpers.
///
/// The enum intentionally spans HTTP/1.x and HTTP/2 helper APIs while stack
/// adapters migrate onto narrower FSM surfaces. Callers should convert errors at
/// the boundary for the helper they are using rather than treating every variant
/// as reachable from every protocol path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServerError {
    NeedMore,
    Parse,
    InvalidRequest,
    InvalidResponse,
    /// An HTTP header or pseudo-header is invalid.
    InvalidHeader,
    InvalidContentLength,
    TooManyHeaders {
        limit: usize,
        actual: usize,
    },
    BodyTooLarge {
        limit: usize,
        actual: usize,
    },
    HeaderTooLarge {
        limit: usize,
        actual: usize,
    },
    UnsupportedMethod,
    UnsupportedVersion,
    UnsupportedTransferEncoding,
    InvalidFrame,
    /// An HTTP/2 flow-control or send-capacity operation was rejected.
    FlowControlViolation,
    /// An outbound block operation violated connection wire ordering.
    InvalidOutboundState,
    /// TLS selected an ALPN protocol this HTTP state machine does not implement.
    UnsupportedAlpnProtocol,
    /// The peer reset an active HTTP/2 stream.
    PeerReset {
        /// The reset stream identifier.
        stream_id: u32,
        /// The peer-provided HTTP/2 error code.
        error_code: u32,
    },
    /// The peer sent an HTTP/2 GOAWAY frame.
    PeerGoaway {
        /// The largest stream identifier the peer may have processed.
        last_stream_id: u32,
        /// The peer-provided HTTP/2 error code.
        error_code: u32,
    },
    InvalidPreface,
    InvalidHpack,
    UnsupportedHpack,
    MalformedMessage,
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for ServerError {}

impl ServerError {
    /// Returns stable, allocation-free semantic information about this failure.
    pub fn classify(&self) -> crate::HttpErrorInfo {
        use crate::{HttpErrorInfo, HttpErrorKind, HttpErrorScope, LimitViolation};

        let (kind, scope, detail, limit) = match *self {
            Self::HeaderTooLarge { limit, actual } => (
                HttpErrorKind::HeadersTooLarge,
                HttpErrorScope::Message,
                "HTTP headers exceed the configured limit",
                Some(LimitViolation::new(limit, Some(actual))),
            ),
            Self::TooManyHeaders { limit, actual } => (
                HttpErrorKind::TooManyHeaders,
                HttpErrorScope::Message,
                "HTTP message contains too many headers",
                Some(LimitViolation::new(limit, Some(actual))),
            ),
            Self::BodyTooLarge { limit, actual } => (
                HttpErrorKind::BodyTooLarge,
                HttpErrorScope::Message,
                "HTTP body exceeds the configured limit",
                Some(LimitViolation::new(limit, Some(actual))),
            ),
            Self::NeedMore => (
                HttpErrorKind::NeedMoreInput,
                HttpErrorScope::Connection,
                "HTTP operation unexpectedly required more input",
                None,
            ),
            Self::Parse | Self::InvalidRequest | Self::InvalidResponse | Self::MalformedMessage => {
                (
                    HttpErrorKind::MalformedMessage,
                    HttpErrorScope::Message,
                    "HTTP message is malformed",
                    None,
                )
            }
            Self::InvalidHeader => (
                HttpErrorKind::InvalidHeader,
                HttpErrorScope::Message,
                "HTTP header is invalid",
                None,
            ),
            Self::InvalidContentLength => (
                HttpErrorKind::InvalidContentLength,
                HttpErrorScope::Message,
                "HTTP content-length is invalid",
                None,
            ),
            Self::UnsupportedMethod
            | Self::UnsupportedVersion
            | Self::UnsupportedTransferEncoding => (
                HttpErrorKind::UnsupportedFeature,
                HttpErrorScope::Message,
                "HTTP message uses an unsupported feature",
                None,
            ),
            Self::UnsupportedHpack => (
                HttpErrorKind::UnsupportedFeature,
                HttpErrorScope::Connection,
                "HTTP/2 message uses unsupported header compression",
                None,
            ),
            Self::UnsupportedAlpnProtocol => (
                HttpErrorKind::UnsupportedFeature,
                HttpErrorScope::Connection,
                "TLS negotiated an unsupported ALPN protocol",
                None,
            ),
            Self::InvalidFrame => (
                HttpErrorKind::InvalidFraming,
                HttpErrorScope::Connection,
                "HTTP/2 frame is invalid",
                None,
            ),
            Self::FlowControlViolation => (
                HttpErrorKind::FlowControlViolation,
                HttpErrorScope::Connection,
                "HTTP/2 flow-control operation was rejected",
                None,
            ),
            Self::InvalidOutboundState => (
                HttpErrorKind::InvalidState,
                HttpErrorScope::Connection,
                "HTTP/2 connection state rejected the outbound operation",
                None,
            ),
            Self::PeerReset { stream_id, .. } => (
                HttpErrorKind::PeerReset,
                HttpErrorScope::Stream(stream_id),
                "the peer reset the HTTP/2 stream",
                None,
            ),
            Self::PeerGoaway { .. } => (
                HttpErrorKind::PeerGoaway,
                HttpErrorScope::Connection,
                "the peer sent HTTP/2 GOAWAY",
                None,
            ),
            Self::InvalidPreface => (
                HttpErrorKind::InvalidFraming,
                HttpErrorScope::Connection,
                "HTTP/2 connection preface is invalid",
                None,
            ),
            Self::InvalidHpack => (
                HttpErrorKind::Compression,
                HttpErrorScope::Connection,
                "HTTP/2 header compression is invalid",
                None,
            ),
        };
        HttpErrorInfo::new(kind, scope, detail, limit)
    }
}
