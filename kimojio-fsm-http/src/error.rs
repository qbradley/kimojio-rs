use core::fmt;

/// Stable semantic categories for HTTP state-machine failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum HttpErrorKind {
    /// The message syntax or required metadata is malformed.
    MalformedMessage,
    /// Message or frame boundaries are invalid.
    InvalidFraming,
    /// A header or pseudo-header is invalid.
    InvalidHeader,
    /// A content-length value is invalid or inconsistent.
    InvalidContentLength,
    /// The message requests an unsupported protocol feature.
    UnsupportedFeature,
    /// HTTP/2 header compression failed.
    Compression,
    /// HTTP/2 flow-control rules were violated.
    FlowControlViolation,
    /// The peer reset an HTTP/2 stream.
    PeerReset,
    /// The peer ended an HTTP/2 connection with GOAWAY.
    PeerGoaway,
    /// The operation is invalid in the current state.
    InvalidState,
    /// The operation requires more input before it can proceed.
    NeedMoreInput,
    /// Encoded headers exceed a configured byte limit.
    HeadersTooLarge,
    /// The number of headers exceeds a configured limit.
    TooManyHeaders,
    /// A message body exceeds a configured byte limit.
    BodyTooLarge,
}

impl fmt::Display for HttpErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::MalformedMessage => "malformed HTTP message",
            Self::InvalidFraming => "invalid HTTP framing",
            Self::InvalidHeader => "invalid HTTP header",
            Self::InvalidContentLength => "invalid HTTP content length",
            Self::UnsupportedFeature => "unsupported HTTP feature",
            Self::Compression => "HTTP/2 header compression error",
            Self::FlowControlViolation => "HTTP/2 flow-control violation",
            Self::PeerReset => "HTTP/2 peer reset",
            Self::PeerGoaway => "HTTP/2 peer GOAWAY",
            Self::InvalidState => "invalid HTTP state",
            Self::NeedMoreInput => "more HTTP input required",
            Self::HeadersTooLarge => "HTTP headers too large",
            Self::TooManyHeaders => "too many HTTP headers",
            Self::BodyTooLarge => "HTTP body too large",
        })
    }
}

/// The protocol boundary affected by an HTTP state-machine failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum HttpErrorScope {
    /// The failure is confined to one HTTP message.
    Message,
    /// The failure affects the HTTP connection.
    Connection,
    /// The failure is confined to the identified HTTP/2 stream.
    Stream(u32),
}

impl fmt::Display for HttpErrorScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message => formatter.write_str("HTTP message"),
            Self::Connection => formatter.write_str("HTTP connection"),
            Self::Stream(stream_id) => write!(formatter, "HTTP/2 stream {stream_id}"),
        }
    }
}

/// A configured limit and the observed value that violated it, when known.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LimitViolation {
    limit: usize,
    actual: Option<usize>,
}

impl LimitViolation {
    /// Creates limit-violation metadata.
    pub const fn new(limit: usize, actual: Option<usize>) -> Self {
        Self { limit, actual }
    }

    /// Returns the configured limit.
    pub const fn limit(&self) -> usize {
        self.limit
    }

    /// Returns the observed value, when the producing FSM knew it.
    pub const fn actual(&self) -> Option<usize> {
        self.actual
    }
}

/// Allocation-free semantic information about an HTTP state-machine failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HttpErrorInfo {
    kind: HttpErrorKind,
    scope: HttpErrorScope,
    detail: &'static str,
    limit: Option<LimitViolation>,
}

impl HttpErrorInfo {
    pub(crate) const fn new(
        kind: HttpErrorKind,
        scope: HttpErrorScope,
        detail: &'static str,
        limit: Option<LimitViolation>,
    ) -> Self {
        Self {
            kind,
            scope,
            detail,
            limit,
        }
    }

    /// Returns the stable semantic category.
    pub const fn kind(&self) -> HttpErrorKind {
        self.kind
    }

    /// Returns the affected protocol boundary.
    pub const fn scope(&self) -> HttpErrorScope {
        self.scope
    }

    /// Returns a stable, allocation-free diagnostic.
    pub const fn detail(&self) -> &'static str {
        self.detail
    }

    /// Returns configured-limit metadata for resource-limit failures.
    pub const fn limit(&self) -> Option<LimitViolation> {
        self.limit
    }
}

impl fmt::Display for HttpErrorInfo {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.scope, self.detail)
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        Error, H2ErrorCode, H2ErrorScope, H2ProtocolError, HttpErrorKind, HttpErrorScope,
        ServerError,
    };

    #[test]
    fn classifies_representative_error_types() {
        let client = Error::BodyTooLarge {
            limit: 8,
            actual: 13,
        }
        .classify();
        assert_eq!(client.kind(), HttpErrorKind::BodyTooLarge);
        assert_eq!(client.scope(), HttpErrorScope::Message);
        assert_eq!(client.limit().unwrap().limit(), 8);
        assert_eq!(client.limit().unwrap().actual(), Some(13));

        let server = ServerError::InvalidHpack.classify();
        assert_eq!(server.kind(), HttpErrorKind::Compression);
        assert_eq!(server.scope(), HttpErrorScope::Connection);

        let protocol = H2ProtocolError::stream(
            7,
            H2ErrorCode::FlowControlError,
            "stream flow-control window overflowed",
        )
        .classify();
        assert_eq!(protocol.kind(), HttpErrorKind::FlowControlViolation);
        assert_eq!(protocol.scope(), HttpErrorScope::Stream(7));
        assert_eq!(protocol.detail(), "stream flow-control window overflowed");
        assert_eq!(
            protocol,
            H2ProtocolError {
                scope: H2ErrorScope::Stream(7),
                code: H2ErrorCode::FlowControlError,
                debug: "stream flow-control window overflowed",
                hpack_error: None,
                http_error_kind: None,
                limit: None,
            }
            .classify()
        );
    }
}
