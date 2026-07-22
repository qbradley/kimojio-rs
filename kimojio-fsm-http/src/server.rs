use core::fmt;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque, hash_map::Entry};
use std::time::{Duration, Instant};

use crate::{Header, HttpLimits, hpack_field_size, parse_content_length};

pub const CLIENT_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

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

const H2_MIN_MAX_FRAME_SIZE: usize = 16_384;
const H2_MAX_MAX_FRAME_SIZE: usize = 16_777_215;
const H2_MAX_WINDOW_SIZE: u32 = 2_147_483_647;
const H2_DEFAULT_MAX_HEADER_LIST_SIZE: usize = 64 * 1024;
/// Default active-stream limit for network-facing FSM HTTP owners.
///
/// Owners may explicitly configure a higher bounded limit. The fair scheduler
/// uses collision-resistant randomized hashing for peer-selected stream IDs.
pub const H2_DEFAULT_MAX_ACTIVE_STREAMS: usize = crate::limits::DEFAULT_MAX_ACTIVE_STREAMS;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServerRequest<'a> {
    pub method: &'a str,
    pub target: &'a str,
    pub version: u8,
}

/// Parsed HTTP/1.x request head borrowed from caller-provided input and header scratch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Http1RequestHead<'headers, 'input> {
    /// HTTP method token.
    pub method: &'input str,
    /// Request target, including path and optional query.
    pub target: &'input str,
    /// HTTP minor version: `0` for HTTP/1.0 and `1` for HTTP/1.1.
    pub version: u8,
    /// Parsed headers backed by caller-provided scratch storage.
    pub headers: &'headers [httparse::Header<'input>],
}

/// Parsed HTTP/1.x response head borrowed from caller-provided input and header scratch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Http1ResponseHead<'headers, 'input> {
    /// HTTP minor version: `0` for HTTP/1.0 and `1` for HTTP/1.1.
    pub version: u8,
    /// Numeric status code.
    pub status: u16,
    /// Optional reason phrase.
    pub reason: &'input str,
    /// Parsed headers backed by caller-provided scratch storage.
    pub headers: &'headers [httparse::Header<'input>],
}

/// HTTP/1.x body framing mode inferred from request or response headers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Http1BodyKind {
    /// No body is permitted or expected.
    Empty,
    /// A fixed-length body with the declared byte count.
    ContentLength(usize),
    /// `Transfer-Encoding: chunked` framing.
    Chunked,
    /// Response body is delimited by peer EOF.
    Eof,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Http1ChunkedEvent<'input> {
    NeedInput,
    Chunk {
        chunk: &'input [u8],
        consumed: usize,
    },
    Complete {
        consumed: usize,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Http1ChunkedBody {
    max_body_bytes: usize,
    max_metadata_bytes: usize,
    max_trailer_bytes: usize,
    consumed_body_bytes: usize,
}

impl Http1ChunkedBody {
    pub const fn new(max_body_bytes: usize) -> Self {
        Self::with_metadata_limits(max_body_bytes, 1024, 8 * 1024)
    }

    pub const fn with_metadata_limits(
        max_body_bytes: usize,
        max_metadata_bytes: usize,
        max_trailer_bytes: usize,
    ) -> Self {
        Self {
            max_body_bytes,
            max_metadata_bytes,
            max_trailer_bytes,
            consumed_body_bytes: 0,
        }
    }

    pub fn next_event<'input>(
        &mut self,
        input: &'input [u8],
    ) -> Result<Http1ChunkedEvent<'input>, ServerError> {
        let Some(line_end) = find_crlf(input) else {
            if input.len() > self.max_metadata_bytes {
                return Err(ServerError::HeaderTooLarge {
                    limit: self.max_metadata_bytes,
                    actual: input.len(),
                });
            }
            return Ok(Http1ChunkedEvent::NeedInput);
        };
        if line_end > self.max_metadata_bytes {
            return Err(ServerError::HeaderTooLarge {
                limit: self.max_metadata_bytes,
                actual: line_end,
            });
        }
        let size = parse_chunk_size(&input[..line_end])?;
        let data_start = line_end + 2;
        if size == 0 {
            let trailer = &input[data_start..];
            if trailer.starts_with(b"\r\n") {
                return Ok(Http1ChunkedEvent::Complete {
                    consumed: data_start + 2,
                });
            }
            let Some(trailer_end) = find_header_end(trailer) else {
                if trailer.len() >= self.max_trailer_bytes {
                    return Err(ServerError::HeaderTooLarge {
                        limit: self.max_trailer_bytes,
                        actual: trailer.len(),
                    });
                }
                return Ok(Http1ChunkedEvent::NeedInput);
            };
            if trailer_end > self.max_trailer_bytes {
                return Err(ServerError::HeaderTooLarge {
                    limit: self.max_trailer_bytes,
                    actual: trailer_end,
                });
            }
            return Ok(Http1ChunkedEvent::Complete {
                consumed: data_start + trailer_end,
            });
        }

        let total = data_start
            .checked_add(size)
            .and_then(|end| end.checked_add(2))
            .ok_or(ServerError::Parse)?;
        let actual = self.consumed_body_bytes.saturating_add(size);
        if actual > self.max_body_bytes {
            return Err(ServerError::BodyTooLarge {
                limit: self.max_body_bytes,
                actual,
            });
        }
        if input.len() < total {
            return Ok(Http1ChunkedEvent::NeedInput);
        }
        if &input[data_start + size..total] != b"\r\n" {
            return Err(ServerError::Parse);
        }
        self.consumed_body_bytes = actual;
        Ok(Http1ChunkedEvent::Chunk {
            chunk: &input[data_start..data_start + size],
            consumed: total,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServerResponse<'a> {
    pub status: u16,
    pub reason: &'a str,
    pub headers: &'a [Header<'a>],
    pub body: &'a [u8],
}

#[derive(Debug, Default)]
pub struct Http1Server;

/// Generic HTTP/1.x message codec facade.
///
/// `Http1Server` remains as the original static-file-server compatibility name,
/// while this alias is the preferred name for adapters that parse both request
/// and response heads or perform generic HTTP/1.x body-framing decisions.
pub type Http1Codec = Http1Server;

/// Direction parsed by an [`Http1ConnectionDecoder`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Http1MessageRole {
    /// Parse an inbound request.
    Request,
    /// Parse a response to the supplied request method.
    Response {
        /// Method of the request whose response is being parsed.
        request_method: String,
    },
}

/// Borrowed HTTP/1.x head emitted by [`Http1ConnectionDecoder`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Http1MessageHead<'headers, 'input> {
    /// An inbound request head.
    Request(Http1RequestHead<'headers, 'input>),
    /// An inbound response head.
    Response(Http1ResponseHead<'headers, 'input>),
}

/// Incremental event emitted by the role-neutral HTTP/1.x decoder.
///
/// After every event with a `consumed` field, callers must remove exactly that
/// many bytes from their input before calling [`Http1ConnectionDecoder::next_event`]
/// again. Body and trailer references are backed by that caller-owned input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Http1ConnectionEvent<'headers, 'input> {
    /// More transport input is required.
    NeedInput,
    /// A request or response head was parsed.
    Head {
        /// Parsed head.
        head: Http1MessageHead<'headers, 'input>,
        /// Framing selected from the head.
        body: Http1BodyKind,
        /// Number of head bytes consumed.
        consumed: usize,
        /// The response is informational and another response head follows.
        informational: bool,
    },
    /// One decoded body chunk is ready.
    Body {
        /// Caller-owned decoded body bytes.
        chunk: &'input [u8],
        /// Number of wire bytes consumed.
        consumed: usize,
    },
    /// Terminal chunk trailers were parsed.
    Trailers {
        /// Parsed trailer fields.
        fields: &'headers [httparse::Header<'input>],
        /// Number of terminal chunk and trailer bytes consumed.
        consumed: usize,
    },
    /// A successful CONNECT response or `101` switches to opaque protocol bytes.
    ProtocolSwitch {
        /// Final response head that established the switch.
        head: Http1MessageHead<'headers, 'input>,
        /// Number of HTTP message bytes consumed by this event.
        consumed: usize,
    },
    /// The HTTP message is complete.
    Complete,
}

/// Incremental role-neutral HTTP/1.x head and body decoder.
///
/// This decoder has no I/O or input buffer ownership. It centralizes request
/// and response framing, informational responses, CONNECT/upgrade boundaries,
/// chunk trailers, and EOF-delimited completion for runtime adapters without
/// changing the specialized [`crate::HttpClient`] compatibility API.
#[derive(Debug)]
pub struct Http1ConnectionDecoder {
    codec: Http1Codec,
    role: Http1MessageRole,
    state: Http1DecoderState,
    limits: HttpLimits,
    header_count: usize,
    header_bytes: usize,
    received_body_bytes: usize,
}

#[derive(Debug)]
enum Http1DecoderState {
    Head,
    Fixed(usize),
    Chunked(Http1ChunkedBody),
    Eof,
    AfterTrailers,
    Complete,
    Failed,
}

impl Http1ConnectionDecoder {
    /// Creates a decoder ready for a request message.
    pub fn request(limits: HttpLimits) -> Self {
        Self {
            codec: Http1Codec::default(),
            role: Http1MessageRole::Request,
            state: Http1DecoderState::Head,
            limits,
            header_count: 0,
            header_bytes: 0,
            received_body_bytes: 0,
        }
    }

    /// Creates a decoder for a response to `request_method`.
    pub fn response(request_method: impl Into<String>, limits: HttpLimits) -> Self {
        Self {
            codec: Http1Codec::default(),
            role: Http1MessageRole::Response {
                request_method: request_method.into(),
            },
            state: Http1DecoderState::Head,
            limits,
            header_count: 0,
            header_bytes: 0,
            received_body_bytes: 0,
        }
    }

    /// Returns the immutable parser direction.
    pub fn role(&self) -> &Http1MessageRole {
        &self.role
    }

    /// Resets a completed decoder for another message with the same role.
    ///
    /// The previous message must have emitted [`Http1ConnectionEvent::Complete`].
    /// Header, body, chunk, and trailer accounting is discarded before the next
    /// head is accepted.
    pub fn begin_next_message(&mut self) -> Result<(), ServerError> {
        if !matches!(self.state, Http1DecoderState::Complete) {
            return Err(ServerError::InvalidOutboundState);
        }
        self.state = Http1DecoderState::Head;
        self.header_count = 0;
        self.header_bytes = 0;
        self.received_body_bytes = 0;
        Ok(())
    }

    pub(crate) fn set_response_request_method(
        &mut self,
        request_method: &str,
    ) -> Result<(), ServerError> {
        if !matches!(self.state, Http1DecoderState::Head) {
            return Err(ServerError::InvalidOutboundState);
        }
        let Http1MessageRole::Response {
            request_method: current,
        } = &mut self.role
        else {
            return Err(ServerError::InvalidOutboundState);
        };
        current.clear();
        current.push_str(request_method);
        Ok(())
    }

    pub(crate) fn set_max_body_bytes(&mut self, limit: usize) -> Result<(), ServerError> {
        if !matches!(self.state, Http1DecoderState::Head) {
            return Err(ServerError::InvalidOutboundState);
        }
        self.limits = self.limits.set_max_body_bytes(limit);
        Ok(())
    }

    /// Advances one head, body, trailer, switch, or completion event.
    pub fn next_event<'headers, 'input>(
        &mut self,
        input: &'input [u8],
        headers: &'headers mut [httparse::Header<'input>],
    ) -> Result<Http1ConnectionEvent<'headers, 'input>, ServerError> {
        let result = match &mut self.state {
            Http1DecoderState::Head => self.next_head(input, headers),
            Http1DecoderState::Fixed(remaining) => {
                if *remaining == 0 {
                    self.state = Http1DecoderState::Complete;
                    Ok(Http1ConnectionEvent::Complete)
                } else if input.is_empty() {
                    Ok(Http1ConnectionEvent::NeedInput)
                } else {
                    let consumed = input.len().min(*remaining);
                    let actual = self.received_body_bytes.saturating_add(consumed);
                    if actual > self.limits.max_body_bytes() {
                        return Err(ServerError::BodyTooLarge {
                            limit: self.limits.max_body_bytes(),
                            actual,
                        });
                    }
                    self.received_body_bytes = actual;
                    *remaining -= consumed;
                    Ok(Http1ConnectionEvent::Body {
                        chunk: &input[..consumed],
                        consumed,
                    })
                }
            }
            Http1DecoderState::Chunked(body) => match body.next_event(input)? {
                Http1ChunkedEvent::NeedInput => Ok(Http1ConnectionEvent::NeedInput),
                Http1ChunkedEvent::Chunk { chunk, consumed } => {
                    self.received_body_bytes = self.received_body_bytes.saturating_add(chunk.len());
                    Ok(Http1ConnectionEvent::Body { chunk, consumed })
                }
                Http1ChunkedEvent::Complete { .. } => {
                    let line_end = find_crlf(input).ok_or(ServerError::NeedMore)?;
                    let trailer_start = line_end + 2;
                    let trailer_count = if input[trailer_start..].starts_with(b"\r\n") {
                        0
                    } else {
                        let trailer_len = find_header_end(&input[trailer_start..])
                            .ok_or(ServerError::NeedMore)?;
                        http1_trailer_count(&input[trailer_start..trailer_start + trailer_len])
                    };
                    if trailer_count > self.limits.max_headers() {
                        return Err(ServerError::TooManyHeaders {
                            limit: self.limits.max_headers(),
                            actual: trailer_count,
                        });
                    }
                    let (fields, consumed) = if matches!(self.role, Http1MessageRole::Request) {
                        Http1Codec::parse_request_chunked_trailers(input, headers)?
                    } else {
                        Http1Codec::parse_chunked_trailers(input, headers)?
                    };
                    let actual_count = self.header_count.saturating_add(fields.len());
                    if actual_count > self.limits.max_headers() {
                        return Err(ServerError::TooManyHeaders {
                            limit: self.limits.max_headers(),
                            actual: actual_count,
                        });
                    }
                    let actual_bytes = self.header_bytes.saturating_add(consumed);
                    if actual_bytes > self.limits.max_header_bytes() {
                        return Err(ServerError::HeaderTooLarge {
                            limit: self.limits.max_header_bytes(),
                            actual: actual_bytes,
                        });
                    }
                    self.state = Http1DecoderState::AfterTrailers;
                    Ok(Http1ConnectionEvent::Trailers { fields, consumed })
                }
            },
            Http1DecoderState::Eof => {
                if input.is_empty() {
                    Ok(Http1ConnectionEvent::NeedInput)
                } else {
                    let actual = self.received_body_bytes.saturating_add(input.len());
                    if actual > self.limits.max_body_bytes() {
                        return Err(ServerError::BodyTooLarge {
                            limit: self.limits.max_body_bytes(),
                            actual,
                        });
                    }
                    self.received_body_bytes = actual;
                    Ok(Http1ConnectionEvent::Body {
                        chunk: input,
                        consumed: input.len(),
                    })
                }
            }
            Http1DecoderState::AfterTrailers => {
                self.state = Http1DecoderState::Complete;
                Ok(Http1ConnectionEvent::Complete)
            }
            Http1DecoderState::Complete => Ok(Http1ConnectionEvent::Complete),
            Http1DecoderState::Failed => Err(ServerError::MalformedMessage),
        };
        if result.is_err() {
            self.state = Http1DecoderState::Failed;
        }
        result
    }

    /// Marks an EOF-delimited message complete after the transport reaches EOF.
    ///
    /// Returns `true` only when EOF was a valid delimiter for the current
    /// message. Fixed and chunked bodies reject premature EOF.
    pub fn finish_eof(&mut self) -> Result<bool, ServerError> {
        match self.state {
            Http1DecoderState::Eof => {
                self.state = Http1DecoderState::Complete;
                Ok(true)
            }
            Http1DecoderState::Complete => Ok(true),
            Http1DecoderState::Fixed(_) | Http1DecoderState::Chunked(_) => {
                self.state = Http1DecoderState::Failed;
                Err(ServerError::MalformedMessage)
            }
            Http1DecoderState::Head
            | Http1DecoderState::AfterTrailers
            | Http1DecoderState::Failed => {
                self.state = Http1DecoderState::Failed;
                Err(ServerError::MalformedMessage)
            }
        }
    }

    fn next_head<'headers, 'input>(
        &mut self,
        input: &'input [u8],
        headers: &'headers mut [httparse::Header<'input>],
    ) -> Result<Http1ConnectionEvent<'headers, 'input>, ServerError> {
        let Some(head_len) = find_header_end(input) else {
            if input.len() >= self.limits.max_header_bytes() {
                return Err(ServerError::HeaderTooLarge {
                    limit: self.limits.max_header_bytes(),
                    actual: input.len(),
                });
            }
            return Ok(Http1ConnectionEvent::NeedInput);
        };
        if head_len > self.limits.max_header_bytes() {
            return Err(ServerError::HeaderTooLarge {
                limit: self.limits.max_header_bytes(),
                actual: head_len,
            });
        }
        let header_count = http1_header_count(&input[..head_len]);
        if header_count > self.limits.max_headers() {
            return Err(ServerError::TooManyHeaders {
                limit: self.limits.max_headers(),
                actual: header_count,
            });
        }
        self.header_count = header_count;
        self.header_bytes = head_len;
        self.received_body_bytes = 0;
        match &self.role {
            Http1MessageRole::Request => {
                let (head, consumed) = match self.codec.parse_request_head(input, headers) {
                    Ok(head) => head,
                    Err(ServerError::NeedMore) => return Ok(Http1ConnectionEvent::NeedInput),
                    Err(error) => return Err(error),
                };
                let body = Http1Codec::request_body_kind(head.headers)?;
                self.set_body_state(body)?;
                Ok(Http1ConnectionEvent::Head {
                    head: Http1MessageHead::Request(head),
                    body,
                    consumed,
                    informational: false,
                })
            }
            Http1MessageRole::Response { request_method } => {
                let (head, consumed) = match self.codec.parse_response_head(input, headers) {
                    Ok(head) => head,
                    Err(ServerError::NeedMore) => return Ok(Http1ConnectionEvent::NeedInput),
                    Err(error) => return Err(error),
                };
                let informational = (100..=199).contains(&head.status) && head.status != 101;
                let switches_protocol = head.status == 101
                    || (request_method.eq_ignore_ascii_case("CONNECT")
                        && (200..=299).contains(&head.status));
                if let Some(actual) = content_length(head.headers)?
                    && actual > self.limits.max_body_bytes()
                {
                    return Err(ServerError::BodyTooLarge {
                        limit: self.limits.max_body_bytes(),
                        actual,
                    });
                }
                let body =
                    Http1Codec::response_body_kind(request_method, head.status, head.headers)?;
                if switches_protocol {
                    self.state = Http1DecoderState::Complete;
                    Ok(Http1ConnectionEvent::ProtocolSwitch {
                        head: Http1MessageHead::Response(head),
                        consumed,
                    })
                } else if informational {
                    self.state = Http1DecoderState::Head;
                    Ok(Http1ConnectionEvent::Head {
                        head: Http1MessageHead::Response(head),
                        body,
                        consumed,
                        informational: true,
                    })
                } else {
                    self.set_body_state(body)?;
                    Ok(Http1ConnectionEvent::Head {
                        head: Http1MessageHead::Response(head),
                        body,
                        consumed,
                        informational: false,
                    })
                }
            }
        }
    }

    fn set_body_state(&mut self, body: Http1BodyKind) -> Result<(), ServerError> {
        if let Http1BodyKind::ContentLength(actual) = body
            && actual > self.limits.max_body_bytes()
        {
            return Err(ServerError::BodyTooLarge {
                limit: self.limits.max_body_bytes(),
                actual,
            });
        }
        self.state = match body {
            Http1BodyKind::Empty => Http1DecoderState::Complete,
            Http1BodyKind::ContentLength(length) => Http1DecoderState::Fixed(length),
            Http1BodyKind::Chunked => {
                Http1DecoderState::Chunked(Http1ChunkedBody::with_metadata_limits(
                    self.limits.max_body_bytes(),
                    1024,
                    self.limits.max_header_bytes(),
                ))
            }
            Http1BodyKind::Eof => Http1DecoderState::Eof,
        };
        Ok(())
    }
}

impl Http1Server {
    /// Parses a general HTTP/1.0 or HTTP/1.1 request head without method filtering.
    pub fn parse_request_head<'headers, 'input>(
        &mut self,
        input: &'input [u8],
        headers: &'headers mut [httparse::Header<'input>],
    ) -> Result<(Http1RequestHead<'headers, 'input>, usize), ServerError> {
        let header_capacity = headers.len();
        let mut request = httparse::Request::new(headers);
        let parsed = request.parse(input).map_err(|error| match error {
            httparse::Error::TooManyHeaders => ServerError::TooManyHeaders {
                limit: header_capacity,
                actual: header_capacity.saturating_add(1),
            },
            _ => ServerError::Parse,
        })?;
        let consumed = match parsed {
            httparse::Status::Complete(consumed) => consumed,
            httparse::Status::Partial => return Err(ServerError::NeedMore),
        };
        let method = request.method.ok_or(ServerError::InvalidRequest)?;
        let target = request.path.ok_or(ServerError::InvalidRequest)?;
        let version = request.version.ok_or(ServerError::InvalidRequest)?;
        if !matches!(version, 0 | 1) {
            return Err(ServerError::UnsupportedVersion);
        }
        if version == 1 {
            validate_http1_host(request.headers)?;
        }
        Ok((
            Http1RequestHead {
                method,
                target,
                version,
                headers: request.headers,
            },
            consumed,
        ))
    }

    /// Parses the legacy GET/HEAD-only HTTP/1.1 request surface.
    pub fn parse_request<'input>(
        &mut self,
        input: &'input [u8],
        headers: &mut [httparse::Header<'input>],
    ) -> Result<(ServerRequest<'input>, usize), ServerError> {
        let (request, consumed) = self.parse_request_head(input, headers)?;
        let method = request.method;
        if !matches!(method, "GET" | "HEAD") {
            return Err(ServerError::UnsupportedMethod);
        }
        let target = request.target;
        let version = request.version;
        if version != 1 {
            return Err(ServerError::UnsupportedVersion);
        }
        Ok((
            ServerRequest {
                method,
                target,
                version,
            },
            consumed,
        ))
    }

    /// Parses a general HTTP/1.0 or HTTP/1.1 response head.
    pub fn parse_response_head<'headers, 'input>(
        &mut self,
        input: &'input [u8],
        headers: &'headers mut [httparse::Header<'input>],
    ) -> Result<(Http1ResponseHead<'headers, 'input>, usize), ServerError> {
        let header_capacity = headers.len();
        let mut response = httparse::Response::new(headers);
        let parsed = response.parse(input).map_err(|error| match error {
            httparse::Error::TooManyHeaders => ServerError::TooManyHeaders {
                limit: header_capacity,
                actual: header_capacity.saturating_add(1),
            },
            _ => ServerError::Parse,
        })?;
        let consumed = match parsed {
            httparse::Status::Complete(consumed) => consumed,
            httparse::Status::Partial => return Err(ServerError::NeedMore),
        };
        let version = response.version.ok_or(ServerError::InvalidResponse)?;
        if !matches!(version, 0 | 1) {
            return Err(ServerError::UnsupportedVersion);
        }
        let status = response.code.ok_or(ServerError::InvalidResponse)?;
        Ok((
            Http1ResponseHead {
                version,
                status,
                reason: response.reason.unwrap_or(""),
                headers: response.headers,
            },
            consumed,
        ))
    }

    /// Infers request body framing from `Content-Length` and `Transfer-Encoding` headers.
    pub fn request_body_kind(
        headers: &[httparse::Header<'_>],
    ) -> Result<Http1BodyKind, ServerError> {
        if let Some(kind) = transfer_encoding(headers)? {
            ensure_no_content_length(headers)?;
            return Ok(kind);
        }
        content_length(headers)
            .map(|len| len.map_or(Http1BodyKind::Empty, Http1BodyKind::ContentLength))
    }

    /// Infers response body framing for a request method/status/header combination.
    pub fn response_body_kind(
        request_method: &str,
        status: u16,
        headers: &[httparse::Header<'_>],
    ) -> Result<Http1BodyKind, ServerError> {
        if request_method.eq_ignore_ascii_case("HEAD") || matches!(status, 100..=199 | 204 | 304) {
            return Ok(Http1BodyKind::Empty);
        }
        if let Some(kind) = transfer_encoding(headers)? {
            ensure_no_content_length(headers)?;
            return Ok(kind);
        }
        if let Some(len) = content_length(headers)? {
            return Ok(Http1BodyKind::ContentLength(len));
        }
        Ok(Http1BodyKind::Eof)
    }

    /// Parses the final zero-sized chunk and its trailer fields.
    ///
    /// The supplied input must begin at a terminal chunk boundary. Forbidden
    /// framing, routing, and control fields are rejected before the message is
    /// made reusable.
    pub fn parse_chunked_trailers<'headers, 'input>(
        input: &'input [u8],
        headers: &'headers mut [httparse::Header<'input>],
    ) -> Result<(&'headers [httparse::Header<'input>], usize), ServerError> {
        Self::parse_chunked_trailers_with_policy(input, headers, false)
    }

    /// Parses request chunk trailers, discarding prohibited fields before a
    /// reusable request is exposed to the caller.
    ///
    /// This preserves the approved HTTP/1 profile's request-side
    /// `fields-discarded` outcome without weakening the response decoder's
    /// strict forbidden-trailer rejection.
    pub fn parse_request_chunked_trailers<'headers, 'input>(
        input: &'input [u8],
        headers: &'headers mut [httparse::Header<'input>],
    ) -> Result<(&'headers [httparse::Header<'input>], usize), ServerError> {
        Self::parse_chunked_trailers_with_policy(input, headers, true)
    }

    fn parse_chunked_trailers_with_policy<'headers, 'input>(
        input: &'input [u8],
        headers: &'headers mut [httparse::Header<'input>],
        discard_forbidden: bool,
    ) -> Result<(&'headers [httparse::Header<'input>], usize), ServerError> {
        let line_end = find_crlf(input).ok_or(ServerError::NeedMore)?;
        if parse_chunk_size(&input[..line_end])? != 0 {
            return Err(ServerError::MalformedMessage);
        }
        let trailer_start = line_end + 2;
        let header_capacity = headers.len();
        let parsed = httparse::parse_headers(&input[trailer_start..], headers).map_err(
            |error| match error {
                httparse::Error::TooManyHeaders => ServerError::TooManyHeaders {
                    limit: header_capacity,
                    actual: header_capacity.saturating_add(1),
                },
                _ => ServerError::Parse,
            },
        )?;
        let (trailer_len, fields) = match parsed {
            httparse::Status::Complete(value) => value,
            httparse::Status::Partial => return Err(ServerError::NeedMore),
        };
        if fields.iter().any(|field| forbidden_trailer(field.name)) {
            if discard_forbidden {
                return Ok((&[], trailer_start + trailer_len));
            }
            return Err(ServerError::MalformedMessage);
        }
        Ok((fields, trailer_start + trailer_len))
    }

    pub fn response_bytes(response: ServerResponse<'_>, head_only: bool) -> Vec<u8> {
        let mut bytes = Self::response_head_bytes(
            response.status,
            response.reason,
            response.headers,
            response.body.len(),
        );
        if !head_only {
            bytes.extend_from_slice(response.body);
        }
        bytes
    }

    pub fn response_head_bytes(
        status: u16,
        reason: &str,
        headers: &[Header<'_>],
        content_length: usize,
    ) -> Vec<u8> {
        Self::response_head_bytes_with_version(1, status, reason, headers, Some(content_length))
            .expect("HTTP/1.1 response version is supported")
    }

    pub fn request_head_bytes_with_version(
        method: &str,
        target: &str,
        version: u8,
        headers: &[httparse::Header<'_>],
        content_length: Option<usize>,
    ) -> Result<Vec<u8>, ServerError> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(method.as_bytes());
        bytes.push(b' ');
        bytes.extend_from_slice(target.as_bytes());
        bytes.push(b' ');
        write_http_version(&mut bytes, version)?;
        bytes.extend_from_slice(b"\r\n");
        write_raw_headers(&mut bytes, headers);
        if let Some(content_length) = content_length {
            bytes.extend_from_slice(b"content-length: ");
            push_decimal(&mut bytes, content_length);
            bytes.extend_from_slice(b"\r\n");
        }
        bytes.extend_from_slice(b"\r\n");
        Ok(bytes)
    }

    /// Serializes a request head while enforcing message resource limits.
    pub fn request_head_bytes_with_raw_headers_and_limits(
        method: &str,
        target: &str,
        version: u8,
        headers: &[httparse::Header<'_>],
        body_length: usize,
        limits: HttpLimits,
    ) -> Result<Vec<u8>, ServerError> {
        if body_length > limits.max_body_bytes() {
            return Err(ServerError::BodyTooLarge {
                limit: limits.max_body_bytes(),
                actual: body_length,
            });
        }
        if headers.len() > limits.max_headers() {
            return Err(ServerError::TooManyHeaders {
                limit: limits.max_headers(),
                actual: headers.len(),
            });
        }
        let bytes = Self::request_head_bytes_with_version(method, target, version, headers, None)?;
        if bytes.len() > limits.max_header_bytes() {
            return Err(ServerError::HeaderTooLarge {
                limit: limits.max_header_bytes(),
                actual: bytes.len(),
            });
        }
        Ok(bytes)
    }

    /// Serializes an HTTP/1.0 or HTTP/1.1 response head with optional `Content-Length`.
    pub fn response_head_bytes_with_version(
        version: u8,
        status: u16,
        reason: &str,
        headers: &[Header<'_>],
        content_length: Option<usize>,
    ) -> Result<Vec<u8>, ServerError> {
        let mut bytes = Vec::new();
        write_http_version(&mut bytes, version)?;
        bytes.push(b' ');
        push_decimal(&mut bytes, status as usize);
        if !reason.is_empty() {
            bytes.push(b' ');
            bytes.extend_from_slice(reason.as_bytes());
        }
        bytes.extend_from_slice(b"\r\n");
        if let Some(content_length) = content_length {
            bytes.extend_from_slice(b"content-length: ");
            push_decimal(&mut bytes, content_length);
            bytes.extend_from_slice(b"\r\n");
        }
        write_str_headers(&mut bytes, headers);
        bytes.extend_from_slice(b"\r\n");
        Ok(bytes)
    }

    pub fn response_head_bytes_with_raw_headers(
        version: u8,
        status: u16,
        reason: &str,
        headers: &[httparse::Header<'_>],
        content_length: Option<usize>,
    ) -> Result<Vec<u8>, ServerError> {
        let mut bytes = Vec::new();
        write_http_version(&mut bytes, version)?;
        bytes.push(b' ');
        push_decimal(&mut bytes, status as usize);
        if !reason.is_empty() {
            bytes.push(b' ');
            bytes.extend_from_slice(reason.as_bytes());
        }
        bytes.extend_from_slice(b"\r\n");
        if let Some(content_length) = content_length {
            bytes.extend_from_slice(b"content-length: ");
            push_decimal(&mut bytes, content_length);
            bytes.extend_from_slice(b"\r\n");
        }
        write_raw_headers(&mut bytes, headers);
        bytes.extend_from_slice(b"\r\n");
        Ok(bytes)
    }

    /// Serializes a response head while enforcing message resource limits.
    pub fn response_head_bytes_with_raw_headers_and_limits(
        version: u8,
        status: u16,
        reason: &str,
        headers: &[httparse::Header<'_>],
        content_length: Option<usize>,
        body_length: usize,
        limits: HttpLimits,
    ) -> Result<Vec<u8>, ServerError> {
        if body_length > limits.max_body_bytes() {
            return Err(ServerError::BodyTooLarge {
                limit: limits.max_body_bytes(),
                actual: body_length,
            });
        }
        let header_count = headers
            .len()
            .saturating_add(usize::from(content_length.is_some()));
        if header_count > limits.max_headers() {
            return Err(ServerError::TooManyHeaders {
                limit: limits.max_headers(),
                actual: header_count,
            });
        }
        let bytes = Self::response_head_bytes_with_raw_headers(
            version,
            status,
            reason,
            headers,
            content_length,
        )?;
        if bytes.len() > limits.max_header_bytes() {
            return Err(ServerError::HeaderTooLarge {
                limit: limits.max_header_bytes(),
                actual: bytes.len(),
            });
        }
        Ok(bytes)
    }

    pub fn response_body_chunk(chunk: &[u8]) -> &[u8] {
        chunk
    }

    pub fn chunked_body_bytes(body: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        Self::append_chunked_body_bytes(&mut bytes, body);
        bytes
    }

    pub fn append_chunked_body_bytes(bytes: &mut Vec<u8>, body: &[u8]) {
        if !body.is_empty() {
            push_hex(bytes, body.len());
            bytes.extend_from_slice(b"\r\n");
            bytes.extend_from_slice(body);
            bytes.extend_from_slice(b"\r\n");
        }
        bytes.extend_from_slice(b"0\r\n\r\n");
    }

    pub fn chunked_body_prefix(body_len: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        Self::push_chunked_body_prefix(&mut bytes, body_len);
        bytes
    }

    /// Writes a chunk size line straight into an existing output buffer.
    ///
    /// The outbound streaming path emits one of these per chunk, so building a
    /// throwaway `Vec` for each and copying it out would put an allocation on a
    /// hot path that has none.
    pub fn push_chunked_body_prefix(output: &mut Vec<u8>, body_len: usize) {
        if body_len != 0 {
            push_hex(output, body_len);
            output.extend_from_slice(b"\r\n");
        }
    }

    pub const fn chunked_body_suffix(body_is_empty: bool) -> &'static [u8] {
        if body_is_empty {
            b"0\r\n\r\n"
        } else {
            b"\r\n0\r\n\r\n"
        }
    }
}

fn content_length(headers: &[httparse::Header<'_>]) -> Result<Option<usize>, ServerError> {
    let mut parsed = None;
    for value in header_values(headers, "content-length") {
        let len = parse_content_length(value).ok_or(ServerError::InvalidContentLength)?;
        if let Some(previous) = parsed
            && previous != len
        {
            return Err(ServerError::InvalidContentLength);
        }

        parsed = Some(len);
    }
    Ok(parsed)
}

pub(crate) fn http1_header_count(head: &[u8]) -> usize {
    let Some(first_line_end) = find_crlf(head) else {
        return 0;
    };
    head[first_line_end + 2..]
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty() && *line != b"\r")
        .count()
}

fn http1_trailer_count(trailers: &[u8]) -> usize {
    trailers
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty() && *line != b"\r")
        .count()
}

fn ensure_no_content_length(headers: &[httparse::Header<'_>]) -> Result<(), ServerError> {
    if header_values(headers, "content-length").next().is_some() {
        Err(ServerError::InvalidContentLength)
    } else {
        Ok(())
    }
}

fn transfer_encoding(
    headers: &[httparse::Header<'_>],
) -> Result<Option<Http1BodyKind>, ServerError> {
    let mut present = false;
    let mut chunked_codings = 0usize;
    for value in header_values(headers, "transfer-encoding") {
        present = true;
        for token in value.split(|byte| *byte == b',') {
            let token = trim_ascii(token);
            if token.is_empty() {
                continue;
            }
            if token.eq_ignore_ascii_case(b"chunked") {
                chunked_codings = chunked_codings.saturating_add(1);
            } else {
                return Err(ServerError::UnsupportedTransferEncoding);
            }
        }
    }
    if !present {
        return Ok(None);
    }
    // RFC 9112 section 6.1 forbids applying the chunked coding more than once,
    // and requires at least one coding when the field is present. Both shapes
    // are request-smuggling vectors, because a peer that resolves the framing
    // differently (or silently falls back to Content-Length) desynchronises
    // from us on the same byte stream.
    if chunked_codings != 1 {
        return Err(ServerError::InvalidRequest);
    }
    Ok(Some(Http1BodyKind::Chunked))
}

/// Enforces the RFC 9112 section 3.2 `Host` requirement for HTTP/1.1 requests.
///
/// A server must reject a request that omits `Host` or carries more than one
/// `Host` field line: intermediaries that disagree about which value applies
/// can otherwise be steered into routing one message to two different origins.
fn validate_http1_host(headers: &[httparse::Header<'_>]) -> Result<(), ServerError> {
    let mut seen = 0usize;
    for _ in header_values(headers, "host") {
        seen = seen.saturating_add(1);
        if seen > 1 {
            return Err(ServerError::InvalidRequest);
        }
    }
    if seen == 0 {
        return Err(ServerError::InvalidRequest);
    }
    Ok(())
}

fn header_values<'a>(
    headers: &'a [httparse::Header<'_>],
    name: &'a str,
) -> impl Iterator<Item = &'a [u8]> + 'a {
    headers
        .iter()
        .filter(move |header| header.name.eq_ignore_ascii_case(name))
        .map(|header| header.value)
}

pub(crate) fn forbidden_trailer(name: &str) -> bool {
    [
        "connection",
        "content-length",
        "host",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ]
    .into_iter()
    .any(|forbidden| name.eq_ignore_ascii_case(forbidden))
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while let Some((first, rest)) = bytes.split_first()
        && first.is_ascii_whitespace()
    {
        bytes = rest;
    }
    while let Some((last, rest)) = bytes.split_last()
        && last.is_ascii_whitespace()
    {
        bytes = rest;
    }
    bytes
}

fn parse_chunk_size(bytes: &[u8]) -> Result<usize, ServerError> {
    let bytes = trim_ascii(bytes);
    let (size_field, extension) = match bytes.iter().position(|byte| *byte == b';') {
        Some(index) => (&bytes[..index], Some(&bytes[index + 1..])),
        None => (bytes, None),
    };
    if size_field.is_empty() || size_field.iter().any(|byte| byte.is_ascii_whitespace()) {
        return Err(ServerError::Parse);
    }
    if let Some(extension) = extension
        && extension.iter().any(|byte| matches!(*byte, b'\r' | b'\n'))
    {
        return Err(ServerError::Parse);
    }
    let size = size_field.iter().copied();
    let mut value = 0usize;
    let mut saw_digit = false;
    for byte in size {
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return Err(ServerError::Parse),
        };
        value = value
            .checked_mul(16)
            .and_then(|value| value.checked_add(digit as usize))
            .ok_or(ServerError::Parse)?;
        saw_digit = true;
    }
    if saw_digit {
        Ok(value)
    } else {
        Err(ServerError::Parse)
    }
}

fn find_crlf(bytes: &[u8]) -> Option<usize> {
    bytes.windows(2).position(|window| window == b"\r\n")
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

fn write_http_version(output: &mut Vec<u8>, version: u8) -> Result<(), ServerError> {
    match version {
        0 => output.extend_from_slice(b"HTTP/1.0"),
        1 => output.extend_from_slice(b"HTTP/1.1"),
        _ => return Err(ServerError::UnsupportedVersion),
    }
    Ok(())
}

fn write_str_headers(output: &mut Vec<u8>, headers: &[Header<'_>]) {
    for header in headers {
        output.extend_from_slice(header.name.as_bytes());
        output.extend_from_slice(b": ");
        output.extend_from_slice(header.value.as_bytes());
        output.extend_from_slice(b"\r\n");
    }
}

fn write_raw_headers(output: &mut Vec<u8>, headers: &[httparse::Header<'_>]) {
    for header in headers {
        output.extend_from_slice(header.name.as_bytes());
        output.extend_from_slice(b": ");
        output.extend_from_slice(header.value);
        output.extend_from_slice(b"\r\n");
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum H2FrameType {
    Data,
    Headers,
    Priority,
    RstStream,
    Settings,
    PushPromise,
    Ping,
    Goaway,
    WindowUpdate,
    Continuation,
    Unknown(u8),
}

impl H2FrameType {
    /// Converts a raw HTTP/2 frame type byte into a known frame type.
    pub fn from_u8(value: u8) -> Result<Self, ServerError> {
        match value {
            0 => Ok(Self::Data),
            1 => Ok(Self::Headers),
            2 => Ok(Self::Priority),
            3 => Ok(Self::RstStream),
            4 => Ok(Self::Settings),
            5 => Ok(Self::PushPromise),
            6 => Ok(Self::Ping),
            7 => Ok(Self::Goaway),
            8 => Ok(Self::WindowUpdate),
            9 => Ok(Self::Continuation),
            _ => Err(ServerError::InvalidFrame),
        }
    }

    /// Converts a raw HTTP/2 frame type byte, preserving unknown extension types.
    pub const fn from_raw(value: u8) -> Self {
        match value {
            0 => Self::Data,
            1 => Self::Headers,
            2 => Self::Priority,
            3 => Self::RstStream,
            4 => Self::Settings,
            5 => Self::PushPromise,
            6 => Self::Ping,
            7 => Self::Goaway,
            8 => Self::WindowUpdate,
            9 => Self::Continuation,
            _ => Self::Unknown(value),
        }
    }

    /// Returns the wire type byte.
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Data => 0,
            Self::Headers => 1,
            Self::Priority => 2,
            Self::RstStream => 3,
            Self::Settings => 4,
            Self::PushPromise => 5,
            Self::Ping => 6,
            Self::Goaway => 7,
            Self::WindowUpdate => 8,
            Self::Continuation => 9,
            Self::Unknown(value) => value,
        }
    }

    /// Returns whether this is an unknown extension frame type.
    pub const fn is_unknown(self) -> bool {
        matches!(self, Self::Unknown(_))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct H2Frame {
    pub frame_type: H2FrameType,
    pub flags: u8,
    pub stream_id: u32,
    pub payload: Vec<u8>,
}

/// A decoded HTTP/2 frame that borrows its payload from the input buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2FrameRef<'a> {
    pub frame_type: H2FrameType,
    pub flags: u8,
    pub stream_id: u32,
    pub payload: &'a [u8],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2FrameHead {
    pub frame_type: H2FrameType,
    pub flags: u8,
    pub stream_id: u32,
    pub payload_len: usize,
}

impl H2Frame {
    pub fn as_ref(&self) -> H2FrameRef<'_> {
        H2FrameRef {
            frame_type: self.frame_type,
            flags: self.flags,
            stream_id: self.stream_id,
            payload: &self.payload,
        }
    }

    pub fn encode(&self, output: &mut Vec<u8>) {
        self.as_ref().encode(output);
    }

    pub fn encode_header(
        frame_type: H2FrameType,
        flags: u8,
        stream_id: u32,
        payload_len: usize,
        output: &mut Vec<u8>,
    ) {
        output.push(((payload_len >> 16) & 0xff) as u8);
        output.push(((payload_len >> 8) & 0xff) as u8);
        output.push((payload_len & 0xff) as u8);
        output.push(frame_type.as_u8());
        output.push(flags);
        output.extend_from_slice(&(stream_id & 0x7fff_ffff).to_be_bytes());
    }

    pub fn decode_header(input: &[u8]) -> Result<H2FrameHead, ServerError> {
        if input.len() < 9 {
            return Err(ServerError::NeedMore);
        }
        let payload_len =
            ((input[0] as usize) << 16) | ((input[1] as usize) << 8) | input[2] as usize;
        let frame_type = H2FrameType::from_raw(input[3]);
        let flags = input[4];
        let stream_id = u32::from_be_bytes([input[5], input[6], input[7], input[8]]) & 0x7fff_ffff;
        Ok(H2FrameHead {
            frame_type,
            flags,
            stream_id,
            payload_len,
        })
    }

    pub fn decode(input: &[u8]) -> Result<(Self, usize), ServerError> {
        Self::decode_outcome(input).into_result()
    }

    pub fn decode_with_max_frame_size(
        input: &[u8],
        max_frame_size: usize,
    ) -> Result<(Self, usize), ServerError> {
        Self::decode_outcome_with_max_frame_size(input, max_frame_size).into_result()
    }

    pub fn decode_outcome(input: &[u8]) -> H2DecodeOutcome {
        Self::decode_outcome_with_max_frame_size(input, H2_MAX_MAX_FRAME_SIZE)
    }

    pub fn decode_outcome_with_max_frame_size(
        input: &[u8],
        max_frame_size: usize,
    ) -> H2DecodeOutcome {
        match H2FrameRef::decode_outcome_with_max_frame_size(input, max_frame_size) {
            H2FrameRefDecodeOutcome::Frame { frame, consumed } => H2DecodeOutcome::Frame {
                frame: frame.to_owned(),
                consumed,
            },
            H2FrameRefDecodeOutcome::NeedMore => H2DecodeOutcome::NeedMore,
            H2FrameRefDecodeOutcome::Error(error) => H2DecodeOutcome::Error(error),
        }
    }
}

impl<'a> H2FrameRef<'a> {
    pub fn encode(self, output: &mut Vec<u8>) {
        H2Frame::encode_header(
            self.frame_type,
            self.flags,
            self.stream_id,
            self.payload.len(),
            output,
        );
        output.extend_from_slice(self.payload);
    }

    pub fn decode(input: &'a [u8]) -> Result<(Self, usize), ServerError> {
        Self::decode_outcome(input).into_result()
    }

    pub fn decode_with_max_frame_size(
        input: &'a [u8],
        max_frame_size: usize,
    ) -> Result<(Self, usize), ServerError> {
        Self::decode_outcome_with_max_frame_size(input, max_frame_size).into_result()
    }

    pub fn decode_outcome(input: &'a [u8]) -> H2FrameRefDecodeOutcome<'a> {
        Self::decode_outcome_with_max_frame_size(input, H2_MAX_MAX_FRAME_SIZE)
    }

    pub fn decode_outcome_with_max_frame_size(
        input: &'a [u8],
        max_frame_size: usize,
    ) -> H2FrameRefDecodeOutcome<'a> {
        let head = match H2Frame::decode_header(input) {
            Ok(head) => head,
            Err(ServerError::NeedMore) => return H2FrameRefDecodeOutcome::NeedMore,
            Err(error) => {
                return H2FrameRefDecodeOutcome::Error(h2_error_from_server_error(error, None));
            }
        };
        if head.payload_len > max_frame_size {
            return H2FrameRefDecodeOutcome::Error(H2ProtocolError::connection(
                H2ErrorCode::FrameSizeError,
                "HTTP/2 frame exceeds configured maximum frame size",
            ));
        }
        match Self::decode_with_head(input, head) {
            Ok((frame, consumed)) => H2FrameRefDecodeOutcome::Frame { frame, consumed },
            Err(ServerError::NeedMore) => H2FrameRefDecodeOutcome::NeedMore,
            Err(error) => {
                H2FrameRefDecodeOutcome::Error(h2_error_from_server_error(error, Some(head)))
            }
        }
    }

    pub fn to_owned(self) -> H2Frame {
        H2Frame {
            frame_type: self.frame_type,
            flags: self.flags,
            stream_id: self.stream_id,
            payload: self.payload.to_vec(),
        }
    }

    pub const fn head(self) -> H2FrameHead {
        H2FrameHead {
            frame_type: self.frame_type,
            flags: self.flags,
            stream_id: self.stream_id,
            payload_len: self.payload.len(),
        }
    }

    fn decode_with_head(input: &'a [u8], head: H2FrameHead) -> Result<(Self, usize), ServerError> {
        let total = 9usize
            .checked_add(head.payload_len)
            .ok_or(ServerError::InvalidFrame)?;
        if input.len() < total {
            return Err(ServerError::NeedMore);
        }
        Ok((
            Self {
                frame_type: head.frame_type,
                flags: head.flags,
                stream_id: head.stream_id,
                payload: &input[9..total],
            },
            total,
        ))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum H2FrameRefDecodeOutcome<'a> {
    Frame {
        frame: H2FrameRef<'a>,
        consumed: usize,
    },
    NeedMore,
    Error(H2ProtocolError),
}

impl<'a> H2FrameRefDecodeOutcome<'a> {
    pub fn into_result(self) -> Result<(H2FrameRef<'a>, usize), ServerError> {
        match self {
            Self::Frame { frame, consumed } => Ok((frame, consumed)),
            Self::NeedMore => Err(ServerError::NeedMore),
            Self::Error(error) => Err(error.into()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum H2DecodeOutcome {
    Frame { frame: H2Frame, consumed: usize },
    NeedMore,
    Error(H2ProtocolError),
}

impl H2DecodeOutcome {
    pub fn into_result(self) -> Result<(H2Frame, usize), ServerError> {
        match self {
            Self::Frame { frame, consumed } => Ok((frame, consumed)),
            Self::NeedMore => Err(ServerError::NeedMore),
            Self::Error(error) => Err(error.into()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum H2ErrorScope {
    Connection,
    Stream(u32),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum H2ErrorCode {
    NoError = 0,
    ProtocolError = 1,
    InternalError = 2,
    FlowControlError = 3,
    SettingsTimeout = 4,
    StreamClosed = 5,
    FrameSizeError = 6,
    RefusedStream = 7,
    Cancel = 8,
    CompressionError = 9,
    ConnectError = 10,
    EnhanceYourCalm = 11,
    InadequateSecurity = 12,
    Http11Required = 13,
}

impl H2ErrorCode {
    /// Returns the HTTP/2 wire value for this error code.
    pub const fn as_u32(self) -> u32 {
        self as u32
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2ProtocolError {
    pub scope: H2ErrorScope,
    pub code: H2ErrorCode,
    pub debug: &'static str,
    pub hpack_error: Option<H2HpackError>,
    /// Semantic resource-limit category, when this is a limit violation.
    pub http_error_kind: Option<crate::HttpErrorKind>,
    /// Configured and observed values for a resource-limit violation.
    pub limit: Option<crate::LimitViolation>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
/// Stable, content-free categories for HPACK failures.
pub enum H2HpackError {
    /// An indexed representation refers outside the static and dynamic tables.
    HeaderIndexOutOfBounds,
    /// A prefixed integer is truncated or overflows.
    IntegerDecoding,
    /// A string length or payload is truncated or invalid.
    StringDecoding,
    /// A dynamic table size update exceeds the configured decoder maximum.
    InvalidMaxDynamicSize,
    /// A dynamic table size update has an invalid value, count, or position.
    InvalidTableSizeUpdate,
    /// A Huffman string contains EOS, invalid padding, or an invalid code.
    InvalidHuffman,
    /// A dynamic table size update appears after a field representation.
    TableSizeUpdateAfterField,
    /// The decoded field-section size exceeds the caller's limit.
    HeaderListTooLarge,
    /// The encoded header block exceeds the connection's configured limit.
    EncodedHeaderBlockTooLarge,
    /// Header field size accounting overflowed.
    FieldSizeOverflow,
    /// Internal HPACK state counters exhausted their representable range.
    StateOverflow,
    /// The decoder was already invalidated by a compression failure.
    DecoderPoisoned,
    /// A governed HPACK allocation could not be satisfied.
    AllocationFailed,
}

impl std::fmt::Display for H2HpackError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::HeaderIndexOutOfBounds => "HPACK header index is out of bounds",
            Self::IntegerDecoding => "HPACK integer is invalid",
            Self::StringDecoding => "HPACK string is invalid",
            Self::InvalidMaxDynamicSize => "HPACK dynamic table size update is invalid",
            Self::InvalidTableSizeUpdate => "HPACK dynamic table size update is invalid",
            Self::InvalidHuffman => "HPACK Huffman string is invalid",
            Self::TableSizeUpdateAfterField => {
                "HPACK dynamic table size update follows a header field"
            }
            Self::HeaderListTooLarge => "decoded HPACK header list exceeds its limit",
            Self::EncodedHeaderBlockTooLarge => "encoded HTTP/2 header block exceeds its limit",
            Self::FieldSizeOverflow => "HPACK field size overflows the platform size",
            Self::StateOverflow => "HPACK state exceeds its representable range",
            Self::DecoderPoisoned => "HPACK decoder is unavailable after a compression failure",
            Self::AllocationFailed => "HPACK storage allocation failed",
        })
    }
}

impl std::error::Error for H2HpackError {}

/// A content-free, directional snapshot of one connection's HPACK lifetime counters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct H2HpackDiagnosticsSnapshot {
    pub encoded_blocks: u64,
    pub decoded_blocks: u64,
    pub indexed_fields: u64,
    pub incremental_fields: u64,
    pub without_indexing_fields: u64,
    pub never_indexed_fields: u64,
    pub huffman_strings: u64,
    pub plain_strings: u64,
    pub table_size_updates: u64,
    pub table_insertions: u64,
    pub table_evictions: u64,
    pub compression_errors: u64,
    pub local_limit_failures: u64,
    pub field_octets: u64,
    pub wire_octets: u64,
}

/// An exact reduced HPACK wire-to-field-octet ratio.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct H2HpackEffectiveness {
    pub encoded_wire_octets: u64,
    pub uncompressed_field_octets: u64,
}

impl H2HpackDiagnosticsSnapshot {
    /// Returns an exact reduced `wire / field` fraction when neither operand
    /// saturated and at least one uncompressed field octet was observed.
    pub fn effectiveness(self) -> Option<H2HpackEffectiveness> {
        if self.wire_octets == u64::MAX || self.field_octets == 0 || self.field_octets == u64::MAX {
            return None;
        }
        let divisor = greatest_common_divisor(self.wire_octets, self.field_octets);
        Some(H2HpackEffectiveness {
            encoded_wire_octets: self.wire_octets / divisor,
            uncompressed_field_octets: self.field_octets / divisor,
        })
    }
}

impl From<crate::hpack::Diagnostics> for H2HpackDiagnosticsSnapshot {
    fn from(value: crate::hpack::Diagnostics) -> Self {
        Self {
            encoded_blocks: value.encoded_blocks,
            decoded_blocks: value.decoded_blocks,
            indexed_fields: value.indexed_fields,
            incremental_fields: value.incremental_fields,
            without_indexing_fields: value.without_indexing_fields,
            never_indexed_fields: value.never_indexed_fields,
            huffman_strings: value.huffman_strings,
            plain_strings: value.plain_strings,
            table_size_updates: value.table_size_updates,
            table_insertions: value.table_insertions,
            table_evictions: value.table_evictions,
            compression_errors: value.compression_errors,
            local_limit_failures: value.header_list_too_large,
            field_octets: value.field_bytes,
            wire_octets: value.wire_bytes,
        }
    }
}

const fn greatest_common_divisor(mut left: u64, mut right: u64) -> u64 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    if left == 0 { 1 } else { left }
}

impl From<crate::hpack::Error> for H2HpackError {
    fn from(error: crate::hpack::Error) -> Self {
        match error {
            crate::hpack::Error::InvalidIndex => Self::HeaderIndexOutOfBounds,
            crate::hpack::Error::IntegerOverflow | crate::hpack::Error::TruncatedInteger => {
                Self::IntegerDecoding
            }
            crate::hpack::Error::TruncatedString => Self::StringDecoding,
            crate::hpack::Error::InvalidMaxDynamicSize => Self::InvalidMaxDynamicSize,
            crate::hpack::Error::InvalidTableSizeUpdate => Self::InvalidTableSizeUpdate,
            crate::hpack::Error::TableSizeUpdateAfterField => Self::TableSizeUpdateAfterField,
            crate::hpack::Error::InvalidHuffman => Self::InvalidHuffman,
            crate::hpack::Error::HeaderListTooLarge { .. } => Self::HeaderListTooLarge,
            crate::hpack::Error::FieldSizeOverflow => Self::FieldSizeOverflow,
            crate::hpack::Error::StateOverflow => Self::StateOverflow,
            crate::hpack::Error::DecoderPoisoned => Self::DecoderPoisoned,
            crate::hpack::Error::AllocationFailed => Self::AllocationFailed,
        }
    }
}

impl From<H2ProtocolError> for ServerError {
    fn from(error: H2ProtocolError) -> Self {
        match error.code {
            H2ErrorCode::CompressionError => Self::InvalidHpack,
            H2ErrorCode::FrameSizeError
            | H2ErrorCode::ProtocolError
            | H2ErrorCode::FlowControlError
            | H2ErrorCode::StreamClosed
            | H2ErrorCode::RefusedStream
            | H2ErrorCode::Cancel
            | H2ErrorCode::SettingsTimeout
            | H2ErrorCode::EnhanceYourCalm
            | H2ErrorCode::NoError
            | H2ErrorCode::InternalError
            | H2ErrorCode::ConnectError
            | H2ErrorCode::InadequateSecurity
            | H2ErrorCode::Http11Required => Self::InvalidFrame,
        }
    }
}

impl H2ProtocolError {
    /// Returns whether this failure terminates the connection or one stream.
    pub const fn scope(&self) -> H2ErrorScope {
        self.scope
    }

    /// Returns the HTTP/2 error code that must be sent to the peer.
    pub const fn error_code(&self) -> H2ErrorCode {
        self.code
    }

    /// Returns stable, allocation-free semantic information about this failure.
    pub fn classify(&self) -> crate::HttpErrorInfo {
        use crate::{HttpErrorInfo, HttpErrorKind, HttpErrorScope};

        let kind = self.http_error_kind.unwrap_or(match self.code {
            H2ErrorCode::FlowControlError => HttpErrorKind::FlowControlViolation,
            H2ErrorCode::FrameSizeError => HttpErrorKind::InvalidFraming,
            H2ErrorCode::CompressionError => HttpErrorKind::Compression,
            H2ErrorCode::ProtocolError => HttpErrorKind::MalformedMessage,
            H2ErrorCode::StreamClosed
            | H2ErrorCode::InternalError
            | H2ErrorCode::SettingsTimeout => HttpErrorKind::InvalidState,
            H2ErrorCode::RefusedStream | H2ErrorCode::Cancel => HttpErrorKind::PeerReset,
            H2ErrorCode::NoError
            | H2ErrorCode::ConnectError
            | H2ErrorCode::EnhanceYourCalm
            | H2ErrorCode::InadequateSecurity
            | H2ErrorCode::Http11Required => HttpErrorKind::UnsupportedFeature,
        });
        let scope = match self.scope {
            H2ErrorScope::Connection => HttpErrorScope::Connection,
            H2ErrorScope::Stream(stream_id) => HttpErrorScope::Stream(stream_id),
        };
        HttpErrorInfo::new(kind, scope, self.debug, self.limit)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum H2FrameOutcome<E> {
    Event(E),
    Ignored,
    Error(H2ProtocolError),
}

impl<E> H2FrameOutcome<E> {
    fn map_event<T>(self, map: impl FnOnce(E) -> T) -> H2FrameOutcome<T> {
        match self {
            Self::Event(event) => H2FrameOutcome::Event(map(event)),
            Self::Ignored => H2FrameOutcome::Ignored,
            Self::Error(error) => H2FrameOutcome::Error(error),
        }
    }
}

impl H2ProtocolError {
    pub const fn connection(code: H2ErrorCode, debug: &'static str) -> Self {
        Self {
            scope: H2ErrorScope::Connection,
            code,
            debug,
            hpack_error: None,
            http_error_kind: None,
            limit: None,
        }
    }

    pub const fn stream(stream_id: u32, code: H2ErrorCode, debug: &'static str) -> Self {
        Self {
            scope: H2ErrorScope::Stream(stream_id),
            code,
            debug,
            hpack_error: None,
            http_error_kind: None,
            limit: None,
        }
    }

    pub const fn hpack(
        scope: H2ErrorScope,
        hpack_error: H2HpackError,
        debug: &'static str,
    ) -> Self {
        Self {
            scope,
            code: H2ErrorCode::CompressionError,
            debug,
            hpack_error: Some(hpack_error),
            http_error_kind: None,
            limit: None,
        }
    }

    const fn resource_limit(
        scope: H2ErrorScope,
        kind: crate::HttpErrorKind,
        limit: usize,
        actual: usize,
        debug: &'static str,
    ) -> Self {
        Self {
            scope,
            code: H2ErrorCode::EnhanceYourCalm,
            debug,
            hpack_error: None,
            http_error_kind: Some(kind),
            limit: Some(crate::LimitViolation::new(limit, Some(actual))),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2Limits {
    pub max_frame_size: usize,
    pub max_settings_entries: usize,
    pub max_encoded_header_block_size: usize,
    pub max_header_list_size: usize,
    pub max_header_table_size: usize,
    pub max_continuation_frames: usize,
    pub max_active_streams: usize,
    pub max_queued_control_frames: usize,
    pub max_queued_data_bytes: usize,
    pub max_closed_stream_tombstones: usize,
}

impl Default for H2Limits {
    fn default() -> Self {
        Self {
            max_frame_size: H2Settings::default().max_frame_size,
            max_settings_entries: 64,
            max_encoded_header_block_size: H2_DEFAULT_MAX_HEADER_LIST_SIZE,
            max_header_list_size: H2_DEFAULT_MAX_HEADER_LIST_SIZE,
            max_header_table_size: H2Settings::default().header_table_size as usize,
            max_continuation_frames: 64,
            max_active_streams: H2_DEFAULT_MAX_ACTIVE_STREAMS,
            max_queued_control_frames: 10_000,
            max_queued_data_bytes: 16 * 1024 * 1024,
            max_closed_stream_tombstones: 1_024,
        }
    }
}

impl H2Limits {
    /// Derives HPACK, active-stream, and queued-DATA bounds from shared HTTP limits.
    pub fn from_http_limits(limits: HttpLimits) -> Self {
        Self {
            max_encoded_header_block_size: limits.max_header_bytes(),
            max_header_list_size: limits.max_header_bytes(),
            max_active_streams: limits.max_active_streams(),
            max_queued_data_bytes: limits.max_body_bytes(),
            ..Self::default()
        }
    }
}

impl From<HttpLimits> for H2Limits {
    fn from(limits: HttpLimits) -> Self {
        Self::from_http_limits(limits)
    }
}

fn validate_h2_limits(limits: H2Limits) -> Result<(), ServerError> {
    if limits.max_frame_size < H2_MIN_MAX_FRAME_SIZE
        || limits.max_frame_size > H2_MAX_MAX_FRAME_SIZE
        || limits.max_active_streams == 0
    {
        return Err(ServerError::InvalidFrame);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2FlowControlWindow {
    available: i32,
}

impl H2FlowControlWindow {
    pub fn new(size: u32) -> Result<Self, ServerError> {
        if size > H2_MAX_WINDOW_SIZE {
            return Err(ServerError::FlowControlViolation);
        }
        Ok(Self {
            available: size as i32,
        })
    }

    pub const fn available(self) -> i32 {
        self.available
    }

    pub fn consume(&mut self, amount: usize) -> Result<(), ServerError> {
        let amount = i32::try_from(amount).map_err(|_| ServerError::FlowControlViolation)?;
        if amount > self.available {
            return Err(ServerError::FlowControlViolation);
        }
        self.available -= amount;
        Ok(())
    }

    pub fn increase(&mut self, amount: u32) -> Result<(), ServerError> {
        if amount == 0 {
            return Err(ServerError::FlowControlViolation);
        }
        let amount = i32::try_from(amount).map_err(|_| ServerError::FlowControlViolation)?;
        let next = self
            .available
            .checked_add(amount)
            .ok_or(ServerError::FlowControlViolation)?;
        if next > H2_MAX_WINDOW_SIZE as i32 {
            return Err(ServerError::FlowControlViolation);
        }
        self.available = next;
        Ok(())
    }

    pub fn adjust(&mut self, delta: i32) -> Result<(), ServerError> {
        let next = self
            .available
            .checked_add(delta)
            .ok_or(ServerError::FlowControlViolation)?;
        if next > H2_MAX_WINDOW_SIZE as i32 {
            return Err(ServerError::FlowControlViolation);
        }
        self.available = next;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2ReceiveWindow {
    limit: usize,
    available: usize,
    pending_update: usize,
    extra_credit: usize,
    threshold_divisor: usize,
    adaptive: Option<H2AdaptiveReceiveWindow>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct H2AdaptiveReceiveWindow {
    max_limit: usize,
    target_rtt: Duration,
    sample_started_at: Instant,
    sample_received: usize,
    sample_consumed: usize,
    sample_needs_reanchor: bool,
    fast_turnovers: u8,
    blocked_since: Option<Instant>,
    total_received: u64,
    total_consumed: u64,
    total_blocked: Duration,
    last_blocked: Duration,
    rtt_proxy: Duration,
    estimated_bdp: usize,
    growth_events: u64,
    growth_bytes: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct H2ReceiveWindowDiagnostics {
    pub current_window_bytes: usize,
    pub max_window_bytes: usize,
    pub received_bytes: u64,
    pub consumed_bytes: u64,
    pub blocked_time: Duration,
    pub last_blocked_time: Duration,
    pub rtt_proxy: Duration,
    pub estimated_bdp_bytes: usize,
    pub growth_events: u64,
    pub growth_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2SendCapacity {
    pub stream_id: u32,
    pub sendable_bytes: usize,
    pub has_pending_data: bool,
    pub stream_window_blocked: bool,
    pub connection_window_blocked: bool,
}

impl H2SendCapacity {
    pub fn new(
        stream_id: u32,
        pending_bytes: usize,
        stream_window: i32,
        connection_window: usize,
    ) -> Self {
        let stream_credit = usize::try_from(stream_window).unwrap_or(0);
        let sendable_bytes = pending_bytes.min(stream_credit).min(connection_window);
        Self {
            stream_id,
            sendable_bytes,
            has_pending_data: pending_bytes > 0,
            stream_window_blocked: pending_bytes > 0 && stream_window <= 0,
            connection_window_blocked: pending_bytes > 0 && connection_window == 0,
        }
    }
}

/// A state-checked DATA frame header and borrowed-payload length.
///
/// The payload remains owned by the adapter. After the header and the indicated
/// payload prefix have been handed to the transport, the adapter must call the
/// matching connection's `commit_data_frame` method.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2DataFramePlan {
    stream_id: u32,
    payload_len: usize,
    end_stream: bool,
    header: [u8; 9],
}

impl H2DataFramePlan {
    pub const fn stream_id(self) -> u32 {
        self.stream_id
    }

    pub const fn payload_len(self) -> usize {
        self.payload_len
    }

    pub const fn end_stream(self) -> bool {
        self.end_stream
    }

    pub const fn header(&self) -> &[u8; 9] {
        &self.header
    }
}

fn h2_data_frame_plan(stream_id: u32, payload_len: usize, end_stream: bool) -> H2DataFramePlan {
    debug_assert!(stream_id != 0 && stream_id <= 0x7fff_ffff);
    debug_assert!(payload_len <= H2_MAX_MAX_FRAME_SIZE);
    let payload_len_u32 = payload_len as u32;
    H2DataFramePlan {
        stream_id,
        payload_len,
        end_stream,
        header: [
            (payload_len_u32 >> 16) as u8,
            (payload_len_u32 >> 8) as u8,
            payload_len_u32 as u8,
            0,
            u8::from(end_stream),
            (stream_id >> 24) as u8 & 0x7f,
            (stream_id >> 16) as u8,
            (stream_id >> 8) as u8,
            stream_id as u8,
        ],
    }
}

/// Inclusive upper bounds for the scheduler's per-stream selection histogram.
pub const H2_SCHEDULER_SELECTION_BUCKET_UPPER_BOUNDS: [u64; 8] = [0, 1, 3, 7, 15, 31, 63, u64::MAX];

/// Mutually exclusive primary reason for a failed HTTP/2 send-capacity pass.
///
/// This exhaustive enum is the closed HTTP/2 window-exhaustion domain:
/// capacity can be blocked by either the stream window or the connection
/// window. Non-window flush stops belong to the adapter's stop-reason type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum H2FlowStall {
    StreamWindow,
    ConnectionWindow,
}

impl H2FlowStall {
    /// Classifies one failed pass, giving connection exhaustion precedence.
    pub const fn classify(
        stream_window_blocked: bool,
        connection_window_blocked: bool,
    ) -> Option<Self> {
        if connection_window_blocked {
            Some(Self::ConnectionWindow)
        } else if stream_window_blocked {
            Some(Self::StreamWindow)
        } else {
            None
        }
    }
}

/// Fixed-size, saturating HTTP/2 flow-control counters for one connection.
///
/// Adapters detect protocol events but record all flow events in this native
/// accumulator. The counters cover the connection lifetime and never reset.
/// This is a trusted, low-level counter carrier: callers must classify failed
/// passes connection-first and must record only post-startup WINDOW_UPDATE
/// refunds accepted by the caller-owned downstream output layer. Supported
/// adapters enforce that ownership boundary before invoking the raw mutators.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct H2FlowDiagnostics {
    /// Failed passes attributed to stream-window exhaustion.
    pub stream_window_stalls: u64,
    /// Failed passes attributed to connection-window exhaustion.
    pub connection_window_stalls: u64,
    /// Post-startup stream WINDOW_UPDATE refunds accepted by downstream output.
    pub stream_window_updates: u64,
    /// Post-startup connection WINDOW_UPDATE refunds accepted by downstream output.
    pub connection_window_updates: u64,
    /// Stream receive-credit bytes returned by post-startup WINDOW_UPDATE frames.
    pub stream_window_update_bytes: u64,
    /// Connection receive-credit bytes returned by post-startup WINDOW_UPDATE frames.
    pub connection_window_update_bytes: u64,
}

impl H2FlowDiagnostics {
    /// Records one mutually exclusive failed-pass reason.
    pub fn record_stall(&mut self, reason: H2FlowStall) {
        match reason {
            H2FlowStall::StreamWindow => self.record_stream_window_stall(),
            H2FlowStall::ConnectionWindow => self.record_connection_window_stall(),
        }
    }

    /// Records one failed pass caused by stream-window exhaustion.
    pub fn record_stream_window_stall(&mut self) {
        self.stream_window_stalls = self.stream_window_stalls.saturating_add(1);
    }

    /// Records one failed pass caused by connection-window exhaustion.
    pub fn record_connection_window_stall(&mut self) {
        self.connection_window_stalls = self.connection_window_stalls.saturating_add(1);
    }

    /// Records one classified post-startup receive-credit refund committed to output.
    pub fn record_window_update(&mut self, stream_id: u32, amount: usize) {
        let amount = u64::try_from(amount).unwrap_or(u64::MAX);
        if stream_id == 0 {
            self.connection_window_updates = self.connection_window_updates.saturating_add(1);
            self.connection_window_update_bytes =
                self.connection_window_update_bytes.saturating_add(amount);
        } else {
            self.stream_window_updates = self.stream_window_updates.saturating_add(1);
            self.stream_window_update_bytes =
                self.stream_window_update_bytes.saturating_add(amount);
        }
    }

    /// Materializes a snapshot with current adapter and scheduler gauges.
    pub fn snapshot(
        self,
        pending_capacity_streams: usize,
        scheduler: H2FairStreamSchedulerDiagnostics,
    ) -> H2FlowDiagnosticsSnapshot {
        H2FlowDiagnosticsSnapshot {
            stream_window_stalls: self.stream_window_stalls,
            connection_window_stalls: self.connection_window_stalls,
            stream_window_updates: self.stream_window_updates,
            connection_window_updates: self.connection_window_updates,
            stream_window_update_bytes: self.stream_window_update_bytes,
            connection_window_update_bytes: self.connection_window_update_bytes,
            pending_capacity_streams,
            scheduler,
        }
    }
}

/// Runtime-neutral HTTP/2 flow-control and fair-scheduler snapshot.
///
/// Counter fields are saturating connection-lifetime values. Gauge fields
/// describe the connection when the snapshot was materialized.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct H2FlowDiagnosticsSnapshot {
    /// Failed passes attributed to stream-window exhaustion.
    pub stream_window_stalls: u64,
    /// Failed passes attributed to connection-window exhaustion.
    pub connection_window_stalls: u64,
    /// Post-startup stream WINDOW_UPDATE refunds accepted by downstream output.
    pub stream_window_updates: u64,
    /// Post-startup connection WINDOW_UPDATE refunds accepted by downstream output.
    pub connection_window_updates: u64,
    /// Stream receive-credit bytes returned by post-startup WINDOW_UPDATE frames.
    pub stream_window_update_bytes: u64,
    /// Connection receive-credit bytes returned by post-startup WINDOW_UPDATE frames.
    pub connection_window_update_bytes: u64,
    /// Enrolled streams with pending DATA and no applicable send credit.
    pub pending_capacity_streams: usize,
    /// Lifetime fair-scheduler diagnostics for the connection.
    pub scheduler: H2FairStreamSchedulerDiagnostics,
}

/// Bounded scheduler counters and lifetime scheduler-turn distribution.
///
/// All `u64` counters saturate. The fixed histogram describes successful fair
/// scheduler turns; it is workload-shape telemetry, not by itself proof of
/// fairness because it does not encode stream demand or eligibility time.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct H2FairStreamSchedulerDiagnostics {
    /// Number of stream IDs observed over the connection lifetime.
    ///
    /// Includes both currently tracked and retired streams and saturates at
    /// `usize::MAX`.
    pub observed_streams: usize,
    /// Streams currently retained, including queued and temporarily drained streams.
    pub tracked_streams: usize,
    /// Streams currently enrolled for arbitration, including capacity-blocked streams.
    pub queued_streams: usize,
    /// Connection-lifetime calls to [`H2FairStreamScheduler::next_ready`].
    pub selection_attempts: u64,
    /// Connection-lifetime successful fair selections returned by `next_ready`.
    pub selections: u64,
    /// Direct flush invocations that emitted at least one DATA frame.
    ///
    /// This is neither a DATA-frame count nor a fair-turn-equivalent
    /// denominator. It does not contribute to `selections` or the histogram.
    pub immediate_dispatches: u64,
    /// Connection-lifetime fair attempts that found no eligible stream.
    pub no_ready_attempts: u64,
    /// Connection-lifetime live-stream readiness probes that returned false.
    ///
    /// The intrusive queue contains no stale entries, so this excludes no
    /// hidden cleanup work.
    pub skipped_streams: u64,
    /// Maximum live-stream readiness probes performed by one fair attempt.
    pub max_scan_depth: usize,
    /// Minimum fair selections for an observed stream, or zero if none exist.
    pub min_stream_selections: u64,
    /// Maximum fair selections for an observed stream, or zero if none exist.
    pub max_stream_selections: u64,
    /// Observed active and retired streams grouped by inclusive fair-turn bounds.
    ///
    /// Bucket counts saturate at `usize::MAX`.
    pub stream_selection_buckets: [usize; H2_SCHEDULER_SELECTION_BUCKET_UPPER_BOUNDS.len()],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct H2ScheduledStream {
    stream_id: u32,
    queued: bool,
    previous: Option<usize>,
    next: Option<usize>,
    selections: u64,
}

type H2StreamSlots = HashMap<u32, usize>;

/// Connection-local round-robin scheduler for HTTP/2 stream send opportunities.
///
/// `register` enrolls a stream, `mark_drained` temporarily removes it from
/// arbitration while retaining its lifetime selection count, and `remove`
/// retires that count permanently. All lifecycle operations are idempotent.
///
/// One scheduler belongs to exactly one HTTP/2 connection. Within that
/// connection, stream IDs are monotonic and an ID must never be registered
/// again after `remove`. A drained but not removed stream may be re-registered.
/// Network-facing owners enforce their configured active-stream limit before
/// calling `register`.
/// The intrusive queue has one physical entry per queued stream, so lifecycle
/// churn cannot retain stale queue tokens. Equality compares operational queue
/// state and intentionally ignores diagnostic history.
#[derive(Clone, Debug, Default)]
pub struct H2FairStreamScheduler {
    stream_slots: H2StreamSlots,
    slots: Vec<H2ScheduledStream>,
    free_head: Option<usize>,
    ready_head: Option<usize>,
    ready_tail: Option<usize>,
    queued_streams: usize,
    selection_attempts: u64,
    selections: u64,
    immediate_dispatches: u64,
    no_ready_attempts: u64,
    skipped_streams: u64,
    max_scan_depth: usize,
    retired_streams: usize,
    retired_min_stream_selections: u64,
    retired_max_stream_selections: u64,
    retired_stream_selection_buckets: [usize; H2_SCHEDULER_SELECTION_BUCKET_UPPER_BOUNDS.len()],
}

impl PartialEq for H2FairStreamScheduler {
    fn eq(&self, other: &Self) -> bool {
        self.slot_stream_id(self.ready_head) == other.slot_stream_id(other.ready_head)
            && self.slot_stream_id(self.ready_tail) == other.slot_stream_id(other.ready_tail)
            && self.queued_streams == other.queued_streams
            && self.stream_slots.len() == other.stream_slots.len()
            && self.stream_slots.iter().all(|(stream_id, slot)| {
                let stream = &self.slots[*slot];
                other.stream_slots.get(stream_id).is_some_and(|other_slot| {
                    let other_stream = &other.slots[*other_slot];
                    stream.queued == other_stream.queued
                        && self.slot_stream_id(stream.previous)
                            == other.slot_stream_id(other_stream.previous)
                        && self.slot_stream_id(stream.next)
                            == other.slot_stream_id(other_stream.next)
                })
            })
    }
}

impl Eq for H2FairStreamScheduler {}

impl H2FairStreamScheduler {
    /// Creates a scheduler pre-sized up to the default active-stream cap.
    ///
    /// The map reserves additional deletion headroom so repeated full-cap
    /// generations can reuse storage without rehash allocation. Larger logical
    /// caps grow on demand instead of creating unbounded eager reservations.
    pub fn with_capacity(max_active_streams: usize) -> Self {
        let initial_capacity = max_active_streams.min(H2_DEFAULT_MAX_ACTIVE_STREAMS);
        Self {
            stream_slots: HashMap::with_capacity(initial_capacity.saturating_mul(2)),
            slots: Vec::with_capacity(initial_capacity),
            ..Self::default()
        }
    }

    /// Enrolls a new or temporarily drained stream at the back of the queue.
    ///
    /// A stream ID retired by [`Self::remove`] must not be registered again.
    pub fn register(&mut self, stream_id: u32) {
        let slot = match self.stream_slots.entry(stream_id) {
            Entry::Occupied(entry) => {
                let slot = *entry.get();
                if self.slots[slot].queued {
                    return;
                }
                slot
            }
            Entry::Vacant(entry) => {
                let stream = H2ScheduledStream {
                    stream_id,
                    queued: false,
                    previous: None,
                    next: None,
                    selections: 0,
                };
                let slot = if let Some(slot) = self.free_head {
                    self.free_head = self.slots[slot].next;
                    self.slots[slot] = stream;
                    slot
                } else {
                    let slot = self.slots.len();
                    self.slots.push(stream);
                    slot
                };
                entry.insert(slot);
                slot
            }
        };
        self.enqueue(slot);
    }

    /// Retires a stream and contributes its fair-selection count exactly once.
    pub fn remove(&mut self, stream_id: u32) {
        let Some(slot) = self.stream_slots.remove(&stream_id) else {
            return;
        };
        self.unlink(slot);
        let selections = self.slots[slot].selections;
        if self.retired_streams == 0 {
            self.retired_min_stream_selections = selections;
        } else {
            self.retired_min_stream_selections = self.retired_min_stream_selections.min(selections);
        }
        self.retired_max_stream_selections = self.retired_max_stream_selections.max(selections);
        self.retired_streams = self.retired_streams.saturating_add(1);
        let bucket = h2_scheduler_selection_bucket(selections);
        self.retired_stream_selection_buckets[bucket] =
            self.retired_stream_selection_buckets[bucket].saturating_add(1);

        let stream = &mut self.slots[slot];
        stream.previous = None;
        stream.next = self.free_head;
        stream.selections = 0;
        self.free_head = Some(slot);
    }

    /// Selects the next eligible stream in round-robin order.
    pub fn next_ready<F>(&mut self, mut is_ready: F) -> Option<u32>
    where
        F: FnMut(u32) -> bool,
    {
        self.selection_attempts = self.selection_attempts.saturating_add(1);
        let len = self.queued_streams;
        let mut scan_depth = 0usize;
        for _ in 0..len {
            let slot = self.ready_head?;
            let stream_id = self.slots[slot].stream_id;
            self.rotate_front_to_back(slot);
            scan_depth = scan_depth.saturating_add(1);
            if is_ready(stream_id) {
                let stream = &mut self.slots[slot];
                stream.selections = stream.selections.saturating_add(1);
                self.selections = self.selections.saturating_add(1);
                self.max_scan_depth = self.max_scan_depth.max(scan_depth);
                return Some(stream_id);
            }
            self.skipped_streams = self.skipped_streams.saturating_add(1);
        }
        self.max_scan_depth = self.max_scan_depth.max(scan_depth);
        self.no_ready_attempts = self.no_ready_attempts.saturating_add(1);
        None
    }

    /// Records an immediate adapter dispatch that emitted DATA.
    pub fn record_immediate_dispatch(&mut self) {
        self.immediate_dispatches = self.immediate_dispatches.saturating_add(1);
    }

    /// Temporarily removes a drained stream from fair arbitration in O(1).
    pub fn mark_drained(&mut self, stream_id: u32) {
        if let Some(slot) = self.stream_slots.get(&stream_id).copied() {
            self.unlink(slot);
        }
    }

    /// Returns the number of streams enrolled for fair arbitration.
    pub fn len(&self) -> usize {
        self.queued_streams
    }

    /// Returns whether no stream is enrolled for fair arbitration.
    pub fn is_empty(&self) -> bool {
        self.queued_streams == 0
    }

    /// Counts enrolled streams matching an adapter-defined capacity predicate.
    pub fn pending_capacity_streams<F>(&self, mut is_pending: F) -> usize
    where
        F: FnMut(u32) -> bool,
    {
        self.stream_slots
            .iter()
            .filter(|(stream_id, slot)| self.slots[**slot].queued && is_pending(**stream_id))
            .count()
    }

    /// Returns fixed-size lifetime scheduling diagnostics without allocation.
    pub fn diagnostics(&self) -> H2FairStreamSchedulerDiagnostics {
        let mut diagnostics = H2FairStreamSchedulerDiagnostics {
            observed_streams: self.retired_streams.saturating_add(self.stream_slots.len()),
            tracked_streams: self.stream_slots.len(),
            queued_streams: self.queued_streams,
            selection_attempts: self.selection_attempts,
            selections: self.selections,
            immediate_dispatches: self.immediate_dispatches,
            no_ready_attempts: self.no_ready_attempts,
            skipped_streams: self.skipped_streams,
            max_scan_depth: self.max_scan_depth,
            min_stream_selections: self.retired_min_stream_selections,
            max_stream_selections: self.retired_max_stream_selections,
            stream_selection_buckets: self.retired_stream_selection_buckets,
        };

        if diagnostics.observed_streams == 0 {
            return diagnostics;
        }

        if self.retired_streams == 0 {
            diagnostics.min_stream_selections = u64::MAX;
        }
        for slot in self.stream_slots.values() {
            let stream = &self.slots[*slot];
            diagnostics.min_stream_selections =
                diagnostics.min_stream_selections.min(stream.selections);
            diagnostics.max_stream_selections =
                diagnostics.max_stream_selections.max(stream.selections);
            let bucket = h2_scheduler_selection_bucket(stream.selections);
            diagnostics.stream_selection_buckets[bucket] =
                diagnostics.stream_selection_buckets[bucket].saturating_add(1);
        }
        diagnostics
    }

    fn enqueue(&mut self, slot: usize) {
        let previous = self.ready_tail;
        if let Some(tail) = previous {
            self.slots[tail].next = Some(slot);
        } else {
            self.ready_head = Some(slot);
        }
        let stream = &mut self.slots[slot];
        stream.queued = true;
        stream.previous = previous;
        stream.next = None;
        self.ready_tail = Some(slot);
        self.queued_streams = self.queued_streams.saturating_add(1);
    }

    fn unlink(&mut self, slot: usize) {
        let stream = &self.slots[slot];
        if !stream.queued {
            return;
        }
        let previous = stream.previous;
        let next = stream.next;

        if let Some(previous) = previous {
            self.slots[previous].next = next;
        } else {
            self.ready_head = next;
        }
        if let Some(next) = next {
            self.slots[next].previous = previous;
        } else {
            self.ready_tail = previous;
        }
        let stream = &mut self.slots[slot];
        stream.queued = false;
        stream.previous = None;
        stream.next = None;
        self.queued_streams = self.queued_streams.saturating_sub(1);
    }

    fn rotate_front_to_back(&mut self, slot: usize) {
        if self.ready_head == self.ready_tail {
            return;
        }
        let next = self.slots[slot]
            .next
            .expect("multi-stream scheduler head has a successor");
        let tail = self.ready_tail.expect("non-empty scheduler has a tail");

        self.slots[next].previous = None;
        self.slots[tail].next = Some(slot);
        let stream = &mut self.slots[slot];
        stream.previous = Some(tail);
        stream.next = None;
        self.ready_head = Some(next);
        self.ready_tail = Some(slot);
    }

    fn slot_stream_id(&self, slot: Option<usize>) -> Option<u32> {
        slot.map(|slot| self.slots[slot].stream_id)
    }
}

fn h2_scheduler_selection_bucket(selections: u64) -> usize {
    H2_SCHEDULER_SELECTION_BUCKET_UPPER_BOUNDS
        .iter()
        .position(|upper_bound| selections <= *upper_bound)
        .expect("final scheduler selection bucket is unbounded")
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct H2BackpressureState {
    pub pending_streams: usize,
    pub queued_data_bytes: usize,
    pub queued_control_frames: usize,
    pub data_over_limit: bool,
    pub control_over_limit: bool,
}

impl H2BackpressureState {
    pub fn new(
        pending_streams: usize,
        queued_data_bytes: usize,
        queued_control_frames: usize,
        limits: H2Limits,
    ) -> Self {
        Self {
            pending_streams,
            queued_data_bytes,
            queued_control_frames,
            data_over_limit: queued_data_bytes > limits.max_queued_data_bytes,
            control_over_limit: queued_control_frames > limits.max_queued_control_frames,
        }
    }

    pub const fn should_read_more(self) -> bool {
        !self.data_over_limit && !self.control_over_limit
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum H2SettingsSyncState {
    #[default]
    Synced,
    WaitingAck,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct H2ControlDiagnostics {
    pub settings_frames: u64,
    pub settings_acks: u64,
    pub pings: u64,
    pub ping_acks: u64,
    pub resets: u64,
    pub goaways: u64,
    pub ignored_frames: u64,
    pub control_rejections: u64,
    pub settings_rejections: u64,
    pub window_update_rejections: u64,
    pub ping_rejections: u64,
    pub reset_rejections: u64,
    pub goaway_rejections: u64,
    pub priority_rejections: u64,
}

impl H2ControlDiagnostics {
    fn record_rejection(&mut self, frame_type: H2FrameType) {
        self.control_rejections = self.control_rejections.saturating_add(1);
        match frame_type {
            H2FrameType::Settings => {
                self.settings_rejections = self.settings_rejections.saturating_add(1);
            }
            H2FrameType::WindowUpdate => {
                self.window_update_rejections = self.window_update_rejections.saturating_add(1);
            }
            H2FrameType::Ping => {
                self.ping_rejections = self.ping_rejections.saturating_add(1);
            }
            H2FrameType::RstStream => {
                self.reset_rejections = self.reset_rejections.saturating_add(1);
            }
            H2FrameType::Goaway => {
                self.goaway_rejections = self.goaway_rejections.saturating_add(1);
            }
            H2FrameType::Priority => {
                self.priority_rejections = self.priority_rejections.saturating_add(1);
            }
            H2FrameType::Data
            | H2FrameType::Headers
            | H2FrameType::Continuation
            | H2FrameType::PushPromise
            | H2FrameType::Unknown(_) => {}
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum H2TimerIntent {
    SettingsAckTimeout,
    GracefulShutdownPing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum H2ShutdownIntent {
    None,
    Drain { last_stream_id: u32 },
    Close,
}

impl H2ReceiveWindow {
    pub const fn new(limit: usize) -> Self {
        Self {
            limit,
            available: limit,
            pending_update: 0,
            extra_credit: 0,
            threshold_divisor: 4,
            adaptive: None,
        }
    }

    pub fn with_adaptive_growth(
        limit: usize,
        max_limit: usize,
        target_rtt: Duration,
        now: Instant,
    ) -> Result<Self, ServerError> {
        if limit == 0
            || limit > H2_MAX_WINDOW_SIZE as usize
            || max_limit < limit
            || max_limit > H2_MAX_WINDOW_SIZE as usize
            || target_rtt.is_zero()
        {
            return Err(ServerError::InvalidFrame);
        }
        Ok(Self {
            adaptive: Some(H2AdaptiveReceiveWindow {
                max_limit,
                target_rtt,
                sample_started_at: now,
                sample_received: 0,
                sample_consumed: 0,
                sample_needs_reanchor: false,
                fast_turnovers: 0,
                blocked_since: None,
                total_received: 0,
                total_consumed: 0,
                total_blocked: Duration::ZERO,
                last_blocked: Duration::ZERO,
                rtt_proxy: target_rtt,
                estimated_bdp: limit,
                growth_events: 0,
                growth_bytes: 0,
            }),
            ..Self::new(limit)
        })
    }

    pub const fn limit(self) -> usize {
        self.limit
    }

    pub const fn available(self) -> usize {
        self.available
    }

    pub const fn pending_update(self) -> usize {
        self.pending_update
    }

    pub fn receive_data(&mut self, amount: usize) -> Result<(), ServerError> {
        if self.adaptive.is_some() {
            return self.receive_data_at(amount, Instant::now());
        }
        self.receive_data_inner(amount)
    }

    pub fn receive_data_at(&mut self, amount: usize, now: Instant) -> Result<(), ServerError> {
        if amount > self.available {
            return Err(ServerError::InvalidFrame);
        }
        if let Some(mut adaptive) = self.adaptive {
            if amount > 0 && (adaptive.total_received == 0 || adaptive.sample_needs_reanchor) {
                adaptive.sample_started_at = now;
                adaptive.sample_needs_reanchor = false;
            }
            if let Some(blocked_since) = adaptive.blocked_since.take() {
                let blocked = now
                    .checked_duration_since(blocked_since)
                    .unwrap_or_default();
                adaptive.last_blocked = blocked;
                adaptive.total_blocked = adaptive.total_blocked.saturating_add(blocked);
                adaptive.rtt_proxy = adaptive.target_rtt.max(blocked);
            }
            let elapsed = now
                .checked_duration_since(adaptive.sample_started_at)
                .unwrap_or_default();
            if elapsed > adaptive.target_rtt && adaptive.sample_received < self.limit {
                adaptive.sample_started_at = now;
                adaptive.sample_received = 0;
                adaptive.sample_consumed = 0;
                adaptive.fast_turnovers = 0;
            }
            adaptive.sample_received = adaptive.sample_received.saturating_add(amount);
            adaptive.total_received = adaptive
                .total_received
                .saturating_add(u64::try_from(amount).unwrap_or(u64::MAX));
            self.adaptive = Some(adaptive);
        }
        self.receive_data_inner(amount)?;
        if self.available == 0
            && let Some(adaptive) = self.adaptive.as_mut()
            && adaptive.blocked_since.is_none()
        {
            adaptive.blocked_since = Some(now);
        }
        Ok(())
    }

    fn receive_data_inner(&mut self, amount: usize) -> Result<(), ServerError> {
        if amount > self.available {
            return Err(ServerError::InvalidFrame);
        }
        self.available -= amount;
        Ok(())
    }

    pub fn grant_extra_credit_for_frame(
        &mut self,
        frame_len: usize,
        buffered_len: usize,
    ) -> Result<Option<usize>, ServerError> {
        let remaining = frame_len.saturating_sub(buffered_len);
        if remaining <= self.available {
            return Ok(None);
        }
        let needed = remaining - self.available;
        let normal_credit = self.pending_update.min(needed);
        if normal_credit > 0 {
            self.pending_update -= normal_credit;
            self.available = self
                .available
                .checked_add(normal_credit)
                .ok_or(ServerError::InvalidFrame)?;
        }
        let extra_needed = needed - normal_credit;
        if extra_needed > 0 {
            self.grant_extra_credit(extra_needed)?;
        }
        Ok(Some(needed))
    }

    pub fn grant_extra_credit(&mut self, amount: usize) -> Result<Option<usize>, ServerError> {
        if amount == 0 {
            return Ok(None);
        }
        let available = self
            .available
            .checked_add(amount)
            .ok_or(ServerError::InvalidFrame)?;
        if available > H2_MAX_WINDOW_SIZE as usize {
            return Err(ServerError::InvalidFrame);
        }
        self.available = available;
        self.extra_credit = self
            .extra_credit
            .checked_add(amount)
            .ok_or(ServerError::InvalidFrame)?;
        Ok(Some(amount))
    }

    /// Rolls back a DATA debit rejected before the owner accepts its payload.
    ///
    /// Unlike [`Self::grant_extra_credit`], rollback restores existing credit
    /// without creating debt that a later valid consumption must absorb.
    pub fn rollback_received_data(&mut self, amount: usize) -> Result<Option<usize>, ServerError> {
        if amount == 0 {
            return Ok(None);
        }
        let available = self
            .available
            .checked_add(amount)
            .ok_or(ServerError::InvalidFrame)?;
        if available > H2_MAX_WINDOW_SIZE as usize {
            return Err(ServerError::InvalidFrame);
        }
        self.available = available;
        if let Some(adaptive) = self.adaptive.as_mut() {
            adaptive.sample_received = adaptive.sample_received.saturating_sub(amount);
            if adaptive.sample_received == 0 && adaptive.total_consumed == 0 {
                adaptive.sample_needs_reanchor = true;
            }
            if self.available > 0 {
                adaptive.blocked_since = None;
            }
        }
        Ok(Some(amount))
    }

    pub fn consume_data(&mut self, amount: usize) -> Result<Option<usize>, ServerError> {
        if self.adaptive.is_some() {
            return self.consume_data_at(amount, Instant::now());
        }
        self.consume_data_inner(amount)
    }

    pub fn consume_data_at(
        &mut self,
        amount: usize,
        now: Instant,
    ) -> Result<Option<usize>, ServerError> {
        if let Some(adaptive) = self.adaptive.as_mut() {
            adaptive.sample_consumed = adaptive.sample_consumed.saturating_add(amount);
            adaptive.total_consumed = adaptive
                .total_consumed
                .saturating_add(u64::try_from(amount).unwrap_or(u64::MAX));
        }
        let update = self.consume_data_inner(amount)?.unwrap_or(0);
        let growth = self.adaptive_growth(now)?;
        let update = update
            .checked_add(growth)
            .ok_or(ServerError::InvalidFrame)?;
        Ok((update > 0).then_some(update))
    }

    fn consume_data_inner(&mut self, amount: usize) -> Result<Option<usize>, ServerError> {
        let mut reclaim = amount;
        if self.extra_credit > 0 {
            let extra = self.extra_credit.min(reclaim);
            self.extra_credit -= extra;
            reclaim -= extra;
        }
        self.pending_update = self
            .pending_update
            .checked_add(reclaim)
            .ok_or(ServerError::InvalidFrame)?;
        let threshold = (self.limit / self.threshold_divisor).max(1);
        if self.pending_update < threshold {
            return Ok(None);
        }
        let update = self.pending_update;
        self.pending_update = 0;
        self.available = self
            .available
            .checked_add(update)
            .ok_or(ServerError::InvalidFrame)?;
        Ok(Some(update))
    }

    fn adaptive_growth(&mut self, now: Instant) -> Result<usize, ServerError> {
        let Some(mut adaptive) = self.adaptive else {
            return Ok(0);
        };
        if adaptive.sample_received < self.limit || adaptive.sample_consumed < self.limit {
            return Ok(0);
        }

        let elapsed = now
            .checked_duration_since(adaptive.sample_started_at)
            .unwrap_or_default();
        let elapsed_nanos = elapsed.as_nanos().max(1);
        let estimated_bdp = (adaptive.sample_received as u128)
            .saturating_mul(adaptive.rtt_proxy.as_nanos())
            .checked_div(elapsed_nanos)
            .unwrap_or(u128::MAX)
            .min(H2_MAX_WINDOW_SIZE as u128);
        adaptive.estimated_bdp =
            usize::try_from(estimated_bdp).unwrap_or(H2_MAX_WINDOW_SIZE as usize);

        if elapsed <= adaptive.target_rtt {
            let turnovers = (adaptive.sample_received.min(adaptive.sample_consumed) / self.limit)
                .max(1)
                .min(u8::MAX as usize) as u8;
            adaptive.fast_turnovers = adaptive.fast_turnovers.saturating_add(turnovers);
        } else {
            adaptive.fast_turnovers = 0;
        }
        adaptive.sample_started_at = now;
        adaptive.sample_received = 0;
        adaptive.sample_consumed = 0;

        let mut growth = 0;
        if adaptive.fast_turnovers >= 2 && self.limit < adaptive.max_limit {
            let minimum_target = self.limit.saturating_add((self.limit / 2).max(1));
            let maximum_target = self.limit.saturating_mul(2);
            let target = adaptive
                .estimated_bdp
                .max(minimum_target)
                .min(maximum_target)
                .min(adaptive.max_limit);
            growth = target.saturating_sub(self.limit);
            if growth > 0 {
                let available = self
                    .available
                    .checked_add(growth)
                    .ok_or(ServerError::InvalidFrame)?;
                if available > H2_MAX_WINDOW_SIZE as usize {
                    return Err(ServerError::InvalidFrame);
                }
                self.available = available;
                self.limit = target;
                adaptive.growth_events = adaptive.growth_events.saturating_add(1);
                adaptive.growth_bytes = adaptive
                    .growth_bytes
                    .saturating_add(u64::try_from(growth).unwrap_or(u64::MAX));
                adaptive.fast_turnovers = 0;
            }
        }
        self.adaptive = Some(adaptive);
        Ok(growth)
    }

    pub fn diagnostics(self) -> H2ReceiveWindowDiagnostics {
        let Some(adaptive) = self.adaptive else {
            return H2ReceiveWindowDiagnostics {
                current_window_bytes: self.limit,
                max_window_bytes: self.limit,
                ..H2ReceiveWindowDiagnostics::default()
            };
        };
        H2ReceiveWindowDiagnostics {
            current_window_bytes: self.limit,
            max_window_bytes: adaptive.max_limit,
            received_bytes: adaptive.total_received,
            consumed_bytes: adaptive.total_consumed,
            blocked_time: adaptive.total_blocked,
            last_blocked_time: adaptive.last_blocked,
            rtt_proxy: adaptive.rtt_proxy,
            estimated_bdp_bytes: adaptive.estimated_bdp,
            growth_events: adaptive.growth_events,
            growth_bytes: adaptive.growth_bytes,
        }
    }
}

/// HTTP/2 SETTINGS identifiers understood by the FSM HTTP compatibility surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum H2SettingId {
    HeaderTableSize = 0x1,
    EnablePush = 0x2,
    MaxConcurrentStreams = 0x3,
    InitialWindowSize = 0x4,
    MaxFrameSize = 0x5,
    MaxHeaderListSize = 0x6,
}

impl H2SettingId {
    /// Converts a raw SETTINGS identifier into a known setting, ignoring unknown IDs.
    pub fn from_u16(value: u16) -> Option<Self> {
        match value {
            0x1 => Some(Self::HeaderTableSize),
            0x2 => Some(Self::EnablePush),
            0x3 => Some(Self::MaxConcurrentStreams),
            0x4 => Some(Self::InitialWindowSize),
            0x5 => Some(Self::MaxFrameSize),
            0x6 => Some(Self::MaxHeaderListSize),
            _ => None,
        }
    }
}

/// One decoded HTTP/2 SETTINGS entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2Setting {
    /// SETTINGS identifier.
    pub id: H2SettingId,
    /// Raw 32-bit SETTINGS value.
    pub value: u32,
}

impl H2Setting {
    /// Creates a SETTINGS entry from a known identifier and raw value.
    pub const fn new(id: H2SettingId, value: u32) -> Self {
        Self { id, value }
    }
}

/// Applied peer HTTP/2 settings relevant to frame/header adapters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2Settings {
    /// Header compression table size advertised by the peer.
    pub header_table_size: u32,
    /// Whether server push is enabled.
    pub enable_push: bool,
    /// Maximum concurrent streams advertised by the peer.
    pub max_concurrent_streams: u32,
    /// Initial stream flow-control window size.
    pub initial_window_size: u32,
    /// Maximum frame payload size.
    pub max_frame_size: usize,
    /// Maximum header list size advertised by the peer.
    pub max_header_list_size: u32,
}

impl Default for H2Settings {
    fn default() -> Self {
        Self {
            header_table_size: 4096,
            enable_push: true,
            max_concurrent_streams: u32::MAX,
            initial_window_size: 65_535,
            max_frame_size: 16 * 1024,
            max_header_list_size: u32::MAX,
        }
    }
}

impl H2Settings {
    /// Applies and validates one decoded SETTINGS entry.
    pub fn apply(&mut self, setting: H2Setting) -> Result<(), ServerError> {
        match setting.id {
            H2SettingId::HeaderTableSize => self.header_table_size = setting.value,
            H2SettingId::EnablePush => match setting.value {
                0 => self.enable_push = false,
                1 => self.enable_push = true,
                _ => return Err(ServerError::InvalidFrame),
            },
            H2SettingId::MaxConcurrentStreams => self.max_concurrent_streams = setting.value,
            H2SettingId::InitialWindowSize => {
                if setting.value > H2_MAX_WINDOW_SIZE {
                    return Err(ServerError::InvalidFrame);
                }
                self.initial_window_size = setting.value;
            }
            H2SettingId::MaxFrameSize => {
                self.max_frame_size = validate_max_frame_size(setting.value)?;
            }
            H2SettingId::MaxHeaderListSize => self.max_header_list_size = setting.value,
        }
        Ok(())
    }

    /// Applies a SETTINGS payload after it has been decoded into entries.
    pub fn apply_all(&mut self, settings: &[H2Setting]) -> Result<(), ServerError> {
        for &setting in settings {
            self.apply(setting)?;
        }
        Ok(())
    }

    /// Decodes a SETTINGS frame payload, ignoring unknown identifiers as required by HTTP/2.
    pub fn decode_payload(payload: &[u8]) -> Result<Vec<H2Setting>, ServerError> {
        Self::decode_payload_with_limit(payload, usize::MAX)
    }

    /// Decodes a SETTINGS payload and rejects payloads above a configured entry count.
    pub fn decode_payload_with_limit(
        payload: &[u8],
        max_entries: usize,
    ) -> Result<Vec<H2Setting>, ServerError> {
        if !payload.len().is_multiple_of(6) {
            return Err(ServerError::InvalidFrame);
        }
        let entries = payload.len() / 6;
        if entries > max_entries {
            return Err(ServerError::InvalidFrame);
        }
        payload
            .chunks_exact(6)
            .filter_map(|setting| {
                let id = u16::from_be_bytes([setting[0], setting[1]]);
                H2SettingId::from_u16(id).map(|id| {
                    Ok(H2Setting::new(
                        id,
                        u32::from_be_bytes([setting[2], setting[3], setting[4], setting[5]]),
                    ))
                })
            })
            .collect()
    }

    /// Encodes SETTINGS entries into a frame payload.
    pub fn encode_payload(settings: &[H2Setting], output: &mut Vec<u8>) {
        for setting in settings {
            output.extend_from_slice(&(setting.id as u16).to_be_bytes());
            output.extend_from_slice(&setting.value.to_be_bytes());
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct H2Request {
    pub stream_id: u32,
    pub method: String,
    pub path: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct H2Header {
    pub name: String,
    pub value: String,
    pub sensitive: bool,
}

impl H2Header {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            sensitive: false,
        }
    }

    pub const fn with_sensitive(mut self, sensitive: bool) -> Self {
        self.sensitive = sensitive;
        self
    }

    /// Borrows a regular text field for sensitivity-preserving forwarding.
    ///
    /// Pseudo-fields are interpreted into typed request/response roles during
    /// projection. They are therefore not exposed as the original occurrence.
    pub fn try_as_raw_occurrence(&self) -> Result<H2RawHeaderRef<'_>, H2HeaderProjectionError> {
        if self.name.starts_with(':') {
            return Err(H2HeaderProjectionError::PseudoHeaderNotForwardable);
        }
        Ok(
            H2RawHeaderRef::new(self.name.as_bytes(), self.value.as_bytes())
                .with_sensitive(self.sensitive),
        )
    }
}

/// Failure to project a canonical byte occurrence into the text convenience view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum H2HeaderProjectionError {
    NameNotUtf8,
    ValueNotUtf8,
    MalformedPseudoHeaders,
    PseudoHeaderNotForwardable,
}

impl fmt::Display for H2HeaderProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NameNotUtf8 => "HTTP/2 header name is not UTF-8",
            Self::ValueNotUtf8 => "HTTP/2 header value is not UTF-8",
            Self::MalformedPseudoHeaders => "HTTP/2 pseudo-header projection is invalid",
            Self::PseudoHeaderNotForwardable => {
                "typed HTTP/2 pseudo-header is not the original forwardable occurrence"
            }
        })
    }
}

impl std::error::Error for H2HeaderProjectionError {}

#[derive(Clone, Debug, Eq, PartialEq)]
/// The source-compatible owned HTTP/2 name/value pair.
pub struct H2RawHeader {
    pub name: Vec<u8>,
    pub value: Vec<u8>,
}

impl H2RawHeader {
    pub fn new(name: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }

    /// Borrows this occurrence without copying its field bytes.
    pub fn as_ref(&self) -> H2RawHeaderRef<'_> {
        H2RawHeaderRef {
            name: &self.name,
            value: &self.value,
            sensitive: false,
        }
    }

    /// Converts this pair to a sensitivity-bearing field occurrence.
    pub fn with_sensitive(self, sensitive: bool) -> H2HeaderField {
        H2HeaderField {
            name: self.name,
            value: self.value,
            sensitive,
        }
    }
}

/// An owned occurrence for the fallible, sensitivity-preserving codec APIs.
pub use crate::hpack::HeaderField as H2HeaderField;

impl H2HeaderField {
    /// Borrows this occurrence without copying its field bytes.
    pub fn as_ref(&self) -> H2RawHeaderRef<'_> {
        H2RawHeaderRef {
            name: &self.name,
            value: &self.value,
            sensitive: self.sensitive,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// A borrowed, byte-preserving HTTP/2 header occurrence.
pub struct H2RawHeaderRef<'a> {
    /// Header or pseudo-header name bytes.
    pub name: &'a [u8],
    /// Header value bytes.
    pub value: &'a [u8],
    /// Whether this occurrence must use HPACK's never-indexed representation.
    pub sensitive: bool,
}

impl<'a> H2RawHeaderRef<'a> {
    /// Creates a nonsensitive borrowed header occurrence.
    pub const fn new(name: &'a [u8], value: &'a [u8]) -> Self {
        Self {
            name,
            value,
            sensitive: false,
        }
    }

    /// Sets whether this occurrence is sensitive.
    pub const fn with_sensitive(mut self, sensitive: bool) -> Self {
        self.sensitive = sensitive;
        self
    }

    /// Copies this occurrence into a sensitivity-bearing owned representation.
    pub fn to_owned(self) -> H2HeaderField {
        H2HeaderField {
            name: self.name.to_vec(),
            value: self.value.to_vec(),
            sensitive: self.sensitive,
        }
    }
}

/// A stateful HPACK encoder for one HTTP/2 direction.
///
/// State belongs to exactly one peer-bound direction and must be reused in
/// wire order:
///
/// ```
/// use kimojio_fsm_http::{H2HeaderBlockEncoder, H2RawHeaderRef};
///
/// let mut encoder = H2HeaderBlockEncoder::new();
/// let secret =
///     H2RawHeaderRef::new(b"authorization", b"token").with_sensitive(true);
/// let block = encoder.try_encode_ref(&[secret])?;
/// assert_eq!(block[0] & 0xf0, 0x10);
/// # Ok::<(), kimojio_fsm_http::H2HpackError>(())
/// ```
pub struct H2HeaderBlockEncoder {
    inner: Option<crate::hpack::Encoder>,
}

impl Clone for H2HeaderBlockEncoder {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.inner.clone_from(&source.inner);
    }
}

impl H2HeaderBlockEncoder {
    /// Creates an encoder with RFC 7541's 4,096-byte initial table capacity.
    pub const fn new() -> Self {
        Self { inner: None }
    }

    fn inner_mut(&mut self) -> &mut crate::hpack::Encoder {
        self.inner.get_or_insert_with(crate::hpack::Encoder::new)
    }

    /// Fallibly copies this encoder for a reversible pre-handoff transaction.
    ///
    /// The source encoder is unchanged if copying any retained history fails.
    #[doc(hidden)]
    pub fn try_clone_for_transaction(&self) -> Result<Self, H2HpackError> {
        let inner = self
            .inner
            .as_ref()
            .map(crate::hpack::Encoder::try_clone)
            .transpose()
            .map_err(H2HpackError::from)?;
        Ok(Self { inner })
    }

    /// Fallibly refreshes reusable transaction storage from another encoder.
    ///
    /// The source encoder is unchanged on failure.
    #[doc(hidden)]
    pub fn try_clone_from_for_transaction(&mut self, source: &Self) -> Result<(), H2HpackError> {
        match source.inner.as_ref() {
            Some(source) => self
                .inner_mut()
                .try_clone_from(source)
                .map_err(H2HpackError::from),
            None => {
                self.inner = None;
                Ok(())
            }
        }
    }

    /// Queues a dynamic table size update for the next encoded block.
    ///
    /// Multiple calls before [`Self::encode`] preserve the smallest requested
    /// size followed by the final size, as required by RFC 7541 section 4.2.
    /// Values above 1,048,576 octets are clamped to that local ceiling.
    pub fn set_max_table_size(&mut self, max_table_size: usize) {
        self.inner_mut().set_max_table_size(max_table_size);
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn set_allocation_failure_after_for_testing(
        &mut self,
        successful_allocations: Option<usize>,
    ) {
        self.inner_mut()
            .set_allocation_failure_after(successful_allocations);
    }

    /// Encodes source-compatible nonsensitive pairs.
    ///
    /// Use [`Self::try_encode`] when allocation failure must be reported.
    pub fn encode(&mut self, headers: &[H2RawHeader]) -> Vec<u8> {
        self.try_encode(headers)
            .expect("HPACK allocation failed in compatibility encoder")
    }

    /// Fallibly encodes source-compatible nonsensitive pairs.
    pub fn try_encode(&mut self, headers: &[H2RawHeader]) -> Result<Vec<u8>, H2HpackError> {
        self.inner_mut()
            .encode_by(headers.len(), |index| {
                let header = &headers[index];
                crate::hpack::HeaderFieldRef {
                    name: &header.name,
                    value: &header.value,
                    sensitive: false,
                }
            })
            .map_err(Into::into)
    }

    /// Fallibly encodes owned sensitivity-bearing occurrences.
    pub fn try_encode_fields(
        &mut self,
        headers: &[H2HeaderField],
    ) -> Result<Vec<u8>, H2HpackError> {
        self.inner_mut()
            .encode_by(headers.len(), |index| {
                let header = &headers[index];
                crate::hpack::HeaderFieldRef {
                    name: &header.name,
                    value: &header.value,
                    sensitive: header.sensitive,
                }
            })
            .map_err(Into::into)
    }

    /// Fallibly encodes borrowed sensitivity-bearing occurrences.
    pub fn try_encode_ref(
        &mut self,
        headers: &[H2RawHeaderRef<'_>],
    ) -> Result<Vec<u8>, H2HpackError> {
        self.inner_mut()
            .encode_by(headers.len(), |index| {
                let header = headers[index];
                crate::hpack::HeaderFieldRef {
                    name: header.name,
                    value: header.value,
                    sensitive: header.sensitive,
                }
            })
            .map_err(Into::into)
    }

    /// Compatibility alias for [`Self::try_encode_ref`].
    pub fn encode_ref(&mut self, headers: &[H2RawHeaderRef<'_>]) -> Result<Vec<u8>, H2HpackError> {
        self.try_encode_ref(headers)
    }

    /// Returns a content-free snapshot for this outbound codec history.
    pub fn diagnostics(&mut self) -> H2HpackDiagnosticsSnapshot {
        self.inner_mut().diagnostics().into()
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn test_diagnostics(&mut self) -> crate::hpack::Diagnostics {
        self.inner_mut().diagnostics()
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn test_table_snapshot(&mut self) -> crate::hpack::TestTableSnapshot {
        self.inner_mut().test_table_snapshot()
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn test_configured_max_size(&mut self) -> usize {
        self.inner_mut().test_configured_max_size()
    }
}

impl Default for H2HeaderBlockEncoder {
    fn default() -> Self {
        Self::new()
    }
}

/// A stateful HPACK decoder for one HTTP/2 direction.
///
/// A compression failure poisons the decoder. A decoded field-section limit
/// failure does not: compression state is committed and the next block remains
/// decodable.
///
/// ```
/// use kimojio_fsm_http::{
///     H2HeaderBlockDecoder, H2HeaderBlockEncoder, H2RawHeader,
/// };
///
/// let field = H2RawHeader::new(b"x-bytes", [0, 0x80, 0xff]);
/// let block = H2HeaderBlockEncoder::new().encode(&[field.clone()]);
/// let decoded = H2HeaderBlockDecoder::new()
///     .decode_with_limit(&block, 1024)
///     .unwrap();
/// assert_eq!(decoded, [field]);
/// ```
pub struct H2HeaderBlockDecoder {
    inner: crate::hpack::Decoder,
}

impl H2HeaderBlockDecoder {
    /// Creates a decoder with RFC 7541's 4,096-byte initial table capacity.
    pub fn new() -> Self {
        Self {
            inner: crate::hpack::Decoder::new(),
        }
    }

    /// Sets the maximum dynamic table size accepted from subsequent blocks.
    ///
    /// Reducing the maximum requires the next block to begin with a size update
    /// no greater than the smallest reduction observed since the prior block.
    /// Values above 1,048,576 octets are clamped to that local ceiling.
    pub fn set_max_table_size(&mut self, max_table_size: usize) {
        self.inner.set_max_allowed_table_size(max_table_size);
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn set_allocation_failure_after_for_testing(
        &mut self,
        successful_allocations: Option<usize>,
    ) {
        self.inner
            .set_allocation_failure_after(successful_allocations);
    }

    /// Decodes one block through the source-compatible error and pair types.
    pub fn decode_with_limit(
        &mut self,
        bytes: &[u8],
        max_header_list_size: usize,
    ) -> Result<Vec<H2RawHeader>, ServerError> {
        let headers = self
            .try_decode_with_limit(bytes, max_header_list_size)
            .map_err(|_| ServerError::InvalidHpack)?;
        if headers.iter().any(|header| header.sensitive) {
            return Err(ServerError::InvalidHpack);
        }
        Ok(headers
            .into_iter()
            .map(|header| H2RawHeader::new(header.name, header.value))
            .collect())
    }

    /// Fallibly decodes one block with stable HPACK categories and sensitivity.
    pub fn try_decode_with_limit(
        &mut self,
        bytes: &[u8],
        max_header_list_size: usize,
    ) -> Result<Vec<H2HeaderField>, H2ProtocolError> {
        let headers = decode_hpack_with_limit(&mut self.inner, bytes, max_header_list_size)
            .map_err(|error| match error {
                H2HeaderDecodeError::Hpack(error) => {
                    let category = H2HpackError::from(error);
                    if category == H2HpackError::AllocationFailed {
                        H2ProtocolError {
                            scope: H2ErrorScope::Connection,
                            code: H2ErrorCode::InternalError,
                            debug: "HPACK storage allocation failed",
                            hpack_error: Some(category),
                            http_error_kind: None,
                            limit: None,
                        }
                    } else {
                        H2ProtocolError::hpack(
                            H2ErrorScope::Connection,
                            category,
                            "invalid HPACK block",
                        )
                    }
                }
                H2HeaderDecodeError::HeaderListTooLarge { actual } => {
                    let mut error = H2ProtocolError::resource_limit(
                        H2ErrorScope::Connection,
                        crate::HttpErrorKind::HeadersTooLarge,
                        max_header_list_size,
                        actual,
                        "decoded header list exceeds configured limit",
                    );
                    error.hpack_error = Some(H2HpackError::HeaderListTooLarge);
                    error
                }
            })?;
        Ok(headers)
    }

    /// Returns a content-free snapshot for this inbound codec history.
    pub fn diagnostics(&self) -> H2HpackDiagnosticsSnapshot {
        self.inner.diagnostics().into()
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn test_diagnostics(&self) -> crate::hpack::Diagnostics {
        self.inner.diagnostics()
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn test_table_snapshot(&self) -> crate::hpack::TestTableSnapshot {
        self.inner.test_table_snapshot()
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn test_configured_max_size(&self) -> usize {
        self.inner.test_configured_max_size()
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn test_post_limit_allocations(
        &self,
    ) -> Option<crate::hpack::TestPostLimitAllocationSnapshot> {
        self.inner.test_post_limit_allocations()
    }
}

enum H2HeaderDecodeError {
    Hpack(crate::hpack::Error),
    HeaderListTooLarge { actual: usize },
}

type H2DecodedHeaderList = Vec<crate::hpack::HeaderField>;

fn decode_hpack_with_limit(
    decoder: &mut crate::hpack::Decoder,
    bytes: &[u8],
    max_header_list_size: usize,
) -> Result<H2DecodedHeaderList, H2HeaderDecodeError> {
    match decoder.decode(bytes, max_header_list_size) {
        Ok(headers) => Ok(headers),
        Err(crate::hpack::Error::HeaderListTooLarge { actual }) => {
            Err(H2HeaderDecodeError::HeaderListTooLarge { actual })
        }
        Err(error) => Err(H2HeaderDecodeError::Hpack(error)),
    }
}

impl Default for H2HeaderBlockDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2InitialWindowSizeChange {
    pub previous: u32,
    pub current: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum H2ByteStreamEvent<P = Vec<u8>> {
    Settings {
        initial_window_size: Option<H2InitialWindowSizeChange>,
    },
    Ping {
        ack: bool,
    },
    WindowUpdate {
        stream_id: u32,
        increment: u32,
    },
    RequestHeaders {
        stream_id: u32,
        headers: Vec<H2HeaderField>,
        end_stream: bool,
    },
    Data {
        stream_id: u32,
        payload: P,
        flow_control_len: usize,
        end_stream: bool,
    },
    DiscardedData {
        stream_id: u32,
        flow_control_len: usize,
    },
    Trailers {
        stream_id: u32,
        headers: Vec<H2HeaderField>,
    },
    Reset {
        stream_id: u32,
        error_code: u32,
    },
    Goaway {
        last_stream_id: u32,
        error_code: u32,
    },
}

pub type H2ByteStreamEventRef<'a> = H2ByteStreamEvent<&'a [u8]>;

impl H2ByteStreamEventRef<'_> {
    pub fn into_owned(self) -> H2ByteStreamEvent {
        match self {
            Self::Settings {
                initial_window_size,
            } => H2ByteStreamEvent::Settings {
                initial_window_size,
            },
            Self::Ping { ack } => H2ByteStreamEvent::Ping { ack },
            Self::WindowUpdate {
                stream_id,
                increment,
            } => H2ByteStreamEvent::WindowUpdate {
                stream_id,
                increment,
            },
            Self::RequestHeaders {
                stream_id,
                headers,
                end_stream,
            } => H2ByteStreamEvent::RequestHeaders {
                stream_id,
                headers,
                end_stream,
            },
            Self::Data {
                stream_id,
                payload,
                flow_control_len,
                end_stream,
            } => H2ByteStreamEvent::Data {
                stream_id,
                payload: payload.to_vec(),
                flow_control_len,
                end_stream,
            },
            Self::DiscardedData {
                stream_id,
                flow_control_len,
            } => H2ByteStreamEvent::DiscardedData {
                stream_id,
                flow_control_len,
            },
            Self::Trailers { stream_id, headers } => {
                H2ByteStreamEvent::Trailers { stream_id, headers }
            }
            Self::Reset {
                stream_id,
                error_code,
            } => H2ByteStreamEvent::Reset {
                stream_id,
                error_code,
            },
            Self::Goaway {
                last_stream_id,
                error_code,
            } => H2ByteStreamEvent::Goaway {
                last_stream_id,
                error_code,
            },
        }
    }
}

impl<P> H2ByteStreamEvent<P> {
    pub fn try_into_text(self) -> Result<H2StreamEvent<P>, H2HeaderProjectionError> {
        Ok(match self {
            Self::Settings {
                initial_window_size,
            } => H2StreamEvent::Settings {
                initial_window_size,
            },
            Self::Ping { ack } => H2StreamEvent::Ping { ack },
            Self::WindowUpdate {
                stream_id,
                increment,
            } => H2StreamEvent::WindowUpdate {
                stream_id,
                increment,
            },
            Self::RequestHeaders {
                stream_id,
                headers,
                end_stream,
            } => {
                let headers = project_h2_headers(headers)?;
                let request = request_from_headers(stream_id, &headers)
                    .map_err(|_| H2HeaderProjectionError::MalformedPseudoHeaders)?;
                H2StreamEvent::RequestHeaders {
                    request,
                    headers,
                    end_stream,
                }
            }
            Self::Data {
                stream_id,
                payload,
                flow_control_len,
                end_stream,
            } => H2StreamEvent::Data {
                stream_id,
                payload,
                flow_control_len,
                end_stream,
            },
            Self::DiscardedData {
                stream_id,
                flow_control_len,
            } => H2StreamEvent::DiscardedData {
                stream_id,
                flow_control_len,
            },
            Self::Trailers { stream_id, headers } => H2StreamEvent::Trailers {
                stream_id,
                headers: project_h2_headers(headers)?,
            },
            Self::Reset {
                stream_id,
                error_code,
            } => H2StreamEvent::Reset {
                stream_id,
                error_code,
            },
            Self::Goaway {
                last_stream_id,
                error_code,
            } => H2StreamEvent::Goaway {
                last_stream_id,
                error_code,
            },
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum H2StreamEvent<P = Vec<u8>> {
    Settings {
        initial_window_size: Option<H2InitialWindowSizeChange>,
    },
    Ping {
        ack: bool,
    },
    WindowUpdate {
        stream_id: u32,
        increment: u32,
    },
    RequestHeaders {
        request: H2Request,
        headers: Vec<H2Header>,
        end_stream: bool,
    },
    Data {
        stream_id: u32,
        payload: P,
        flow_control_len: usize,
        end_stream: bool,
    },
    /// DATA ignored after a reset but still chargeable to connection flow control.
    ///
    /// Owners must debit the padding-inclusive `flow_control_len`; whether that
    /// credit is later refunded is owner-specific policy. Exhaustive matches
    /// must include this variant.
    DiscardedData {
        stream_id: u32,
        flow_control_len: usize,
    },
    Trailers {
        stream_id: u32,
        headers: Vec<H2Header>,
    },
    Reset {
        stream_id: u32,
        error_code: u32,
    },
    Goaway {
        last_stream_id: u32,
        error_code: u32,
    },
}

/// HTTP/2 server event whose DATA payload borrows the decoded frame input.
pub type H2StreamEventRef<'a> = H2StreamEvent<&'a [u8]>;

impl H2StreamEventRef<'_> {
    /// Converts a borrowed event into the compatibility owned representation.
    pub fn into_owned(self) -> H2StreamEvent {
        match self {
            Self::Settings {
                initial_window_size,
            } => H2StreamEvent::Settings {
                initial_window_size,
            },
            Self::Ping { ack } => H2StreamEvent::Ping { ack },
            Self::WindowUpdate {
                stream_id,
                increment,
            } => H2StreamEvent::WindowUpdate {
                stream_id,
                increment,
            },
            Self::RequestHeaders {
                request,
                headers,
                end_stream,
            } => H2StreamEvent::RequestHeaders {
                request,
                headers,
                end_stream,
            },
            Self::Data {
                stream_id,
                payload,
                flow_control_len,
                end_stream,
            } => H2StreamEvent::Data {
                stream_id,
                payload: payload.to_vec(),
                flow_control_len,
                end_stream,
            },
            Self::DiscardedData {
                stream_id,
                flow_control_len,
            } => H2StreamEvent::DiscardedData {
                stream_id,
                flow_control_len,
            },
            Self::Trailers { stream_id, headers } => H2StreamEvent::Trailers { stream_id, headers },
            Self::Reset {
                stream_id,
                error_code,
            } => H2StreamEvent::Reset {
                stream_id,
                error_code,
            },
            Self::Goaway {
                last_stream_id,
                error_code,
            } => H2StreamEvent::Goaway {
                last_stream_id,
                error_code,
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum H2ByteClientEvent<P = Vec<u8>> {
    Settings {
        initial_window_size: Option<H2InitialWindowSizeChange>,
    },
    Ping {
        ack: bool,
    },
    WindowUpdate {
        stream_id: u32,
        increment: u32,
    },
    ResponseHeaders {
        stream_id: u32,
        headers: Vec<H2HeaderField>,
        end_stream: bool,
    },
    Data {
        stream_id: u32,
        payload: P,
        flow_control_len: usize,
        end_stream: bool,
    },
    DiscardedData {
        stream_id: u32,
        flow_control_len: usize,
    },
    Trailers {
        stream_id: u32,
        headers: Vec<H2HeaderField>,
    },
    Reset {
        stream_id: u32,
        error_code: u32,
    },
    Goaway {
        last_stream_id: u32,
        error_code: u32,
    },
}

pub type H2ByteClientEventRef<'a> = H2ByteClientEvent<&'a [u8]>;

impl H2ByteClientEventRef<'_> {
    pub fn into_owned(self) -> H2ByteClientEvent {
        match self {
            Self::Settings {
                initial_window_size,
            } => H2ByteClientEvent::Settings {
                initial_window_size,
            },
            Self::Ping { ack } => H2ByteClientEvent::Ping { ack },
            Self::WindowUpdate {
                stream_id,
                increment,
            } => H2ByteClientEvent::WindowUpdate {
                stream_id,
                increment,
            },
            Self::ResponseHeaders {
                stream_id,
                headers,
                end_stream,
            } => H2ByteClientEvent::ResponseHeaders {
                stream_id,
                headers,
                end_stream,
            },
            Self::Data {
                stream_id,
                payload,
                flow_control_len,
                end_stream,
            } => H2ByteClientEvent::Data {
                stream_id,
                payload: payload.to_vec(),
                flow_control_len,
                end_stream,
            },
            Self::DiscardedData {
                stream_id,
                flow_control_len,
            } => H2ByteClientEvent::DiscardedData {
                stream_id,
                flow_control_len,
            },
            Self::Trailers { stream_id, headers } => {
                H2ByteClientEvent::Trailers { stream_id, headers }
            }
            Self::Reset {
                stream_id,
                error_code,
            } => H2ByteClientEvent::Reset {
                stream_id,
                error_code,
            },
            Self::Goaway {
                last_stream_id,
                error_code,
            } => H2ByteClientEvent::Goaway {
                last_stream_id,
                error_code,
            },
        }
    }
}

impl<P> H2ByteClientEvent<P> {
    pub fn try_into_text(self) -> Result<H2ClientEvent<P>, H2HeaderProjectionError> {
        Ok(match self {
            Self::Settings {
                initial_window_size,
            } => H2ClientEvent::Settings {
                initial_window_size,
            },
            Self::Ping { ack } => H2ClientEvent::Ping { ack },
            Self::WindowUpdate {
                stream_id,
                increment,
            } => H2ClientEvent::WindowUpdate {
                stream_id,
                increment,
            },
            Self::ResponseHeaders {
                stream_id,
                headers,
                end_stream,
            } => {
                let headers = project_h2_headers(headers)?;
                let status = status_from_headers(&headers)
                    .map_err(|_| H2HeaderProjectionError::MalformedPseudoHeaders)?;
                H2ClientEvent::ResponseHeaders {
                    stream_id,
                    status,
                    headers,
                    end_stream,
                }
            }
            Self::Data {
                stream_id,
                payload,
                flow_control_len,
                end_stream,
            } => H2ClientEvent::Data {
                stream_id,
                payload,
                flow_control_len,
                end_stream,
            },
            Self::DiscardedData {
                stream_id,
                flow_control_len,
            } => H2ClientEvent::DiscardedData {
                stream_id,
                flow_control_len,
            },
            Self::Trailers { stream_id, headers } => H2ClientEvent::Trailers {
                stream_id,
                headers: project_h2_headers(headers)?,
            },
            Self::Reset {
                stream_id,
                error_code,
            } => H2ClientEvent::Reset {
                stream_id,
                error_code,
            },
            Self::Goaway {
                last_stream_id,
                error_code,
            } => H2ClientEvent::Goaway {
                last_stream_id,
                error_code,
            },
        })
    }
}

fn projection_error(stream_id: u32) -> H2ProtocolError {
    H2ProtocolError::stream(
        stream_id,
        H2ErrorCode::ProtocolError,
        "HTTP/2 byte event cannot be represented by the text convenience view",
    )
}

fn project_stream_outcome<P>(
    outcome: H2FrameOutcome<H2ByteStreamEvent<P>>,
    stream_id: u32,
) -> H2FrameOutcome<H2StreamEvent<P>> {
    match outcome {
        H2FrameOutcome::Event(event) => match event.try_into_text() {
            Ok(event) => H2FrameOutcome::Event(event),
            Err(_) => H2FrameOutcome::Error(projection_error(stream_id)),
        },
        H2FrameOutcome::Ignored => H2FrameOutcome::Ignored,
        H2FrameOutcome::Error(error) => H2FrameOutcome::Error(error),
    }
}

fn project_client_outcome<P>(
    outcome: H2FrameOutcome<H2ByteClientEvent<P>>,
    stream_id: u32,
) -> H2FrameOutcome<H2ClientEvent<P>> {
    match outcome {
        H2FrameOutcome::Event(event) => match event.try_into_text() {
            Ok(event) => H2FrameOutcome::Event(event),
            Err(_) => H2FrameOutcome::Error(projection_error(stream_id)),
        },
        H2FrameOutcome::Ignored => H2FrameOutcome::Ignored,
        H2FrameOutcome::Error(error) => H2FrameOutcome::Error(error),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum H2ClientEvent<P = Vec<u8>> {
    Settings {
        initial_window_size: Option<H2InitialWindowSizeChange>,
    },
    Ping {
        ack: bool,
    },
    WindowUpdate {
        stream_id: u32,
        increment: u32,
    },
    ResponseHeaders {
        stream_id: u32,
        status: u16,
        headers: Vec<H2Header>,
        end_stream: bool,
    },
    Data {
        stream_id: u32,
        payload: P,
        flow_control_len: usize,
        end_stream: bool,
    },
    /// DATA ignored after a reset but still chargeable to connection flow control.
    ///
    /// Owners must debit the padding-inclusive `flow_control_len`; whether that
    /// credit is later refunded is owner-specific policy. Exhaustive matches
    /// must include this variant.
    DiscardedData {
        stream_id: u32,
        flow_control_len: usize,
    },
    Trailers {
        stream_id: u32,
        headers: Vec<H2Header>,
    },
    Reset {
        stream_id: u32,
        error_code: u32,
    },
    Goaway {
        last_stream_id: u32,
        error_code: u32,
    },
}

/// HTTP/2 client event whose DATA payload borrows the decoded frame input.
pub type H2ClientEventRef<'a> = H2ClientEvent<&'a [u8]>;

impl H2ClientEventRef<'_> {
    /// Converts a borrowed event into the compatibility owned representation.
    pub fn into_owned(self) -> H2ClientEvent {
        match self {
            Self::Settings {
                initial_window_size,
            } => H2ClientEvent::Settings {
                initial_window_size,
            },
            Self::Ping { ack } => H2ClientEvent::Ping { ack },
            Self::WindowUpdate {
                stream_id,
                increment,
            } => H2ClientEvent::WindowUpdate {
                stream_id,
                increment,
            },
            Self::ResponseHeaders {
                stream_id,
                status,
                headers,
                end_stream,
            } => H2ClientEvent::ResponseHeaders {
                stream_id,
                status,
                headers,
                end_stream,
            },
            Self::Data {
                stream_id,
                payload,
                flow_control_len,
                end_stream,
            } => H2ClientEvent::Data {
                stream_id,
                payload: payload.to_vec(),
                flow_control_len,
                end_stream,
            },
            Self::DiscardedData {
                stream_id,
                flow_control_len,
            } => H2ClientEvent::DiscardedData {
                stream_id,
                flow_control_len,
            },
            Self::Trailers { stream_id, headers } => H2ClientEvent::Trailers { stream_id, headers },
            Self::Reset {
                stream_id,
                error_code,
            } => H2ClientEvent::Reset {
                stream_id,
                error_code,
            },
            Self::Goaway {
                last_stream_id,
                error_code,
            } => H2ClientEvent::Goaway {
                last_stream_id,
                error_code,
            },
        }
    }
}

/// Receipt for one complete outbound header transaction assigned to connection wire order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2OutboundCommit {
    sequence: u64,
}

impl H2OutboundCommit {
    /// Monotonic connection-local wire-order sequence.
    pub const fn sequence(self) -> u64 {
        self.sequence
    }
}

/// Borrowed view of the next complete outbound transaction in connection wire order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2OutboundBlockRef<'a> {
    commit: H2OutboundCommit,
    bytes: &'a [u8],
}

impl<'a> H2OutboundBlockRef<'a> {
    /// Receipt that must be acknowledged after this block is handed to the transport.
    pub const fn commit(self) -> H2OutboundCommit {
        self.commit
    }

    /// Complete serialized transaction bytes.
    pub const fn bytes(self) -> &'a [u8] {
        self.bytes
    }
}

#[derive(Debug)]
struct H2QueuedOutboundBlock {
    commit: H2OutboundCommit,
    bytes: Vec<u8>,
}

#[derive(Debug, Default)]
struct H2OutboundQueue {
    blocks: VecDeque<H2QueuedOutboundBlock>,
    next_sequence: u64,
    #[cfg(feature = "hpack-test-support")]
    allocation_failure_after: Option<usize>,
}

impl H2OutboundQueue {
    fn reserve_commit(&mut self) -> Result<H2OutboundCommit, H2HpackError> {
        let commit = H2OutboundCommit {
            sequence: self
                .next_sequence
                .checked_add(1)
                .ok_or(H2HpackError::StateOverflow)?,
        };
        if self.blocks.len() == self.blocks.capacity() {
            #[cfg(feature = "hpack-test-support")]
            if let Some(remaining) = self.allocation_failure_after.as_mut() {
                if *remaining == 0 {
                    return Err(H2HpackError::AllocationFailed);
                }
                *remaining -= 1;
            }
            self.blocks
                .try_reserve(1)
                .map_err(|_| H2HpackError::AllocationFailed)?;
        }
        Ok(commit)
    }

    fn push_reserved(&mut self, commit: H2OutboundCommit, bytes: Vec<u8>) {
        debug_assert_eq!(commit.sequence, self.next_sequence + 1);
        self.next_sequence = commit.sequence;
        self.blocks
            .push_back(H2QueuedOutboundBlock { commit, bytes });
    }

    fn front(&self) -> Option<H2OutboundBlockRef<'_>> {
        self.blocks.front().map(|block| H2OutboundBlockRef {
            commit: block.commit,
            bytes: &block.bytes,
        })
    }

    fn acknowledge(&mut self, commit: H2OutboundCommit) -> Result<(), ServerError> {
        if self
            .blocks
            .front()
            .is_none_or(|block| block.commit != commit)
        {
            return Err(ServerError::InvalidOutboundState);
        }
        self.blocks.pop_front();
        Ok(())
    }

    #[cfg(feature = "hpack-test-support")]
    fn set_allocation_failure_after(&mut self, successful_allocations: Option<usize>) {
        self.allocation_failure_after = successful_allocations;
    }
}

enum H2OutboundHeaderBlockTarget<'a> {
    Server(&'a mut H2Server),
    Client(&'a mut H2Client),
}

/// A reversible outbound header transaction.
///
/// Dropping this value abandons the transaction without changing connection
/// compression state. [`Self::commit`] atomically assigns the complete framed
/// block to the owning connection's outbound queue.
pub struct H2OutboundHeaderBlock<'connection, 'headers> {
    target: H2OutboundHeaderBlockTarget<'connection>,
    stream_id: u32,
    headers: &'headers [H2HeaderField],
    end_stream: bool,
}

impl H2OutboundHeaderBlock<'_, '_> {
    /// Assigns this complete block to connection-owned wire order.
    pub fn commit(self) -> Result<H2OutboundCommit, H2ProtocolError> {
        let Self {
            target,
            stream_id,
            headers,
            end_stream,
        } = self;
        match target {
            H2OutboundHeaderBlockTarget::Server(server) => {
                server.enqueue_outbound_header_block(stream_id, headers, end_stream, 0, |_| {})
            }
            H2OutboundHeaderBlockTarget::Client(client) => {
                client.enqueue_outbound_header_block(stream_id, headers, end_stream, 0, |_| {})
            }
        }
    }
}

struct H2ConnectionCodecs {
    inbound: crate::hpack::Decoder,
    outbound: crate::hpack::Encoder,
}

impl H2ConnectionCodecs {
    fn new() -> Self {
        Self {
            inbound: crate::hpack::Decoder::new(),
            outbound: crate::hpack::Encoder::new(),
        }
    }
}

pub struct H2Server {
    preface_seen: bool,
    settings_seen: bool,
    settings: H2Settings,
    local_initial_window_size: u32,
    local_connection_window_size: u32,
    header_codecs: Option<H2ConnectionCodecs>,
    outbound_queue: Box<H2OutboundQueue>,
    request_streams: HashMap<u32, H2StreamState>,
    response_active_streams: HashMap<u32, H2FlowControlWindow>,
    response_sent_data: HashMap<u32, H2SentBodyState>,
    send_connection_window: H2FlowControlWindow,
    closed_streams: HashSet<u32>,
    reset_tolerant_streams: HashSet<u32>,
    closed_stream_order: VecDeque<u32>,
    max_peer_stream_id: u32,
    control_budget: H2ControlFrameBudget,
    limits: H2Limits,
    http_limits: HttpLimits,
    request_body_limit: Option<usize>,
    header_block: H2HeaderBlockAssembler,
    last_hpack_error: Option<H2HpackError>,
    last_protocol_error: Option<H2ProtocolError>,
    reported_protocol_error: Option<H2ProtocolError>,
    last_compat_error: Option<ServerError>,
    last_compat_error_recoverable: bool,
    handled_compat_error: Option<ServerError>,
    handled_stream_error: Option<H2ProtocolError>,
    terminal_protocol_error: Option<H2ProtocolError>,
    local_settings_state: H2SettingsSyncState,
    received_goaway_last_stream_id: Option<u32>,
    sent_goaway_last_stream_id: Option<u32>,
    control_diagnostics: H2ControlDiagnostics,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct H2SentBodyState {
    sent: usize,
    limit: Option<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct H2StreamState {
    content_length: Option<usize>,
    received_data_len: usize,
    body_limit: Option<usize>,
    data_forbidden: bool,
    header_count: usize,
    header_bytes: usize,
}

enum H2HeaderStreamDisposition {
    Accept,
    Refuse,
}

impl H2StreamState {
    fn new_raw(
        headers: &[H2HeaderField],
        limits: HttpLimits,
        body_limit: Option<usize>,
    ) -> Result<Self, ServerError> {
        enforce_h2_field_limits(headers, H2HeaderValidationRole::Request, limits)?;
        let content_length = content_length_from_raw_headers(headers)?;
        enforce_optional_body_size(content_length.unwrap_or(0), body_limit)?;
        let (header_count, header_bytes) =
            h2_field_totals(headers, H2HeaderValidationRole::Request);
        Ok(Self {
            content_length,
            received_data_len: 0,
            body_limit,
            data_forbidden: false,
            header_count,
            header_bytes,
        })
    }

    fn receive_data(&mut self, amount: usize, end_stream: bool) -> Result<(), ServerError> {
        if self.data_forbidden {
            return Err(ServerError::InvalidFrame);
        }
        self.received_data_len =
            self.received_data_len
                .checked_add(amount)
                .ok_or(ServerError::BodyTooLarge {
                    limit: self.body_limit.unwrap_or(usize::MAX),
                    actual: usize::MAX,
                })?;
        enforce_optional_body_size(self.received_data_len, self.body_limit)?;
        if let Some(expected) = self.content_length {
            if self.received_data_len > expected {
                return Err(ServerError::InvalidContentLength);
            }
            if end_stream && self.received_data_len != expected {
                return Err(ServerError::InvalidContentLength);
            }
        }
        Ok(())
    }

    fn new_response_raw(
        headers: &[H2HeaderField],
        request_is_head: bool,
        status: u16,
        limits: HttpLimits,
        body_limit: Option<usize>,
    ) -> Result<Self, ServerError> {
        enforce_h2_field_limits(headers, H2HeaderValidationRole::Response, limits)?;
        let content_length = content_length_from_raw_headers(headers)?;
        if status == 204 && content_length.is_some() {
            return Err(ServerError::InvalidContentLength);
        }
        if status == 205 && content_length.is_some_and(|length| length != 0) {
            return Err(ServerError::InvalidContentLength);
        }
        let data_forbidden = request_is_head || matches!(status, 204 | 205 | 304);
        enforce_optional_body_size(content_length.unwrap_or(0), body_limit)?;
        let (header_count, header_bytes) =
            h2_field_totals(headers, H2HeaderValidationRole::Response);
        Ok(Self {
            content_length: if data_forbidden { None } else { content_length },
            received_data_len: 0,
            body_limit,
            data_forbidden,
            header_count,
            header_bytes,
        })
    }

    fn accept_trailers(
        &mut self,
        headers: &[H2HeaderField],
        limits: HttpLimits,
    ) -> Result<(), ServerError> {
        let (count, bytes) = h2_field_totals(headers, H2HeaderValidationRole::Trailers);
        let actual_count = self.header_count.saturating_add(count);
        if actual_count > limits.max_headers() {
            return Err(ServerError::TooManyHeaders {
                limit: limits.max_headers(),
                actual: actual_count,
            });
        }
        let actual_bytes = self.header_bytes.saturating_add(bytes);
        if actual_bytes > limits.max_header_bytes() {
            return Err(ServerError::HeaderTooLarge {
                limit: limits.max_header_bytes(),
                actual: actual_bytes,
            });
        }
        self.header_count = actual_count;
        self.header_bytes = actual_bytes;
        Ok(())
    }

    fn finish(&self) -> Result<(), ServerError> {
        if let Some(expected) = self.content_length
            && self.received_data_len != expected
        {
            return Err(ServerError::InvalidContentLength);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
struct H2HeaderBlockAssembler {
    pending: Option<H2PendingHeaderBlock>,
    #[cfg(feature = "hpack-test-support")]
    allocation_failure_after: Option<usize>,
}

#[derive(Clone, Debug)]
struct H2PendingHeaderBlock {
    stream_id: u32,
    flags: u8,
    block: Vec<u8>,
    encoded_len: usize,
    continuation_frames: usize,
    self_dependency: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct H2CompleteHeaderBlock<'a> {
    stream_id: u32,
    flags: u8,
    block: Cow<'a, [u8]>,
    self_dependency: bool,
}

enum H2HeaderBlockAssemblyError {
    Protocol(ServerError),
    EncodedLimit(usize),
    Allocation,
}

impl From<ServerError> for H2HeaderBlockAssemblyError {
    fn from(error: ServerError) -> Self {
        Self::Protocol(error)
    }
}

impl H2HeaderBlockAssembler {
    fn accept<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
        limits: H2Limits,
    ) -> Result<Option<H2CompleteHeaderBlock<'a>>, H2HeaderBlockAssemblyError> {
        match frame.frame_type {
            H2FrameType::Headers => self.accept_headers(frame, limits),
            H2FrameType::Continuation => self.accept_continuation(frame, limits),
            _ if self.pending.is_some() => Err(H2HeaderBlockAssemblyError::Protocol(
                ServerError::InvalidFrame,
            )),
            _ => Ok(None),
        }
    }

    fn accept_headers<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
        limits: H2Limits,
    ) -> Result<Option<H2CompleteHeaderBlock<'a>>, H2HeaderBlockAssemblyError> {
        if self.pending.is_some() || frame.stream_id == 0 {
            return Err(H2HeaderBlockAssemblyError::Protocol(
                ServerError::InvalidFrame,
            ));
        }
        let self_dependency = header_has_self_dependency(frame);
        let block = headers_payload(frame.stream_id, frame.flags, frame.payload)
            .map_err(H2HeaderBlockAssemblyError::Protocol)?;
        self.check_header_block_len(block.len(), limits)?;
        if frame.flags & 0x4 != 0 {
            return Ok(Some(H2CompleteHeaderBlock {
                stream_id: frame.stream_id,
                flags: frame.flags,
                block: Cow::Borrowed(block),
                self_dependency,
            }));
        }
        let mut owned = Vec::new();
        self.try_reserve(&mut owned, block.len())?;
        owned.extend_from_slice(block);
        self.pending = Some(H2PendingHeaderBlock {
            stream_id: frame.stream_id,
            flags: frame.flags,
            block: owned,
            encoded_len: block.len(),
            continuation_frames: 0,
            self_dependency,
        });
        Ok(None)
    }

    fn accept_continuation<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
        limits: H2Limits,
    ) -> Result<Option<H2CompleteHeaderBlock<'a>>, H2HeaderBlockAssemblyError> {
        let Some(mut pending) = self.pending.take() else {
            return Err(H2HeaderBlockAssemblyError::Protocol(
                ServerError::InvalidFrame,
            ));
        };
        if frame.stream_id != pending.stream_id {
            self.pending = Some(pending);
            return Err(H2HeaderBlockAssemblyError::Protocol(
                ServerError::InvalidFrame,
            ));
        }
        pending.continuation_frames = pending.continuation_frames.checked_add(1).ok_or(
            H2HeaderBlockAssemblyError::Protocol(ServerError::InvalidFrame),
        )?;
        if pending.continuation_frames > limits.max_continuation_frames {
            self.pending = Some(pending);
            return Err(H2HeaderBlockAssemblyError::Protocol(
                ServerError::InvalidFrame,
            ));
        }
        let next_len = pending
            .encoded_len
            .checked_add(frame.payload.len())
            .ok_or(H2HeaderBlockAssemblyError::EncodedLimit(usize::MAX))?;
        self.check_header_block_len(next_len, limits)?;
        self.try_reserve(&mut pending.block, frame.payload.len())?;
        pending.block.extend_from_slice(frame.payload);
        pending.encoded_len = next_len;
        if frame.flags & 0x4 != 0 {
            Ok(Some(H2CompleteHeaderBlock {
                stream_id: pending.stream_id,
                flags: pending.flags | 0x4,
                block: Cow::Owned(pending.block),
                self_dependency: pending.self_dependency,
            }))
        } else {
            self.pending = Some(pending);
            Ok(None)
        }
    }

    fn check_header_block_len(
        &self,
        len: usize,
        limits: H2Limits,
    ) -> Result<(), H2HeaderBlockAssemblyError> {
        if len > limits.max_encoded_header_block_size {
            Err(H2HeaderBlockAssemblyError::EncodedLimit(len))
        } else {
            Ok(())
        }
    }

    fn try_reserve(
        &mut self,
        block: &mut Vec<u8>,
        additional: usize,
    ) -> Result<(), H2HeaderBlockAssemblyError> {
        if additional <= block.capacity().saturating_sub(block.len()) {
            return Ok(());
        }
        #[cfg(feature = "hpack-test-support")]
        if let Some(remaining) = self.allocation_failure_after.as_mut() {
            if *remaining == 0 {
                return Err(H2HeaderBlockAssemblyError::Allocation);
            }
            *remaining -= 1;
        }
        block
            .try_reserve(additional)
            .map_err(|_| H2HeaderBlockAssemblyError::Allocation)
    }

    #[cfg(feature = "hpack-test-support")]
    fn set_allocation_failure_after(&mut self, successful_allocations: Option<usize>) {
        self.allocation_failure_after = successful_allocations;
    }

    #[cfg(feature = "hpack-test-support")]
    fn set_pending_encoded_len(&mut self, encoded_len: usize) {
        self.pending
            .as_mut()
            .expect("test must first create pending header assembly")
            .encoded_len = encoded_len;
    }
}

fn accept_header_frame_for_connection<'a>(
    assembler: &mut H2HeaderBlockAssembler,
    frame: H2FrameRef<'a>,
    limits: H2Limits,
    last_protocol_error: &mut Option<H2ProtocolError>,
    terminal_protocol_error: &mut Option<H2ProtocolError>,
) -> Result<Option<H2CompleteHeaderBlock<'a>>, ServerError> {
    match assembler.accept(frame, limits) {
        Ok(block) => Ok(block),
        Err(H2HeaderBlockAssemblyError::Protocol(error)) => {
            *last_protocol_error = Some(H2ProtocolError::connection(
                H2ErrorCode::ProtocolError,
                "invalid HTTP/2 header-block framing",
            ));
            Err(error)
        }
        Err(H2HeaderBlockAssemblyError::EncodedLimit(actual)) => {
            assembler.pending = None;
            let limit = limits.max_encoded_header_block_size;
            let error = encoded_header_limit_error(limit, actual);
            *last_protocol_error = Some(error);
            *terminal_protocol_error = Some(error);
            Err(ServerError::HeaderTooLarge { limit, actual })
        }
        Err(H2HeaderBlockAssemblyError::Allocation) => {
            assembler.pending = None;
            let error = allocation_terminal_error();
            *last_protocol_error = Some(error);
            *terminal_protocol_error = Some(error);
            Err(ServerError::InvalidFrame)
        }
    }
}

fn header_has_self_dependency(frame: H2FrameRef<'_>) -> bool {
    if frame.frame_type != H2FrameType::Headers || frame.flags & 0x20 == 0 {
        return false;
    }
    let Ok(payload) = strip_padding(frame.flags, frame.payload) else {
        return false;
    };
    payload.len() >= 5
        && (u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]) & 0x7fff_ffff)
            == frame.stream_id
}

#[derive(Clone, Debug)]
struct H2ControlFrameBudget {
    settings: u32,
    window_update: u32,
    ping: u32,
    reset: u32,
    goaway: u32,
    priority: u32,
    last_refill: Instant,
}

const H2_CONTROL_BUDGET_REFILL_INTERVAL: Duration = Duration::from_secs(1);
// A peer can legitimately return one stream and one connection WINDOW_UPDATE
// for each validated protocol-progress frame.
const H2_WINDOW_UPDATE_CREDITS_PER_PROGRESS_FRAME: u32 = 2;

#[derive(Clone, Copy)]
struct H2ControlFrameBudgetLimits {
    settings: u32,
    window_update: u32,
    ping: u32,
    reset: u32,
    goaway: u32,
    priority: u32,
}

const H2_CONTROL_FRAME_BUDGET_LIMITS: H2ControlFrameBudgetLimits = H2ControlFrameBudgetLimits {
    settings: 64,
    window_update: 4096,
    ping: 64,
    reset: 256,
    goaway: 4,
    priority: 256,
};

impl Default for H2ControlFrameBudget {
    fn default() -> Self {
        Self {
            settings: H2_CONTROL_FRAME_BUDGET_LIMITS.settings,
            window_update: H2_CONTROL_FRAME_BUDGET_LIMITS.window_update,
            ping: H2_CONTROL_FRAME_BUDGET_LIMITS.ping,
            reset: H2_CONTROL_FRAME_BUDGET_LIMITS.reset,
            goaway: H2_CONTROL_FRAME_BUDGET_LIMITS.goaway,
            priority: H2_CONTROL_FRAME_BUDGET_LIMITS.priority,
            last_refill: Instant::now(),
        }
    }
}

impl H2ControlFrameBudget {
    fn record_control(&mut self, frame_type: H2FrameType) -> Result<(), ServerError> {
        self.refill_if_due(Instant::now());
        self.record_control_without_refill(frame_type)
    }

    fn record_control_without_refill(
        &mut self,
        frame_type: H2FrameType,
    ) -> Result<(), ServerError> {
        let remaining = match frame_type {
            H2FrameType::Settings => &mut self.settings,
            H2FrameType::WindowUpdate => &mut self.window_update,
            H2FrameType::Ping => &mut self.ping,
            H2FrameType::RstStream => &mut self.reset,
            H2FrameType::Goaway => &mut self.goaway,
            H2FrameType::Priority => &mut self.priority,
            H2FrameType::Data
            | H2FrameType::Headers
            | H2FrameType::Continuation
            | H2FrameType::PushPromise
            | H2FrameType::Unknown(_) => return Ok(()),
        };
        let Some(next) = remaining.checked_sub(1) else {
            return Err(ServerError::InvalidFrame);
        };
        *remaining = next;
        Ok(())
    }

    fn refill_after_meaningful_progress(&mut self, now: Instant) {
        self.refill_if_due(now);
        self.window_update = self
            .window_update
            .saturating_add(H2_WINDOW_UPDATE_CREDITS_PER_PROGRESS_FRAME)
            .min(H2_CONTROL_FRAME_BUDGET_LIMITS.window_update);
    }

    fn refill_if_due(&mut self, now: Instant) {
        if now.duration_since(self.last_refill) >= H2_CONTROL_BUDGET_REFILL_INTERVAL {
            self.settings = H2_CONTROL_FRAME_BUDGET_LIMITS.settings;
            self.window_update = H2_CONTROL_FRAME_BUDGET_LIMITS.window_update;
            self.ping = H2_CONTROL_FRAME_BUDGET_LIMITS.ping;
            self.reset = H2_CONTROL_FRAME_BUDGET_LIMITS.reset;
            self.goaway = H2_CONTROL_FRAME_BUDGET_LIMITS.goaway;
            self.priority = H2_CONTROL_FRAME_BUDGET_LIMITS.priority;
            self.last_refill = now;
        }
    }

    #[cfg(test)]
    fn record_control_at(
        &mut self,
        frame_type: H2FrameType,
        now: Instant,
    ) -> Result<(), ServerError> {
        self.refill_if_due(now);
        self.record_control_without_refill(frame_type)
    }
}

impl Default for H2Server {
    fn default() -> Self {
        Self {
            preface_seen: false,
            settings_seen: false,
            settings: H2Settings::default(),
            local_initial_window_size: H2Settings::default().initial_window_size,
            local_connection_window_size: H2Settings::default().initial_window_size,
            header_codecs: Some(H2ConnectionCodecs::new()),
            outbound_queue: Box::default(),
            request_streams: HashMap::new(),
            response_active_streams: HashMap::new(),
            response_sent_data: HashMap::new(),
            send_connection_window: H2FlowControlWindow::new(
                H2Settings::default().initial_window_size,
            )
            .expect("the default HTTP/2 window is valid"),
            closed_streams: HashSet::new(),
            reset_tolerant_streams: HashSet::new(),
            closed_stream_order: VecDeque::new(),
            max_peer_stream_id: 0,
            control_budget: H2ControlFrameBudget::default(),
            limits: H2Limits::default(),
            http_limits: HttpLimits::default(),
            request_body_limit: Some(HttpLimits::new().max_body_bytes()),
            header_block: H2HeaderBlockAssembler::default(),
            last_hpack_error: None,
            last_protocol_error: None,
            reported_protocol_error: None,
            last_compat_error: None,
            last_compat_error_recoverable: false,
            handled_compat_error: None,
            handled_stream_error: None,
            terminal_protocol_error: None,
            local_settings_state: H2SettingsSyncState::Synced,
            received_goaway_last_stream_id: None,
            sent_goaway_last_stream_id: None,
            control_diagnostics: H2ControlDiagnostics::default(),
        }
    }
}

impl H2Server {
    /// Creates protocol state for an adapter that owns the connection HPACK pair.
    ///
    /// Header blocks must be decoded by the adapter and supplied through
    /// [`Self::accept_external_header_fields`]. This prevents a second active
    /// codec history in layered connection implementations.
    pub fn for_external_hpack_adapter() -> Self {
        Self {
            header_codecs: None,
            ..Self::default()
        }
    }

    pub fn with_local_flow_control(
        initial_stream_window: u32,
        initial_connection_window: u32,
    ) -> Result<Self, ServerError> {
        Self::with_local_flow_control_and_limits(
            initial_stream_window,
            initial_connection_window,
            H2Limits::default(),
        )
    }

    pub fn with_local_flow_control_and_limits(
        initial_stream_window: u32,
        initial_connection_window: u32,
        limits: H2Limits,
    ) -> Result<Self, ServerError> {
        let http_limits = HttpLimits::new()
            .set_max_header_bytes(limits.max_header_list_size)
            .set_max_active_streams(limits.max_active_streams)
            .set_max_body_bytes(limits.max_queued_data_bytes);
        Self::with_local_flow_control_and_http_limits(
            initial_stream_window,
            initial_connection_window,
            limits,
            http_limits,
        )
    }

    /// Creates protocol state with explicit HTTP and HTTP/2 resource limits.
    pub fn with_local_flow_control_and_http_limits(
        initial_stream_window: u32,
        initial_connection_window: u32,
        limits: H2Limits,
        http_limits: HttpLimits,
    ) -> Result<Self, ServerError> {
        validate_h2_limits(limits)?;
        let mut settings = H2Settings::default();
        settings.apply(H2Setting::new(
            H2SettingId::InitialWindowSize,
            initial_stream_window,
        ))?;
        if initial_connection_window < H2Settings::default().initial_window_size {
            return Err(ServerError::InvalidFrame);
        }
        let mut server = Self {
            local_initial_window_size: initial_stream_window,
            local_connection_window_size: initial_connection_window,
            limits,
            http_limits,
            request_body_limit: Some(http_limits.max_body_bytes()),
            ..Self::default()
        };
        server
            .header_codecs
            .as_mut()
            .expect("default server owns HPACK codecs")
            .inbound
            .set_max_allowed_table_size(limits.max_header_table_size);
        Ok(server)
    }

    pub fn with_limits(limits: H2Limits) -> Result<Self, ServerError> {
        Self::with_local_flow_control_and_limits(
            H2Settings::default().initial_window_size,
            H2Settings::default().initial_window_size,
            limits,
        )
    }

    pub const fn settings(&self) -> &H2Settings {
        &self.settings
    }

    pub(crate) fn stream_request_bodies(&mut self) {
        debug_assert!(self.request_streams.is_empty());
        self.request_body_limit = None;
    }

    pub(crate) fn stream_response_body(&mut self, stream_id: u32) -> Result<(), ServerError> {
        let body = self
            .response_sent_data
            .get_mut(&stream_id)
            .ok_or(ServerError::InvalidOutboundState)?;
        body.limit = None;
        Ok(())
    }

    pub fn inbound_hpack_diagnostics(&self) -> H2HpackDiagnosticsSnapshot {
        self.header_codecs
            .as_ref()
            .map_or_else(H2HpackDiagnosticsSnapshot::default, |codecs| {
                codecs.inbound.diagnostics().into()
            })
    }

    pub fn outbound_hpack_diagnostics(&self) -> H2HpackDiagnosticsSnapshot {
        self.header_codecs
            .as_ref()
            .map_or_else(H2HpackDiagnosticsSnapshot::default, |codecs| {
                codecs.outbound.diagnostics().into()
            })
    }

    /// Returns the next complete outbound transaction in connection wire order.
    ///
    /// Check that the FIFO front has the expected receipt before assigning its
    /// bytes. Leave the block unacknowledged if assignment fails.
    ///
    /// ```
    /// use kimojio_fsm_http::{H2OutboundCommit, H2Server, ServerError};
    ///
    /// fn handoff(
    ///     server: &mut H2Server,
    ///     commit: H2OutboundCommit,
    ///     assign: impl FnOnce(&[u8]) -> Result<(), ServerError>,
    /// ) -> Result<(), ServerError> {
    ///     let block = server
    ///         .next_outbound_block()
    ///         .ok_or(ServerError::InvalidOutboundState)?;
    ///     if block.commit() != commit {
    ///         return Err(ServerError::InvalidOutboundState);
    ///     }
    ///     assign(block.bytes())?;
    ///     server.acknowledge_outbound_block(commit)
    /// }
    /// ```
    pub fn next_outbound_block(&self) -> Option<H2OutboundBlockRef<'_>> {
        self.outbound_queue.front()
    }

    /// Acknowledges that the next complete outbound transaction was handed to the transport.
    pub fn acknowledge_outbound_block(
        &mut self,
        commit: H2OutboundCommit,
    ) -> Result<(), ServerError> {
        self.outbound_queue.acknowledge(commit)
    }

    /// Begins a reversible raw header-block transaction.
    pub fn prepare_outbound_header_block<'connection, 'headers>(
        &'connection mut self,
        stream_id: u32,
        headers: &'headers [H2HeaderField],
        end_stream: bool,
    ) -> Result<H2OutboundHeaderBlock<'connection, 'headers>, H2ProtocolError> {
        if let Some(error) = self.terminal_protocol_error {
            return Err(error);
        }
        Ok(H2OutboundHeaderBlock {
            target: H2OutboundHeaderBlockTarget::Server(self),
            stream_id,
            headers,
            end_stream,
        })
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn set_inbound_allocation_failure_after_for_testing(
        &mut self,
        successful_allocations: Option<usize>,
    ) {
        self.header_codecs
            .as_mut()
            .expect("test connection owns HPACK codecs")
            .inbound
            .set_allocation_failure_after(successful_allocations);
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn set_outbound_allocation_failure_after_for_testing(
        &mut self,
        successful_allocations: Option<usize>,
    ) {
        self.outbound_queue
            .set_allocation_failure_after(successful_allocations);
        self.header_codecs
            .as_mut()
            .expect("test connection owns HPACK codecs")
            .outbound
            .set_allocation_failure_after(successful_allocations);
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn set_assembly_allocation_failure_after_for_testing(
        &mut self,
        successful_allocations: Option<usize>,
    ) {
        self.header_block
            .set_allocation_failure_after(successful_allocations);
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn set_pending_encoded_header_block_len_for_testing(&mut self, encoded_len: usize) {
        self.header_block.set_pending_encoded_len(encoded_len);
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn inbound_table_for_testing(&self) -> crate::hpack::TestTableSnapshot {
        self.header_codecs
            .as_ref()
            .expect("test connection owns HPACK codecs")
            .inbound
            .test_table_snapshot()
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn outbound_table_for_testing(&self) -> crate::hpack::TestTableSnapshot {
        self.header_codecs
            .as_ref()
            .expect("test connection owns HPACK codecs")
            .outbound
            .test_table_snapshot()
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn saturate_hpack_diagnostics_for_testing(&mut self) {
        let diagnostics = crate::hpack::Diagnostics {
            encoded_blocks: u64::MAX,
            decoded_blocks: u64::MAX,
            indexed_fields: u64::MAX,
            incremental_fields: u64::MAX,
            without_indexing_fields: u64::MAX,
            never_indexed_fields: u64::MAX,
            huffman_strings: u64::MAX,
            plain_strings: u64::MAX,
            table_size_updates: u64::MAX,
            table_insertions: u64::MAX,
            table_evictions: u64::MAX,
            compression_errors: u64::MAX,
            header_list_too_large: u64::MAX,
            field_bytes: u64::MAX,
            wire_bytes: u64::MAX,
        };
        let codecs = self
            .header_codecs
            .as_mut()
            .expect("test connection owns HPACK codecs");
        codecs.inbound.set_diagnostics_for_testing(diagnostics);
        codecs.outbound.set_diagnostics_for_testing(diagnostics);
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn terminal_for_testing(&self) -> bool {
        self.terminal_protocol_error.is_some()
    }

    pub fn record_progress_frame(&mut self) {
        self.control_budget
            .refill_after_meaningful_progress(Instant::now());
    }

    fn record_control(&mut self, frame_type: H2FrameType) -> Result<(), ServerError> {
        match self.control_budget.record_control(frame_type) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.control_diagnostics.record_rejection(frame_type);
                self.last_protocol_error = Some(H2ProtocolError::connection(
                    H2ErrorCode::EnhanceYourCalm,
                    "peer exceeded the HTTP/2 control-frame budget",
                ));
                Err(error)
            }
        }
    }

    fn apply_send_window_update(
        &mut self,
        stream_id: u32,
        increment: u32,
    ) -> Result<(), ServerError> {
        if stream_id == 0 {
            return self.send_connection_window.increase(increment);
        }
        if let Some(window) = self.response_active_streams.get_mut(&stream_id) {
            return window.increase(increment);
        }
        if self.closed_streams.contains(&stream_id) || self.request_streams.contains_key(&stream_id)
        {
            return Ok(());
        }
        self.last_protocol_error = Some(H2ProtocolError::connection(
            H2ErrorCode::ProtocolError,
            "WINDOW_UPDATE referenced an idle client stream",
        ));
        Err(ServerError::InvalidFrame)
    }

    pub fn mark_local_settings_sent(&mut self) {
        self.local_settings_state = H2SettingsSyncState::WaitingAck;
    }

    pub const fn control_diagnostics(&self) -> H2ControlDiagnostics {
        self.control_diagnostics
    }

    pub const fn timer_intent(&self) -> Option<H2TimerIntent> {
        match self.local_settings_state {
            H2SettingsSyncState::WaitingAck => Some(H2TimerIntent::SettingsAckTimeout),
            H2SettingsSyncState::Synced => None,
        }
    }

    pub fn shutdown_intent(&self) -> H2ShutdownIntent {
        match self.sent_goaway_last_stream_id {
            Some(last_stream_id)
                if !self.request_streams.is_empty() || !self.response_active_streams.is_empty() =>
            {
                H2ShutdownIntent::Drain { last_stream_id }
            }
            Some(_) => H2ShutdownIntent::Close,
            None => H2ShutdownIntent::None,
        }
    }

    /// Returns the largest client-initiated stream identifier processed so far.
    pub const fn highest_processed_stream_id(&self) -> u32 {
        self.max_peer_stream_id
    }

    pub fn goaway_frame(
        &mut self,
        last_stream_id: u32,
        error_code: u32,
    ) -> Result<Vec<u8>, ServerError> {
        if let Some(previous) = self.sent_goaway_last_stream_id
            && last_stream_id > previous
        {
            return Err(ServerError::InvalidFrame);
        }
        self.sent_goaway_last_stream_id = Some(last_stream_id);
        self.control_diagnostics.goaways = self.control_diagnostics.goaways.saturating_add(1);
        let mut output = Vec::new();
        H2Frame {
            frame_type: H2FrameType::Goaway,
            flags: 0,
            stream_id: 0,
            payload: [
                (last_stream_id & 0x7fff_ffff).to_be_bytes(),
                error_code.to_be_bytes(),
            ]
            .concat(),
        }
        .encode(&mut output);
        Ok(output)
    }

    /// Builds a GOAWAY frame with a typed HTTP/2 error code.
    pub fn goaway_frame_with_code(
        &mut self,
        last_stream_id: u32,
        error_code: H2ErrorCode,
    ) -> Result<Vec<u8>, ServerError> {
        self.goaway_frame(last_stream_id, error_code.as_u32())
    }

    /// Builds a RST_STREAM frame for a failed stream.
    pub fn rst_stream_frame(
        &mut self,
        stream_id: u32,
        error_code: u32,
    ) -> Result<Vec<u8>, ServerError> {
        if stream_id == 0 || stream_id > 0x7fff_ffff {
            return Err(ServerError::InvalidFrame);
        }
        self.control_diagnostics.resets = self.control_diagnostics.resets.saturating_add(1);
        let mut output = Vec::new();
        H2Frame {
            frame_type: H2FrameType::RstStream,
            flags: 0,
            stream_id,
            payload: error_code.to_be_bytes().to_vec(),
        }
        .encode(&mut output);
        Ok(output)
    }

    /// Builds a RST_STREAM frame with a typed HTTP/2 error code.
    pub fn rst_stream_frame_with_code(
        &mut self,
        stream_id: u32,
        error_code: H2ErrorCode,
    ) -> Result<Vec<u8>, ServerError> {
        self.rst_stream_frame(stream_id, error_code.as_u32())
    }

    pub(crate) fn take_reported_protocol_error(&mut self) -> Option<H2ProtocolError> {
        self.reported_protocol_error.take()
    }

    pub(crate) fn take_handled_stream_error(&mut self) -> Option<H2ProtocolError> {
        self.handled_stream_error.take()
    }

    pub(crate) fn take_handled_compat_error(&mut self) -> Option<ServerError> {
        self.handled_compat_error.take()
    }

    /// Rejects a PUSH_PROMISE frame sent by a client.
    ///
    /// Only servers may promise streams, so RFC 9113 section 8.4 requires a
    /// server that receives PUSH_PROMISE to treat it as a connection error of
    /// type PROTOCOL_ERROR.
    fn reject_client_push_promise(&mut self) -> ServerError {
        let error = H2ProtocolError::connection(
            H2ErrorCode::ProtocolError,
            "clients must not send PUSH_PROMISE frames",
        );
        self.last_protocol_error = Some(error);
        self.terminal_protocol_error = Some(error);
        ServerError::InvalidFrame
    }

    fn validate_peer_reset_frame(&mut self, frame: H2FrameRef<'_>) -> Result<(), ServerError> {
        if frame.stream_id == 0 {
            self.last_protocol_error = Some(H2ProtocolError::connection(
                H2ErrorCode::ProtocolError,
                "RST_STREAM used stream zero",
            ));
            return Err(ServerError::InvalidFrame);
        }
        if frame.payload.len() != 4 {
            self.last_protocol_error = Some(H2ProtocolError::connection(
                H2ErrorCode::FrameSizeError,
                "RST_STREAM payload length is not four octets",
            ));
            return Err(ServerError::InvalidFrame);
        }
        if frame.stream_id.is_multiple_of(2)
            || (!self.request_streams.contains_key(&frame.stream_id)
                && !self.response_active_streams.contains_key(&frame.stream_id)
                && !self.closed_streams.contains(&frame.stream_id))
        {
            self.last_protocol_error = Some(H2ProtocolError::connection(
                H2ErrorCode::ProtocolError,
                "RST_STREAM referenced an idle client stream",
            ));
            return Err(ServerError::InvalidFrame);
        }
        Ok(())
    }

    fn validate_peer_priority_frame(&mut self, frame: H2FrameRef<'_>) -> Result<(), ServerError> {
        if let Err(error) = priority_payload(frame.stream_id, frame.payload) {
            self.last_protocol_error = Some(if frame.stream_id == 0 {
                H2ProtocolError::connection(H2ErrorCode::ProtocolError, "PRIORITY used stream zero")
            } else if frame.payload.len() != 5 {
                H2ProtocolError::connection(
                    H2ErrorCode::FrameSizeError,
                    "PRIORITY payload length is not five octets",
                )
            } else {
                H2ProtocolError::stream(
                    frame.stream_id,
                    H2ErrorCode::ProtocolError,
                    "PRIORITY dependency references its own stream",
                )
            });
            return Err(error);
        }
        Ok(())
    }

    fn peer_window_update_increment(&mut self, frame: H2FrameRef<'_>) -> Result<u32, ServerError> {
        match window_update_increment(frame.payload) {
            Ok(increment) => Ok(increment),
            Err(error) => {
                self.last_protocol_error = Some(if frame.payload.len() != 4 {
                    H2ProtocolError::connection(
                        H2ErrorCode::FrameSizeError,
                        "WINDOW_UPDATE payload length is not four octets",
                    )
                } else if frame.stream_id == 0 {
                    H2ProtocolError::connection(
                        H2ErrorCode::ProtocolError,
                        "connection WINDOW_UPDATE increment is zero",
                    )
                } else {
                    H2ProtocolError::stream(
                        frame.stream_id,
                        H2ErrorCode::ProtocolError,
                        "stream WINDOW_UPDATE increment is zero",
                    )
                });
                Err(error)
            }
        }
    }

    fn peer_data_payload<'a>(&mut self, frame: H2FrameRef<'a>) -> Result<&'a [u8], ServerError> {
        match data_payload(frame.flags, frame.payload) {
            Ok(payload) => Ok(payload),
            Err(error) => {
                self.last_protocol_error = Some(H2ProtocolError::connection(
                    H2ErrorCode::ProtocolError,
                    "DATA padding is invalid",
                ));
                Err(error)
            }
        }
    }

    /// Accepts bytes through the compatibility request-only interface.
    ///
    /// Reset-tolerant DATA is consumed without delivering a request, preserving
    /// the behavior from before discarded-DATA events were exposed. Owners that
    /// account connection receive credit must use [`Self::accept_event_ref`] or
    /// [`Self::accept_event`] instead.
    pub fn accept(
        &mut self,
        input: &[u8],
    ) -> Result<(Option<H2Request>, usize, Vec<u8>), ServerError> {
        let mut consumed = 0usize;
        let mut output = Vec::new();
        while consumed < input.len() {
            let (event, used, mut event_output) = self.accept_event_ref(&input[consumed..])?;
            consumed += used;
            output.append(&mut event_output);
            match event {
                Some(H2StreamEvent::RequestHeaders { request, .. }) => {
                    return Ok((Some(request), consumed, output));
                }
                Some(
                    H2StreamEvent::Settings { .. }
                    | H2StreamEvent::Ping { .. }
                    | H2StreamEvent::WindowUpdate { .. }
                    | H2StreamEvent::DiscardedData { .. },
                ) => {}
                Some(_) => return Err(ServerError::InvalidFrame),
                None => return Ok((None, consumed, output)),
            }
            if used == 0 {
                return Ok((None, consumed, output));
            }
        }
        Ok((None, consumed, output))
    }

    pub fn accept_event_bytes(
        &mut self,
        input: &[u8],
    ) -> Result<(Option<H2ByteStreamEvent>, usize, Vec<u8>), ServerError> {
        let (event, consumed, output) = self.accept_event_bytes_ref(input)?;
        if let Some(error) = self.handled_compat_error.take() {
            self.reported_protocol_error = self.handled_stream_error;
            return Err(error);
        }
        Ok((
            event.map(H2ByteStreamEventRef::into_owned),
            consumed,
            output,
        ))
    }

    pub fn accept_event_bytes_ref<'a>(
        &mut self,
        input: &'a [u8],
    ) -> Result<(Option<H2ByteStreamEventRef<'a>>, usize, Vec<u8>), ServerError> {
        self.reported_protocol_error = None;
        if let Some(error) = self.terminal_protocol_error {
            self.reported_protocol_error = Some(error);
            return Err(error.into());
        }
        let mut consumed = 0usize;
        let mut output = Vec::new();
        if !self.preface_seen {
            if input.len() < CLIENT_PREFACE.len() {
                if CLIENT_PREFACE.starts_with(input) {
                    return Ok((None, 0, output));
                }
                self.reported_protocol_error = Some(H2ProtocolError::connection(
                    H2ErrorCode::ProtocolError,
                    "invalid HTTP/2 client preface",
                ));
                return Err(ServerError::InvalidPreface);
            }
            if &input[..CLIENT_PREFACE.len()] != CLIENT_PREFACE {
                self.reported_protocol_error = Some(H2ProtocolError::connection(
                    H2ErrorCode::ProtocolError,
                    "invalid HTTP/2 client preface",
                ));
                return Err(ServerError::InvalidPreface);
            }
            self.preface_seen = true;
            consumed += CLIENT_PREFACE.len();
        }
        if !self.settings_seen {
            let (frame, used) = match H2FrameRef::decode_outcome_with_max_frame_size(
                &input[consumed..],
                self.limits.max_frame_size,
            ) {
                H2FrameRefDecodeOutcome::Frame { frame, consumed } => (frame, consumed),
                H2FrameRefDecodeOutcome::NeedMore => return Ok((None, consumed, output)),
                H2FrameRefDecodeOutcome::Error(error) => {
                    self.reported_protocol_error = Some(error);
                    return Err(error.into());
                }
            };
            if frame.frame_type != H2FrameType::Settings
                || frame.flags & 0x1 != 0
                || frame.stream_id != 0
            {
                self.reported_protocol_error = Some(H2ProtocolError::connection(
                    H2ErrorCode::ProtocolError,
                    "HTTP/2 client connection preface must start with SETTINGS",
                ));
                return Err(ServerError::InvalidFrame);
            }
            if let Err(error) = self.apply_settings(frame.payload) {
                let protocol_error =
                    self.h2_error_from_server_error(error.clone(), Some(frame.head()));
                self.reported_protocol_error = Some(protocol_error);
                return Err(error);
            }
            self.settings_seen = true;
            consumed += used;
            self.encode_local_settings(&mut output);
            self.local_settings_state = H2SettingsSyncState::WaitingAck;
            H2Frame {
                frame_type: H2FrameType::Settings,
                flags: 0x1,
                stream_id: 0,
                payload: Vec::new(),
            }
            .encode(&mut output);
            self.encode_local_connection_window_update(&mut output);
        }
        if input.len() == consumed {
            return Ok((None, consumed, output));
        }
        let (frame, used) = match H2FrameRef::decode_outcome_with_max_frame_size(
            &input[consumed..],
            self.limits.max_frame_size,
        ) {
            H2FrameRefDecodeOutcome::Frame { frame, consumed } => (frame, consumed),
            H2FrameRefDecodeOutcome::NeedMore => return Ok((None, consumed, output)),
            H2FrameRefDecodeOutcome::Error(error) => {
                self.reported_protocol_error = Some(error);
                return Err(error.into());
            }
        };
        consumed += used;
        let (event, mut event_output) = self.accept_frame_bytes_ref(frame)?;
        output.append(&mut event_output);
        Ok((event, consumed, output))
    }

    pub fn accept_event(
        &mut self,
        input: &[u8],
    ) -> Result<(Option<H2StreamEvent>, usize, Vec<u8>), ServerError> {
        let (event, consumed, output) = self.accept_event_bytes(input)?;
        if let Some(error) = self.handled_compat_error.take() {
            self.reported_protocol_error = self.handled_stream_error;
            return Err(error);
        }
        let event = event
            .map(H2ByteStreamEvent::try_into_text)
            .transpose()
            .map_err(|_| ServerError::MalformedMessage)?;
        Ok((event, consumed, output))
    }

    pub fn accept_event_ref<'a>(
        &mut self,
        input: &'a [u8],
    ) -> Result<(Option<H2StreamEventRef<'a>>, usize, Vec<u8>), ServerError> {
        let (event, consumed, output) = self.accept_event_bytes_ref(input)?;
        if let Some(error) = self.handled_compat_error.take() {
            self.reported_protocol_error = self.handled_stream_error;
            return Err(error);
        }
        let event = event
            .map(H2ByteStreamEvent::try_into_text)
            .transpose()
            .map_err(|_| ServerError::MalformedMessage)?;
        Ok((event, consumed, output))
    }

    pub fn accept_frame_bytes(
        &mut self,
        frame: H2Frame,
    ) -> Result<(Option<H2ByteStreamEvent>, Vec<u8>), ServerError> {
        let (outcome, output) = self.accept_frame_bytes_typed(frame);
        match outcome {
            H2FrameOutcome::Event(event) => Ok((Some(event), output)),
            H2FrameOutcome::Ignored => Ok((None, output)),
            H2FrameOutcome::Error(error) => {
                let output = self.finish_compat_frame_error(error)?;
                Ok((None, output))
            }
        }
    }

    pub fn accept_frame_bytes_ref<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> Result<(Option<H2ByteStreamEventRef<'a>>, Vec<u8>), ServerError> {
        let (outcome, output) = self.accept_frame_bytes_ref_typed(frame);
        match outcome {
            H2FrameOutcome::Event(event) => Ok((Some(event), output)),
            H2FrameOutcome::Ignored => Ok((None, output)),
            H2FrameOutcome::Error(error) => {
                let output = self.finish_compat_frame_error(error)?;
                Ok((None, output))
            }
        }
    }

    pub fn accept_frame_bytes_typed(
        &mut self,
        frame: H2Frame,
    ) -> (H2FrameOutcome<H2ByteStreamEvent>, Vec<u8>) {
        let (outcome, output) = self.accept_frame_bytes_ref_typed(frame.as_ref());
        (outcome.map_event(H2ByteStreamEventRef::into_owned), output)
    }

    pub fn accept_frame_bytes_ref_typed<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> (H2FrameOutcome<H2ByteStreamEventRef<'a>>, Vec<u8>) {
        self.last_compat_error = None;
        self.last_compat_error_recoverable = false;
        self.handled_compat_error = None;
        self.handled_stream_error = None;
        if let Some(error) = self.terminal_protocol_error {
            return (H2FrameOutcome::Error(error), Vec::new());
        }
        if !self.settings_seen
            && (!matches!(frame.frame_type, H2FrameType::Settings)
                || frame.flags & 0x1 != 0
                || frame.stream_id != 0)
        {
            return (
                H2FrameOutcome::Error(H2ProtocolError::connection(
                    H2ErrorCode::ProtocolError,
                    "HTTP/2 client connection preface must start with SETTINGS",
                )),
                Vec::new(),
            );
        }
        if let Some(pending) = self.header_block.pending.as_ref()
            && (!matches!(frame.frame_type, H2FrameType::Continuation)
                || frame.stream_id != pending.stream_id)
        {
            return (
                H2FrameOutcome::Error(H2ProtocolError::connection(
                    H2ErrorCode::ProtocolError,
                    "HTTP/2 header block continuation sequence violated",
                )),
                Vec::new(),
            );
        }
        let head = frame.head();
        match self.accept_frame_compat(frame) {
            Ok((Some(event), output)) => (H2FrameOutcome::Event(event), output),
            Ok((None, output)) => (H2FrameOutcome::Ignored, output),
            Err(error) => {
                let explicitly_classified = self.last_protocol_error.is_some();
                let typed = self.h2_error_from_server_error(error.clone(), Some(head));
                if let H2ProtocolError {
                    scope: H2ErrorScope::Stream(_),
                    code: H2ErrorCode::RefusedStream,
                    ..
                } = typed
                {
                    return match self.handle_stream_error(typed) {
                        Ok(output) => (H2FrameOutcome::Ignored, output),
                        Err(reset_error) => {
                            self.last_compat_error = Some(reset_error.clone());
                            (
                                H2FrameOutcome::Error(
                                    self.h2_error_from_server_error(reset_error, Some(head)),
                                ),
                                Vec::new(),
                            )
                        }
                    };
                }
                self.last_compat_error_recoverable =
                    explicitly_classified || recoverable_h2_message_error(&error);
                self.last_compat_error = Some(error);
                (H2FrameOutcome::Error(typed), Vec::new())
            }
        }
    }

    pub fn accept_frame(
        &mut self,
        frame: H2Frame,
    ) -> Result<(Option<H2StreamEvent>, Vec<u8>), ServerError> {
        let (outcome, output) = self.accept_frame_typed(frame);
        match outcome {
            H2FrameOutcome::Event(event) => Ok((Some(event), output)),
            H2FrameOutcome::Ignored => Ok((None, output)),
            H2FrameOutcome::Error(error) => Err(error.into()),
        }
    }

    pub fn accept_frame_ref<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> Result<(Option<H2StreamEventRef<'a>>, Vec<u8>), ServerError> {
        let (outcome, output) = self.accept_frame_ref_typed(frame);
        match outcome {
            H2FrameOutcome::Event(event) => Ok((Some(event), output)),
            H2FrameOutcome::Ignored => Ok((None, output)),
            H2FrameOutcome::Error(error) => Err(error.into()),
        }
    }

    pub fn accept_frame_typed(
        &mut self,
        frame: H2Frame,
    ) -> (H2FrameOutcome<H2StreamEvent>, Vec<u8>) {
        let stream_id = frame.stream_id;
        let (outcome, output) = self.accept_frame_bytes_typed(frame);
        (project_stream_outcome(outcome, stream_id), output)
    }

    pub fn accept_frame_ref_typed<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> (H2FrameOutcome<H2StreamEventRef<'a>>, Vec<u8>) {
        let stream_id = frame.stream_id;
        let (outcome, output) = self.accept_frame_bytes_ref_typed(frame);
        (project_stream_outcome(outcome, stream_id), output)
    }

    fn accept_frame_compat<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> Result<(Option<H2ByteStreamEventRef<'a>>, Vec<u8>), ServerError> {
        if self.header_block.pending.is_some()
            && !matches!(frame.frame_type, H2FrameType::Continuation)
        {
            return Err(ServerError::InvalidFrame);
        }
        let mut output = Vec::new();
        let event = match frame.frame_type {
            H2FrameType::Settings => {
                self.record_control(H2FrameType::Settings)?;
                if frame.stream_id != 0 {
                    return Err(ServerError::InvalidFrame);
                }
                let initial_window_size = if frame.flags & 0x1 != 0 {
                    if !frame.payload.is_empty() {
                        return Err(ServerError::InvalidFrame);
                    }
                    if self.local_settings_state != H2SettingsSyncState::WaitingAck {
                        return Err(ServerError::InvalidFrame);
                    }
                    self.local_settings_state = H2SettingsSyncState::Synced;
                    self.control_diagnostics.settings_acks =
                        self.control_diagnostics.settings_acks.saturating_add(1);
                    None
                } else {
                    self.settings_seen = true;
                    self.control_diagnostics.settings_frames =
                        self.control_diagnostics.settings_frames.saturating_add(1);
                    let initial_window_size = self.apply_settings(frame.payload)?;
                    H2Frame {
                        frame_type: H2FrameType::Settings,
                        flags: 0x1,
                        stream_id: 0,
                        payload: Vec::new(),
                    }
                    .encode(&mut output);
                    initial_window_size
                };
                H2ByteStreamEvent::Settings {
                    initial_window_size,
                }
            }
            H2FrameType::WindowUpdate => {
                self.record_control(H2FrameType::WindowUpdate)?;
                let increment = self.peer_window_update_increment(frame)?;
                self.apply_send_window_update(frame.stream_id, increment)?;
                H2ByteStreamEvent::WindowUpdate {
                    stream_id: frame.stream_id,
                    increment,
                }
            }
            H2FrameType::Ping => {
                self.record_control(H2FrameType::Ping)?;
                if frame.stream_id != 0 || frame.payload.len() != 8 {
                    return Err(ServerError::InvalidFrame);
                }
                let ack = frame.flags & 0x1 != 0;
                if ack {
                    self.control_diagnostics.ping_acks =
                        self.control_diagnostics.ping_acks.saturating_add(1);
                } else {
                    self.control_diagnostics.pings =
                        self.control_diagnostics.pings.saturating_add(1);
                }
                if !ack {
                    H2FrameRef {
                        frame_type: H2FrameType::Ping,
                        flags: 0x1,
                        stream_id: 0,
                        payload: frame.payload,
                    }
                    .encode(&mut output);
                }
                H2ByteStreamEvent::Ping { ack }
            }
            H2FrameType::Priority => {
                self.record_control(H2FrameType::Priority)?;
                self.validate_peer_priority_frame(frame)?;
                return Ok((None, output));
            }
            H2FrameType::RstStream => {
                self.record_control(H2FrameType::RstStream)?;
                self.validate_peer_reset_frame(frame)?;
                self.control_diagnostics.resets = self.control_diagnostics.resets.saturating_add(1);
                self.request_streams.remove(&frame.stream_id);
                self.response_active_streams.remove(&frame.stream_id);
                // The peer knows it reset this stream, so nothing it sends
                // afterwards can be in flight and reset tolerance would only
                // mask a protocol violation.
                self.remember_closed_stream(frame.stream_id);
                H2ByteStreamEvent::Reset {
                    stream_id: frame.stream_id,
                    error_code: u32::from_be_bytes([
                        frame.payload[0],
                        frame.payload[1],
                        frame.payload[2],
                        frame.payload[3],
                    ]),
                }
            }
            H2FrameType::Headers => {
                if let Some(block) = accept_header_frame_for_connection(
                    &mut self.header_block,
                    frame,
                    self.limits,
                    &mut self.last_protocol_error,
                    &mut self.terminal_protocol_error,
                )? {
                    self.event_from_complete_headers(block)?
                } else {
                    return Ok((None, output));
                }
            }
            H2FrameType::Data => {
                let payload = self.peer_data_payload(frame)?;
                let discard = self.closed_streams.contains(&frame.stream_id)
                    && self.reset_tolerant_streams.contains(&frame.stream_id);
                self.validate_data_frame(frame.stream_id, payload.len(), frame.flags & 0x1 != 0)?;
                if !payload.is_empty() || frame.flags & 0x1 != 0 {
                    self.record_progress_frame();
                }
                if discard {
                    H2ByteStreamEvent::DiscardedData {
                        stream_id: frame.stream_id,
                        flow_control_len: frame.payload.len(),
                    }
                } else {
                    H2ByteStreamEvent::Data {
                        stream_id: frame.stream_id,
                        payload,
                        flow_control_len: frame.payload.len(),
                        end_stream: frame.flags & 0x1 != 0,
                    }
                }
            }
            H2FrameType::Goaway => {
                self.record_control(H2FrameType::Goaway)?;
                if frame.stream_id != 0 || frame.payload.len() < 8 {
                    return Err(ServerError::InvalidFrame);
                }
                let mut last_stream_id = u32::from_be_bytes([
                    frame.payload[0],
                    frame.payload[1],
                    frame.payload[2],
                    frame.payload[3],
                ]);
                last_stream_id &= 0x7fff_ffff;
                self.received_goaway_last_stream_id = Some(last_stream_id);
                self.control_diagnostics.goaways =
                    self.control_diagnostics.goaways.saturating_add(1);
                H2ByteStreamEvent::Goaway {
                    last_stream_id,
                    error_code: u32::from_be_bytes([
                        frame.payload[4],
                        frame.payload[5],
                        frame.payload[6],
                        frame.payload[7],
                    ]),
                }
            }
            H2FrameType::Continuation => {
                if let Some(block) = accept_header_frame_for_connection(
                    &mut self.header_block,
                    frame,
                    self.limits,
                    &mut self.last_protocol_error,
                    &mut self.terminal_protocol_error,
                )? {
                    self.event_from_complete_headers(block)?
                } else {
                    return Ok((None, output));
                }
            }
            H2FrameType::PushPromise => {
                return Err(self.reject_client_push_promise());
            }
            H2FrameType::Unknown(_) => return Ok((None, output)),
        };
        Ok((Some(event), output))
    }

    /// Classifies a frame while advancing HTTP/2 connection state.
    ///
    /// This is not a pure inspection helper: SETTINGS, HPACK/header-block,
    /// stream lifecycle, control-budget, and diagnostic state are updated.
    pub fn classify_frame(&mut self, frame: H2Frame) -> Result<Option<H2StreamEvent>, ServerError> {
        match self.classify_frame_typed(frame) {
            H2FrameOutcome::Event(event) => Ok(Some(event)),
            H2FrameOutcome::Ignored => Ok(None),
            H2FrameOutcome::Error(error) => Err(error.into()),
        }
    }

    /// Borrowed variant of [`Self::classify_frame`].
    pub fn classify_frame_ref<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> Result<Option<H2StreamEventRef<'a>>, ServerError> {
        match self.classify_frame_ref_typed(frame) {
            H2FrameOutcome::Event(event) => Ok(Some(event)),
            H2FrameOutcome::Ignored => Ok(None),
            H2FrameOutcome::Error(error) => Err(error.into()),
        }
    }

    /// Typed variant of [`Self::classify_frame`] with the same stateful effects.
    pub fn classify_frame_typed(&mut self, frame: H2Frame) -> H2FrameOutcome<H2StreamEvent> {
        let stream_id = frame.stream_id;
        project_stream_outcome(
            self.classify_frame_bytes_ref_typed(frame.as_ref())
                .map_event(H2ByteStreamEventRef::into_owned),
            stream_id,
        )
    }

    /// Borrowed typed variant of [`Self::classify_frame`].
    pub fn classify_frame_ref_typed<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> H2FrameOutcome<H2StreamEventRef<'a>> {
        let stream_id = frame.stream_id;
        project_stream_outcome(self.classify_frame_bytes_ref_typed(frame), stream_id)
    }

    fn classify_frame_bytes_ref_typed<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> H2FrameOutcome<H2ByteStreamEventRef<'a>> {
        if let Some(error) = self.terminal_protocol_error {
            return H2FrameOutcome::Error(error);
        }
        if !self.settings_seen
            && (!matches!(frame.frame_type, H2FrameType::Settings)
                || frame.flags & 0x1 != 0
                || frame.stream_id != 0)
        {
            return H2FrameOutcome::Error(H2ProtocolError::connection(
                H2ErrorCode::ProtocolError,
                "HTTP/2 client connection preface must start with SETTINGS",
            ));
        }
        if let Some(pending) = self.header_block.pending.as_ref()
            && (!matches!(frame.frame_type, H2FrameType::Continuation)
                || frame.stream_id != pending.stream_id)
        {
            return H2FrameOutcome::Error(H2ProtocolError::connection(
                H2ErrorCode::ProtocolError,
                "HTTP/2 header block continuation sequence violated",
            ));
        }
        let head = frame.head();
        match self.accept_frame_without_output(frame) {
            Ok((Some(event), ())) => H2FrameOutcome::Event(event),
            Ok((None, ())) => H2FrameOutcome::Ignored,
            Err(error) => H2FrameOutcome::Error(self.h2_error_from_server_error(error, Some(head))),
        }
    }

    fn accept_frame_without_output<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> Result<(Option<H2ByteStreamEventRef<'a>>, ()), ServerError> {
        let event = match frame.frame_type {
            H2FrameType::Settings => {
                self.record_control(H2FrameType::Settings)?;
                if frame.stream_id != 0 {
                    return Err(ServerError::InvalidFrame);
                }
                let initial_window_size = if frame.flags & 0x1 != 0 {
                    if !frame.payload.is_empty() {
                        return Err(ServerError::InvalidFrame);
                    }
                    if self.local_settings_state != H2SettingsSyncState::WaitingAck {
                        return Err(ServerError::InvalidFrame);
                    }
                    self.local_settings_state = H2SettingsSyncState::Synced;
                    self.control_diagnostics.settings_acks =
                        self.control_diagnostics.settings_acks.saturating_add(1);
                    None
                } else {
                    self.settings_seen = true;
                    self.control_diagnostics.settings_frames =
                        self.control_diagnostics.settings_frames.saturating_add(1);
                    self.apply_settings(frame.payload)?
                };
                H2ByteStreamEvent::Settings {
                    initial_window_size,
                }
            }
            H2FrameType::WindowUpdate => {
                self.record_control(H2FrameType::WindowUpdate)?;
                let increment = self.peer_window_update_increment(frame)?;
                self.apply_send_window_update(frame.stream_id, increment)?;
                H2ByteStreamEvent::WindowUpdate {
                    stream_id: frame.stream_id,
                    increment,
                }
            }
            H2FrameType::Ping => {
                self.record_control(H2FrameType::Ping)?;
                if frame.stream_id != 0 || frame.payload.len() != 8 {
                    return Err(ServerError::InvalidFrame);
                }
                if frame.flags & 0x1 != 0 {
                    self.control_diagnostics.ping_acks =
                        self.control_diagnostics.ping_acks.saturating_add(1);
                } else {
                    self.control_diagnostics.pings =
                        self.control_diagnostics.pings.saturating_add(1);
                }
                H2ByteStreamEvent::Ping {
                    ack: frame.flags & 0x1 != 0,
                }
            }
            H2FrameType::Priority => {
                self.record_control(H2FrameType::Priority)?;
                self.validate_peer_priority_frame(frame)?;
                return Ok((None, ()));
            }
            H2FrameType::RstStream => {
                self.record_control(H2FrameType::RstStream)?;
                self.validate_peer_reset_frame(frame)?;
                self.control_diagnostics.resets = self.control_diagnostics.resets.saturating_add(1);
                self.request_streams.remove(&frame.stream_id);
                self.response_active_streams.remove(&frame.stream_id);
                // The peer knows it reset this stream, so nothing it sends
                // afterwards can be in flight and reset tolerance would only
                // mask a protocol violation.
                self.remember_closed_stream(frame.stream_id);
                H2ByteStreamEvent::Reset {
                    stream_id: frame.stream_id,
                    error_code: u32::from_be_bytes([
                        frame.payload[0],
                        frame.payload[1],
                        frame.payload[2],
                        frame.payload[3],
                    ]),
                }
            }
            H2FrameType::Headers => {
                if let Some(block) = accept_header_frame_for_connection(
                    &mut self.header_block,
                    frame,
                    self.limits,
                    &mut self.last_protocol_error,
                    &mut self.terminal_protocol_error,
                )? {
                    self.event_from_complete_headers(block)?
                } else {
                    return Ok((None, ()));
                }
            }
            H2FrameType::Data => {
                let payload = self.peer_data_payload(frame)?;
                let discard = self.closed_streams.contains(&frame.stream_id)
                    && self.reset_tolerant_streams.contains(&frame.stream_id);
                self.validate_data_frame(frame.stream_id, payload.len(), frame.flags & 0x1 != 0)?;
                if !payload.is_empty() || frame.flags & 0x1 != 0 {
                    self.record_progress_frame();
                }
                if discard {
                    H2ByteStreamEvent::DiscardedData {
                        stream_id: frame.stream_id,
                        flow_control_len: frame.payload.len(),
                    }
                } else {
                    H2ByteStreamEvent::Data {
                        stream_id: frame.stream_id,
                        payload,
                        flow_control_len: frame.payload.len(),
                        end_stream: frame.flags & 0x1 != 0,
                    }
                }
            }
            H2FrameType::Goaway => {
                self.record_control(H2FrameType::Goaway)?;
                if frame.stream_id != 0 || frame.payload.len() < 8 {
                    return Err(ServerError::InvalidFrame);
                }
                let mut last_stream_id = u32::from_be_bytes([
                    frame.payload[0],
                    frame.payload[1],
                    frame.payload[2],
                    frame.payload[3],
                ]);
                last_stream_id &= 0x7fff_ffff;
                self.received_goaway_last_stream_id = Some(last_stream_id);
                self.control_diagnostics.goaways =
                    self.control_diagnostics.goaways.saturating_add(1);
                H2ByteStreamEvent::Goaway {
                    last_stream_id,
                    error_code: u32::from_be_bytes([
                        frame.payload[4],
                        frame.payload[5],
                        frame.payload[6],
                        frame.payload[7],
                    ]),
                }
            }
            H2FrameType::Continuation => {
                if let Some(block) = accept_header_frame_for_connection(
                    &mut self.header_block,
                    frame,
                    self.limits,
                    &mut self.last_protocol_error,
                    &mut self.terminal_protocol_error,
                )? {
                    self.event_from_complete_headers(block)?
                } else {
                    return Ok((None, ()));
                }
            }
            H2FrameType::PushPromise => {
                return Err(self.reject_client_push_promise());
            }
            H2FrameType::Unknown(_) => return Ok((None, ())),
        };
        Ok((Some(event), ()))
    }

    fn event_from_complete_headers<'a>(
        &mut self,
        block: H2CompleteHeaderBlock<'a>,
    ) -> Result<H2ByteStreamEventRef<'a>, ServerError> {
        let event = self.event_from_complete_headers_parts::<&'a [u8]>(
            block.stream_id,
            block.flags,
            &block.block,
            block.self_dependency,
        )?;
        self.record_progress_frame();
        Ok(event)
    }

    fn event_from_complete_headers_parts<P>(
        &mut self,
        stream_id: u32,
        flags: u8,
        block: &[u8],
        self_dependency: bool,
    ) -> Result<H2ByteStreamEvent<P>, ServerError> {
        let role = if self.request_streams.contains_key(&stream_id) {
            H2HeaderValidationRole::Trailers
        } else {
            H2HeaderValidationRole::Request
        };
        let headers = self.decode_h2_header_fields(stream_id, block, role)?;
        let disposition = self.validate_header_stream_state(stream_id, flags)?;
        if self_dependency {
            self.last_protocol_error = Some(H2ProtocolError::stream(
                stream_id,
                H2ErrorCode::ProtocolError,
                "HEADERS priority dependency references its own stream",
            ));
            return Err(ServerError::InvalidFrame);
        }
        // A refused block can mutate the connection-wide HPACK table, so the
        // capacity decision must happen only after the complete block decodes.
        if let H2HeaderStreamDisposition::Refuse = disposition {
            self.record_refused_stream(stream_id);
            return Err(ServerError::InvalidFrame);
        }
        self.event_from_decoded_header_fields(stream_id, flags, headers)
    }

    fn validate_header_stream_state(
        &mut self,
        stream_id: u32,
        flags: u8,
    ) -> Result<H2HeaderStreamDisposition, ServerError> {
        if self.request_streams.contains_key(&stream_id) {
            return if flags & 0x1 != 0 {
                Ok(H2HeaderStreamDisposition::Accept)
            } else {
                self.last_protocol_error = Some(H2ProtocolError::stream(
                    stream_id,
                    H2ErrorCode::ProtocolError,
                    "request trailers did not end the stream",
                ));
                Err(ServerError::InvalidFrame)
            };
        }
        if stream_id == 0 || stream_id.is_multiple_of(2) || stream_id > 0x7fff_ffff {
            self.last_protocol_error = Some(H2ProtocolError::connection(
                H2ErrorCode::ProtocolError,
                "client used an invalid request stream identifier",
            ));
            return Err(ServerError::InvalidFrame);
        }
        if self.sent_goaway_last_stream_id.is_some() {
            self.last_protocol_error = Some(H2ProtocolError::stream(
                stream_id,
                H2ErrorCode::RefusedStream,
                "new stream arrived after GOAWAY",
            ));
            return Err(ServerError::InvalidFrame);
        }
        if self.closed_streams.contains(&stream_id) {
            self.last_protocol_error = Some(H2ProtocolError::stream(
                stream_id,
                H2ErrorCode::StreamClosed,
                "HEADERS arrived after the stream closed",
            ));
            return Err(ServerError::InvalidFrame);
        }
        if stream_id <= self.max_peer_stream_id {
            self.last_protocol_error = Some(H2ProtocolError::connection(
                H2ErrorCode::ProtocolError,
                "client opened a stream identifier out of order",
            ));
            return Err(ServerError::InvalidFrame);
        }
        if self.active_stream_count() >= self.limits.max_active_streams {
            Ok(H2HeaderStreamDisposition::Refuse)
        } else {
            Ok(H2HeaderStreamDisposition::Accept)
        }
    }

    fn record_refused_stream(&mut self, stream_id: u32) {
        // RFC 9113 section 5.1.2 makes this a stream error so siblings survive.
        self.max_peer_stream_id = stream_id;
        self.remember_reset_tolerant_stream(stream_id);
        self.last_protocol_error = Some(H2ProtocolError::stream(
            stream_id,
            H2ErrorCode::RefusedStream,
            "peer exceeded SETTINGS_MAX_CONCURRENT_STREAMS",
        ));
    }

    fn event_from_decoded_header_fields<P>(
        &mut self,
        stream_id: u32,
        flags: u8,
        headers: Vec<H2HeaderField>,
    ) -> Result<H2ByteStreamEvent<P>, ServerError> {
        let end_stream = flags & 0x1 != 0;
        if self.request_streams.contains_key(&stream_id) {
            if !end_stream {
                return Err(ServerError::InvalidFrame);
            }
            let state = self
                .request_streams
                .get_mut(&stream_id)
                .ok_or(ServerError::InvalidFrame)?;
            state.accept_trailers(&headers, self.http_limits)?;
            state.finish()?;
            self.request_streams.remove(&stream_id);
            self.remember_closed_stream(stream_id);
            Ok(H2ByteStreamEvent::Trailers { stream_id, headers })
        } else {
            if self.sent_goaway_last_stream_id.is_some() {
                return Err(ServerError::InvalidFrame);
            }
            if stream_id.is_multiple_of(2)
                || stream_id <= self.max_peer_stream_id
                || self.closed_streams.contains(&stream_id)
                || self.active_stream_count() >= self.limits.max_active_streams
            {
                return Err(ServerError::InvalidFrame);
            }
            self.max_peer_stream_id = stream_id;
            self.response_active_streams.insert(
                stream_id,
                H2FlowControlWindow::new(self.settings.initial_window_size)
                    .expect("validated peer initial window"),
            );
            self.response_sent_data.insert(
                stream_id,
                H2SentBodyState {
                    sent: 0,
                    limit: Some(self.http_limits.max_body_bytes()),
                },
            );
            if !end_stream {
                self.request_streams.insert(
                    stream_id,
                    H2StreamState::new_raw(&headers, self.http_limits, self.request_body_limit)?,
                );
            } else {
                H2StreamState::new_raw(&headers, self.http_limits, self.request_body_limit)?
                    .finish()?;
                self.remember_closed_stream(stream_id);
            }
            Ok(H2ByteStreamEvent::RequestHeaders {
                stream_id,
                headers,
                end_stream,
            })
        }
    }

    /// Accepts fields decoded by an adapter-owned connection HPACK decoder.
    pub fn accept_external_header_fields(
        &mut self,
        stream_id: u32,
        flags: u8,
        headers: Vec<H2HeaderField>,
    ) -> Result<H2ByteStreamEvent, ServerError> {
        if self.header_codecs.is_some() || !self.settings_seen {
            return Err(ServerError::InvalidFrame);
        }
        let disposition = self.validate_header_stream_state(stream_id, flags)?;
        let role = if self.request_streams.contains_key(&stream_id) {
            H2HeaderValidationRole::Trailers
        } else {
            H2HeaderValidationRole::Request
        };
        self.validate_external_header_fields(stream_id, role, &headers)?;
        if let H2HeaderStreamDisposition::Refuse = disposition {
            self.record_refused_stream(stream_id);
            return Err(ServerError::InvalidFrame);
        }
        self.event_from_decoded_header_fields(stream_id, flags & 0x5, headers)
    }

    pub fn accept_complete_header_block_bytes(
        &mut self,
        stream_id: u32,
        flags: u8,
        block: &[u8],
    ) -> Result<H2ByteStreamEvent, ServerError> {
        if let Some(error) = self.terminal_protocol_error {
            return Err(error.into());
        }
        if !self.settings_seen {
            return Err(ServerError::InvalidFrame);
        }
        if block.len() > self.limits.max_encoded_header_block_size {
            let limit = self.limits.max_encoded_header_block_size;
            let actual = block.len();
            let error = encoded_header_limit_error(limit, actual);
            self.last_protocol_error = Some(error);
            self.terminal_protocol_error = Some(error);
            return Err(ServerError::HeaderTooLarge { limit, actual });
        }
        self.event_from_complete_headers_parts::<Vec<u8>>(stream_id, flags & 0x5, block, false)
    }

    pub fn accept_complete_header_block(
        &mut self,
        stream_id: u32,
        flags: u8,
        block: &[u8],
    ) -> Result<H2StreamEvent, ServerError> {
        self.accept_complete_header_block_bytes(stream_id, flags, block)?
            .try_into_text()
            .map_err(|_| ServerError::MalformedMessage)
    }

    pub fn validate_data_frame(
        &mut self,
        stream_id: u32,
        payload_len: usize,
        end_stream: bool,
    ) -> Result<(), ServerError> {
        if stream_id == 0 {
            self.last_protocol_error = Some(H2ProtocolError::connection(
                H2ErrorCode::ProtocolError,
                "invalid HTTP/2 frame",
            ));
            return Err(ServerError::InvalidFrame);
        }
        if self.closed_streams.contains(&stream_id) {
            if self.reset_tolerant_streams.contains(&stream_id) {
                return Ok(());
            }
            self.last_protocol_error = Some(H2ProtocolError::stream(
                stream_id,
                H2ErrorCode::StreamClosed,
                "DATA arrived after the stream closed",
            ));
            return Err(ServerError::InvalidFrame);
        }
        let Some(state) = self.request_streams.get_mut(&stream_id) else {
            self.last_protocol_error = Some(H2ProtocolError::connection(
                H2ErrorCode::ProtocolError,
                "DATA referenced an idle client stream",
            ));
            return Err(ServerError::InvalidFrame);
        };
        state.receive_data(payload_len, end_stream)?;
        if end_stream {
            self.request_streams.remove(&stream_id);
            self.remember_closed_stream(stream_id);
        }
        Ok(())
    }

    /// Abandons a server-side stream and records reset-tolerant closure.
    ///
    /// A retained tombstone surfaces later in-flight DATA as
    /// [`H2StreamEvent::DiscardedData`] so the owner can charge connection
    /// flow control. A zero tombstone limit retains no reset tolerance.
    pub fn close_stream(&mut self, stream_id: u32) {
        self.request_streams.remove(&stream_id);
        self.response_active_streams.remove(&stream_id);
        self.response_sent_data.remove(&stream_id);
        self.remember_reset_tolerant_stream(stream_id);
    }

    fn active_stream_count(&self) -> usize {
        let request_only = self
            .request_streams
            .keys()
            .filter(|stream_id| !self.response_active_streams.contains_key(stream_id))
            .count();
        self.response_active_streams
            .len()
            .saturating_add(request_only)
    }

    pub fn finish_response_stream(&mut self, stream_id: u32) {
        self.response_active_streams.remove(&stream_id);
        self.response_sent_data.remove(&stream_id);
        if !self.request_streams.contains_key(&stream_id) {
            self.remember_closed_stream(stream_id);
        }
    }

    fn remember_closed_stream(&mut self, stream_id: u32) {
        if self.closed_streams.insert(stream_id) {
            self.closed_stream_order.push_back(stream_id);
        }
        while self.closed_streams.len() > self.limits.max_closed_stream_tombstones {
            if let Some(expired) = self.closed_stream_order.pop_front() {
                self.closed_streams.remove(&expired);
                self.reset_tolerant_streams.remove(&expired);
            } else {
                break;
            }
        }
    }

    fn remember_reset_tolerant_stream(&mut self, stream_id: u32) {
        self.remember_closed_stream(stream_id);
        if self.closed_streams.contains(&stream_id) {
            self.reset_tolerant_streams.insert(stream_id);
        }
    }

    fn finish_compat_frame_error(
        &mut self,
        protocol_error: H2ProtocolError,
    ) -> Result<Vec<u8>, ServerError> {
        let original = self.last_compat_error.take();
        let recoverable = std::mem::take(&mut self.last_compat_error_recoverable);
        if recoverable && matches!(protocol_error.scope, H2ErrorScope::Stream(_)) {
            let compatibility = compatibility_error(original, protocol_error);
            return match self.handle_stream_error(protocol_error) {
                Ok(output) => {
                    self.handled_compat_error = Some(compatibility);
                    Ok(output)
                }
                Err(error) => {
                    self.reported_protocol_error =
                        Some(h2_error_from_server_error(error.clone(), None));
                    Err(error)
                }
            };
        }
        self.reported_protocol_error = Some(protocol_error);
        Err(compatibility_error(original, protocol_error))
    }

    fn handle_stream_error(
        &mut self,
        protocol_error: H2ProtocolError,
    ) -> Result<Vec<u8>, ServerError> {
        let H2ErrorScope::Stream(stream_id) = protocol_error.scope else {
            return Err(ServerError::InvalidFrame);
        };
        let output = self.rst_stream_frame_with_code(stream_id, protocol_error.code)?;
        if !stream_id.is_multiple_of(2) {
            self.max_peer_stream_id = self.max_peer_stream_id.max(stream_id);
        }
        self.close_stream(stream_id);
        self.handled_stream_error = Some(protocol_error);
        Ok(output)
    }

    fn h2_error_from_server_error(
        &mut self,
        error: ServerError,
        head: Option<H2FrameHead>,
    ) -> H2ProtocolError {
        if let Some(typed) = self.last_protocol_error.take() {
            return typed;
        }
        let unclassified_connection_error = matches!(
            error,
            ServerError::NeedMore
                | ServerError::InvalidFrame
                | ServerError::InvalidPreface
                | ServerError::PeerGoaway { .. }
        );
        let is_hpack_error = matches!(
            error,
            ServerError::InvalidHpack | ServerError::UnsupportedHpack
        );
        let mut typed = h2_error_from_server_error(error, head);
        if unclassified_connection_error {
            typed.scope = H2ErrorScope::Connection;
        }
        if is_hpack_error && let Some(hpack_error) = self.last_hpack_error.take() {
            typed = H2ProtocolError::hpack(typed.scope, hpack_error, typed.debug);
        }
        typed
    }

    fn enqueue_outbound_header_block(
        &mut self,
        stream_id: u32,
        fields: &[H2HeaderField],
        end_stream: bool,
        reserved_tail: usize,
        append_tail: impl FnOnce(&mut Vec<u8>),
    ) -> Result<H2OutboundCommit, H2ProtocolError> {
        enforce_h2_outbound_field_limits(
            stream_id,
            fields.len(),
            |index| fields[index].as_ref(),
            self.http_limits,
        )?;
        self.enqueue_outbound_header_block_by(
            stream_id,
            fields.len(),
            |index| fields[index].as_ref(),
            end_stream,
            reserved_tail,
            append_tail,
        )
    }

    fn enqueue_outbound_header_block_by<'a>(
        &mut self,
        stream_id: u32,
        field_count: usize,
        field_at: impl Fn(usize) -> H2RawHeaderRef<'a> + Copy,
        end_stream: bool,
        reserved_tail: usize,
        append_tail: impl FnOnce(&mut Vec<u8>),
    ) -> Result<H2OutboundCommit, H2ProtocolError> {
        if let Some(error) = self.terminal_protocol_error {
            return Err(error);
        }
        let commit = self
            .outbound_queue
            .reserve_commit()
            .map_err(outbound_hpack_error)?;
        let encoder = &mut self
            .header_codecs
            .as_mut()
            .ok_or_else(|| outbound_hpack_error(H2HpackError::StateOverflow))?
            .outbound;
        let mut output = encode_connection_header_frames_by(
            encoder,
            stream_id,
            field_count,
            field_at,
            end_stream,
            self.settings.max_frame_size,
            reserved_tail,
        )
        .map_err(outbound_hpack_error)?;
        append_tail(&mut output);
        self.outbound_queue.push_reserved(commit, output);
        Ok(commit)
    }

    pub fn response_frames(
        &mut self,
        stream_id: u32,
        status: u16,
        body: &[u8],
        end_stream: bool,
    ) -> Result<H2OutboundCommit, H2ProtocolError> {
        self.response_frames_with_headers(stream_id, status, &[], body, end_stream)
    }

    /// Assigns response HEADERS and DATA to the connection-owned outbound queue.
    ///
    /// Call [`Self::next_outbound_block`] to hand the complete transaction to
    /// the transport, then call [`Self::acknowledge_outbound_block`]. This
    /// commit-bearing return replaces the former freely reorderable `Vec<u8>`.
    pub fn response_frames_with_headers(
        &mut self,
        stream_id: u32,
        status: u16,
        headers: &[Header<'_>],
        body: &[u8],
        end_stream: bool,
    ) -> Result<H2OutboundCommit, H2ProtocolError> {
        let fields = headers
            .iter()
            .map(|header| H2HeaderField {
                name: header.name.as_bytes().to_vec(),
                value: header.value.as_bytes().to_vec(),
                sensitive: false,
            })
            .collect::<Vec<_>>();
        self.response_frames_with_raw_headers(stream_id, status, &fields, body, end_stream)
    }

    pub fn response_frames_with_raw_headers(
        &mut self,
        stream_id: u32,
        status: u16,
        headers: &[H2HeaderField],
        body: &[u8],
        end_stream: bool,
    ) -> Result<H2OutboundCommit, H2ProtocolError> {
        if body.len() > self.http_limits.max_body_bytes() {
            return Err(h2_body_limit_error(stream_id, self.http_limits, body.len()));
        }
        let mut status_storage = [0; 20];
        let status = decimal_bytes(usize::from(status), &mut status_storage);
        let mut content_length_storage = [0; 20];
        let content_length = decimal_bytes(body.len(), &mut content_length_storage);
        let field_count = headers
            .len()
            .checked_add(2)
            .ok_or_else(|| outbound_hpack_error(H2HpackError::AllocationFailed))?;
        enforce_h2_outbound_field_limits(
            stream_id,
            field_count,
            |index| match index {
                0 => H2RawHeaderRef::new(b":status", status),
                1 => H2RawHeaderRef::new(b"content-length", content_length),
                _ => headers[index - 2].as_ref(),
            },
            self.http_limits,
        )?;
        let max_frame_size = self.settings.max_frame_size;
        self.enqueue_outbound_header_block_by(
            stream_id,
            field_count,
            |index| match index {
                0 => H2RawHeaderRef::new(b":status", status),
                1 => H2RawHeaderRef::new(b"content-length", content_length),
                _ => headers[index - 2].as_ref(),
            },
            body.is_empty() && end_stream,
            data_frames_encoded_len(body.len(), max_frame_size),
            |output| {
                encode_data_frames(stream_id, body, end_stream, max_frame_size, output);
            },
        )
    }

    pub fn response_headers_frame(
        &mut self,
        stream_id: u32,
        status: u16,
        headers: &[Header<'_>],
        end_stream: bool,
    ) -> Result<H2OutboundCommit, H2ProtocolError> {
        let fields = headers
            .iter()
            .map(|header| H2HeaderField {
                name: header.name.as_bytes().to_vec(),
                value: header.value.as_bytes().to_vec(),
                sensitive: false,
            })
            .collect::<Vec<_>>();
        self.response_headers_frame_with_raw_headers(stream_id, status, &fields, end_stream)
    }

    pub fn response_headers_frame_with_raw_headers(
        &mut self,
        stream_id: u32,
        status: u16,
        headers: &[H2HeaderField],
        end_stream: bool,
    ) -> Result<H2OutboundCommit, H2ProtocolError> {
        let body_length = content_length_from_raw_headers(headers)
            .ok()
            .flatten()
            .unwrap_or(0);
        self.response_headers_frame_with_raw_headers_and_body_length(
            stream_id,
            status,
            headers,
            body_length,
            end_stream,
        )
    }

    /// Queues response headers after enforcing header and complete-body limits.
    pub fn response_headers_frame_with_raw_headers_and_body_length(
        &mut self,
        stream_id: u32,
        status: u16,
        headers: &[H2HeaderField],
        body_length: usize,
        end_stream: bool,
    ) -> Result<H2OutboundCommit, H2ProtocolError> {
        if body_length > self.http_limits.max_body_bytes() {
            return Err(h2_body_limit_error(
                stream_id,
                self.http_limits,
                body_length,
            ));
        }
        let mut status_storage = [0; 20];
        let status = decimal_bytes(usize::from(status), &mut status_storage);
        let field_count = headers
            .len()
            .checked_add(1)
            .ok_or_else(|| outbound_hpack_error(H2HpackError::AllocationFailed))?;
        enforce_h2_outbound_field_limits(
            stream_id,
            field_count,
            |index| {
                if index == 0 {
                    H2RawHeaderRef::new(b":status", status)
                } else {
                    headers[index - 1].as_ref()
                }
            },
            self.http_limits,
        )?;
        self.enqueue_outbound_header_block_by(
            stream_id,
            field_count,
            |index| {
                if index == 0 {
                    H2RawHeaderRef::new(b":status", status)
                } else {
                    headers[index - 1].as_ref()
                }
            },
            end_stream,
            0,
            |_| {},
        )
    }

    /// Reports peer-advertised DATA capacity for an active response stream.
    pub fn send_capacity(
        &self,
        stream_id: u32,
        pending_bytes: usize,
    ) -> Result<H2SendCapacity, ServerError> {
        let stream_window = self
            .response_active_streams
            .get(&stream_id)
            .ok_or(ServerError::FlowControlViolation)?
            .available();
        Ok(H2SendCapacity::new(
            stream_id,
            pending_bytes,
            stream_window,
            usize::try_from(self.send_connection_window.available()).unwrap_or(0),
        ))
    }

    /// Plans one DATA frame without copying its payload or mutating send state.
    pub fn prepare_data_frame(
        &self,
        stream_id: u32,
        pending_bytes: usize,
        end_stream: bool,
    ) -> Result<Option<H2DataFramePlan>, ServerError> {
        self.prepare_data_frame_inner(stream_id, pending_bytes, end_stream, false)
    }

    pub(crate) fn prepare_data_frame_before_trailers(
        &self,
        stream_id: u32,
        pending_bytes: usize,
    ) -> Result<Option<H2DataFramePlan>, ServerError> {
        self.prepare_data_frame_inner(stream_id, pending_bytes, false, true)
    }

    fn prepare_data_frame_inner(
        &self,
        stream_id: u32,
        pending_bytes: usize,
        end_stream: bool,
        allow_empty_nonterminal: bool,
    ) -> Result<Option<H2DataFramePlan>, ServerError> {
        let sent = self
            .response_sent_data
            .get(&stream_id)
            .copied()
            .ok_or(ServerError::FlowControlViolation)?;
        let actual = sent.sent.saturating_add(pending_bytes);
        enforce_optional_body_size(actual, sent.limit)?;
        let capacity = self.send_capacity(stream_id, pending_bytes)?;
        if pending_bytes != 0 && capacity.sendable_bytes == 0 {
            return Ok(None);
        }
        if pending_bytes == 0 && !end_stream && !allow_empty_nonterminal {
            return Ok(None);
        }
        let payload_len = capacity.sendable_bytes.min(self.settings.max_frame_size);
        Ok(Some(h2_data_frame_plan(
            stream_id,
            payload_len,
            end_stream && payload_len == pending_bytes,
        )))
    }

    /// Commits a successfully handed-off DATA frame to flow-control and stream state.
    pub fn commit_data_frame(&mut self, plan: H2DataFramePlan) -> Result<(), ServerError> {
        let capacity = self.send_capacity(plan.stream_id, plan.payload_len)?;
        if capacity.sendable_bytes != plan.payload_len
            || plan.payload_len > self.settings.max_frame_size
        {
            return Err(ServerError::FlowControlViolation);
        }
        let stream_window = self
            .response_active_streams
            .get_mut(&plan.stream_id)
            .ok_or(ServerError::FlowControlViolation)?;
        stream_window.consume(plan.payload_len)?;
        self.send_connection_window.consume(plan.payload_len)?;
        let sent = self
            .response_sent_data
            .get_mut(&plan.stream_id)
            .ok_or(ServerError::FlowControlViolation)?;
        sent.sent = sent.sent.saturating_add(plan.payload_len);
        enforce_optional_body_size(sent.sent, sent.limit)?;
        if plan.end_stream {
            self.finish_response_stream(plan.stream_id);
        }
        Ok(())
    }

    pub fn data_frame(&self, stream_id: u32, payload: &[u8], end_stream: bool) -> Vec<u8> {
        let mut output = Vec::new();
        let mut remaining = payload;
        while !remaining.is_empty() {
            let frame_len = remaining.len().min(self.settings.max_frame_size);
            let (chunk, rest) = remaining.split_at(frame_len);
            remaining = rest;
            H2Frame::encode_header(
                H2FrameType::Data,
                if end_stream && remaining.is_empty() {
                    0x1
                } else {
                    0
                },
                stream_id,
                chunk.len(),
                &mut output,
            );
            output.extend_from_slice(chunk);
        }
        if payload.is_empty() && end_stream {
            H2Frame {
                frame_type: H2FrameType::Data,
                flags: 0x1,
                stream_id,
                payload: Vec::new(),
            }
            .encode(&mut output);
        }
        output
    }

    pub fn trailers_frame(
        &mut self,
        stream_id: u32,
        headers: &[Header<'_>],
    ) -> Result<H2OutboundCommit, H2ProtocolError> {
        let fields = headers
            .iter()
            .map(|header| H2HeaderField {
                name: header.name.as_bytes().to_vec(),
                value: header.value.as_bytes().to_vec(),
                sensitive: false,
            })
            .collect::<Vec<_>>();
        self.trailers_frame_with_raw_headers(stream_id, &fields)
    }

    pub fn trailers_frame_with_raw_headers(
        &mut self,
        stream_id: u32,
        headers: &[H2HeaderField],
    ) -> Result<H2OutboundCommit, H2ProtocolError> {
        enforce_h2_outbound_field_limits(
            stream_id,
            headers.len(),
            |index| headers[index].as_ref(),
            self.http_limits,
        )?;
        self.enqueue_outbound_header_block(stream_id, headers, true, 0, |_| {})
    }

    fn decode_h2_header_fields(
        &mut self,
        stream_id: u32,
        block: &[u8],
        role: H2HeaderValidationRole,
    ) -> Result<Vec<H2HeaderField>, ServerError> {
        let headers = decode_connection_header_fields(
            &mut self
                .header_codecs
                .as_mut()
                .ok_or(ServerError::InvalidFrame)?
                .inbound,
            block,
            self.limits.max_header_list_size,
            stream_id,
            role,
            H2ConnectionHpackErrorState {
                last_hpack_error: &mut self.last_hpack_error,
                last_protocol_error: &mut self.last_protocol_error,
                terminal_protocol_error: &mut self.terminal_protocol_error,
            },
        )?;
        enforce_h2_field_limits(&headers, role, self.http_limits)?;
        Ok(headers)
    }

    fn validate_external_header_fields(
        &mut self,
        stream_id: u32,
        role: H2HeaderValidationRole,
        headers: &[H2HeaderField],
    ) -> Result<(), ServerError> {
        if let Err(validation) = validate_decoded_header_fields(headers, role) {
            self.last_protocol_error = Some(H2ProtocolError::stream(
                stream_id,
                H2ErrorCode::ProtocolError,
                "HTTP/2 header field validation failed",
            ));
            return Err(validation.into());
        }
        enforce_h2_field_limits(headers, role, self.http_limits)
    }

    pub fn discard_hpack_block(&mut self, block: &[u8]) -> Result<(), ServerError> {
        if let Some(error) = self.terminal_protocol_error {
            return Err(error.into());
        }
        if block.len() > self.limits.max_encoded_header_block_size {
            let limit = self.limits.max_encoded_header_block_size;
            let actual = block.len();
            let error = encoded_header_limit_error(limit, actual);
            self.last_protocol_error = Some(error);
            self.terminal_protocol_error = Some(error);
            return Err(ServerError::HeaderTooLarge { limit, actual });
        }
        let Some(codecs) = self.header_codecs.as_mut() else {
            return Err(ServerError::InvalidFrame);
        };
        match codecs
            .inbound
            .decode(block, self.limits.max_header_list_size)
        {
            Ok(_) => Ok(()),
            Err(crate::hpack::Error::HeaderListTooLarge { actual }) => {
                let limit = self.limits.max_header_list_size;
                self.last_protocol_error = Some(decoded_header_limit_error(
                    H2ErrorScope::Connection,
                    limit,
                    actual,
                ));
                Err(ServerError::HeaderTooLarge { limit, actual })
            }
            Err(crate::hpack::Error::AllocationFailed) => {
                let error = allocation_terminal_error();
                self.last_protocol_error = Some(error);
                self.terminal_protocol_error = Some(error);
                Err(ServerError::InvalidFrame)
            }
            Err(error) => {
                self.last_hpack_error = Some(error.into());
                self.terminal_protocol_error = Some(poisoned_connection_error());
                Err(ServerError::InvalidHpack)
            }
        }
    }

    fn apply_settings(
        &mut self,
        payload: &[u8],
    ) -> Result<Option<H2InitialWindowSizeChange>, ServerError> {
        let previous = self.settings.initial_window_size;
        let decoded =
            H2Settings::decode_payload_with_limit(payload, self.limits.max_settings_entries)?;
        if decoded.iter().any(|setting| {
            setting.id == H2SettingId::InitialWindowSize && setting.value > H2_MAX_WINDOW_SIZE
        }) {
            self.last_protocol_error = Some(H2ProtocolError::connection(
                H2ErrorCode::FlowControlError,
                "SETTINGS_INITIAL_WINDOW_SIZE exceeds the HTTP/2 window maximum",
            ));
            return Err(ServerError::InvalidFrame);
        }
        let mut settings = self.settings;
        settings.apply_all(&decoded)?;
        for setting in &decoded {
            if setting.id == H2SettingId::HeaderTableSize
                && let Some(codecs) = self.header_codecs.as_mut()
            {
                codecs.outbound.set_max_table_size(setting.value as usize);
            }
        }
        let current = settings.initial_window_size;
        if previous != current {
            let delta = i64::from(current) - i64::from(previous);
            let delta = i32::try_from(delta).map_err(|_| ServerError::FlowControlViolation)?;
            for window in self.response_active_streams.values() {
                let mut adjusted = *window;
                if adjusted.adjust(delta).is_err() {
                    self.last_protocol_error = Some(H2ProtocolError::connection(
                        H2ErrorCode::FlowControlError,
                        "SETTINGS_INITIAL_WINDOW_SIZE overflowed an active send window",
                    ));
                    return Err(ServerError::FlowControlViolation);
                }
            }
            for window in self.response_active_streams.values_mut() {
                window.adjust(delta)?;
            }
        }
        self.settings = settings;
        Ok((previous != current).then_some(H2InitialWindowSizeChange { previous, current }))
    }

    fn encode_local_settings(&self, output: &mut Vec<u8>) {
        let mut payload = Vec::new();
        let mut settings = Vec::new();
        if self.limits.max_header_table_size != H2Settings::default().header_table_size as usize {
            settings.push(H2Setting::new(
                H2SettingId::HeaderTableSize,
                self.limits
                    .max_header_table_size
                    .min(crate::hpack::MAX_TABLE_SIZE) as u32,
            ));
        }
        if self.local_initial_window_size != H2Settings::default().initial_window_size {
            settings.push(H2Setting::new(
                H2SettingId::InitialWindowSize,
                self.local_initial_window_size,
            ));
        }
        if self.limits.max_active_streams != usize::MAX {
            settings.push(H2Setting::new(
                H2SettingId::MaxConcurrentStreams,
                self.limits.max_active_streams.min(u32::MAX as usize) as u32,
            ));
        }
        if self.limits.max_header_list_size != usize::MAX {
            settings.push(H2Setting::new(
                H2SettingId::MaxHeaderListSize,
                self.limits.max_header_list_size.min(u32::MAX as usize) as u32,
            ));
        }
        H2Settings::encode_payload(&settings, &mut payload);
        H2Frame {
            frame_type: H2FrameType::Settings,
            flags: 0,
            stream_id: 0,
            payload,
        }
        .encode(output);
    }

    fn encode_local_connection_window_update(&self, output: &mut Vec<u8>) {
        let default = H2Settings::default().initial_window_size;
        let Some(increment) = self.local_connection_window_size.checked_sub(default) else {
            return;
        };
        if increment == 0 {
            return;
        }
        H2Frame {
            frame_type: H2FrameType::WindowUpdate,
            flags: 0,
            stream_id: 0,
            payload: increment.to_be_bytes().to_vec(),
        }
        .encode(output);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct H2ClientOpenStream {
    send_window: H2FlowControlWindow,
    local_end_stream: bool,
    request_is_head: bool,
    sent_data_len: usize,
    request_body_limit: Option<usize>,
    response_body_limit: Option<usize>,
}

pub struct H2Client {
    next_stream_id: u32,
    preface_sent: bool,
    settings: H2Settings,
    local_initial_window_size: u32,
    local_connection_window_size: u32,
    settings_seen: bool,
    header_codecs: Option<H2ConnectionCodecs>,
    outbound_queue: Box<H2OutboundQueue>,
    response_streams: HashMap<u32, H2StreamState>,
    open_streams: HashMap<u32, H2ClientOpenStream>,
    send_connection_window: H2FlowControlWindow,
    closed_streams: HashSet<u32>,
    reset_tolerant_streams: HashSet<u32>,
    closed_stream_order: VecDeque<u32>,
    control_budget: H2ControlFrameBudget,
    limits: H2Limits,
    http_limits: HttpLimits,
    header_block: H2HeaderBlockAssembler,
    last_hpack_error: Option<H2HpackError>,
    last_protocol_error: Option<H2ProtocolError>,
    last_compat_error: Option<ServerError>,
    terminal_protocol_error: Option<H2ProtocolError>,
    local_settings_state: H2SettingsSyncState,
    received_goaway_last_stream_id: Option<u32>,
    sent_goaway_last_stream_id: Option<u32>,
    control_diagnostics: H2ControlDiagnostics,
}

impl Default for H2Client {
    fn default() -> Self {
        Self {
            next_stream_id: 1,
            preface_sent: false,
            settings: H2Settings::default(),
            local_initial_window_size: H2Settings::default().initial_window_size,
            local_connection_window_size: H2Settings::default().initial_window_size,
            settings_seen: false,
            header_codecs: Some(H2ConnectionCodecs::new()),
            outbound_queue: Box::default(),
            response_streams: HashMap::new(),
            open_streams: HashMap::new(),
            send_connection_window: H2FlowControlWindow::new(
                H2Settings::default().initial_window_size,
            )
            .expect("the default HTTP/2 window is valid"),
            closed_streams: HashSet::new(),
            reset_tolerant_streams: HashSet::new(),
            closed_stream_order: VecDeque::new(),
            control_budget: H2ControlFrameBudget::default(),
            limits: H2Limits::default(),
            http_limits: HttpLimits::default(),
            header_block: H2HeaderBlockAssembler::default(),
            last_hpack_error: None,
            last_protocol_error: None,
            last_compat_error: None,
            terminal_protocol_error: None,
            local_settings_state: H2SettingsSyncState::Synced,
            received_goaway_last_stream_id: None,
            sent_goaway_last_stream_id: None,
            control_diagnostics: H2ControlDiagnostics::default(),
        }
    }
}

impl H2Client {
    /// Creates protocol state for an adapter that owns the connection HPACK pair.
    ///
    /// Header blocks must be decoded by the adapter and supplied through
    /// [`Self::accept_external_header_fields`].
    pub fn for_external_hpack_adapter() -> Self {
        Self {
            header_codecs: None,
            ..Self::default()
        }
    }

    pub fn with_local_flow_control(
        initial_stream_window: u32,
        initial_connection_window: u32,
    ) -> Result<Self, ServerError> {
        Self::with_local_flow_control_and_limits(
            initial_stream_window,
            initial_connection_window,
            H2Limits::default(),
        )
    }

    pub fn with_local_flow_control_and_limits(
        initial_stream_window: u32,
        initial_connection_window: u32,
        limits: H2Limits,
    ) -> Result<Self, ServerError> {
        let http_limits = HttpLimits::new()
            .set_max_header_bytes(limits.max_header_list_size)
            .set_max_active_streams(limits.max_active_streams)
            .set_max_body_bytes(limits.max_queued_data_bytes);
        Self::with_local_flow_control_and_http_limits(
            initial_stream_window,
            initial_connection_window,
            limits,
            http_limits,
        )
    }

    /// Creates protocol state with explicit HTTP and HTTP/2 resource limits.
    pub fn with_local_flow_control_and_http_limits(
        initial_stream_window: u32,
        initial_connection_window: u32,
        limits: H2Limits,
        http_limits: HttpLimits,
    ) -> Result<Self, ServerError> {
        validate_h2_limits(limits)?;
        let mut settings = H2Settings::default();
        settings.apply(H2Setting::new(
            H2SettingId::InitialWindowSize,
            initial_stream_window,
        ))?;
        if initial_connection_window < H2Settings::default().initial_window_size {
            return Err(ServerError::InvalidFrame);
        }
        let mut client = Self {
            local_initial_window_size: initial_stream_window,
            local_connection_window_size: initial_connection_window,
            limits,
            http_limits,
            ..Self::default()
        };
        client
            .header_codecs
            .as_mut()
            .expect("default client owns HPACK codecs")
            .inbound
            .set_max_allowed_table_size(limits.max_header_table_size);
        Ok(client)
    }

    pub fn with_limits(limits: H2Limits) -> Result<Self, ServerError> {
        Self::with_local_flow_control_and_limits(
            H2Settings::default().initial_window_size,
            H2Settings::default().initial_window_size,
            limits,
        )
    }

    pub const fn settings(&self) -> &H2Settings {
        &self.settings
    }

    pub fn inbound_hpack_diagnostics(&self) -> H2HpackDiagnosticsSnapshot {
        self.header_codecs
            .as_ref()
            .map_or_else(H2HpackDiagnosticsSnapshot::default, |codecs| {
                codecs.inbound.diagnostics().into()
            })
    }

    pub fn outbound_hpack_diagnostics(&self) -> H2HpackDiagnosticsSnapshot {
        self.header_codecs
            .as_ref()
            .map_or_else(H2HpackDiagnosticsSnapshot::default, |codecs| {
                codecs.outbound.diagnostics().into()
            })
    }

    /// Returns the next complete outbound transaction in connection wire order.
    pub fn next_outbound_block(&self) -> Option<H2OutboundBlockRef<'_>> {
        self.outbound_queue.front()
    }

    /// Acknowledges that the next complete outbound transaction was handed to the transport.
    pub fn acknowledge_outbound_block(
        &mut self,
        commit: H2OutboundCommit,
    ) -> Result<(), ServerError> {
        self.outbound_queue.acknowledge(commit)
    }

    /// Begins a reversible raw header-block transaction.
    pub fn prepare_outbound_header_block<'connection, 'headers>(
        &'connection mut self,
        stream_id: u32,
        headers: &'headers [H2HeaderField],
        end_stream: bool,
    ) -> Result<H2OutboundHeaderBlock<'connection, 'headers>, H2ProtocolError> {
        if let Some(error) = self.terminal_protocol_error {
            return Err(error);
        }
        Ok(H2OutboundHeaderBlock {
            target: H2OutboundHeaderBlockTarget::Client(self),
            stream_id,
            headers,
            end_stream,
        })
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn set_inbound_allocation_failure_after_for_testing(
        &mut self,
        successful_allocations: Option<usize>,
    ) {
        self.header_codecs
            .as_mut()
            .expect("test connection owns HPACK codecs")
            .inbound
            .set_allocation_failure_after(successful_allocations);
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn set_outbound_allocation_failure_after_for_testing(
        &mut self,
        successful_allocations: Option<usize>,
    ) {
        self.outbound_queue
            .set_allocation_failure_after(successful_allocations);
        self.header_codecs
            .as_mut()
            .expect("test connection owns HPACK codecs")
            .outbound
            .set_allocation_failure_after(successful_allocations);
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn set_assembly_allocation_failure_after_for_testing(
        &mut self,
        successful_allocations: Option<usize>,
    ) {
        self.header_block
            .set_allocation_failure_after(successful_allocations);
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn set_pending_encoded_header_block_len_for_testing(&mut self, encoded_len: usize) {
        self.header_block.set_pending_encoded_len(encoded_len);
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn inbound_table_for_testing(&self) -> crate::hpack::TestTableSnapshot {
        self.header_codecs
            .as_ref()
            .expect("test connection owns HPACK codecs")
            .inbound
            .test_table_snapshot()
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn outbound_table_for_testing(&self) -> crate::hpack::TestTableSnapshot {
        self.header_codecs
            .as_ref()
            .expect("test connection owns HPACK codecs")
            .outbound
            .test_table_snapshot()
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn saturate_hpack_diagnostics_for_testing(&mut self) {
        let diagnostics = crate::hpack::Diagnostics {
            encoded_blocks: u64::MAX,
            decoded_blocks: u64::MAX,
            indexed_fields: u64::MAX,
            incremental_fields: u64::MAX,
            without_indexing_fields: u64::MAX,
            never_indexed_fields: u64::MAX,
            huffman_strings: u64::MAX,
            plain_strings: u64::MAX,
            table_size_updates: u64::MAX,
            table_insertions: u64::MAX,
            table_evictions: u64::MAX,
            compression_errors: u64::MAX,
            header_list_too_large: u64::MAX,
            field_bytes: u64::MAX,
            wire_bytes: u64::MAX,
        };
        let codecs = self
            .header_codecs
            .as_mut()
            .expect("test connection owns HPACK codecs");
        codecs.inbound.set_diagnostics_for_testing(diagnostics);
        codecs.outbound.set_diagnostics_for_testing(diagnostics);
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn terminal_for_testing(&self) -> bool {
        self.terminal_protocol_error.is_some()
    }

    pub fn record_progress_frame(&mut self) {
        self.control_budget
            .refill_after_meaningful_progress(Instant::now());
    }

    fn record_control(&mut self, frame_type: H2FrameType) -> Result<(), ServerError> {
        match self.control_budget.record_control(frame_type) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.control_diagnostics.record_rejection(frame_type);
                Err(error)
            }
        }
    }

    fn apply_send_window_update(
        &mut self,
        stream_id: u32,
        increment: u32,
    ) -> Result<(), ServerError> {
        if stream_id == 0 {
            return self.send_connection_window.increase(increment);
        }
        if let Some(stream) = self.open_streams.get_mut(&stream_id) {
            return stream.send_window.increase(increment);
        }
        if self.closed_streams.contains(&stream_id) {
            return Ok(());
        }
        Err(ServerError::InvalidFrame)
    }

    pub const fn control_diagnostics(&self) -> H2ControlDiagnostics {
        self.control_diagnostics
    }

    pub const fn timer_intent(&self) -> Option<H2TimerIntent> {
        match self.local_settings_state {
            H2SettingsSyncState::WaitingAck => Some(H2TimerIntent::SettingsAckTimeout),
            H2SettingsSyncState::Synced => None,
        }
    }

    pub fn shutdown_intent(&self) -> H2ShutdownIntent {
        match self.sent_goaway_last_stream_id {
            Some(last_stream_id) if !self.open_streams.is_empty() => {
                H2ShutdownIntent::Drain { last_stream_id }
            }
            Some(_) => H2ShutdownIntent::Close,
            None => H2ShutdownIntent::None,
        }
    }

    pub fn goaway_frame(
        &mut self,
        last_stream_id: u32,
        error_code: u32,
    ) -> Result<Vec<u8>, ServerError> {
        if let Some(previous) = self.sent_goaway_last_stream_id
            && last_stream_id > previous
        {
            return Err(ServerError::InvalidFrame);
        }
        self.sent_goaway_last_stream_id = Some(last_stream_id);
        self.control_diagnostics.goaways = self.control_diagnostics.goaways.saturating_add(1);
        let mut output = Vec::new();
        H2Frame {
            frame_type: H2FrameType::Goaway,
            flags: 0,
            stream_id: 0,
            payload: [
                (last_stream_id & 0x7fff_ffff).to_be_bytes(),
                error_code.to_be_bytes(),
            ]
            .concat(),
        }
        .encode(&mut output);
        Ok(output)
    }

    pub fn connection_preface(&mut self) -> Vec<u8> {
        if self.preface_sent {
            return Vec::new();
        }
        self.preface_sent = true;
        let mut output = Vec::new();
        output.extend_from_slice(CLIENT_PREFACE);
        self.encode_local_settings(&mut output);
        self.local_settings_state = H2SettingsSyncState::WaitingAck;
        self.encode_local_connection_window_update(&mut output);
        output
    }

    fn enqueue_outbound_header_block(
        &mut self,
        stream_id: u32,
        fields: &[H2HeaderField],
        end_stream: bool,
        reserved_tail: usize,
        append_tail: impl FnOnce(&mut Vec<u8>),
    ) -> Result<H2OutboundCommit, H2ProtocolError> {
        enforce_h2_outbound_field_limits(
            stream_id,
            fields.len(),
            |index| fields[index].as_ref(),
            self.http_limits,
        )?;
        self.enqueue_outbound_header_block_by(
            stream_id,
            fields.len(),
            |index| fields[index].as_ref(),
            end_stream,
            reserved_tail,
            append_tail,
        )
    }

    fn enqueue_outbound_header_block_by<'a>(
        &mut self,
        stream_id: u32,
        field_count: usize,
        field_at: impl Fn(usize) -> H2RawHeaderRef<'a> + Copy,
        end_stream: bool,
        reserved_tail: usize,
        append_tail: impl FnOnce(&mut Vec<u8>),
    ) -> Result<H2OutboundCommit, H2ProtocolError> {
        if let Some(error) = self.terminal_protocol_error {
            return Err(error);
        }
        let commit = self
            .outbound_queue
            .reserve_commit()
            .map_err(outbound_hpack_error)?;
        let encoder = &mut self
            .header_codecs
            .as_mut()
            .ok_or_else(|| outbound_hpack_error(H2HpackError::StateOverflow))?
            .outbound;
        let mut output = encode_connection_header_frames_by(
            encoder,
            stream_id,
            field_count,
            field_at,
            end_stream,
            self.settings.max_frame_size,
            reserved_tail,
        )
        .map_err(outbound_hpack_error)?;
        append_tail(&mut output);
        self.outbound_queue.push_reserved(commit, output);
        Ok(commit)
    }

    pub fn open_stream(
        &mut self,
        method: &str,
        scheme: &str,
        authority: &str,
        path: &str,
        headers: &[Header<'_>],
        end_stream: bool,
    ) -> Result<(u32, H2OutboundCommit), ServerError> {
        let fields = headers
            .iter()
            .map(|header| H2HeaderField {
                name: header.name.as_bytes().to_vec(),
                value: header.value.as_bytes().to_vec(),
                sensitive: false,
            })
            .collect::<Vec<_>>();
        self.open_stream_with_raw_headers(method, scheme, authority, path, &fields, end_stream)
            .map_err(ServerError::from)
    }

    pub fn open_stream_with_raw_headers(
        &mut self,
        method: &str,
        scheme: &str,
        authority: &str,
        path: &str,
        headers: &[H2HeaderField],
        end_stream: bool,
    ) -> Result<(u32, H2OutboundCommit), H2ProtocolError> {
        let stream_id = self
            .next_stream_for_open()
            .map_err(|_| projection_error(0))?;
        self.open_streams
            .try_reserve(1)
            .map_err(|_| outbound_hpack_error(H2HpackError::AllocationFailed))?;
        let field_count = headers
            .len()
            .checked_add(4)
            .ok_or_else(|| outbound_hpack_error(H2HpackError::AllocationFailed))?;
        enforce_h2_outbound_field_limits(
            stream_id,
            field_count,
            |index| match index {
                0 => H2RawHeaderRef::new(b":method", method.as_bytes()),
                1 => H2RawHeaderRef::new(b":scheme", scheme.as_bytes()),
                2 => H2RawHeaderRef::new(b":authority", authority.as_bytes()),
                3 => H2RawHeaderRef::new(b":path", path.as_bytes()),
                _ => headers[index - 4].as_ref(),
            },
            self.http_limits,
        )?;
        let body_length = content_length_from_raw_headers(headers)
            .ok()
            .flatten()
            .unwrap_or(0);
        if body_length > self.http_limits.max_body_bytes() {
            return Err(h2_body_limit_error(
                stream_id,
                self.http_limits,
                body_length,
            ));
        }
        let commit = self.enqueue_outbound_header_block_by(
            stream_id,
            field_count,
            |index| match index {
                0 => H2RawHeaderRef::new(b":method", method.as_bytes()),
                1 => H2RawHeaderRef::new(b":scheme", scheme.as_bytes()),
                2 => H2RawHeaderRef::new(b":authority", authority.as_bytes()),
                3 => H2RawHeaderRef::new(b":path", path.as_bytes()),
                _ => headers[index - 4].as_ref(),
            },
            end_stream,
            0,
            |_| {},
        )?;
        self.next_stream_id = self
            .next_stream_id
            .checked_add(2)
            .expect("validated stream ID has a successor");
        self.open_streams.insert(
            stream_id,
            H2ClientOpenStream {
                send_window: H2FlowControlWindow::new(self.settings.initial_window_size)
                    .expect("validated peer initial window"),
                local_end_stream: end_stream,
                request_is_head: method.eq_ignore_ascii_case("HEAD"),
                sent_data_len: 0,
                request_body_limit: Some(self.http_limits.max_body_bytes()),
                response_body_limit: Some(self.http_limits.max_body_bytes()),
            },
        );
        Ok((stream_id, commit))
    }

    pub fn reserve_stream(&mut self) -> Result<u32, ServerError> {
        let stream_id = self.next_stream_for_open()?;
        self.next_stream_id = self
            .next_stream_id
            .checked_add(2)
            .ok_or(ServerError::InvalidFrame)?;
        self.open_streams.insert(
            stream_id,
            H2ClientOpenStream {
                send_window: H2FlowControlWindow::new(self.settings.initial_window_size)
                    .expect("validated peer initial window"),
                local_end_stream: false,
                request_is_head: false,
                sent_data_len: 0,
                request_body_limit: Some(self.http_limits.max_body_bytes()),
                response_body_limit: Some(self.http_limits.max_body_bytes()),
            },
        );
        Ok(stream_id)
    }

    fn next_stream_for_open(&self) -> Result<u32, ServerError> {
        let stream_id = self.next_stream_id;
        if stream_id == 0 || stream_id > 0x7fff_ffff {
            return Err(ServerError::InvalidFrame);
        }
        if self.received_goaway_last_stream_id.is_some() {
            return Err(ServerError::InvalidFrame);
        }
        let active_stream_limit =
            (self.settings.max_concurrent_streams as usize).min(self.limits.max_active_streams);
        if self.open_streams.len() >= active_stream_limit {
            return Err(ServerError::InvalidFrame);
        }
        Ok(stream_id)
    }

    pub(crate) fn active_stream_limit(&self) -> usize {
        (self.settings.max_concurrent_streams as usize).min(self.limits.max_active_streams)
    }

    pub(crate) fn can_open_stream(&self) -> bool {
        self.next_stream_for_open().is_ok()
    }

    pub(crate) fn reset_stream(
        &mut self,
        stream_id: u32,
        error_code: H2ErrorCode,
    ) -> Result<Vec<u8>, ServerError> {
        if stream_id == 0 || stream_id > 0x7fff_ffff {
            return Err(ServerError::InvalidFrame);
        }
        let mut output = Vec::new();
        H2Frame {
            frame_type: H2FrameType::RstStream,
            flags: 0,
            stream_id,
            payload: error_code.as_u32().to_be_bytes().to_vec(),
        }
        .encode(&mut output);
        self.close_stream(stream_id);
        Ok(output)
    }

    pub(crate) fn stream_body(
        &mut self,
        stream_id: u32,
        request: bool,
        response: bool,
    ) -> Result<(), ServerError> {
        let stream = self
            .open_streams
            .get_mut(&stream_id)
            .ok_or(ServerError::InvalidOutboundState)?;
        if request {
            stream.request_body_limit = None;
        }
        if response {
            stream.response_body_limit = None;
        }
        Ok(())
    }

    /// Reports peer-advertised DATA capacity for an open request stream.
    pub fn send_capacity(
        &self,
        stream_id: u32,
        pending_bytes: usize,
    ) -> Result<H2SendCapacity, ServerError> {
        let stream = self
            .open_streams
            .get(&stream_id)
            .ok_or(ServerError::FlowControlViolation)?;
        if stream.local_end_stream {
            return Err(ServerError::FlowControlViolation);
        }
        Ok(H2SendCapacity::new(
            stream_id,
            pending_bytes,
            stream.send_window.available(),
            usize::try_from(self.send_connection_window.available()).unwrap_or(0),
        ))
    }

    /// Plans one DATA frame without copying its payload or mutating send state.
    pub fn prepare_data_frame(
        &self,
        stream_id: u32,
        pending_bytes: usize,
        end_stream: bool,
    ) -> Result<Option<H2DataFramePlan>, ServerError> {
        let stream = self
            .open_streams
            .get(&stream_id)
            .ok_or(ServerError::FlowControlViolation)?;
        let actual = stream.sent_data_len.saturating_add(pending_bytes);
        enforce_optional_body_size(actual, stream.request_body_limit)?;
        let capacity = self.send_capacity(stream_id, pending_bytes)?;
        if pending_bytes != 0 && capacity.sendable_bytes == 0 {
            return Ok(None);
        }
        if pending_bytes == 0 && !end_stream {
            return Ok(None);
        }
        let payload_len = capacity.sendable_bytes.min(self.settings.max_frame_size);
        Ok(Some(h2_data_frame_plan(
            stream_id,
            payload_len,
            end_stream && payload_len == pending_bytes,
        )))
    }

    /// Commits a successfully handed-off DATA frame to flow-control and stream state.
    pub fn commit_data_frame(&mut self, plan: H2DataFramePlan) -> Result<(), ServerError> {
        let capacity = self.send_capacity(plan.stream_id, plan.payload_len)?;
        if capacity.sendable_bytes != plan.payload_len
            || plan.payload_len > self.settings.max_frame_size
        {
            return Err(ServerError::FlowControlViolation);
        }
        let stream = self
            .open_streams
            .get_mut(&plan.stream_id)
            .ok_or(ServerError::FlowControlViolation)?;
        stream.send_window.consume(plan.payload_len)?;
        stream.sent_data_len = stream.sent_data_len.saturating_add(plan.payload_len);
        enforce_optional_body_size(stream.sent_data_len, stream.request_body_limit)?;
        self.send_connection_window.consume(plan.payload_len)?;
        if plan.end_stream {
            stream.local_end_stream = true;
        }
        Ok(())
    }

    pub fn data_frame(&self, stream_id: u32, payload: &[u8], end_stream: bool) -> Vec<u8> {
        let mut output = Vec::new();
        let mut remaining = payload;
        while !remaining.is_empty() {
            let frame_len = remaining.len().min(self.settings.max_frame_size);
            let (chunk, rest) = remaining.split_at(frame_len);
            remaining = rest;
            H2Frame::encode_header(
                H2FrameType::Data,
                if end_stream && remaining.is_empty() {
                    0x1
                } else {
                    0
                },
                stream_id,
                chunk.len(),
                &mut output,
            );
            output.extend_from_slice(chunk);
        }
        if payload.is_empty() && end_stream {
            H2Frame {
                frame_type: H2FrameType::Data,
                flags: 0x1,
                stream_id,
                payload: Vec::new(),
            }
            .encode(&mut output);
        }
        output
    }

    pub fn trailers_frame(
        &mut self,
        stream_id: u32,
        headers: &[Header<'_>],
    ) -> Result<H2OutboundCommit, H2ProtocolError> {
        let fields = headers
            .iter()
            .map(|header| H2HeaderField {
                name: header.name.as_bytes().to_vec(),
                value: header.value.as_bytes().to_vec(),
                sensitive: false,
            })
            .collect::<Vec<_>>();
        self.trailers_frame_with_raw_headers(stream_id, &fields)
    }

    pub fn trailers_frame_with_raw_headers(
        &mut self,
        stream_id: u32,
        headers: &[H2HeaderField],
    ) -> Result<H2OutboundCommit, H2ProtocolError> {
        self.enqueue_outbound_header_block(stream_id, headers, true, 0, |_| {})
    }

    pub fn accept_bytes(
        &mut self,
        input: &[u8],
    ) -> Result<(Option<H2ByteClientEvent>, usize, Vec<u8>), ServerError> {
        let (event, consumed, output) = self.accept_bytes_ref(input)?;
        Ok((
            event.map(H2ByteClientEventRef::into_owned),
            consumed,
            output,
        ))
    }

    pub fn accept_bytes_ref<'a>(
        &mut self,
        input: &'a [u8],
    ) -> Result<(Option<H2ByteClientEventRef<'a>>, usize, Vec<u8>), ServerError> {
        if let Some(error) = self.terminal_protocol_error {
            return Err(error.into());
        }
        let mut output = Vec::new();
        let (frame, consumed) =
            match H2FrameRef::decode_with_max_frame_size(input, self.limits.max_frame_size) {
                Ok(decoded) => decoded,
                Err(ServerError::NeedMore) => return Ok((None, 0, output)),
                Err(error) => return Err(error),
            };
        match self.accept_frame_bytes_ref_typed(frame) {
            (H2FrameOutcome::Event(event), mut event_output) => {
                output.append(&mut event_output);
                Ok((Some(event), consumed, output))
            }
            (H2FrameOutcome::Ignored, mut event_output) => {
                output.append(&mut event_output);
                Ok((None, consumed, output))
            }
            (H2FrameOutcome::Error(error), _) => {
                Err(compatibility_error(self.last_compat_error.take(), error))
            }
        }
    }

    pub fn accept(
        &mut self,
        input: &[u8],
    ) -> Result<(Option<H2ClientEvent>, usize, Vec<u8>), ServerError> {
        let (event, consumed, output) = self.accept_bytes(input)?;
        let event = event
            .map(H2ByteClientEvent::try_into_text)
            .transpose()
            .map_err(|_| ServerError::MalformedMessage)?;
        Ok((event, consumed, output))
    }

    pub fn accept_ref<'a>(
        &mut self,
        input: &'a [u8],
    ) -> Result<(Option<H2ClientEventRef<'a>>, usize, Vec<u8>), ServerError> {
        let (event, consumed, output) = self.accept_bytes_ref(input)?;
        let event = event
            .map(H2ByteClientEvent::try_into_text)
            .transpose()
            .map_err(|_| ServerError::MalformedMessage)?;
        Ok((event, consumed, output))
    }

    pub fn accept_frame_bytes(
        &mut self,
        frame: H2Frame,
    ) -> Result<(H2ByteClientEvent, Vec<u8>), ServerError> {
        let (outcome, output) = self.accept_frame_bytes_typed(frame);
        match outcome {
            H2FrameOutcome::Event(event) => Ok((event, output)),
            H2FrameOutcome::Ignored => Err(ServerError::InvalidFrame),
            H2FrameOutcome::Error(error) => {
                Err(compatibility_error(self.last_compat_error.take(), error))
            }
        }
    }

    pub fn accept_frame_bytes_ref<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> Result<(H2ByteClientEventRef<'a>, Vec<u8>), ServerError> {
        let (outcome, output) = self.accept_frame_bytes_ref_typed(frame);
        match outcome {
            H2FrameOutcome::Event(event) => Ok((event, output)),
            H2FrameOutcome::Ignored => Err(ServerError::InvalidFrame),
            H2FrameOutcome::Error(error) => {
                Err(compatibility_error(self.last_compat_error.take(), error))
            }
        }
    }

    pub fn accept_frame_bytes_typed(
        &mut self,
        frame: H2Frame,
    ) -> (H2FrameOutcome<H2ByteClientEvent>, Vec<u8>) {
        let (outcome, output) = self.accept_frame_bytes_ref_typed(frame.as_ref());
        (outcome.map_event(H2ByteClientEventRef::into_owned), output)
    }

    pub fn accept_frame_bytes_ref_typed<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> (H2FrameOutcome<H2ByteClientEventRef<'a>>, Vec<u8>) {
        self.last_compat_error = None;
        if let Some(error) = self.terminal_protocol_error {
            return (H2FrameOutcome::Error(error), Vec::new());
        }
        if self.header_block.pending.is_some()
            && (!matches!(frame.frame_type, H2FrameType::Continuation)
                || self
                    .header_block
                    .pending
                    .as_ref()
                    .is_some_and(|pending| frame.stream_id != pending.stream_id))
        {
            return (
                H2FrameOutcome::Error(H2ProtocolError::connection(
                    H2ErrorCode::ProtocolError,
                    "HTTP/2 header block continuation sequence violated",
                )),
                Vec::new(),
            );
        }
        if !self.settings_seen
            && (!matches!(frame.frame_type, H2FrameType::Settings)
                || frame.flags & 0x1 != 0
                || frame.stream_id != 0)
        {
            return (
                H2FrameOutcome::Error(H2ProtocolError::connection(
                    H2ErrorCode::ProtocolError,
                    "HTTP/2 server connection preface must start with SETTINGS",
                )),
                Vec::new(),
            );
        }
        if frame.frame_type.is_unknown() {
            return (H2FrameOutcome::Ignored, Vec::new());
        }
        if matches!(frame.frame_type, H2FrameType::Priority) {
            let head = frame.head();
            if let Err(error) = self.record_control(H2FrameType::Priority) {
                return (
                    H2FrameOutcome::Error(h2_error_from_server_error(error, Some(head))),
                    Vec::new(),
                );
            }
            return if priority_payload(frame.stream_id, frame.payload).is_ok() {
                (H2FrameOutcome::Ignored, Vec::new())
            } else {
                (
                    H2FrameOutcome::Error(h2_error_from_server_error(
                        ServerError::InvalidFrame,
                        Some(head),
                    )),
                    Vec::new(),
                )
            };
        }
        let head = frame.head();
        match self.accept_frame_compat(frame) {
            Ok((event, output)) => (H2FrameOutcome::Event(event), output),
            Err(ServerError::NeedMore) => (H2FrameOutcome::Ignored, Vec::new()),
            Err(error) => {
                self.last_compat_error = Some(error.clone());
                let typed = self.h2_error_from_server_error(error, Some(head));
                (H2FrameOutcome::Error(typed), Vec::new())
            }
        }
    }

    pub fn accept_frame(
        &mut self,
        frame: H2Frame,
    ) -> Result<(H2ClientEvent, Vec<u8>), ServerError> {
        let (outcome, output) = self.accept_frame_typed(frame);
        match outcome {
            H2FrameOutcome::Event(event) => Ok((event, output)),
            H2FrameOutcome::Ignored => Err(ServerError::InvalidFrame),
            H2FrameOutcome::Error(error) => Err(error.into()),
        }
    }

    pub fn accept_frame_ref<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> Result<(H2ClientEventRef<'a>, Vec<u8>), ServerError> {
        let (outcome, output) = self.accept_frame_ref_typed(frame);
        match outcome {
            H2FrameOutcome::Event(event) => Ok((event, output)),
            H2FrameOutcome::Ignored => Err(ServerError::InvalidFrame),
            H2FrameOutcome::Error(error) => Err(error.into()),
        }
    }

    pub fn accept_frame_typed(
        &mut self,
        frame: H2Frame,
    ) -> (H2FrameOutcome<H2ClientEvent>, Vec<u8>) {
        let stream_id = frame.stream_id;
        let (outcome, output) = self.accept_frame_bytes_typed(frame);
        (project_client_outcome(outcome, stream_id), output)
    }

    pub fn accept_frame_ref_typed<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> (H2FrameOutcome<H2ClientEventRef<'a>>, Vec<u8>) {
        let stream_id = frame.stream_id;
        let (outcome, output) = self.accept_frame_bytes_ref_typed(frame);
        (project_client_outcome(outcome, stream_id), output)
    }

    fn accept_frame_compat<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> Result<(H2ByteClientEventRef<'a>, Vec<u8>), ServerError> {
        if self.header_block.pending.is_some()
            && !matches!(frame.frame_type, H2FrameType::Continuation)
        {
            return Err(ServerError::InvalidFrame);
        }
        let mut output = Vec::new();
        let event = match frame.frame_type {
            H2FrameType::Settings => {
                self.record_control(H2FrameType::Settings)?;
                if frame.stream_id != 0 {
                    return Err(ServerError::InvalidFrame);
                }
                let initial_window_size = if frame.flags & 0x1 != 0 {
                    if !frame.payload.is_empty() {
                        return Err(ServerError::InvalidFrame);
                    }
                    if self.local_settings_state != H2SettingsSyncState::WaitingAck {
                        return Err(ServerError::InvalidFrame);
                    }
                    self.local_settings_state = H2SettingsSyncState::Synced;
                    self.control_diagnostics.settings_acks =
                        self.control_diagnostics.settings_acks.saturating_add(1);
                    None
                } else {
                    self.settings_seen = true;
                    self.control_diagnostics.settings_frames =
                        self.control_diagnostics.settings_frames.saturating_add(1);
                    let initial_window_size = self.apply_settings(frame.payload)?;
                    H2Frame {
                        frame_type: H2FrameType::Settings,
                        flags: 0x1,
                        stream_id: 0,
                        payload: Vec::new(),
                    }
                    .encode(&mut output);
                    initial_window_size
                };
                H2ByteClientEvent::Settings {
                    initial_window_size,
                }
            }
            H2FrameType::Ping => {
                self.record_control(H2FrameType::Ping)?;
                if frame.stream_id != 0 || frame.payload.len() != 8 {
                    return Err(ServerError::InvalidFrame);
                }
                let ack = frame.flags & 0x1 != 0;
                if ack {
                    self.control_diagnostics.ping_acks =
                        self.control_diagnostics.ping_acks.saturating_add(1);
                } else {
                    self.control_diagnostics.pings =
                        self.control_diagnostics.pings.saturating_add(1);
                }
                if !ack {
                    H2FrameRef {
                        frame_type: H2FrameType::Ping,
                        flags: 0x1,
                        stream_id: 0,
                        payload: frame.payload,
                    }
                    .encode(&mut output);
                }
                H2ByteClientEvent::Ping { ack }
            }
            H2FrameType::WindowUpdate => {
                self.record_control(H2FrameType::WindowUpdate)?;
                let increment = window_update_increment(frame.payload)?;
                self.apply_send_window_update(frame.stream_id, increment)?;
                H2ByteClientEvent::WindowUpdate {
                    stream_id: frame.stream_id,
                    increment,
                }
            }
            H2FrameType::Headers => {
                if let Some(block) = accept_header_frame_for_connection(
                    &mut self.header_block,
                    frame,
                    self.limits,
                    &mut self.last_protocol_error,
                    &mut self.terminal_protocol_error,
                )? {
                    self.event_from_complete_headers(block)?
                } else {
                    return Err(ServerError::NeedMore);
                }
            }
            H2FrameType::Data => {
                let payload = data_payload(frame.flags, frame.payload)?;
                let discard = self.closed_streams.contains(&frame.stream_id)
                    && self.reset_tolerant_streams.contains(&frame.stream_id);
                self.validate_data_frame(frame.stream_id, payload.len(), frame.flags & 0x1 != 0)?;
                if !payload.is_empty() || frame.flags & 0x1 != 0 {
                    self.record_progress_frame();
                }
                if discard {
                    H2ByteClientEvent::DiscardedData {
                        stream_id: frame.stream_id,
                        flow_control_len: frame.payload.len(),
                    }
                } else {
                    H2ByteClientEvent::Data {
                        stream_id: frame.stream_id,
                        payload,
                        flow_control_len: frame.payload.len(),
                        end_stream: frame.flags & 0x1 != 0,
                    }
                }
            }
            H2FrameType::RstStream => {
                self.record_control(H2FrameType::RstStream)?;
                if frame.stream_id == 0 || frame.payload.len() != 4 {
                    return Err(ServerError::InvalidFrame);
                }
                if !self.open_streams.contains_key(&frame.stream_id)
                    && !self.closed_streams.contains(&frame.stream_id)
                {
                    return Err(ServerError::InvalidFrame);
                }
                if self.closed_streams.contains(&frame.stream_id) {
                    return Err(ServerError::NeedMore);
                }
                self.response_streams.remove(&frame.stream_id);
                self.open_streams.remove(&frame.stream_id);
                self.remember_reset_tolerant_stream(frame.stream_id);
                self.control_diagnostics.resets = self.control_diagnostics.resets.saturating_add(1);
                H2ByteClientEvent::Reset {
                    stream_id: frame.stream_id,
                    error_code: u32::from_be_bytes([
                        frame.payload[0],
                        frame.payload[1],
                        frame.payload[2],
                        frame.payload[3],
                    ]),
                }
            }
            H2FrameType::Goaway => {
                self.record_control(H2FrameType::Goaway)?;
                if frame.stream_id != 0 || frame.payload.len() < 8 {
                    return Err(ServerError::InvalidFrame);
                }
                let mut last_stream_id = u32::from_be_bytes([
                    frame.payload[0],
                    frame.payload[1],
                    frame.payload[2],
                    frame.payload[3],
                ]);
                last_stream_id &= 0x7fff_ffff;
                self.received_goaway_last_stream_id = Some(last_stream_id);
                self.control_diagnostics.goaways =
                    self.control_diagnostics.goaways.saturating_add(1);
                H2ByteClientEvent::Goaway {
                    last_stream_id,
                    error_code: u32::from_be_bytes([
                        frame.payload[4],
                        frame.payload[5],
                        frame.payload[6],
                        frame.payload[7],
                    ]),
                }
            }
            H2FrameType::Priority => {
                self.record_control(H2FrameType::Priority)?;
                return Err(ServerError::InvalidFrame);
            }
            H2FrameType::Continuation => {
                if let Some(block) = accept_header_frame_for_connection(
                    &mut self.header_block,
                    frame,
                    self.limits,
                    &mut self.last_protocol_error,
                    &mut self.terminal_protocol_error,
                )? {
                    self.event_from_complete_headers(block)?
                } else {
                    return Err(ServerError::NeedMore);
                }
            }
            H2FrameType::PushPromise | H2FrameType::Unknown(_) => {
                return Err(ServerError::InvalidFrame);
            }
        };
        Ok((event, output))
    }

    fn apply_settings(
        &mut self,
        payload: &[u8],
    ) -> Result<Option<H2InitialWindowSizeChange>, ServerError> {
        let previous = self.settings.initial_window_size;
        let decoded =
            H2Settings::decode_payload_with_limit(payload, self.limits.max_settings_entries)?;
        if decoded.iter().any(|setting| {
            setting.id == H2SettingId::InitialWindowSize && setting.value > H2_MAX_WINDOW_SIZE
        }) {
            self.last_protocol_error = Some(H2ProtocolError::connection(
                H2ErrorCode::FlowControlError,
                "SETTINGS_INITIAL_WINDOW_SIZE exceeds the HTTP/2 window maximum",
            ));
            return Err(ServerError::InvalidFrame);
        }
        let mut settings = self.settings;
        settings.apply_all(&decoded)?;
        for setting in &decoded {
            if setting.id == H2SettingId::HeaderTableSize
                && let Some(codecs) = self.header_codecs.as_mut()
            {
                codecs.outbound.set_max_table_size(setting.value as usize);
            }
        }
        let current = settings.initial_window_size;
        if previous != current {
            let delta = i64::from(current) - i64::from(previous);
            let delta = i32::try_from(delta).map_err(|_| ServerError::FlowControlViolation)?;
            for stream in self.open_streams.values() {
                let mut adjusted = stream.send_window;
                if adjusted.adjust(delta).is_err() {
                    self.last_protocol_error = Some(H2ProtocolError::connection(
                        H2ErrorCode::FlowControlError,
                        "SETTINGS_INITIAL_WINDOW_SIZE overflowed an active send window",
                    ));
                    return Err(ServerError::FlowControlViolation);
                }
            }
            for stream in self.open_streams.values_mut() {
                stream.send_window.adjust(delta)?;
            }
        }
        self.settings = settings;
        Ok((previous != current).then_some(H2InitialWindowSizeChange { previous, current }))
    }

    fn encode_local_settings(&self, output: &mut Vec<u8>) {
        let mut payload = Vec::new();
        let mut settings = Vec::new();
        if self.limits.max_header_table_size != H2Settings::default().header_table_size as usize {
            settings.push(H2Setting::new(
                H2SettingId::HeaderTableSize,
                self.limits
                    .max_header_table_size
                    .min(crate::hpack::MAX_TABLE_SIZE) as u32,
            ));
        }
        if self.local_initial_window_size != H2Settings::default().initial_window_size {
            settings.push(H2Setting::new(
                H2SettingId::InitialWindowSize,
                self.local_initial_window_size,
            ));
        }
        if self.limits.max_active_streams != usize::MAX {
            settings.push(H2Setting::new(
                H2SettingId::MaxConcurrentStreams,
                self.limits.max_active_streams.min(u32::MAX as usize) as u32,
            ));
        }
        if self.limits.max_header_list_size != usize::MAX {
            settings.push(H2Setting::new(
                H2SettingId::MaxHeaderListSize,
                self.limits.max_header_list_size.min(u32::MAX as usize) as u32,
            ));
        }
        H2Settings::encode_payload(&settings, &mut payload);
        H2Frame {
            frame_type: H2FrameType::Settings,
            flags: 0,
            stream_id: 0,
            payload,
        }
        .encode(output);
    }

    fn encode_local_connection_window_update(&self, output: &mut Vec<u8>) {
        let default = H2Settings::default().initial_window_size;
        let Some(increment) = self.local_connection_window_size.checked_sub(default) else {
            return;
        };
        if increment == 0 {
            return;
        }
        H2Frame {
            frame_type: H2FrameType::WindowUpdate,
            flags: 0,
            stream_id: 0,
            payload: increment.to_be_bytes().to_vec(),
        }
        .encode(output);
    }

    fn event_from_complete_headers<'a>(
        &mut self,
        block: H2CompleteHeaderBlock<'a>,
    ) -> Result<H2ByteClientEventRef<'a>, ServerError> {
        let event = self.event_from_complete_headers_parts::<&'a [u8]>(
            block.stream_id,
            block.flags,
            &block.block,
            block.self_dependency,
        )?;
        self.record_progress_frame();
        Ok(event)
    }

    fn event_from_complete_headers_parts<P>(
        &mut self,
        stream_id: u32,
        flags: u8,
        block: &[u8],
        self_dependency: bool,
    ) -> Result<H2ByteClientEvent<P>, ServerError> {
        let role = if self.response_streams.contains_key(&stream_id) {
            H2HeaderValidationRole::Trailers
        } else {
            H2HeaderValidationRole::Response
        };
        let headers = self.decode_h2_header_fields(stream_id, block, role)?;
        if self_dependency {
            self.last_protocol_error = Some(H2ProtocolError::stream(
                stream_id,
                H2ErrorCode::ProtocolError,
                "HEADERS priority dependency references its own stream",
            ));
            return Err(ServerError::InvalidFrame);
        }
        self.validate_header_stream_state(stream_id, flags)?;
        self.event_from_decoded_header_fields(stream_id, flags, headers)
    }

    fn validate_header_stream_state(&self, stream_id: u32, flags: u8) -> Result<(), ServerError> {
        if self.response_streams.contains_key(&stream_id) {
            return if flags & 0x1 != 0 {
                Ok(())
            } else {
                Err(ServerError::InvalidFrame)
            };
        }
        if !self.open_streams.contains_key(&stream_id) || self.closed_streams.contains(&stream_id) {
            Err(ServerError::InvalidFrame)
        } else {
            Ok(())
        }
    }

    fn event_from_decoded_header_fields<P>(
        &mut self,
        stream_id: u32,
        flags: u8,
        headers: Vec<H2HeaderField>,
    ) -> Result<H2ByteClientEvent<P>, ServerError> {
        if self.response_streams.contains_key(&stream_id) {
            if flags & 0x1 == 0 {
                return Err(ServerError::InvalidFrame);
            }
            let state = self
                .response_streams
                .get_mut(&stream_id)
                .ok_or(ServerError::InvalidFrame)?;
            state.accept_trailers(&headers, self.http_limits)?;
            state.finish()?;
            self.response_streams.remove(&stream_id);
            self.open_streams.remove(&stream_id);
            self.remember_closed_stream(stream_id);
            Ok(H2ByteClientEvent::Trailers { stream_id, headers })
        } else {
            if !self.open_streams.contains_key(&stream_id)
                || self.closed_streams.contains(&stream_id)
            {
                return Err(ServerError::InvalidFrame);
            }
            let status = status_from_raw_headers(&headers)?;
            let informational = (100..=199).contains(&status);
            if informational {
                if flags & 0x1 != 0 {
                    return Err(ServerError::InvalidFrame);
                }
            } else {
                let request_is_head = self
                    .open_streams
                    .get(&stream_id)
                    .is_some_and(|stream| stream.request_is_head);
                let body_limit = self
                    .open_streams
                    .get(&stream_id)
                    .and_then(|stream| stream.response_body_limit);
                let state = H2StreamState::new_response_raw(
                    &headers,
                    request_is_head,
                    status,
                    self.http_limits,
                    body_limit,
                )?;
                if flags & 0x1 == 0 {
                    self.response_streams.insert(stream_id, state);
                } else {
                    state.finish()?;
                    self.open_streams.remove(&stream_id);
                    self.remember_closed_stream(stream_id);
                }
            }
            Ok(H2ByteClientEvent::ResponseHeaders {
                stream_id,
                headers,
                end_stream: flags & 0x1 != 0,
            })
        }
    }

    /// Accepts fields decoded by an adapter-owned connection HPACK decoder.
    pub fn accept_external_header_fields(
        &mut self,
        stream_id: u32,
        flags: u8,
        headers: Vec<H2HeaderField>,
    ) -> Result<H2ByteClientEvent, ServerError> {
        if self.header_codecs.is_some() || !self.settings_seen {
            return Err(ServerError::InvalidFrame);
        }
        self.validate_header_stream_state(stream_id, flags)?;
        let role = if self.response_streams.contains_key(&stream_id) {
            H2HeaderValidationRole::Trailers
        } else {
            H2HeaderValidationRole::Response
        };
        self.validate_external_header_fields(stream_id, role, &headers)?;
        self.event_from_decoded_header_fields(stream_id, flags & 0x5, headers)
    }

    pub fn accept_complete_header_block_bytes(
        &mut self,
        stream_id: u32,
        flags: u8,
        block: &[u8],
    ) -> Result<H2ByteClientEvent, ServerError> {
        if let Some(error) = self.terminal_protocol_error {
            return Err(error.into());
        }
        if !self.settings_seen {
            return Err(ServerError::InvalidFrame);
        }
        if block.len() > self.limits.max_encoded_header_block_size {
            let limit = self.limits.max_encoded_header_block_size;
            let actual = block.len();
            let error = encoded_header_limit_error(limit, actual);
            self.last_protocol_error = Some(error);
            self.terminal_protocol_error = Some(error);
            return Err(ServerError::HeaderTooLarge { limit, actual });
        }
        self.event_from_complete_headers_parts::<Vec<u8>>(stream_id, flags & 0x5, block, false)
    }

    pub fn accept_complete_header_block(
        &mut self,
        stream_id: u32,
        flags: u8,
        block: &[u8],
    ) -> Result<H2ClientEvent, ServerError> {
        self.accept_complete_header_block_bytes(stream_id, flags, block)?
            .try_into_text()
            .map_err(|_| ServerError::MalformedMessage)
    }

    pub fn validate_data_frame(
        &mut self,
        stream_id: u32,
        payload_len: usize,
        end_stream: bool,
    ) -> Result<(), ServerError> {
        if stream_id == 0 {
            return Err(ServerError::InvalidFrame);
        }
        if self.closed_streams.contains(&stream_id) {
            if self.reset_tolerant_streams.contains(&stream_id) {
                return Ok(());
            }
            self.last_protocol_error = Some(H2ProtocolError::stream(
                stream_id,
                H2ErrorCode::StreamClosed,
                "DATA arrived after the stream closed",
            ));
            return Err(ServerError::InvalidFrame);
        }
        let Some(state) = self.response_streams.get_mut(&stream_id) else {
            return Err(ServerError::InvalidFrame);
        };
        state.receive_data(payload_len, end_stream)?;
        if end_stream {
            self.response_streams.remove(&stream_id);
            self.open_streams.remove(&stream_id);
            self.remember_closed_stream(stream_id);
        }
        Ok(())
    }

    /// Abandons an active client stream and records reset-tolerant closure.
    ///
    /// Closing an unknown stream or a stream with a normal completion
    /// tombstone is a no-op. A retained tombstone for an active stream
    /// surfaces later in-flight DATA as [`H2ClientEvent::DiscardedData`] so
    /// the owner can charge connection flow control.
    pub fn close_stream(&mut self, stream_id: u32) {
        let had_response = self.response_streams.remove(&stream_id).is_some();
        let was_open = self.open_streams.remove(&stream_id).is_some() || had_response;
        if self.closed_streams.contains(&stream_id) || !was_open {
            return;
        }
        self.remember_reset_tolerant_stream(stream_id);
    }

    fn remember_closed_stream(&mut self, stream_id: u32) {
        if self.closed_streams.insert(stream_id) {
            self.closed_stream_order.push_back(stream_id);
        }
        while self.closed_streams.len() > self.limits.max_closed_stream_tombstones {
            if let Some(expired) = self.closed_stream_order.pop_front() {
                self.closed_streams.remove(&expired);
                self.reset_tolerant_streams.remove(&expired);
            } else {
                break;
            }
        }
    }

    fn remember_reset_tolerant_stream(&mut self, stream_id: u32) {
        self.remember_closed_stream(stream_id);
        if self.closed_streams.contains(&stream_id) {
            self.reset_tolerant_streams.insert(stream_id);
        }
    }

    fn h2_error_from_server_error(
        &mut self,
        error: ServerError,
        head: Option<H2FrameHead>,
    ) -> H2ProtocolError {
        if let Some(typed) = self.last_protocol_error.take() {
            return typed;
        }
        let is_hpack_error = matches!(
            error,
            ServerError::InvalidHpack | ServerError::UnsupportedHpack
        );
        let mut typed = h2_error_from_server_error(error, head);
        if is_hpack_error && let Some(hpack_error) = self.last_hpack_error.take() {
            typed = H2ProtocolError::hpack(typed.scope, hpack_error, typed.debug);
        }
        typed
    }

    pub fn classify_frame(&mut self, frame: H2Frame) -> Result<H2ClientEvent, ServerError> {
        match self.accept_frame_typed(frame) {
            (H2FrameOutcome::Event(event), _) => Ok(event),
            (H2FrameOutcome::Ignored, _) => Err(ServerError::InvalidFrame),
            (H2FrameOutcome::Error(error), _) => Err(error.into()),
        }
    }

    fn decode_h2_header_fields(
        &mut self,
        stream_id: u32,
        block: &[u8],
        role: H2HeaderValidationRole,
    ) -> Result<Vec<H2HeaderField>, ServerError> {
        let headers = decode_connection_header_fields(
            &mut self
                .header_codecs
                .as_mut()
                .ok_or(ServerError::InvalidFrame)?
                .inbound,
            block,
            self.limits.max_header_list_size,
            stream_id,
            role,
            H2ConnectionHpackErrorState {
                last_hpack_error: &mut self.last_hpack_error,
                last_protocol_error: &mut self.last_protocol_error,
                terminal_protocol_error: &mut self.terminal_protocol_error,
            },
        )?;
        enforce_h2_field_limits(&headers, role, self.http_limits)?;
        Ok(headers)
    }

    fn validate_external_header_fields(
        &mut self,
        stream_id: u32,
        role: H2HeaderValidationRole,
        headers: &[H2HeaderField],
    ) -> Result<(), ServerError> {
        if let Err(validation) = validate_decoded_header_fields(headers, role) {
            self.last_protocol_error = Some(H2ProtocolError::stream(
                stream_id,
                H2ErrorCode::ProtocolError,
                "HTTP/2 header field validation failed",
            ));
            return Err(validation.into());
        }
        enforce_h2_field_limits(headers, role, self.http_limits)
    }

    pub fn discard_hpack_block(&mut self, block: &[u8]) -> Result<(), ServerError> {
        if let Some(error) = self.terminal_protocol_error {
            return Err(error.into());
        }
        if block.len() > self.limits.max_encoded_header_block_size {
            let limit = self.limits.max_encoded_header_block_size;
            let actual = block.len();
            let error = encoded_header_limit_error(limit, actual);
            self.last_protocol_error = Some(error);
            self.terminal_protocol_error = Some(error);
            return Err(ServerError::HeaderTooLarge { limit, actual });
        }
        let Some(codecs) = self.header_codecs.as_mut() else {
            return Err(ServerError::InvalidFrame);
        };
        match codecs
            .inbound
            .decode(block, self.limits.max_header_list_size)
        {
            Ok(_) => Ok(()),
            Err(crate::hpack::Error::HeaderListTooLarge { actual }) => {
                let limit = self.limits.max_header_list_size;
                self.last_protocol_error = Some(decoded_header_limit_error(
                    H2ErrorScope::Connection,
                    limit,
                    actual,
                ));
                Err(ServerError::HeaderTooLarge { limit, actual })
            }
            Err(crate::hpack::Error::AllocationFailed) => {
                let error = allocation_terminal_error();
                self.last_protocol_error = Some(error);
                self.terminal_protocol_error = Some(error);
                Err(ServerError::InvalidFrame)
            }
            Err(error) => {
                self.last_hpack_error = Some(error.into());
                self.terminal_protocol_error = Some(poisoned_connection_error());
                Err(ServerError::InvalidHpack)
            }
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum H2HeaderValidationRole {
    Request,
    Response,
    Trailers,
}

fn h2_field_totals(headers: &[H2HeaderField], role: H2HeaderValidationRole) -> (usize, usize) {
    let regular_count = headers
        .iter()
        .filter(|header| !header.name.starts_with(b":"))
        .count();
    let synthesized_host = matches!(role, H2HeaderValidationRole::Request)
        && headers.iter().any(|header| header.name == b":authority")
        && !headers.iter().any(|header| header.name == b"host");
    let count = regular_count.saturating_add(usize::from(synthesized_host));
    let bytes = headers.iter().fold(0usize, |total, header| {
        total.saturating_add(hpack_field_size(&header.name, &header.value))
    });
    (count, bytes)
}

pub(crate) fn enforce_h2_field_limits(
    headers: &[H2HeaderField],
    role: H2HeaderValidationRole,
    limits: HttpLimits,
) -> Result<(), ServerError> {
    let (count, bytes) = h2_field_totals(headers, role);
    if count > limits.max_headers() {
        return Err(ServerError::TooManyHeaders {
            limit: limits.max_headers(),
            actual: count,
        });
    }
    if bytes > limits.max_header_bytes() {
        return Err(ServerError::HeaderTooLarge {
            limit: limits.max_header_bytes(),
            actual: bytes,
        });
    }
    Ok(())
}

fn enforce_h2_outbound_field_limits<'a>(
    stream_id: u32,
    field_count: usize,
    field_at: impl Fn(usize) -> H2RawHeaderRef<'a>,
    limits: HttpLimits,
) -> Result<(), H2ProtocolError> {
    let mut count = 0usize;
    let mut bytes = 0usize;
    for index in 0..field_count {
        let field = field_at(index);
        if !field.name.starts_with(b":") {
            count = count.saturating_add(1);
        }
        bytes = bytes.saturating_add(hpack_field_size(field.name, field.value));
    }
    let scope = H2ErrorScope::Stream(stream_id);
    if count > limits.max_headers() {
        return Err(H2ProtocolError::resource_limit(
            scope,
            crate::HttpErrorKind::TooManyHeaders,
            limits.max_headers(),
            count,
            "HTTP/2 header count exceeds configured limit",
        ));
    }
    if bytes > limits.max_header_bytes() {
        return Err(H2ProtocolError::resource_limit(
            scope,
            crate::HttpErrorKind::HeadersTooLarge,
            limits.max_header_bytes(),
            bytes,
            "HTTP/2 header list exceeds configured limit",
        ));
    }
    Ok(())
}

fn h2_body_limit_error(stream_id: u32, limits: HttpLimits, actual: usize) -> H2ProtocolError {
    H2ProtocolError::resource_limit(
        H2ErrorScope::Stream(stream_id),
        crate::HttpErrorKind::BodyTooLarge,
        limits.max_body_bytes(),
        actual,
        "HTTP/2 body exceeds configured limit",
    )
}

fn enforce_optional_body_size(actual: usize, limit: Option<usize>) -> Result<(), ServerError> {
    if let Some(limit) = limit
        && actual > limit
    {
        Err(ServerError::BodyTooLarge { limit, actual })
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum H2ValidatedName {
    Other,
    Method,
    Scheme,
    Authority,
    Path,
    Status,
    Te,
    ContentLength,
    Forbidden,
    UnknownPseudo,
}

#[derive(Clone, Copy)]
enum H2ContentLengthState {
    Start,
    Digits,
    OwsAfterDigits,
    AfterComma,
}

struct H2HeaderValidator {
    role: H2HeaderValidationRole,
    invalid: bool,
    invalid_content_length: bool,
    saw_regular: bool,
    saw_method: bool,
    saw_scheme: bool,
    saw_authority: bool,
    saw_path: bool,
    saw_status: bool,
    content_length: Option<usize>,
    name: [u8; 32],
    name_len: usize,
    name_starts_with_colon: bool,
    current_name: H2ValidatedName,
    value: [u8; 32],
    value_len: usize,
    value_ends_with_whitespace: bool,
    content_length_state: H2ContentLengthState,
    content_length_item: usize,
    content_length_field_value: Option<usize>,
}

impl H2HeaderValidator {
    fn new(role: H2HeaderValidationRole) -> Self {
        Self {
            role,
            invalid: false,
            invalid_content_length: false,
            saw_regular: false,
            saw_method: false,
            saw_scheme: false,
            saw_authority: false,
            saw_path: false,
            saw_status: false,
            content_length: None,
            name: [0; 32],
            name_len: 0,
            name_starts_with_colon: false,
            current_name: H2ValidatedName::Other,
            value: [0; 32],
            value_len: 0,
            value_ends_with_whitespace: false,
            content_length_state: H2ContentLengthState::Start,
            content_length_item: 0,
            content_length_field_value: None,
        }
    }

    fn finish(mut self) -> Result<(), H2HeaderValidationError> {
        match self.role {
            H2HeaderValidationRole::Request => {
                self.invalid |= !self.saw_method || !self.saw_scheme || !self.saw_path;
            }
            H2HeaderValidationRole::Response => self.invalid |= !self.saw_status,
            H2HeaderValidationRole::Trailers => {}
        }
        if self.invalid_content_length {
            Err(H2HeaderValidationError::InvalidContentLength)
        } else if self.invalid {
            Err(H2HeaderValidationError::MalformedMessage)
        } else {
            Ok(())
        }
    }

    /// Returns the field name only when it fit the inline buffer.
    ///
    /// `name_len` counts every byte the peer sent, not the bytes retained, so
    /// it can exceed the buffer. A name that overflows cannot match any name
    /// this validator recognises, and `get` reports that as `None` without
    /// indexing past the buffer.
    fn stored_name(&self) -> Option<&[u8]> {
        self.name.get(..self.name_len)
    }

    /// Returns the field value only when it fit the inline buffer.
    fn stored_value(&self) -> Option<&[u8]> {
        self.value.get(..self.value_len)
    }

    fn classify_name(&self) -> H2ValidatedName {
        let Some(name) = self.stored_name() else {
            return if self.name_starts_with_colon {
                H2ValidatedName::UnknownPseudo
            } else {
                H2ValidatedName::Other
            };
        };
        match name {
            b":method" => H2ValidatedName::Method,
            b":scheme" => H2ValidatedName::Scheme,
            b":authority" => H2ValidatedName::Authority,
            b":path" => H2ValidatedName::Path,
            b":status" => H2ValidatedName::Status,
            b"te" => H2ValidatedName::Te,
            b"content-length" => H2ValidatedName::ContentLength,
            b"connection" | b"keep-alive" | b"proxy-connection" | b"transfer-encoding"
            | b"upgrade" => H2ValidatedName::Forbidden,
            _ if self.name_starts_with_colon => H2ValidatedName::UnknownPseudo,
            _ => H2ValidatedName::Other,
        }
    }

    fn mark_pseudo(&mut self, name: H2ValidatedName) {
        if self.saw_regular {
            self.invalid = true;
        }
        let seen = match name {
            H2ValidatedName::Method if matches!(self.role, H2HeaderValidationRole::Request) => {
                &mut self.saw_method
            }
            H2ValidatedName::Scheme if matches!(self.role, H2HeaderValidationRole::Request) => {
                &mut self.saw_scheme
            }
            H2ValidatedName::Authority if matches!(self.role, H2HeaderValidationRole::Request) => {
                &mut self.saw_authority
            }
            H2ValidatedName::Path if matches!(self.role, H2HeaderValidationRole::Request) => {
                &mut self.saw_path
            }
            H2ValidatedName::Status if matches!(self.role, H2HeaderValidationRole::Response) => {
                &mut self.saw_status
            }
            _ => {
                self.invalid = true;
                return;
            }
        };
        if *seen {
            self.invalid = true;
        }
        *seen = true;
    }

    fn finish_content_length_item(&mut self) {
        if !matches!(
            self.content_length_state,
            H2ContentLengthState::Digits | H2ContentLengthState::OwsAfterDigits
        ) {
            self.invalid = true;
            self.invalid_content_length = true;
            return;
        }
        let parsed = self.content_length_item;
        if self
            .content_length_field_value
            .replace(parsed)
            .is_some_and(|existing| existing != parsed)
        {
            self.invalid = true;
            self.invalid_content_length = true;
        }
    }

    fn content_length_value_byte(&mut self, byte: u8) {
        match (self.content_length_state, byte) {
            (H2ContentLengthState::Start | H2ContentLengthState::AfterComma, b'0'..=b'9')
            | (H2ContentLengthState::Digits, b'0'..=b'9') => {
                let Some(parsed) = self
                    .content_length_item
                    .checked_mul(10)
                    .and_then(|number| number.checked_add(usize::from(byte - b'0')))
                else {
                    self.invalid = true;
                    self.invalid_content_length = true;
                    return;
                };
                self.content_length_item = parsed;
                self.content_length_state = H2ContentLengthState::Digits;
            }
            (H2ContentLengthState::Digits, b' ' | b'\t') => {
                self.content_length_state = H2ContentLengthState::OwsAfterDigits;
            }
            (
                H2ContentLengthState::AfterComma | H2ContentLengthState::OwsAfterDigits,
                b' ' | b'\t',
            ) => {}
            (H2ContentLengthState::Digits | H2ContentLengthState::OwsAfterDigits, b',') => {
                self.finish_content_length_item();
                self.content_length_item = 0;
                self.content_length_state = H2ContentLengthState::AfterComma;
            }
            _ => {
                self.invalid = true;
                self.invalid_content_length = true;
            }
        }
    }

    fn finish_content_length(&mut self) {
        self.finish_content_length_item();
        let Some(parsed) = self.content_length_field_value else {
            self.invalid = true;
            self.invalid_content_length = true;
            return;
        };
        if self
            .content_length
            .replace(parsed)
            .is_some_and(|existing| existing != parsed)
        {
            self.invalid = true;
            self.invalid_content_length = true;
        }
    }
}

impl crate::hpack::HeaderFieldVisitor for H2HeaderValidator {
    fn start_field(&mut self, _sensitive: bool) {
        self.name = [0; 32];
        self.name_len = 0;
        self.name_starts_with_colon = false;
        self.current_name = H2ValidatedName::Other;
        self.value = [0; 32];
        self.value_len = 0;
        self.value_ends_with_whitespace = false;
        self.content_length_state = H2ContentLengthState::Start;
        self.content_length_item = 0;
        self.content_length_field_value = None;
    }

    fn name_byte(&mut self, byte: u8) {
        if self.name_len == 0 {
            self.name_starts_with_colon = byte == b':';
        }
        if self.name_len < self.name.len() {
            self.name[self.name_len] = byte;
        }
        if byte.is_ascii_uppercase()
            || (byte == b':' && self.name_len != 0)
            || (byte != b':' && !is_h2_field_name_byte(byte))
        {
            self.invalid = true;
        }
        self.name_len = self.name_len.saturating_add(1);
    }

    fn end_name(&mut self) {
        if self.name_len == 0 {
            self.invalid = true;
        }
        self.current_name = self.classify_name();
        if self.name_starts_with_colon {
            self.mark_pseudo(self.current_name);
        } else {
            self.saw_regular = true;
            if matches!(
                self.current_name,
                H2ValidatedName::Forbidden | H2ValidatedName::UnknownPseudo
            ) {
                self.invalid = true;
            }
        }
    }

    fn value_byte(&mut self, byte: u8) {
        if self.current_name == H2ValidatedName::ContentLength {
            self.content_length_value_byte(byte);
        }
        if self.value_len < self.value.len() {
            self.value[self.value_len] = byte;
        }
        let whitespace = matches!(byte, b' ' | b'\t');
        if matches!(byte, 0 | b'\r' | b'\n') || (self.value_len == 0 && whitespace) {
            self.invalid = true;
        }
        self.value_ends_with_whitespace = whitespace;
        self.value_len = self.value_len.saturating_add(1);
    }

    fn end_field(&mut self) {
        if self.value_ends_with_whitespace {
            self.invalid = true;
        }
        match self.current_name {
            H2ValidatedName::Te => {
                if self.stored_value() != Some(b"trailers") {
                    self.invalid = true;
                }
            }
            H2ValidatedName::Status => {
                let valid = self.stored_value().is_some_and(|value| {
                    value.len() == 3 && value.iter().all(u8::is_ascii_digit) && value != b"101"
                });
                if !valid {
                    self.invalid = true;
                }
            }
            H2ValidatedName::ContentLength => self.finish_content_length(),
            _ => {}
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum H2HeaderValidationError {
    MalformedMessage,
    InvalidContentLength,
}

impl From<H2HeaderValidationError> for ServerError {
    fn from(error: H2HeaderValidationError) -> Self {
        match error {
            H2HeaderValidationError::MalformedMessage => Self::MalformedMessage,
            H2HeaderValidationError::InvalidContentLength => Self::InvalidContentLength,
        }
    }
}

pub(crate) fn validate_decoded_header_fields(
    headers: &[H2HeaderField],
    role: H2HeaderValidationRole,
) -> Result<(), H2HeaderValidationError> {
    use crate::hpack::HeaderFieldVisitor;

    let mut validator = H2HeaderValidator::new(role);
    for header in headers {
        validator.start_field(header.sensitive);
        for &byte in &header.name {
            validator.name_byte(byte);
        }
        validator.end_name();
        for &byte in &header.value {
            validator.value_byte(byte);
        }
        validator.end_field();
    }
    validator.finish()
}

fn is_h2_field_name_byte(byte: u8) -> bool {
    byte.is_ascii_lowercase()
        || byte.is_ascii_digit()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn decoded_header_limit_error(scope: H2ErrorScope, limit: usize, actual: usize) -> H2ProtocolError {
    let mut error = H2ProtocolError::resource_limit(
        scope,
        crate::HttpErrorKind::HeadersTooLarge,
        limit,
        actual,
        "decoded header list exceeds configured limit",
    );
    error.hpack_error = Some(H2HpackError::HeaderListTooLarge);
    error
}

fn encoded_header_limit_error(limit: usize, actual: usize) -> H2ProtocolError {
    let mut error = H2ProtocolError::resource_limit(
        H2ErrorScope::Connection,
        crate::HttpErrorKind::HeadersTooLarge,
        limit,
        actual,
        "encoded header block exceeds configured limit",
    );
    error.hpack_error = Some(H2HpackError::EncodedHeaderBlockTooLarge);
    error
}

fn allocation_terminal_error() -> H2ProtocolError {
    H2ProtocolError {
        scope: H2ErrorScope::Connection,
        code: H2ErrorCode::InternalError,
        debug: "HPACK storage allocation failed",
        hpack_error: Some(H2HpackError::AllocationFailed),
        http_error_kind: None,
        limit: None,
    }
}

fn poisoned_connection_error() -> H2ProtocolError {
    H2ProtocolError::hpack(
        H2ErrorScope::Connection,
        H2HpackError::DecoderPoisoned,
        "HPACK decoder is unavailable after a compression failure",
    )
}

fn outbound_hpack_error(category: H2HpackError) -> H2ProtocolError {
    H2ProtocolError {
        scope: H2ErrorScope::Connection,
        code: H2ErrorCode::InternalError,
        debug: "outbound HPACK block could not be handed off",
        hpack_error: Some(category),
        http_error_kind: None,
        limit: None,
    }
}

struct H2ConnectionHpackErrorState<'a> {
    last_hpack_error: &'a mut Option<H2HpackError>,
    last_protocol_error: &'a mut Option<H2ProtocolError>,
    terminal_protocol_error: &'a mut Option<H2ProtocolError>,
}

fn decode_connection_header_fields(
    decoder: &mut crate::hpack::Decoder,
    block: &[u8],
    max_header_list_size: usize,
    stream_id: u32,
    role: H2HeaderValidationRole,
    errors: H2ConnectionHpackErrorState<'_>,
) -> Result<Vec<H2HeaderField>, ServerError> {
    let mut validator = H2HeaderValidator::new(role);
    let decoded = decoder.decode_with_visitor(block, max_header_list_size, &mut validator);
    let validation = validator.finish();
    match decoded {
        Ok(headers) => match validation {
            Ok(()) => Ok(headers),
            Err(validation) => {
                *errors.last_protocol_error = Some(H2ProtocolError::stream(
                    stream_id,
                    H2ErrorCode::ProtocolError,
                    "HTTP/2 header field validation failed",
                ));
                Err(validation.into())
            }
        },
        Err(crate::hpack::Error::HeaderListTooLarge { actual }) => {
            if validation.is_err() {
                *errors.last_protocol_error = Some(H2ProtocolError::stream(
                    stream_id,
                    H2ErrorCode::ProtocolError,
                    "HTTP/2 header field validation failed",
                ));
            } else {
                *errors.last_protocol_error = Some(decoded_header_limit_error(
                    H2ErrorScope::Stream(stream_id),
                    max_header_list_size,
                    actual,
                ));
            }
            Err(ServerError::HeaderTooLarge {
                limit: max_header_list_size,
                actual,
            })
        }
        Err(crate::hpack::Error::AllocationFailed) => {
            let error = allocation_terminal_error();
            *errors.last_protocol_error = Some(error);
            *errors.terminal_protocol_error = Some(error);
            Err(ServerError::InvalidFrame)
        }
        Err(error) => {
            *errors.last_hpack_error = Some(error.into());
            *errors.terminal_protocol_error = Some(poisoned_connection_error());
            Err(ServerError::InvalidHpack)
        }
    }
}

fn request_from_headers(stream_id: u32, headers: &[H2Header]) -> Result<H2Request, ServerError> {
    let mut method = None;
    let mut path = None;
    let mut scheme = None;
    let mut authority = None;
    let mut saw_regular = false;
    for header in headers {
        if header.name.bytes().any(|byte| byte.is_ascii_uppercase()) {
            return Err(ServerError::MalformedMessage);
        }
        if matches!(
            header.name.as_str(),
            "connection" | "keep-alive" | "proxy-connection" | "transfer-encoding" | "upgrade"
        ) {
            return Err(ServerError::MalformedMessage);
        }
        if header.name.starts_with(':') {
            if saw_regular {
                return Err(ServerError::MalformedMessage);
            }
        } else {
            saw_regular = true;
        }
        match header.name.as_str() {
            ":method" if method.is_none() => method = Some(header.value.clone()),
            ":path" if path.is_none() => path = Some(header.value.clone()),
            ":scheme" if scheme.is_none() => scheme = Some(header.value.clone()),
            ":authority" if authority.is_none() => authority = Some(header.value.clone()),
            ":authority" => return Err(ServerError::MalformedMessage),
            name if name.starts_with(':') => return Err(ServerError::MalformedMessage),
            "te" if header.value != "trailers" => return Err(ServerError::MalformedMessage),
            "te" => {}
            _ => {}
        }
    }
    let _scheme = scheme.ok_or(ServerError::MalformedMessage)?;
    Ok(H2Request {
        stream_id,
        method: method.ok_or(ServerError::MalformedMessage)?,
        path: path.ok_or(ServerError::MalformedMessage)?,
    })
}

fn validate_regular_header(header: &H2Header) -> Result<(), ServerError> {
    if header.name.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Err(ServerError::MalformedMessage);
    }
    if matches!(
        header.name.as_str(),
        "connection" | "keep-alive" | "proxy-connection" | "transfer-encoding" | "upgrade"
    ) {
        return Err(ServerError::MalformedMessage);
    }
    if header.name == "te" && header.value != "trailers" {
        return Err(ServerError::MalformedMessage);
    }
    Ok(())
}

pub(crate) fn content_length_from_raw_headers(
    headers: &[H2HeaderField],
) -> Result<Option<usize>, ServerError> {
    let mut content_length = None;
    for header in headers {
        if header.name == b"content-length" {
            let parsed =
                parse_content_length(&header.value).ok_or(ServerError::InvalidContentLength)?;
            if content_length
                .replace(parsed)
                .is_some_and(|existing| existing != parsed)
            {
                return Err(ServerError::InvalidContentLength);
            }
        }
    }
    Ok(content_length)
}

fn project_h2_headers(
    headers: Vec<H2HeaderField>,
) -> Result<Vec<H2Header>, H2HeaderProjectionError> {
    project_h2_header_fields(&headers)
}

/// Typed HTTP/2 field-section role used by convenience projections.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum H2HeaderRole {
    /// Request pseudo-header rules.
    Request,
    /// Response pseudo-header rules.
    Response,
    /// Trailer rules, which prohibit pseudo-fields.
    Trailers,
}

/// Validates a typed role and atomically projects canonical fields to text.
pub fn project_h2_header_fields_for_role(
    headers: &[H2HeaderField],
    role: H2HeaderRole,
) -> Result<Vec<H2Header>, H2HeaderProjectionError> {
    let role = match role {
        H2HeaderRole::Request => H2HeaderValidationRole::Request,
        H2HeaderRole::Response => H2HeaderValidationRole::Response,
        H2HeaderRole::Trailers => H2HeaderValidationRole::Trailers,
    };
    validate_decoded_header_fields(headers, role)
        .map_err(|_| H2HeaderProjectionError::MalformedPseudoHeaders)?;
    project_h2_header_fields(headers)
}

/// Atomically projects canonical byte occurrences into the text convenience view.
///
/// The input remains unchanged when any name or value is not UTF-8.
pub fn project_h2_header_fields(
    headers: &[H2HeaderField],
) -> Result<Vec<H2Header>, H2HeaderProjectionError> {
    for header in headers {
        std::str::from_utf8(&header.name).map_err(|_| H2HeaderProjectionError::NameNotUtf8)?;
        std::str::from_utf8(&header.value).map_err(|_| H2HeaderProjectionError::ValueNotUtf8)?;
    }
    Ok(headers
        .iter()
        .map(|header| H2Header {
            name: String::from_utf8(header.name.clone()).expect("validated header name"),
            value: String::from_utf8(header.value.clone()).expect("validated header value"),
            sensitive: header.sensitive,
        })
        .collect())
}

fn status_from_raw_headers(headers: &[H2HeaderField]) -> Result<u16, ServerError> {
    let status = headers
        .iter()
        .find(|header| header.name == b":status")
        .ok_or(ServerError::MalformedMessage)?;
    if status.value.len() != 3 || !status.value.iter().all(u8::is_ascii_digit) {
        return Err(ServerError::MalformedMessage);
    }
    let hundreds = u16::from(status.value[0] - b'0') * 100;
    let tens = u16::from(status.value[1] - b'0') * 10;
    Ok(hundreds + tens + u16::from(status.value[2] - b'0'))
}

fn status_from_headers(headers: &[H2Header]) -> Result<u16, ServerError> {
    let mut status = None;
    let mut saw_regular = false;
    for header in headers {
        if header.name.bytes().any(|byte| byte.is_ascii_uppercase()) {
            return Err(ServerError::MalformedMessage);
        }
        if header.name.starts_with(':') {
            if saw_regular || header.name != ":status" || status.is_some() {
                return Err(ServerError::MalformedMessage);
            }
            status = Some(header);
        } else {
            validate_regular_header(header)?;
            saw_regular = true;
        }
    }
    let status = status.ok_or(ServerError::MalformedMessage)?;
    if status.value.len() != 3 || !status.value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ServerError::MalformedMessage);
    }
    let status = status
        .value
        .parse::<u16>()
        .map_err(|_| ServerError::MalformedMessage)?;
    if status == 101 {
        return Err(ServerError::MalformedMessage);
    }
    Ok(status)
}

fn window_update_increment(payload: &[u8]) -> Result<u32, ServerError> {
    if payload.len() != 4 {
        return Err(ServerError::InvalidFrame);
    }
    let mut increment = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    increment &= 0x7fff_ffff;
    if increment == 0 {
        return Err(ServerError::InvalidFrame);
    }
    Ok(increment)
}

fn data_payload(flags: u8, payload: &[u8]) -> Result<&[u8], ServerError> {
    strip_padding(flags, payload)
}

fn headers_payload(_stream_id: u32, flags: u8, payload: &[u8]) -> Result<&[u8], ServerError> {
    let mut payload = strip_padding(flags, payload)?;
    if flags & 0x20 != 0 {
        if payload.len() < 5 {
            return Err(ServerError::InvalidFrame);
        }
        payload = &payload[5..];
    }
    Ok(payload)
}

fn priority_payload(stream_id: u32, payload: &[u8]) -> Result<(), ServerError> {
    if stream_id == 0 || payload.len() != 5 {
        return Err(ServerError::InvalidFrame);
    }
    validate_priority_dependency(stream_id, payload)
}

fn validate_priority_dependency(stream_id: u32, payload: &[u8]) -> Result<(), ServerError> {
    let dependency =
        u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]) & 0x7fff_ffff;
    if dependency == stream_id {
        return Err(ServerError::InvalidFrame);
    }
    Ok(())
}

fn strip_padding(flags: u8, payload: &[u8]) -> Result<&[u8], ServerError> {
    if flags & 0x8 == 0 {
        return Ok(payload);
    }
    let Some((&pad_len, rest)) = payload.split_first() else {
        return Err(ServerError::InvalidFrame);
    };
    let pad_len = pad_len as usize;
    if pad_len > rest.len() {
        return Err(ServerError::InvalidFrame);
    }
    Ok(&rest[..rest.len() - pad_len])
}

fn h2_error_from_server_error(error: ServerError, head: Option<H2FrameHead>) -> H2ProtocolError {
    let stream_id = head.map(|head| head.stream_id).unwrap_or(0);
    let frame_type = head.map(|head| head.frame_type);
    let connection_scope = matches!(
        error,
        ServerError::InvalidHpack
            | ServerError::UnsupportedHpack
            | ServerError::InvalidOutboundState
    ) || stream_id == 0
        || matches!(
            frame_type,
            Some(H2FrameType::Settings | H2FrameType::Ping | H2FrameType::Goaway)
        );
    let scope = if connection_scope {
        H2ErrorScope::Connection
    } else {
        H2ErrorScope::Stream(stream_id)
    };
    let code = match error {
        ServerError::InvalidHpack | ServerError::UnsupportedHpack => H2ErrorCode::CompressionError,
        ServerError::FlowControlViolation => H2ErrorCode::FlowControlError,
        ServerError::InvalidOutboundState => H2ErrorCode::InternalError,
        ServerError::InvalidFrame => {
            if matches!(
                head.map(|head| head.frame_type),
                Some(H2FrameType::WindowUpdate)
            ) {
                H2ErrorCode::FlowControlError
            } else {
                H2ErrorCode::ProtocolError
            }
        }
        ServerError::NeedMore => H2ErrorCode::ProtocolError,
        ServerError::MalformedMessage => H2ErrorCode::ProtocolError,
        ServerError::InvalidPreface
        | ServerError::UnsupportedAlpnProtocol
        | ServerError::Parse
        | ServerError::PeerReset { .. }
        | ServerError::PeerGoaway { .. }
        | ServerError::InvalidRequest
        | ServerError::InvalidResponse
        | ServerError::InvalidHeader
        | ServerError::InvalidContentLength
        | ServerError::TooManyHeaders { .. }
        | ServerError::BodyTooLarge { .. }
        | ServerError::HeaderTooLarge { .. }
        | ServerError::UnsupportedMethod
        | ServerError::UnsupportedVersion
        | ServerError::UnsupportedTransferEncoding => H2ErrorCode::ProtocolError,
    };
    let debug = match error {
        ServerError::NeedMore => "HTTP/2 frame needs more input",
        ServerError::InvalidHpack => "invalid HTTP/2 HPACK block",
        ServerError::UnsupportedHpack => "unsupported HTTP/2 HPACK representation",
        ServerError::InvalidPreface => "invalid HTTP/2 client preface",
        ServerError::UnsupportedAlpnProtocol => "unsupported TLS ALPN protocol",
        ServerError::InvalidFrame => "invalid HTTP/2 frame",
        ServerError::FlowControlViolation => "HTTP/2 flow-control operation was rejected",
        ServerError::InvalidOutboundState => "invalid HTTP/2 outbound state",
        _ => "invalid HTTP/2 protocol state",
    };
    H2ProtocolError {
        scope,
        code,
        debug,
        hpack_error: None,
        http_error_kind: match error {
            ServerError::HeaderTooLarge { .. } => Some(crate::HttpErrorKind::HeadersTooLarge),
            ServerError::TooManyHeaders { .. } => Some(crate::HttpErrorKind::TooManyHeaders),
            ServerError::BodyTooLarge { .. } => Some(crate::HttpErrorKind::BodyTooLarge),
            _ => None,
        },
        limit: match error {
            ServerError::HeaderTooLarge { limit, actual }
            | ServerError::TooManyHeaders { limit, actual }
            | ServerError::BodyTooLarge { limit, actual } => {
                Some(crate::LimitViolation::new(limit, Some(actual)))
            }
            _ => None,
        },
    }
}

fn compatibility_error(original: Option<ServerError>, protocol: H2ProtocolError) -> ServerError {
    match original {
        Some(
            error @ (ServerError::InvalidContentLength
            | ServerError::HeaderTooLarge { .. }
            | ServerError::TooManyHeaders { .. }
            | ServerError::BodyTooLarge { .. }),
        ) => error,
        _ => protocol.into(),
    }
}

fn recoverable_h2_message_error(error: &ServerError) -> bool {
    matches!(
        error,
        ServerError::Parse
            | ServerError::InvalidRequest
            | ServerError::InvalidResponse
            | ServerError::InvalidHeader
            | ServerError::InvalidContentLength
            | ServerError::TooManyHeaders { .. }
            | ServerError::BodyTooLarge { .. }
            | ServerError::HeaderTooLarge { .. }
            | ServerError::UnsupportedMethod
            | ServerError::UnsupportedVersion
            | ServerError::UnsupportedTransferEncoding
            | ServerError::FlowControlViolation
            | ServerError::MalformedMessage
    )
}

fn validate_max_frame_size(value: u32) -> Result<usize, ServerError> {
    let value = value as usize;
    if !(H2_MIN_MAX_FRAME_SIZE..=H2_MAX_MAX_FRAME_SIZE).contains(&value) {
        return Err(ServerError::InvalidFrame);
    }
    Ok(value)
}

fn encode_connection_header_frames_by<'a>(
    encoder: &mut crate::hpack::Encoder,
    stream_id: u32,
    field_count: usize,
    field_at: impl Fn(usize) -> H2RawHeaderRef<'a> + Copy,
    end_stream: bool,
    max_frame_size: usize,
    reserved_tail: usize,
) -> Result<Vec<u8>, H2HpackError> {
    let maximum_block = encoder
        .maximum_output_len_by(field_count, |index| {
            let field = field_at(index);
            crate::hpack::HeaderFieldRef {
                name: field.name,
                value: field.value,
                sensitive: field.sensitive,
            }
        })
        .map_err(H2HpackError::from)?;
    let maximum_frames = maximum_block.max(1).div_ceil(max_frame_size);
    let maximum_output = maximum_block
        .checked_add(
            maximum_frames
                .checked_mul(9)
                .ok_or(H2HpackError::AllocationFailed)?,
        )
        .and_then(|length| length.checked_add(reserved_tail))
        .ok_or(H2HpackError::AllocationFailed)?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(maximum_output)
        .map_err(|_| H2HpackError::AllocationFailed)?;
    let block = encoder
        .encode_by(field_count, |index| {
            let field = field_at(index);
            crate::hpack::HeaderFieldRef {
                name: field.name,
                value: field.value,
                sensitive: field.sensitive,
            }
        })
        .map_err(H2HpackError::from)?;
    encode_header_block_frames(stream_id, &block, end_stream, max_frame_size, &mut output);
    Ok(output)
}

fn encode_header_block_frames(
    stream_id: u32,
    block: &[u8],
    end_stream: bool,
    max_frame_size: usize,
    output: &mut Vec<u8>,
) {
    if block.is_empty() {
        H2Frame::encode_header(
            H2FrameType::Headers,
            0x4 | u8::from(end_stream),
            stream_id,
            0,
            output,
        );
        return;
    }
    let mut chunks = block.chunks(max_frame_size).peekable();
    let first = chunks.next().expect("nonempty block has a first chunk");
    H2Frame::encode_header(
        H2FrameType::Headers,
        u8::from(end_stream) | if chunks.peek().is_none() { 0x4 } else { 0 },
        stream_id,
        first.len(),
        output,
    );
    output.extend_from_slice(first);
    while let Some(chunk) = chunks.next() {
        H2Frame::encode_header(
            H2FrameType::Continuation,
            if chunks.peek().is_none() { 0x4 } else { 0 },
            stream_id,
            chunk.len(),
            output,
        );
        output.extend_from_slice(chunk);
    }
}

fn data_frames_encoded_len(payload_len: usize, max_frame_size: usize) -> usize {
    if payload_len == 0 {
        0
    } else {
        payload_len.saturating_add(payload_len.div_ceil(max_frame_size).saturating_mul(9))
    }
}

fn decimal_bytes(mut value: usize, storage: &mut [u8; 20]) -> &[u8] {
    let mut start = storage.len();
    loop {
        start -= 1;
        storage[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            return &storage[start..];
        }
    }
}

fn encode_data_frames(
    stream_id: u32,
    payload: &[u8],
    end_stream: bool,
    max_frame_size: usize,
    output: &mut Vec<u8>,
) {
    let mut remaining = payload;
    while !remaining.is_empty() {
        let frame_len = remaining.len().min(max_frame_size);
        let (chunk, rest) = remaining.split_at(frame_len);
        remaining = rest;
        H2Frame::encode_header(
            H2FrameType::Data,
            if end_stream && remaining.is_empty() {
                0x1
            } else {
                0
            },
            stream_id,
            chunk.len(),
            output,
        );
        output.extend_from_slice(chunk);
    }
}

#[cfg(test)]
fn encode_hpack_request_headers(
    method: &str,
    scheme: &str,
    authority: &str,
    path: &str,
    headers: &[Header<'_>],
) -> Vec<u8> {
    let mut fields = Vec::with_capacity(headers.len() + 4);
    fields.push(H2RawHeader::new(":method", method));
    fields.push(H2RawHeader::new(":scheme", scheme));
    fields.push(H2RawHeader::new(":authority", authority));
    fields.push(H2RawHeader::new(":path", path));
    fields.extend(
        headers
            .iter()
            .map(|header| H2RawHeader::new(header.name, header.value)),
    );
    encode_hpack_raw_header_block(&fields)
}

#[cfg(test)]
fn encode_hpack_header_block(headers: &[Header<'_>]) -> Vec<u8> {
    let headers = headers
        .iter()
        .map(|header| H2RawHeader::new(header.name.as_bytes(), header.value.as_bytes()))
        .collect::<Vec<_>>();
    encode_hpack_raw_header_block(&headers)
}

#[cfg(test)]
fn encode_hpack_raw_header_block(headers: &[H2RawHeader]) -> Vec<u8> {
    H2HeaderBlockEncoder::new().encode(headers)
}

#[cfg(test)]
fn encode_hpack_response_headers(
    status: u16,
    content_length: usize,
    headers: &[Header<'_>],
) -> Vec<u8> {
    let mut fields = Vec::with_capacity(headers.len() + 2);
    let status = status.to_string();
    fields.push(H2RawHeader::new(":status", status));
    let content_length = content_length.to_string();
    fields.push(H2RawHeader::new("content-length", content_length));
    fields.extend(
        headers
            .iter()
            .map(|header| H2RawHeader::new(header.name, header.value)),
    );
    encode_hpack_raw_header_block(&fields)
}

#[cfg(test)]
fn hpack_push_string(out: &mut Vec<u8>, value: &[u8]) {
    hpack_push_prefixed_integer(out, value.len(), 0x7f, 0x00);
    out.extend_from_slice(value);
}

#[cfg(test)]
fn hpack_push_prefixed_integer(out: &mut Vec<u8>, value: usize, prefix_mask: u8, first_bits: u8) {
    let prefix_mask = prefix_mask as usize;
    if value < prefix_mask {
        out.push(first_bits | value as u8);
        return;
    }
    out.push(first_bits | prefix_mask as u8);
    let mut remaining = value - prefix_mask;
    while remaining >= 128 {
        out.push(((remaining & 0x7f) as u8) | 0x80);
        remaining >>= 7;
    }
    out.push(remaining as u8);
}

fn push_decimal(output: &mut Vec<u8>, mut value: usize) {
    let mut reversed = [0_u8; 20];
    let mut len = 0usize;
    loop {
        reversed[len] = b'0' + (value % 10) as u8;
        len += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    for index in (0..len).rev() {
        output.push(reversed[index]);
    }
}

fn push_hex(output: &mut Vec<u8>, mut value: usize) {
    let mut reversed = [0_u8; usize::BITS as usize / 4];
    let mut len = 0usize;
    loop {
        let digit = (value & 0xf) as u8;
        reversed[len] = match digit {
            0..=9 => b'0' + digit,
            _ => b'a' + (digit - 10),
        };
        len += 1;
        value >>= 4;
        if value == 0 {
            break;
        }
    }
    for index in (0..len).rev() {
        output.push(reversed[index]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HttpErrorKind;

    fn encoded_frame(
        frame_type: H2FrameType,
        flags: u8,
        stream_id: u32,
        payload: Vec<u8>,
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        H2Frame {
            frame_type,
            flags,
            stream_id,
            payload,
        }
        .encode(&mut bytes);
        bytes
    }

    /// RFC 9113 section 8.4: only servers promise streams, so a client-sent
    /// PUSH_PROMISE is a connection error of type PROTOCOL_ERROR. It must not
    /// be mistaken for an unknown extension frame, which would be ignored.
    #[test]
    fn h2_rejects_a_push_promise_sent_by_the_client() {
        let mut server = h2_server_after_preface();

        // Promised stream 2 followed by an indexed header block.
        let mut payload = 2u32.to_be_bytes().to_vec();
        payload.extend_from_slice(&[0x82, 0x86, 0x84]);
        let frame = encoded_frame(H2FrameType::PushPromise, 0x4, 1, payload);

        assert_eq!(server.accept_event(&frame), Err(ServerError::InvalidFrame));

        let reported = server
            .take_reported_protocol_error()
            .expect("a client PUSH_PROMISE must report a protocol error");
        assert_eq!(reported.error_code(), H2ErrorCode::ProtocolError);
        assert_eq!(reported.scope(), H2ErrorScope::Connection);
    }

    /// PUSH_PROMISE has to decode as its own frame type. Decoding it as an
    /// unknown extension type is what previously caused it to be ignored.
    #[test]
    fn h2_frame_type_five_decodes_as_push_promise() {
        assert_eq!(H2FrameType::from_raw(5), H2FrameType::PushPromise);
        assert_eq!(H2FrameType::from_u8(5), Ok(H2FrameType::PushPromise));
        assert_eq!(H2FrameType::PushPromise.as_u8(), 5);
    }

    /// A stream the peer reset gets no reset-tolerance window. Frames that
    /// arrive afterwards cannot be in flight, so they are protocol violations
    /// rather than a benign race.
    #[test]
    fn h2_data_after_a_peer_reset_is_a_stream_closed_error() {
        let mut server = h2_server_after_preface();
        let headers = encode_hpack_request_headers("POST", "https", "example.com", "/", &[]);
        assert!(
            server
                .accept_event(&encoded_frame(H2FrameType::Headers, 0x4, 1, headers))
                .is_ok()
        );

        let reset = encoded_frame(H2FrameType::RstStream, 0, 1, 8u32.to_be_bytes().to_vec());
        assert!(server.accept_event(&reset).is_ok());

        let data = encoded_frame(H2FrameType::Data, 0, 1, b"late".to_vec());
        assert_eq!(server.accept_event(&data), Err(ServerError::InvalidFrame));

        let reported = server
            .take_reported_protocol_error()
            .expect("DATA on a peer-reset stream must report a protocol error");
        assert_eq!(reported.error_code(), H2ErrorCode::StreamClosed);
        assert_eq!(reported.scope(), H2ErrorScope::Stream(1));
    }

    /// A stream this side reset keeps its tolerance window, because the peer
    /// may already have DATA in flight that it could not have withheld.
    #[test]
    fn h2_data_after_a_local_reset_is_discarded() {
        let mut server = h2_server_after_preface();
        let headers = encode_hpack_request_headers("POST", "https", "example.com", "/", &[]);
        assert!(
            server
                .accept_event(&encoded_frame(H2FrameType::Headers, 0x4, 1, headers))
                .is_ok()
        );
        server.close_stream(1);

        let data = encoded_frame(H2FrameType::Data, 0, 1, b"race".to_vec());
        let (event, _, _) = server.accept_event(&data).unwrap();

        assert_eq!(
            event,
            Some(H2StreamEvent::DiscardedData {
                stream_id: 1,
                flow_control_len: 4,
            })
        );
    }

    #[test]
    fn outbound_acknowledgement_rejects_invalid_state_distinctly() {
        let mut server = H2Server::default();

        assert_eq!(
            server.acknowledge_outbound_block(H2OutboundCommit { sequence: 1 }),
            Err(ServerError::InvalidOutboundState)
        );
    }

    fn take_server_output(server: &mut H2Server, commit: H2OutboundCommit) -> Vec<u8> {
        let block = server.next_outbound_block().unwrap();
        assert_eq!(block.commit(), commit);
        let bytes = block.bytes().to_vec();
        server.acknowledge_outbound_block(commit).unwrap();
        bytes
    }

    fn take_client_output(client: &mut H2Client, commit: H2OutboundCommit) -> Vec<u8> {
        let block = client.next_outbound_block().unwrap();
        assert_eq!(block.commit(), commit);
        let bytes = block.bytes().to_vec();
        client.acknowledge_outbound_block(commit).unwrap();
        bytes
    }

    fn open_stream_bytes(
        client: &mut H2Client,
        method: &str,
        scheme: &str,
        authority: &str,
        path: &str,
        headers: &[Header<'_>],
        end_stream: bool,
    ) -> Result<(u32, Vec<u8>), ServerError> {
        let fields = headers
            .iter()
            .map(|header| H2HeaderField {
                name: header.name.as_bytes().to_vec(),
                value: header.value.as_bytes().to_vec(),
                sensitive: false,
            })
            .collect::<Vec<_>>();
        let (stream_id, commit) = client
            .open_stream_with_raw_headers(method, scheme, authority, path, &fields, end_stream)
            .map_err(ServerError::from)?;
        Ok((stream_id, take_client_output(client, commit)))
    }

    fn response_headers_bytes(
        server: &mut H2Server,
        stream_id: u32,
        status: u16,
        headers: &[Header<'_>],
        end_stream: bool,
    ) -> Vec<u8> {
        let fields = headers
            .iter()
            .map(|header| H2HeaderField {
                name: header.name.as_bytes().to_vec(),
                value: header.value.as_bytes().to_vec(),
                sensitive: false,
            })
            .collect::<Vec<_>>();
        let commit = server
            .response_headers_frame_with_raw_headers(stream_id, status, &fields, end_stream)
            .unwrap();
        take_server_output(server, commit)
    }

    fn response_frames_bytes(
        server: &mut H2Server,
        stream_id: u32,
        status: u16,
        headers: &[Header<'_>],
        body: &[u8],
        end_stream: bool,
    ) -> Vec<u8> {
        let fields = headers
            .iter()
            .map(|header| H2HeaderField {
                name: header.name.as_bytes().to_vec(),
                value: header.value.as_bytes().to_vec(),
                sensitive: false,
            })
            .collect::<Vec<_>>();
        let commit = server
            .response_frames_with_raw_headers(stream_id, status, &fields, body, end_stream)
            .unwrap();
        take_server_output(server, commit)
    }

    fn trailers_bytes(client: &mut H2Client, stream_id: u32, headers: &[Header<'_>]) -> Vec<u8> {
        let fields = headers
            .iter()
            .map(|header| H2HeaderField {
                name: header.name.as_bytes().to_vec(),
                value: header.value.as_bytes().to_vec(),
                sensitive: false,
            })
            .collect::<Vec<_>>();
        let commit = client
            .trailers_frame_with_raw_headers(stream_id, &fields)
            .unwrap();
        take_client_output(client, commit)
    }

    #[test]
    fn http1_parses_get_and_emits_response() {
        let mut server = Http1Server;
        let mut headers = [httparse::EMPTY_HEADER; 8];
        let (request, consumed) = server
            .parse_request(
                b"GET /index.html HTTP/1.1\r\nhost: localhost\r\n\r\n",
                &mut headers,
            )
            .unwrap();
        assert_eq!(request.method, "GET");
        assert_eq!(request.target, "/index.html");
        assert_eq!(consumed, 45);
        let response = Http1Server::response_bytes(
            ServerResponse {
                status: 200,
                reason: "OK",
                headers: &[Header::new("content-type", "text/plain")],
                body: b"hello",
            },
            false,
        );
        assert_eq!(
            std::str::from_utf8(&response).unwrap(),
            "HTTP/1.1 200 OK\r\ncontent-length: 5\r\ncontent-type: text/plain\r\n\r\nhello"
        );
    }

    #[test]
    fn http1_head_omits_response_body() {
        let mut server = Http1Server;
        let mut headers = [httparse::EMPTY_HEADER; 4];
        let (request, _) = server
            .parse_request(
                b"HEAD / HTTP/1.1\r\nhost: example.test\r\n\r\n",
                &mut headers,
            )
            .unwrap();
        assert_eq!(request.method, "HEAD");
        let response = Http1Server::response_bytes(
            ServerResponse {
                status: 200,
                reason: "OK",
                headers: &[],
                body: b"hello",
            },
            true,
        );
        assert!(
            std::str::from_utf8(&response)
                .unwrap()
                .ends_with("\r\n\r\n")
        );
        assert!(!std::str::from_utf8(&response).unwrap().ends_with("hello"));
    }

    #[test]
    fn http1_can_emit_head_and_body_chunks_separately() {
        let head = Http1Server::response_head_bytes(
            200,
            "OK",
            &[Header::new("content-type", "text/plain")],
            11,
        );
        assert_eq!(
            std::str::from_utf8(&head).unwrap(),
            "HTTP/1.1 200 OK\r\ncontent-length: 11\r\ncontent-type: text/plain\r\n\r\n"
        );
        assert_eq!(Http1Server::response_body_chunk(b"hello"), b"hello");
        assert_eq!(Http1Server::response_body_chunk(b" world"), b" world");
    }

    #[test]
    fn http1_general_request_head_supports_methods_versions_headers_and_body_modes() {
        let mut server = Http1Server;
        let mut headers = [httparse::EMPTY_HEADER; 8];
        let (request, consumed) = server
            .parse_request_head(
                b"POST /upload HTTP/1.0\r\nhost: localhost\r\ncontent-length: 5\r\n\r\nhello",
                &mut headers,
            )
            .unwrap();

        assert_eq!(request.method, "POST");
        assert_eq!(request.target, "/upload");
        assert_eq!(request.version, 0);
        assert_eq!(request.headers[0].name, "host");
        assert_eq!(request.headers[1].value, b"5");
        assert_eq!(consumed, 61);
        assert_eq!(
            Http1Server::request_body_kind(request.headers).unwrap(),
            Http1BodyKind::ContentLength(5)
        );
    }

    #[test]
    fn http1_body_kind_helpers_match_stack_http_framing_cases() {
        let mut server = Http1Server;
        let mut headers = [httparse::EMPTY_HEADER; 8];
        let (request, _) = server
            .parse_request_head(
                b"PUT / HTTP/1.1\r\nhost: example.test\r\ntransfer-encoding: chunked\r\n\r\n",
                &mut headers,
            )
            .unwrap();
        assert_eq!(
            Http1Server::request_body_kind(request.headers).unwrap(),
            Http1BodyKind::Chunked
        );

        let mut response_headers = [httparse::EMPTY_HEADER; 8];
        let (response, _) = server
            .parse_response_head(
                b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n",
                &mut response_headers,
            )
            .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(
            Http1Server::response_body_kind("GET", response.status, response.headers).unwrap(),
            Http1BodyKind::Eof
        );
        assert_eq!(
            Http1Server::response_body_kind("HEAD", response.status, response.headers).unwrap(),
            Http1BodyKind::Empty
        );
    }

    #[test]
    fn http1_rejects_conflicting_length_and_transfer_encoding() {
        let mut server = Http1Server;
        let mut headers = [httparse::EMPTY_HEADER; 8];
        let (request, _) = server
            .parse_request_head(
                b"POST / HTTP/1.1\r\nhost: example.test\r\ncontent-length: 1\r\ncontent-length: 2\r\n\r\nx",
                &mut headers,
            )
            .unwrap();
        assert_eq!(
            Http1Server::request_body_kind(request.headers),
            Err(ServerError::InvalidContentLength)
        );

        let mut headers = [httparse::EMPTY_HEADER; 8];
        let (request, _) = server
            .parse_request_head(
                b"POST / HTTP/1.1\r\nhost: example.test\r\ncontent-length: 1\r\ntransfer-encoding: chunked\r\n\r\n0\r\n\r\n",
                &mut headers,
            )
            .unwrap();
        assert_eq!(
            Http1Server::request_body_kind(request.headers),
            Err(ServerError::InvalidContentLength)
        );
    }

    #[test]
    fn http1_chunked_body_events_cover_chunks_trailers_and_limits() {
        let mut body = Http1ChunkedBody::new(6);
        assert_eq!(
            body.next_event(b"3\r\nabc\r\n").unwrap(),
            Http1ChunkedEvent::Chunk {
                chunk: b"abc",
                consumed: 8
            }
        );
        assert_eq!(
            body.next_event(b"3;extension=value\r\ndef\r\n").unwrap(),
            Http1ChunkedEvent::Chunk {
                chunk: b"def",
                consumed: 24
            }
        );
        assert_eq!(
            body.next_event(b"0\r\nx-trailer: yes\r\n\r\n").unwrap(),
            Http1ChunkedEvent::Complete { consumed: 21 }
        );

        let mut limited = Http1ChunkedBody::new(2);
        assert_eq!(
            limited.next_event(b"3\r\n"),
            Err(ServerError::BodyTooLarge {
                limit: 2,
                actual: 3
            })
        );
        let mut partial = Http1ChunkedBody::new(8);
        assert_eq!(
            partial.next_event(b"3\r\nab").unwrap(),
            Http1ChunkedEvent::NeedInput
        );
    }

    #[test]
    fn http1_request_trailers_discard_forbidden_fields_but_responses_reject_them() {
        let input = b"0\r\ncontent-length: 0\r\n\r\n";
        let mut request_headers = [httparse::EMPTY_HEADER; 4];
        let (fields, consumed) =
            Http1Codec::parse_request_chunked_trailers(input, &mut request_headers).unwrap();
        assert!(fields.is_empty());
        assert_eq!(consumed, input.len());

        let mut response_headers = [httparse::EMPTY_HEADER; 4];
        assert_eq!(
            Http1Codec::parse_chunked_trailers(input, &mut response_headers),
            Err(ServerError::MalformedMessage)
        );
    }

    #[test]
    fn http1_chunked_body_rejects_malformed_size_suffix_and_oversized_metadata() {
        for input in [
            b"3 garbage\r\nabc\r\n".as_slice(),
            b"0 garbage\r\n\r\n".as_slice(),
            b"1\tgarbage\r\nx\r\n".as_slice(),
        ] {
            assert_eq!(
                Http1ChunkedBody::new(8).next_event(input),
                Err(ServerError::Parse)
            );
        }

        assert_eq!(
            Http1ChunkedBody::with_metadata_limits(8, 4, 8).next_event(b"12345"),
            Err(ServerError::HeaderTooLarge {
                limit: 4,
                actual: 5
            })
        );
        assert_eq!(
            Http1ChunkedBody::with_metadata_limits(8, 16, 4).next_event(b"0\r\nabcde"),
            Err(ServerError::HeaderTooLarge {
                limit: 4,
                actual: 5
            })
        );
        assert_eq!(
            Http1ChunkedBody::new(8)
                .next_event(b"3;extension=value\r\nabc\r\n")
                .unwrap(),
            Http1ChunkedEvent::Chunk {
                chunk: b"abc",
                consumed: 24
            }
        );
    }

    #[test]
    fn http1_response_head_bytes_supports_version_and_optional_content_length() {
        let head = Http1Server::response_head_bytes_with_version(
            0,
            204,
            "No Content",
            &[Header::new("connection", "close")],
            None,
        )
        .unwrap();

        assert_eq!(
            std::str::from_utf8(&head).unwrap(),
            "HTTP/1.0 204 No Content\r\nconnection: close\r\n\r\n"
        );
        assert_eq!(
            Http1Server::response_head_bytes_with_version(2, 200, "OK", &[], None),
            Err(ServerError::UnsupportedVersion)
        );
    }

    #[test]
    fn http1_chunked_body_bytes_serializes_data_and_terminal_chunk() {
        assert_eq!(
            std::str::from_utf8(&Http1Server::chunked_body_bytes(b"hello world")).unwrap(),
            "b\r\nhello world\r\n0\r\n\r\n"
        );
        assert_eq!(
            std::str::from_utf8(&Http1Server::chunked_body_bytes(b"")).unwrap(),
            "0\r\n\r\n"
        );
        let mut appended = Vec::from(b"prefix".as_slice());
        Http1Server::append_chunked_body_bytes(&mut appended, b"abc");
        assert_eq!(
            std::str::from_utf8(&appended).unwrap(),
            "prefix3\r\nabc\r\n0\r\n\r\n"
        );
        assert_eq!(
            std::str::from_utf8(&Http1Server::chunked_body_prefix(11)).unwrap(),
            "b\r\n"
        );
        assert_eq!(Http1Server::chunked_body_suffix(false), b"\r\n0\r\n\r\n");
    }

    #[test]
    fn http1_rejects_unsupported_method() {
        let mut server = Http1Server;
        let mut headers = [httparse::EMPTY_HEADER; 4];
        assert_eq!(
            server.parse_request(
                b"POST / HTTP/1.1\r\nhost: example.test\r\n\r\n",
                &mut headers
            ),
            Err(ServerError::UnsupportedMethod)
        );
    }

    #[test]
    fn http1_reports_malformed_request() {
        let mut server = Http1Server;
        let mut headers = [httparse::EMPTY_HEADER; 4];
        assert!(matches!(
            server.parse_request(b"GET /\r\n\r\n", &mut headers),
            Err(ServerError::Parse | ServerError::InvalidRequest | ServerError::NeedMore)
        ));
    }

    #[test]
    fn h2_accepts_preface_settings_and_get_headers() {
        let mut input = Vec::new();
        input.extend_from_slice(CLIENT_PREFACE);
        H2Frame {
            frame_type: H2FrameType::Settings,
            flags: 0,
            stream_id: 0,
            payload: Vec::new(),
        }
        .encode(&mut input);
        H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0x5,
            stream_id: 1,
            payload: vec![0x82, 0x86, 0x84],
        }
        .encode(&mut input);

        let mut server = H2Server::default();
        let (request, consumed, output) = server.accept(&input).unwrap();
        assert_eq!(consumed, input.len());
        let request = request.unwrap();
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/");
        assert!(!output.is_empty());

        let response = response_frames_bytes(&mut server, 1, 200, &[], b"hello", true);
        let (headers, used) = H2Frame::decode(&response).unwrap();
        assert_eq!(headers.frame_type, H2FrameType::Headers);
        assert_eq!(headers.flags, 0x4);
        let (data, _) = H2Frame::decode(&response[used..]).unwrap();
        assert_eq!(data.frame_type, H2FrameType::Data);
        assert_eq!(data.flags, 0x1);
        assert_eq!(data.payload, b"hello");
    }

    #[test]
    fn h2_accepts_preface_split_across_inputs() {
        let mut client = h2_client_after_server_settings();
        let input = client.connection_preface();
        let mut server = H2Server::default();

        let (event, consumed, output) = server.accept_event(&input[..5]).unwrap();
        assert_eq!(event, None);
        assert_eq!(consumed, 0);
        assert!(output.is_empty());

        let (event, consumed, output) = server.accept_event(&input).unwrap();
        assert_eq!(event, None);
        assert_eq!(consumed, input.len());
        assert!(!output.is_empty());
    }

    #[test]
    fn h2_accepts_curl_huffman_headers_after_window_update() {
        let input = hex_bytes(
            "505249202a20485454502f322e300d0a0d0a534d0d0a0d0a\
             000012040000000000000300000064000400010000000200000000\
             0000040800000000003e7f000100002b0105000000018286418a\
             089d5c0b8170dc64010f048c627a46460d5485f2bce9a68f7a88\
             25b650c3cb842b8753032a2f2a",
        );
        let mut server = H2Server::default();

        let (request, consumed, output) = server.accept(&input).unwrap();

        assert_eq!(consumed, input.len());
        let request = request.unwrap();
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/hmac/index.html");
        assert!(!output.is_empty());
    }

    #[test]
    fn h2_response_data_is_split_by_peer_frame_size() {
        let mut server = H2Server::default();
        let body = vec![b'a'; 40 * 1024];
        let response = response_frames_bytes(&mut server, 1, 200, &[], &body, true);
        let (_headers, mut offset) = H2Frame::decode(&response).unwrap();
        let mut data_frames = 0usize;
        while offset < response.len() {
            let (frame, used) = H2Frame::decode(&response[offset..]).unwrap();
            assert_eq!(frame.frame_type, H2FrameType::Data);
            assert!(frame.payload.len() <= H2Settings::default().max_frame_size);
            data_frames += 1;
            offset += used;
            if offset == response.len() {
                assert_eq!(frame.flags, 0x1);
            } else {
                assert_eq!(frame.flags, 0);
            }
        }
        assert!(data_frames > 1);
    }

    #[test]
    fn h2_settings_decode_apply_and_encode_stack_compatible_fields() {
        let settings = [
            H2Setting::new(H2SettingId::EnablePush, 0),
            H2Setting::new(H2SettingId::InitialWindowSize, 1024),
            H2Setting::new(H2SettingId::MaxFrameSize, 32 * 1024),
            H2Setting::new(H2SettingId::MaxHeaderListSize, 65_536),
        ];
        let mut payload = Vec::new();
        H2Settings::encode_payload(&settings, &mut payload);
        let decoded = H2Settings::decode_payload(&payload).unwrap();

        let mut applied = H2Settings::default();
        applied.apply_all(&decoded).unwrap();
        assert!(!applied.enable_push);
        assert_eq!(applied.initial_window_size, 1024);
        assert_eq!(applied.max_frame_size, 32 * 1024);
        assert_eq!(applied.max_header_list_size, 65_536);
        assert_eq!(H2FrameType::from_u8(0), Ok(H2FrameType::Data));
        assert_eq!(H2FrameType::from_u8(0xff), Err(ServerError::InvalidFrame));
        assert_eq!(H2FrameType::from_raw(0xff), H2FrameType::Unknown(0xff));
    }

    #[test]
    fn h2_decode_preserves_unknown_extension_frames() {
        let mut frame = Vec::new();
        H2Frame {
            frame_type: H2FrameType::Unknown(0x21),
            flags: 0,
            stream_id: 0,
            payload: b"ext".to_vec(),
        }
        .encode(&mut frame);

        let (decoded, consumed) = H2Frame::decode(&frame).unwrap();

        assert_eq!(consumed, frame.len());
        assert_eq!(decoded.frame_type, H2FrameType::Unknown(0x21));
        assert_eq!(decoded.payload, b"ext");
    }

    #[test]
    fn h2_borrowed_decode_reuses_the_input_payload() {
        let mut input = Vec::new();
        H2Frame {
            frame_type: H2FrameType::Unknown(0x21),
            flags: 0x5,
            stream_id: 7,
            payload: b"borrowed-payload".to_vec(),
        }
        .encode(&mut input);

        let (frame, consumed) = H2FrameRef::decode(&input).unwrap();

        assert_eq!(consumed, input.len());
        assert_eq!(frame.frame_type, H2FrameType::Unknown(0x21));
        assert_eq!(frame.flags, 0x5);
        assert_eq!(frame.stream_id, 7);
        assert_eq!(frame.payload, b"borrowed-payload");
        assert_eq!(frame.payload.as_ptr(), input[9..].as_ptr());

        let owned = frame.to_owned();
        assert_eq!(owned.payload, frame.payload);
        assert_ne!(owned.payload.as_ptr(), frame.payload.as_ptr());
        assert_eq!(
            H2FrameRef::decode(&input[..input.len() - 1]),
            Err(ServerError::NeedMore)
        );
    }

    #[test]
    fn h2_accept_ignores_unknown_extension_frames_after_preface() {
        let mut server = h2_server_after_preface();
        let mut frame = Vec::new();
        H2Frame {
            frame_type: H2FrameType::Unknown(0x21),
            flags: 0,
            stream_id: 0,
            payload: b"ext".to_vec(),
        }
        .encode(&mut frame);

        assert_eq!(
            server.accept_event(&frame).unwrap(),
            (None, frame.len(), Vec::new())
        );
    }

    #[test]
    fn h2_decode_with_max_frame_size_rejects_oversized_payload() {
        let mut frame = Vec::new();
        H2Frame {
            frame_type: H2FrameType::Ping,
            flags: 0,
            stream_id: 0,
            payload: [0; 8].to_vec(),
        }
        .encode(&mut frame);

        assert_eq!(
            H2Frame::decode_with_max_frame_size(&frame, 7),
            Err(ServerError::InvalidFrame)
        );
        assert_eq!(
            H2Frame::decode_outcome_with_max_frame_size(&frame, 7),
            H2DecodeOutcome::Error(H2ProtocolError::connection(
                H2ErrorCode::FrameSizeError,
                "HTTP/2 frame exceeds configured maximum frame size",
            ))
        );
        assert_eq!(
            H2FrameRef::decode_outcome_with_max_frame_size(&frame, 7),
            H2FrameRefDecodeOutcome::Error(H2ProtocolError::connection(
                H2ErrorCode::FrameSizeError,
                "HTTP/2 frame exceeds configured maximum frame size",
            ))
        );
    }

    #[test]
    fn h2_accept_frame_typed_reports_scope_and_ignored_frames() {
        let mut server = h2_server_after_preface();
        let invalid_data = H2Frame {
            frame_type: H2FrameType::Data,
            flags: 0,
            stream_id: 0,
            payload: Vec::new(),
        };

        assert_eq!(
            server.accept_frame_typed(invalid_data).0,
            H2FrameOutcome::Error(H2ProtocolError::connection(
                H2ErrorCode::ProtocolError,
                "invalid HTTP/2 frame",
            ))
        );

        let unknown = H2Frame {
            frame_type: H2FrameType::Unknown(0x21),
            flags: 0,
            stream_id: 1,
            payload: b"ext".to_vec(),
        };

        assert_eq!(
            server.accept_frame_typed(unknown).0,
            H2FrameOutcome::Ignored
        );
    }

    #[test]
    fn h2_accept_frame_typed_reports_connection_scope_for_control_stream_id_errors() {
        for frame_type in [
            H2FrameType::Settings,
            H2FrameType::Ping,
            H2FrameType::Goaway,
        ] {
            let mut server = h2_server_after_preface();
            let payload = match frame_type {
                H2FrameType::Settings => Vec::new(),
                H2FrameType::Ping => [0; 8].to_vec(),
                H2FrameType::Goaway => [0_u32.to_be_bytes(), 0_u32.to_be_bytes()].concat(),
                _ => unreachable!(),
            };
            let frame = H2Frame {
                frame_type,
                flags: 0,
                stream_id: 1,
                payload,
            };

            let H2FrameOutcome::Error(error) = server.accept_frame_typed(frame).0 else {
                panic!("expected typed error");
            };

            assert_eq!(error.scope, H2ErrorScope::Connection);
            assert_eq!(error.code, H2ErrorCode::ProtocolError);
        }
    }

    #[test]
    fn h2_accept_frame_typed_rejects_priority_self_dependency() {
        let priority = H2Frame {
            frame_type: H2FrameType::Priority,
            flags: 0,
            stream_id: 1,
            payload: [1_u32.to_be_bytes().as_slice(), &[0]].concat(),
        };

        let mut server = h2_server_after_preface();
        let H2FrameOutcome::Error(server_error) = server.accept_frame_typed(priority.clone()).0
        else {
            panic!("expected server priority error");
        };
        assert_eq!(server_error.scope, H2ErrorScope::Stream(1));
        assert_eq!(server_error.code, H2ErrorCode::ProtocolError);

        let mut client = h2_client_after_server_settings();
        let H2FrameOutcome::Error(client_error) = client.accept_frame_typed(priority).0 else {
            panic!("expected client priority error");
        };
        assert_eq!(client_error.scope, H2ErrorScope::Stream(1));
        assert_eq!(client_error.code, H2ErrorCode::ProtocolError);
    }

    #[test]
    fn h2_accept_parses_padded_data_payload() {
        let mut server = h2_server_after_preface();
        let mut client = H2Client::default();
        let headers = open_stream_bytes(
            &mut client,
            "POST",
            "https",
            "example.com",
            "/svc",
            &[],
            false,
        )
        .unwrap()
        .1;
        assert!(matches!(
            server.accept_event(&headers).unwrap().0,
            Some(H2StreamEvent::RequestHeaders { .. })
        ));
        let mut data = Vec::new();
        H2Frame {
            frame_type: H2FrameType::Data,
            flags: 0x8,
            stream_id: 1,
            payload: [vec![2], b"hello".to_vec(), vec![0, 0]].concat(),
        }
        .encode(&mut data);

        let (event, consumed, _) = server.accept_event(&data).unwrap();

        assert_eq!(consumed, data.len());
        assert_eq!(
            event,
            Some(H2StreamEvent::Data {
                stream_id: 1,
                payload: b"hello".to_vec(),
                flow_control_len: 8,
                end_stream: false,
            })
        );
    }

    #[test]
    fn h2_accept_strips_headers_priority_metadata() {
        let mut server = h2_server_after_preface();
        let payload = encode_hpack_request_headers("GET", "https", "example.com", "/", &[]);
        let mut frame = Vec::new();
        H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0x24,
            stream_id: 1,
            payload: [[0; 5].as_slice(), payload.as_slice()].concat(),
        }
        .encode(&mut frame);

        let (event, consumed, _) = server.accept_event(&frame).unwrap();

        assert_eq!(consumed, frame.len());
        assert!(matches!(event, Some(H2StreamEvent::RequestHeaders { .. })));

        let payload = encode_hpack_request_headers("GET", "https", "example.com", "/", &[]);
        let mut self_dependency = Vec::new();
        H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0x24,
            stream_id: 1,
            payload: [1_u32.to_be_bytes().as_slice(), &[0], payload.as_slice()].concat(),
        }
        .encode(&mut self_dependency);

        let mut server = h2_server_after_preface();
        assert_eq!(
            server.accept_event(&self_dependency),
            Err(ServerError::InvalidFrame)
        );
    }

    #[test]
    fn h2_accept_assembles_headers_continuation() {
        let mut server = h2_server_after_preface();
        let block = encode_hpack_request_headers("GET", "https", "example.com", "/", &[]);
        let split = block.len() / 2;
        let mut headers = Vec::new();
        H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0,
            stream_id: 1,
            payload: block[..split].to_vec(),
        }
        .encode(&mut headers);
        let mut continuation = Vec::new();
        H2Frame {
            frame_type: H2FrameType::Continuation,
            flags: 0x4,
            stream_id: 1,
            payload: block[split..].to_vec(),
        }
        .encode(&mut continuation);

        assert_eq!(server.accept_event(&headers).unwrap().0, None);
        let (event, consumed, _) = server.accept_event(&continuation).unwrap();

        assert_eq!(consumed, continuation.len());
        assert!(matches!(event, Some(H2StreamEvent::RequestHeaders { .. })));
    }

    #[test]
    fn h2_accept_rejects_interleaved_or_mismatched_continuation() {
        let block = encode_hpack_request_headers("GET", "https", "example.com", "/", &[]);
        let mut headers = Vec::new();
        H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0,
            stream_id: 1,
            payload: block[..1].to_vec(),
        }
        .encode(&mut headers);

        let mut server = h2_server_after_preface();
        server.accept_event(&headers).unwrap();
        assert_eq!(
            server.accept_event(&h2_ping_frame()),
            Err(ServerError::InvalidFrame)
        );

        let mut continuation = Vec::new();
        H2Frame {
            frame_type: H2FrameType::Continuation,
            flags: 0x4,
            stream_id: 3,
            payload: block[1..].to_vec(),
        }
        .encode(&mut continuation);
        let mut server = h2_server_after_preface();
        server.accept_event(&headers).unwrap();
        assert_eq!(
            server.accept_event(&continuation),
            Err(ServerError::InvalidFrame)
        );
    }

    #[test]
    fn h2_typed_continuation_ordering_errors_are_connection_scoped() {
        let block = encode_hpack_request_headers("GET", "https", "example.com", "/", &[]);
        let headers = H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0,
            stream_id: 1,
            payload: block[..1].to_vec(),
        };
        let mut server = h2_server_after_preface();
        assert_eq!(
            server.accept_frame_typed(headers.clone()).0,
            H2FrameOutcome::Ignored
        );
        let H2FrameOutcome::Error(error) = server
            .accept_frame_typed(H2Frame {
                frame_type: H2FrameType::Data,
                flags: 0,
                stream_id: 1,
                payload: Vec::new(),
            })
            .0
        else {
            panic!("expected server ordering error");
        };
        assert_eq!(error.scope, H2ErrorScope::Connection);

        let mut server = h2_server_after_preface();
        assert_eq!(
            server.accept_frame_typed(headers).0,
            H2FrameOutcome::Ignored
        );
        let H2FrameOutcome::Error(error) = server
            .accept_frame_typed(H2Frame {
                frame_type: H2FrameType::Continuation,
                flags: 0x4,
                stream_id: 3,
                payload: block[1..].to_vec(),
            })
            .0
        else {
            panic!("expected server continuation mismatch error");
        };
        assert_eq!(error.scope, H2ErrorScope::Connection);

        let block = encode_hpack_response_headers(200, 0, &[]);
        let headers = H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0,
            stream_id: 1,
            payload: block[..1].to_vec(),
        };
        let mut client = h2_client_after_server_settings();
        assert_eq!(
            client.accept_frame_typed(headers).0,
            H2FrameOutcome::Ignored
        );
        let H2FrameOutcome::Error(error) = client
            .accept_frame_typed(H2Frame {
                frame_type: H2FrameType::Unknown(0x21),
                flags: 0,
                stream_id: 1,
                payload: Vec::new(),
            })
            .0
        else {
            panic!("expected client ordering error");
        };
        assert_eq!(error.scope, H2ErrorScope::Connection);
    }

    #[test]
    fn h2_typed_classify_continuation_ordering_errors_are_connection_scoped() {
        let block = encode_hpack_request_headers("GET", "https", "example.com", "/", &[]);
        let mut server = h2_server_after_preface();
        assert_eq!(
            server.classify_frame_typed(H2Frame {
                frame_type: H2FrameType::Headers,
                flags: 0,
                stream_id: 1,
                payload: block[..1].to_vec(),
            }),
            H2FrameOutcome::Ignored
        );

        let H2FrameOutcome::Error(error) = server.classify_frame_typed(H2Frame {
            frame_type: H2FrameType::Data,
            flags: 0,
            stream_id: 1,
            payload: Vec::new(),
        }) else {
            panic!("expected classify ordering error");
        };

        assert_eq!(error.scope, H2ErrorScope::Connection);
    }

    #[test]
    fn h2_accept_rejects_continuation_count_and_header_list_limits() {
        let mut server = H2Server::with_limits(H2Limits {
            max_continuation_frames: 0,
            ..H2Limits::default()
        })
        .unwrap();
        let mut client = H2Client::default();
        server.accept_event(&client.connection_preface()).unwrap();
        let block = encode_hpack_request_headers("GET", "https", "example.com", "/", &[]);
        let mut headers = Vec::new();
        H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0,
            stream_id: 1,
            payload: block[..1].to_vec(),
        }
        .encode(&mut headers);
        let mut continuation = Vec::new();
        H2Frame {
            frame_type: H2FrameType::Continuation,
            flags: 0x4,
            stream_id: 1,
            payload: block[1..].to_vec(),
        }
        .encode(&mut continuation);
        server.accept_event(&headers).unwrap();
        assert_eq!(
            server.accept_event(&continuation),
            Err(ServerError::InvalidFrame)
        );

        let mut server = H2Server::with_limits(H2Limits {
            max_header_list_size: 1,
            ..H2Limits::default()
        })
        .unwrap();
        let mut client = H2Client::default();
        server.accept_event(&client.connection_preface()).unwrap();
        let mut headers = Vec::new();
        H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0x4,
            stream_id: 1,
            payload: block,
        }
        .encode(&mut headers);
        assert_eq!(
            server.accept_event(&headers),
            Err(ServerError::HeaderTooLarge {
                limit: 1,
                actual: 177
            })
        );
    }

    #[test]
    fn h2_settings_payload_limit_rejects_too_many_entries() {
        let mut payload = Vec::new();
        H2Settings::encode_payload(
            &[
                H2Setting::new(H2SettingId::InitialWindowSize, 1024),
                H2Setting::new(H2SettingId::MaxHeaderListSize, 2048),
            ],
            &mut payload,
        );

        assert_eq!(
            H2Settings::decode_payload_with_limit(&payload, 1),
            Err(ServerError::InvalidFrame)
        );
    }

    #[test]
    fn hpack_encoder_emits_table_size_update_once() {
        let headers = [H2RawHeader::new("custom-key", "custom-value")];
        let mut encoder = H2HeaderBlockEncoder::new();
        encoder.set_max_table_size(0);

        let first = encoder.encode(&headers);
        let second = encoder.encode(&headers);

        assert_eq!(first.first().copied(), Some(0x20));
        assert_ne!(second.first().copied(), Some(0x20));
    }

    #[test]
    fn hpack_typed_error_preserves_decoder_category() {
        let mut server = h2_server_after_preface();
        let frame = H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0x4,
            stream_id: 1,
            payload: vec![0x80],
        };

        let H2FrameOutcome::Error(error) = server.accept_frame_typed(frame).0 else {
            panic!("expected HPACK error");
        };

        assert_eq!(error.code, H2ErrorCode::CompressionError);
        assert_eq!(error.scope, H2ErrorScope::Connection);
        assert_eq!(
            error.hpack_error,
            Some(H2HpackError::HeaderIndexOutOfBounds)
        );
    }

    #[test]
    fn hpack_discard_block_preserves_decoder_dynamic_table_state() {
        let mut server = h2_server_after_preface();
        let block = hpack_literal_with_indexing("custom-key", "custom-value");

        server.discard_hpack_block(&block).unwrap();
        let headers = server
            .header_codecs
            .as_mut()
            .unwrap()
            .inbound
            .decode(&[0xbe], usize::MAX)
            .unwrap();

        assert_eq!(
            headers,
            vec![H2HeaderField::new("custom-key", "custom-value")]
        );
    }

    #[test]
    fn h2_control_budget_refills_for_spaced_idle_ping() {
        let mut budget = H2ControlFrameBudget::default();
        let mut now = Instant::now();
        for _ in 0..200 {
            budget.record_control_at(H2FrameType::Ping, now).unwrap();
            now += H2_CONTROL_BUDGET_REFILL_INTERVAL;
        }
    }

    #[test]
    fn h2_control_budget_rejects_rapid_ping_flood() {
        let mut budget = H2ControlFrameBudget::default();
        let now = Instant::now();
        for _ in 0..H2_CONTROL_FRAME_BUDGET_LIMITS.ping {
            budget.record_control_at(H2FrameType::Ping, now).unwrap();
        }

        assert_eq!(
            budget.record_control_at(H2FrameType::Ping, now),
            Err(ServerError::InvalidFrame)
        );
    }

    #[test]
    fn h2_settings_events_report_initial_window_size_changes() {
        let mut server = H2Server::default();
        let mut client = H2Client::default();
        let preface = client.connection_preface();
        let (_event, _used, _output) = server.accept_event(&preface).unwrap();
        let mut settings = Vec::new();
        H2Frame {
            frame_type: H2FrameType::Settings,
            flags: 0,
            stream_id: 0,
            payload: {
                let mut payload = Vec::new();
                H2Settings::encode_payload(
                    &[H2Setting::new(H2SettingId::InitialWindowSize, 1024)],
                    &mut payload,
                );
                payload
            },
        }
        .encode(&mut settings);

        let (event, _used, output) = server.accept_event(&settings).unwrap();

        assert!(!output.is_empty());
        assert_eq!(
            event,
            Some(H2StreamEvent::Settings {
                initial_window_size: Some(H2InitialWindowSizeChange {
                    previous: 65_535,
                    current: 1024,
                })
            })
        );
    }

    #[test]
    fn h2_control_frame_budget_rejects_ping_flood_and_does_not_reset_after_headers() {
        let mut server = h2_server_after_preface();
        server.control_budget.ping = 2;
        let ping = h2_ping_frame();

        server.accept_event(&ping).unwrap();
        server.accept_event(&ping).unwrap();
        assert_eq!(server.accept_event(&ping), Err(ServerError::InvalidFrame));
        let diagnostics = server.control_diagnostics();
        assert_eq!(diagnostics.control_rejections, 1);
        assert_eq!(diagnostics.ping_rejections, 1);

        let mut client = H2Client::default();
        let (_stream_id, headers) = open_stream_bytes(
            &mut client,
            "POST",
            "http",
            "localhost",
            "/svc/Call",
            &[],
            true,
        )
        .unwrap();
        server.accept_event(&headers).unwrap();
        assert_eq!(server.accept_event(&ping), Err(ServerError::InvalidFrame));
    }

    #[test]
    fn h2_control_frame_budget_is_not_reset_by_immediate_external_progress() {
        let mut server = h2_server_after_preface();
        server.control_budget.ping = 1;
        let ping = h2_ping_frame();

        server.accept_event(&ping).unwrap();
        assert_eq!(server.accept_event(&ping), Err(ServerError::InvalidFrame));

        server.record_progress_frame();
        assert_eq!(server.accept_event(&ping), Err(ServerError::InvalidFrame));
    }

    #[test]
    fn h2_progress_replenishes_only_bounded_window_update_credits() {
        let mut client = h2_client_after_server_settings();
        client.control_budget.window_update = 0;
        client.control_budget.ping = 0;

        client.record_progress_frame();

        assert_eq!(
            client.control_budget.window_update,
            H2_WINDOW_UPDATE_CREDITS_PER_PROGRESS_FRAME
        );
        assert_eq!(client.control_budget.ping, 0);

        let update = h2_window_update_frame(0, 1);
        for _ in 0..H2_WINDOW_UPDATE_CREDITS_PER_PROGRESS_FRAME {
            client.accept(&update).unwrap();
        }
        assert_eq!(client.accept(&update), Err(ServerError::InvalidFrame));

        client.control_budget.window_update = H2_CONTROL_FRAME_BUDGET_LIMITS.window_update - 1;
        client.record_progress_frame();
        assert_eq!(
            client.control_budget.window_update,
            H2_CONTROL_FRAME_BUDGET_LIMITS.window_update
        );
    }

    #[test]
    fn h2_client_control_frame_budget_rejects_ping_flood_and_does_not_reset_after_headers() {
        let mut client = h2_client_after_server_settings();
        client.control_budget.ping = 2;
        let ping = h2_ping_frame();

        client.accept(&ping).unwrap();
        client.accept(&ping).unwrap();
        assert_eq!(client.accept(&ping), Err(ServerError::InvalidFrame));

        open_stream_bytes(&mut client, "GET", "https", "example.com", "/", &[], true).unwrap();
        let headers = h2_response_headers_frame(1);
        client.accept(&headers).unwrap();
        assert_eq!(client.accept(&ping), Err(ServerError::InvalidFrame));
    }

    #[test]
    fn h2_client_rejects_control_churn_and_priority() {
        let mut client = H2Client::default();
        client.control_budget.settings = 1;
        let settings = h2_settings_frame(&[]);
        client.accept(&settings).unwrap();
        assert_eq!(client.accept(&settings), Err(ServerError::InvalidFrame));

        let mut client = h2_client_after_server_settings();
        client.control_budget.window_update = 1;
        let update = h2_window_update_frame(0, 1);
        client.accept(&update).unwrap();
        assert_eq!(client.accept(&update), Err(ServerError::InvalidFrame));

        let mut client = h2_client_after_server_settings();
        client.control_budget.priority = 1;
        let priority = h2_priority_frame(1);
        assert_eq!(
            client.accept(&priority),
            Ok((None, priority.len(), Vec::new()))
        );
        assert_eq!(client.accept(&priority), Err(ServerError::InvalidFrame));
    }

    #[test]
    fn h2_control_frame_budget_rejects_settings_window_reset_and_goaway_churn() {
        let mut server = h2_server_after_preface();
        server.control_budget.settings = 1;
        let settings = h2_settings_frame(&[]);
        server.accept_event(&settings).unwrap();
        assert_eq!(
            server.accept_event(&settings),
            Err(ServerError::InvalidFrame)
        );

        let mut server = h2_server_after_preface();
        server.control_budget.window_update = 1;
        let update = h2_window_update_frame(0, 1);
        server.accept_event(&update).unwrap();
        assert_eq!(server.accept_event(&update), Err(ServerError::InvalidFrame));

        let mut server = h2_server_after_preface();
        let mut client = H2Client::default();
        let (_, headers) = open_stream_bytes(
            &mut client,
            "POST",
            "https",
            "example.com",
            "/svc",
            &[],
            false,
        )
        .unwrap();
        server.accept_event(&headers).unwrap();
        let (_, headers_2) = open_stream_bytes(
            &mut client,
            "POST",
            "https",
            "example.com",
            "/svc2",
            &[],
            false,
        )
        .unwrap();
        server.accept_event(&headers_2).unwrap();
        server.control_budget.reset = 1;
        let reset = h2_reset_frame(1);
        server.accept_event(&reset).unwrap();
        assert_eq!(
            server.accept_event(&h2_reset_frame(3)),
            Err(ServerError::InvalidFrame)
        );

        let mut server = h2_server_after_preface();
        server.control_budget.goaway = 1;
        let goaway = h2_goaway_frame(1);
        server.accept_event(&goaway).unwrap();
        assert_eq!(server.accept_event(&goaway), Err(ServerError::InvalidFrame));
    }

    #[test]
    fn h2_control_settings_ack_state_and_timer_intent() {
        let mut client = H2Client::default();
        assert_eq!(client.timer_intent(), None);
        let _preface = client.connection_preface();
        assert_eq!(
            client.timer_intent(),
            Some(H2TimerIntent::SettingsAckTimeout)
        );

        let mut ack = Vec::new();
        H2Frame {
            frame_type: H2FrameType::Settings,
            flags: 0x1,
            stream_id: 0,
            payload: Vec::new(),
        }
        .encode(&mut ack);

        client.accept(&h2_settings_frame(&[])).unwrap();
        client.accept(&ack).unwrap();
        assert_eq!(client.timer_intent(), None);
        assert_eq!(client.control_diagnostics().settings_acks, 1);
        assert_eq!(client.accept(&ack), Err(ServerError::InvalidFrame));
    }

    #[test]
    fn h2_goaway_frame_enforces_non_increasing_last_stream_id_and_reports_intent() {
        let mut server = H2Server::default();
        let first = server.goaway_frame(u32::MAX >> 1, 0).unwrap();
        assert!(!first.is_empty());
        assert_eq!(server.shutdown_intent(), H2ShutdownIntent::Close);
        assert!(server.goaway_frame(u32::MAX >> 1, 0).is_ok());
        assert_eq!(
            server.goaway_frame(u32::MAX, 0),
            Err(ServerError::InvalidFrame)
        );
        assert_eq!(server.control_diagnostics().goaways, 2);

        let mut server = h2_server_after_preface();
        let mut client = H2Client::default();
        let (_, headers) = open_stream_bytes(
            &mut client,
            "POST",
            "https",
            "example.com",
            "/svc",
            &[],
            false,
        )
        .unwrap();
        server.accept_event(&headers).unwrap();
        server.goaway_frame(1, 0).unwrap();
        assert_eq!(
            server.shutdown_intent(),
            H2ShutdownIntent::Drain { last_stream_id: 1 }
        );

        let mut server = h2_server_after_preface();
        let mut client = H2Client::default();
        let (_, headers) = open_stream_bytes(
            &mut client,
            "GET",
            "https",
            "example.com",
            "/svc",
            &[],
            true,
        )
        .unwrap();
        server.accept_event(&headers).unwrap();
        server.goaway_frame(1, 0).unwrap();
        assert_eq!(
            server.shutdown_intent(),
            H2ShutdownIntent::Drain { last_stream_id: 1 }
        );
        server.finish_response_stream(1);
        assert_eq!(server.shutdown_intent(), H2ShutdownIntent::Close);
    }

    #[test]
    fn h2_control_diagnostics_count_ping_and_goaway() {
        let mut server = h2_server_after_preface();
        let ping = h2_ping_frame();
        server.accept_event(&ping).unwrap();
        server.accept_event(&h2_goaway_frame(1)).unwrap();

        let diagnostics = server.control_diagnostics();
        assert_eq!(diagnostics.pings, 1);
        assert_eq!(diagnostics.goaways, 1);
    }

    #[test]
    fn h2_rejects_invalid_window_update_payloads() {
        let mut server = h2_server_after_preface();
        let zero = h2_window_update_frame(0, 0);
        assert_eq!(server.accept_event(&zero), Err(ServerError::InvalidFrame));

        let mut short = Vec::new();
        H2Frame {
            frame_type: H2FrameType::WindowUpdate,
            flags: 0,
            stream_id: 0,
            payload: vec![0, 0, 1],
        }
        .encode(&mut short);
        assert_eq!(server.accept_event(&short), Err(ServerError::InvalidFrame));
    }

    #[test]
    fn h2_server_stream_events_cover_headers_data_trailers_and_reset() {
        let mut client = H2Client::default();
        let mut input = client.connection_preface();
        let (stream_id, headers) = open_stream_bytes(
            &mut client,
            "POST",
            "http",
            "localhost",
            "/pkg.Service/Call",
            &[Header::new("content-type", "application/grpc")],
            false,
        )
        .unwrap();
        input.extend_from_slice(&headers);
        input.extend_from_slice(&client.data_frame(stream_id, b"abc", false));
        input.extend_from_slice(&trailers_bytes(
            &mut client,
            stream_id,
            &[Header::new("grpc-status", "0")],
        ));
        H2Frame {
            frame_type: H2FrameType::RstStream,
            flags: 0,
            stream_id,
            payload: 8_u32.to_be_bytes().to_vec(),
        }
        .encode(&mut input);

        let mut server = H2Server::default();
        let (event, consumed, output) = server.accept_event(&input).unwrap();
        assert!(matches!(
            event,
            Some(H2StreamEvent::RequestHeaders {
                request: H2Request {
                    ref method,
                    ref path,
                    ..
                },
                end_stream: false,
                ..
            }) if method == "POST" && path == "/pkg.Service/Call"
        ));
        assert!(!output.is_empty());

        let (event, used, _) = server.accept_event(&input[consumed..]).unwrap();
        assert_eq!(
            event,
            Some(H2StreamEvent::Data {
                stream_id,
                payload: b"abc".to_vec(),
                flow_control_len: 3,
                end_stream: false,
            })
        );

        let (event, reset_offset, _) = server.accept_event(&input[consumed + used..]).unwrap();
        assert!(matches!(
            event,
            Some(H2StreamEvent::Trailers {
                stream_id: id,
                ref headers
            }) if id == stream_id
                && headers.iter().any(|h| h.name == "grpc-status" && h.value == "0")
        ));

        let (event, _, _) = server
            .accept_event(&input[consumed + used + reset_offset..])
            .unwrap();
        assert_eq!(
            event,
            Some(H2StreamEvent::Reset {
                stream_id,
                error_code: 8,
            })
        );
    }

    #[test]
    fn h2_client_streams_request_and_accepts_response_events() {
        let mut client = H2Client::default();
        let preface = client.connection_preface();
        assert!(preface.starts_with(CLIENT_PREFACE));
        assert!(client.connection_preface().is_empty());

        let (stream_id, request) = open_stream_bytes(
            &mut client,
            "POST",
            "https",
            "localhost",
            "/svc/Unary",
            &[],
            true,
        )
        .unwrap();
        let (frame, _) = H2Frame::decode(&request).unwrap();
        assert_eq!(frame.frame_type, H2FrameType::Headers);
        assert_eq!(frame.flags, 0x5);

        let mut response = Vec::new();
        H2Frame {
            frame_type: H2FrameType::Settings,
            flags: 0,
            stream_id: 0,
            payload: Vec::new(),
        }
        .encode(&mut response);
        let mut server = H2Server::default();
        response.extend_from_slice(&response_frames_bytes(
            &mut server,
            stream_id,
            200,
            &[Header::new("content-type", "application/grpc")],
            b"hello",
            false,
        ));
        H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0x5,
            stream_id,
            payload: encode_hpack_header_block(&[Header::new("grpc-status", "0")]),
        }
        .encode(&mut response);

        let (event, consumed, ack) = client.accept(&response).unwrap();
        assert_eq!(
            event,
            Some(H2ClientEvent::Settings {
                initial_window_size: None
            })
        );
        assert!(!ack.is_empty());

        let (event, used, _) = client.accept(&response[consumed..]).unwrap();
        assert!(matches!(
            event,
            Some(H2ClientEvent::ResponseHeaders {
                stream_id: id,
                status: 200,
                end_stream: false,
                ..
            }) if id == stream_id
        ));

        let (event, trailer_offset, _) = client.accept(&response[consumed + used..]).unwrap();
        assert_eq!(
            event,
            Some(H2ClientEvent::Data {
                stream_id,
                payload: b"hello".to_vec(),
                flow_control_len: 5,
                end_stream: false,
            })
        );

        let (event, _, _) = client
            .accept(&response[consumed + used + trailer_offset..])
            .unwrap();
        assert!(matches!(
            event,
            Some(H2ClientEvent::Trailers {
                stream_id: id,
                ref headers
            }) if id == stream_id
                && headers.iter().any(|h| h.name == "grpc-status" && h.value == "0")
        ));
    }

    #[test]
    fn h2_client_rejects_response_frames_before_server_settings() {
        let mut client = H2Client::default();
        open_stream_bytes(&mut client, "GET", "https", "example.com", "/", &[], true).unwrap();

        assert_eq!(
            client.accept(&h2_response_headers_frame(1)),
            Err(ServerError::InvalidFrame)
        );
        assert_eq!(
            H2Client::default().accept(&h2_settings_ack_frame()),
            Err(ServerError::InvalidFrame)
        );
    }

    #[test]
    fn h2_client_accepts_response_frames_after_server_settings() {
        let mut client = h2_client_after_server_settings();
        open_stream_bytes(&mut client, "GET", "https", "example.com", "/", &[], true).unwrap();

        let (event, _, _) = client.accept(&h2_response_headers_frame(1)).unwrap();

        assert!(matches!(
            event,
            Some(H2ClientEvent::ResponseHeaders {
                stream_id: 1,
                status: 200,
                ..
            })
        ));
    }

    #[test]
    fn h2_stream_events_cover_split_request_data_window_update_and_goaway() {
        let mut client = H2Client::default();
        let mut input = client.connection_preface();
        let (stream_id, headers) = open_stream_bytes(
            &mut client,
            "POST",
            "http",
            "localhost",
            "/svc/Upload",
            &[],
            false,
        )
        .unwrap();
        input.extend_from_slice(&headers);
        input.extend_from_slice(&client.data_frame(stream_id, b"one", false));
        input.extend_from_slice(&client.data_frame(stream_id, b"two", true));
        H2Frame {
            frame_type: H2FrameType::WindowUpdate,
            flags: 0,
            stream_id,
            payload: 1024_u32.to_be_bytes().to_vec(),
        }
        .encode(&mut input);
        H2Frame {
            frame_type: H2FrameType::Goaway,
            flags: 0,
            stream_id: 0,
            payload: [stream_id.to_be_bytes(), 0_u32.to_be_bytes()].concat(),
        }
        .encode(&mut input);

        let mut server = H2Server::default();
        let (event, mut offset, _) = server.accept_event(&input).unwrap();
        assert!(matches!(event, Some(H2StreamEvent::RequestHeaders { .. })));

        let (event, used, _) = server.accept_event(&input[offset..]).unwrap();
        assert_eq!(
            event,
            Some(H2StreamEvent::Data {
                stream_id,
                payload: b"one".to_vec(),
                flow_control_len: 3,
                end_stream: false,
            })
        );
        offset += used;

        let (event, used, _) = server.accept_event(&input[offset..]).unwrap();
        assert_eq!(
            event,
            Some(H2StreamEvent::Data {
                stream_id,
                payload: b"two".to_vec(),
                flow_control_len: 3,
                end_stream: true,
            })
        );
        offset += used;

        let (event, used, _) = server.accept_event(&input[offset..]).unwrap();
        assert_eq!(
            event,
            Some(H2StreamEvent::WindowUpdate {
                stream_id,
                increment: 1024,
            })
        );
        offset += used;

        let (event, _, _) = server.accept_event(&input[offset..]).unwrap();
        assert_eq!(
            event,
            Some(H2StreamEvent::Goaway {
                last_stream_id: stream_id,
                error_code: 0,
            })
        );
    }

    #[test]
    fn h2_rejects_invalid_settings_max_frame_size() {
        let mut input = Vec::new();
        input.extend_from_slice(CLIENT_PREFACE);
        H2Frame {
            frame_type: H2FrameType::Settings,
            flags: 0,
            stream_id: 0,
            payload: [
                5_u16.to_be_bytes().as_slice(),
                0_u32.to_be_bytes().as_slice(),
            ]
            .concat(),
        }
        .encode(&mut input);

        let mut server = H2Server::default();
        assert_eq!(server.accept_event(&input), Err(ServerError::InvalidFrame));

        let mut client = H2Client::default();
        let mut server_settings = Vec::new();
        H2Frame {
            frame_type: H2FrameType::Settings,
            flags: 0,
            stream_id: 0,
            payload: [
                5_u16.to_be_bytes().as_slice(),
                (H2_MAX_MAX_FRAME_SIZE as u32 + 1).to_be_bytes().as_slice(),
            ]
            .concat(),
        }
        .encode(&mut server_settings);
        assert_eq!(
            client.accept(&server_settings),
            Err(ServerError::InvalidFrame)
        );
    }

    #[test]
    fn h2_stream_tracking_releases_completed_stream_ids() {
        let mut client = H2Client::default();
        let mut server_input = client.connection_preface();
        let (stream_id, headers) = open_stream_bytes(
            &mut client,
            "POST",
            "http",
            "localhost",
            "/svc/One",
            &[],
            false,
        )
        .unwrap();
        server_input.extend_from_slice(&headers);
        server_input.extend_from_slice(&client.data_frame(stream_id, b"done", true));
        let (next_stream_id, next_headers) = open_stream_bytes(
            &mut client,
            "POST",
            "http",
            "localhost",
            "/svc/Two",
            &[],
            false,
        )
        .unwrap();
        server_input.extend_from_slice(&next_headers);
        H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0x5,
            stream_id: next_stream_id,
            payload: encode_hpack_header_block(&[Header::new("grpc-status", "0")]),
        }
        .encode(&mut server_input);

        let mut server = H2Server::default();
        let (_event, mut offset, _) = server.accept_event(&server_input).unwrap();
        assert!(server.request_streams.contains_key(&stream_id));
        let (_event, used, _) = server.accept_event(&server_input[offset..]).unwrap();
        offset += used;
        assert!(!server.request_streams.contains_key(&stream_id));
        let (_event, used, _) = server.accept_event(&server_input[offset..]).unwrap();
        offset += used;
        assert!(server.request_streams.contains_key(&next_stream_id));
        let (_event, _used, _) = server.accept_event(&server_input[offset..]).unwrap();
        assert!(!server.request_streams.contains_key(&next_stream_id));

        let mut h2_client = h2_client_after_server_settings();
        let (_first_stream_id, _) = open_stream_bytes(
            &mut h2_client,
            "POST",
            "http",
            "localhost",
            "/svc/One",
            &[],
            true,
        )
        .unwrap();
        let (client_next_stream_id, _) = open_stream_bytes(
            &mut h2_client,
            "POST",
            "http",
            "localhost",
            "/svc/Two",
            &[],
            true,
        )
        .unwrap();
        assert_eq!(client_next_stream_id, next_stream_id);
        let mut response = Vec::new();
        let mut response_server = H2Server::default();
        response.extend_from_slice(&response_frames_bytes(
            &mut response_server,
            next_stream_id,
            200,
            &[],
            b"ok",
            true,
        ));
        let (_event, consumed, _) = h2_client.accept(&response).unwrap();
        assert!(h2_client.response_streams.contains_key(&next_stream_id));
        let (_event, _used, _) = h2_client.accept(&response[consumed..]).unwrap();
        assert!(!h2_client.response_streams.contains_key(&next_stream_id));
    }

    #[test]
    fn h2_pseudo_header_validation_rejects_invalid_requests() {
        let invalid_cases: Vec<Vec<H2RawHeader>> = vec![
            vec![
                H2RawHeader::new(":method", "GET"),
                H2RawHeader::new(":path", "/"),
            ],
            vec![
                H2RawHeader::new(":method", "GET"),
                H2RawHeader::new(":method", "POST"),
                H2RawHeader::new(":scheme", "https"),
                H2RawHeader::new(":path", "/"),
            ],
            vec![
                H2RawHeader::new(":method", "GET"),
                H2RawHeader::new(":scheme", "https"),
                H2RawHeader::new(":authority", "one.example"),
                H2RawHeader::new(":authority", "two.example"),
                H2RawHeader::new(":path", "/"),
            ],
            vec![
                H2RawHeader::new("x-test", "1"),
                H2RawHeader::new(":method", "GET"),
                H2RawHeader::new(":scheme", "https"),
                H2RawHeader::new(":path", "/"),
            ],
            vec![
                H2RawHeader::new(":method", "GET"),
                H2RawHeader::new(":scheme", "https"),
                H2RawHeader::new(":path", "/"),
                H2RawHeader::new("Host", "example.com"),
            ],
            vec![
                H2RawHeader::new(":method", "GET"),
                H2RawHeader::new(":scheme", "https"),
                H2RawHeader::new(":path", "/"),
                H2RawHeader::new("te", "gzip"),
            ],
        ];

        for headers in invalid_cases {
            let mut encoder = H2HeaderBlockEncoder::new();
            let mut frame = Vec::new();
            H2Frame {
                frame_type: H2FrameType::Headers,
                flags: 0x5,
                stream_id: 1,
                payload: encoder.encode(&headers),
            }
            .encode(&mut frame);
            let mut server = h2_server_after_preface();

            assert_eq!(server.accept_event(&frame), Err(ServerError::InvalidFrame));
        }
    }

    #[test]
    fn h2_typed_malformed_header_errors_are_stream_protocol_errors() {
        let mut encoder = H2HeaderBlockEncoder::new();
        let frame = H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0x5,
            stream_id: 1,
            payload: encoder.encode(&[
                H2RawHeader::new(":method", "GET"),
                H2RawHeader::new(":scheme", "https"),
                H2RawHeader::new(":path", "/"),
                H2RawHeader::new("Host", "example.com"),
            ]),
        };
        let mut server = h2_server_after_preface();

        let H2FrameOutcome::Error(error) = server.accept_frame_typed(frame).0 else {
            panic!("expected malformed header typed error");
        };

        assert_eq!(error.scope, H2ErrorScope::Stream(1));
        assert_eq!(error.code, H2ErrorCode::ProtocolError);
        assert_eq!(error.hpack_error, None);
    }

    #[test]
    fn h2_pseudo_header_validation_rejects_invalid_responses() {
        let invalid_cases: Vec<Vec<H2RawHeader>> = vec![
            vec![H2RawHeader::new("content-type", "application/grpc")],
            vec![
                H2RawHeader::new(":status", "200"),
                H2RawHeader::new(":status", "204"),
            ],
            vec![
                H2RawHeader::new("content-type", "application/grpc"),
                H2RawHeader::new(":status", "200"),
            ],
            vec![H2RawHeader::new(":status", "101")],
            vec![H2RawHeader::new(":status", "99")],
            vec![H2RawHeader::new(":status", "1000")],
        ];

        for headers in invalid_cases {
            let mut client = h2_client_after_server_settings();
            let (stream_id, _) =
                open_stream_bytes(&mut client, "GET", "https", "example.com", "/", &[], true)
                    .unwrap();
            let mut encoder = H2HeaderBlockEncoder::new();
            let mut frame = Vec::new();
            H2Frame {
                frame_type: H2FrameType::Headers,
                flags: 0x5,
                stream_id,
                payload: encoder.encode(&headers),
            }
            .encode(&mut frame);

            assert_eq!(client.accept(&frame), Err(ServerError::InvalidFrame));
        }
    }

    #[test]
    fn h2_content_length_mismatch_is_rejected() {
        let mut client = h2_client_after_server_settings();
        let mut server_input = client.connection_preface();
        let (stream_id, headers) = open_stream_bytes(
            &mut client,
            "POST",
            "https",
            "example.com",
            "/upload",
            &[Header::new("content-length", "4")],
            false,
        )
        .unwrap();
        server_input.extend_from_slice(&headers);
        server_input.extend_from_slice(&client.data_frame(stream_id, b"abc", true));
        let mut server = H2Server::default();
        let (_event, offset, _) = server.accept_event(&server_input).unwrap();

        assert_eq!(
            server.accept_event(&server_input[offset..]),
            Err(ServerError::InvalidContentLength)
        );

        let mut client = h2_client_after_server_settings();
        let (stream_id, _) = open_stream_bytes(
            &mut client,
            "GET",
            "https",
            "example.com",
            "/download",
            &[],
            true,
        )
        .unwrap();
        let mut h2 = H2Server::default();
        let mut response = response_headers_bytes(
            &mut h2,
            stream_id,
            200,
            &[Header::new("content-length", "4")],
            false,
        );
        response.extend_from_slice(&h2.data_frame(stream_id, b"abc", true));
        let (_event, offset, _) = client.accept(&response).unwrap();

        assert_eq!(
            client.accept(&response[offset..]),
            Err(ServerError::InvalidContentLength)
        );
    }

    #[test]
    fn h2_request_content_length_accepts_exact_and_duplicate_equivalent_values() {
        for headers in [
            vec![Header::new("content-length", "3")],
            vec![
                Header::new("content-length", "3"),
                Header::new("content-length", "3"),
            ],
            vec![Header::new("content-length", "3, 3")],
        ] {
            let mut client = h2_client_after_server_settings();
            let mut input = client.connection_preface();
            let (stream_id, request) = open_stream_bytes(
                &mut client,
                "POST",
                "https",
                "example.com",
                "/upload",
                &headers,
                false,
            )
            .unwrap();
            input.extend_from_slice(&request);
            input.extend_from_slice(&client.data_frame(stream_id, b"abc", true));
            let mut server = H2Server::default();
            let (_, consumed, _) = server.accept_event(&input).unwrap();

            assert!(server.accept_event(&input[consumed..]).is_ok());
        }
    }

    #[test]
    fn h2_request_content_length_rejects_too_long_and_invalid_declarations() {
        for (headers, body) in [
            (vec![Header::new("content-length", "2")], b"abc".as_slice()),
            (
                vec![
                    Header::new("content-length", "2"),
                    Header::new("content-length", "3"),
                ],
                b"".as_slice(),
            ),
            (vec![Header::new("content-length", "2, 3")], b"".as_slice()),
            (
                vec![Header::new("content-length", "184467440737095516160")],
                b"".as_slice(),
            ),
        ] {
            let mut client = h2_client_after_server_settings();
            let mut input = client.connection_preface();
            let (stream_id, request) = open_stream_bytes(
                &mut client,
                "POST",
                "https",
                "example.com",
                "/upload",
                &headers,
                body.is_empty(),
            )
            .unwrap();
            input.extend_from_slice(&request);
            if !body.is_empty() {
                input.extend_from_slice(&client.data_frame(stream_id, body, false));
            }
            let mut server = H2Server::default();

            if body.is_empty() {
                assert!(server.accept_event(&input).is_err());
            } else {
                let (_, consumed, _) = server.accept_event(&input).unwrap();
                assert!(server.accept_event(&input[consumed..]).is_err());
            }
        }
    }

    #[test]
    fn h2_response_content_length_honors_no_body_semantics() {
        for (method, status) in [("HEAD", 200), ("GET", 304)] {
            let mut client = h2_client_after_server_settings();
            let (stream_id, _) = open_stream_bytes(
                &mut client,
                method,
                "https",
                "example.com",
                "/resource",
                &[],
                true,
            )
            .unwrap();
            let mut server = H2Server::default();
            let response = response_headers_bytes(
                &mut server,
                stream_id,
                status,
                &[Header::new("content-length", "123")],
                true,
            );

            assert!(client.accept(&response).is_ok());
        }

        let mut client = h2_client_after_server_settings();
        let (stream_id, _) = open_stream_bytes(
            &mut client,
            "GET",
            "https",
            "example.com",
            "/resource",
            &[],
            true,
        )
        .unwrap();
        let mut server = H2Server::default();
        let response = response_headers_bytes(
            &mut server,
            stream_id,
            204,
            &[Header::new("content-length", "0")],
            true,
        );
        assert!(client.accept(&response).is_err());

        let mut client = h2_client_after_server_settings();
        let (stream_id, _) = open_stream_bytes(
            &mut client,
            "HEAD",
            "https",
            "example.com",
            "/resource",
            &[],
            true,
        )
        .unwrap();
        let mut server = H2Server::default();
        let mut response = response_headers_bytes(
            &mut server,
            stream_id,
            200,
            &[Header::new("content-length", "3")],
            false,
        );
        response.extend_from_slice(&server.data_frame(stream_id, b"abc", true));
        let (_, consumed, _) = client.accept(&response).unwrap();
        assert!(client.accept(&response[consumed..]).is_err());
    }

    #[test]
    fn h2_content_length_finishes_on_trailers_but_not_reset_or_goaway() {
        for (declared, trailers_are_valid) in [("3", true), ("4", false)] {
            let mut client = h2_client_after_server_settings();
            let mut input = client.connection_preface();
            let (stream_id, request) = open_stream_bytes(
                &mut client,
                "POST",
                "https",
                "example.com",
                "/upload",
                &[Header::new("content-length", declared)],
                false,
            )
            .unwrap();
            input.extend_from_slice(&request);
            input.extend_from_slice(&client.data_frame(stream_id, b"abc", false));
            input.extend_from_slice(&trailers_bytes(
                &mut client,
                stream_id,
                &[Header::new("x-checksum", "ok")],
            ));
            let mut server = H2Server::default();
            let (_, first, _) = server.accept_event(&input).unwrap();
            let (_, second, _) = server.accept_event(&input[first..]).unwrap();
            let trailers = server.accept_event(&input[first + second..]);

            assert_eq!(trailers.is_ok(), trailers_are_valid);
        }

        for terminal in [h2_reset_frame(1), h2_goaway_frame(1)] {
            let mut client = h2_client_after_server_settings();
            let mut input = client.connection_preface();
            let (_, request) = open_stream_bytes(
                &mut client,
                "POST",
                "https",
                "example.com",
                "/upload",
                &[Header::new("content-length", "4")],
                false,
            )
            .unwrap();
            input.extend_from_slice(&request);
            input.extend_from_slice(&terminal);
            let mut server = H2Server::default();
            let (_, consumed, _) = server.accept_event(&input).unwrap();

            assert!(server.accept_event(&input[consumed..]).is_ok());
        }
    }

    #[test]
    fn h2_trailers_reject_pseudo_headers() {
        let mut client = h2_client_after_server_settings();
        let mut input = client.connection_preface();
        let (stream_id, headers) = open_stream_bytes(
            &mut client,
            "POST",
            "https",
            "example.com",
            "/svc",
            &[],
            false,
        )
        .unwrap();
        input.extend_from_slice(&headers);
        H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0x5,
            stream_id,
            payload: encode_hpack_header_block(&[Header::new(":status", "200")]),
        }
        .encode(&mut input);
        let mut server = H2Server::default();
        let (_event, offset, _) = server.accept_event(&input).unwrap();

        assert_eq!(
            server.accept_event(&input[offset..]),
            Err(ServerError::InvalidFrame)
        );
    }

    #[test]
    fn h2_client_allows_informational_response_before_final_headers() {
        let mut client = h2_client_after_server_settings();
        let (stream_id, _) =
            open_stream_bytes(&mut client, "GET", "https", "example.com", "/", &[], true).unwrap();
        let mut h2 = H2Server::default();
        let mut response = response_headers_bytes(&mut h2, stream_id, 100, &[], false);
        response.extend_from_slice(&response_headers_bytes(&mut h2, stream_id, 200, &[], true));

        let (event, offset, _) = client.accept(&response).unwrap();
        assert!(matches!(
            event,
            Some(H2ClientEvent::ResponseHeaders { status: 100, .. })
        ));
        let (event, _, _) = client.accept(&response[offset..]).unwrap();
        assert!(matches!(
            event,
            Some(H2ClientEvent::ResponseHeaders {
                status: 200,
                end_stream: true,
                ..
            })
        ));
    }

    #[test]
    fn h2_client_enforces_stream_id_and_concurrency_limits() {
        let mut client = H2Client {
            next_stream_id: 0x8000_0001,
            ..H2Client::default()
        };
        assert_eq!(
            open_stream_bytes(&mut client, "GET", "https", "example.com", "/", &[], true,),
            Err(ServerError::InvalidFrame)
        );

        let mut client = H2Client {
            settings: H2Settings {
                max_concurrent_streams: 1,
                ..H2Settings::default()
            },
            ..H2Client::default()
        };
        open_stream_bytes(
            &mut client,
            "GET",
            "https",
            "example.com",
            "/one",
            &[],
            true,
        )
        .unwrap();
        assert_eq!(
            open_stream_bytes(
                &mut client,
                "GET",
                "https",
                "example.com",
                "/two",
                &[],
                true,
            ),
            Err(ServerError::InvalidFrame)
        );
    }

    #[test]
    fn h2_rejects_rst_stream_for_idle_stream() {
        let mut server = h2_server_after_preface();
        assert_eq!(
            server.accept_event(&h2_reset_frame(1)),
            Err(ServerError::InvalidFrame)
        );
    }

    #[test]
    fn h2_rejects_data_after_normal_stream_close() {
        let mut client = H2Client::default();
        let mut input = client.connection_preface();
        let (stream_id, headers) = open_stream_bytes(
            &mut client,
            "POST",
            "https",
            "example.com",
            "/upload",
            &[],
            false,
        )
        .unwrap();
        input.extend_from_slice(&headers);
        input.extend_from_slice(&client.data_frame(stream_id, b"done", true));
        input.extend_from_slice(&client.data_frame(stream_id, b"late", false));
        let mut server = H2Server::default();
        let (_event, offset, _) = server.accept_event(&input).unwrap();
        let (_event, used, _) = server.accept_event(&input[offset..]).unwrap();

        assert_eq!(
            server.accept_event(&input[offset + used..]),
            Err(ServerError::InvalidFrame)
        );
    }

    #[test]
    fn h2_server_discarded_data_preserves_borrowed_owned_and_wrapper_contracts() {
        let mut server = h2_server_after_preface();
        let mut peer = H2Client::default();
        let (stream_id, headers) = open_stream_bytes(
            &mut peer,
            "POST",
            "https",
            "example.com",
            "/upload",
            &[],
            false,
        )
        .unwrap();
        server.accept_event(&headers).unwrap();
        server.close_stream(stream_id);

        for (frame, flow_control_len) in h2_discarded_data_inputs(stream_id) {
            let (event, consumed, output) = server.accept_event_ref(&frame).unwrap();
            assert_eq!(consumed, frame.len());
            assert!(output.is_empty());
            let event = event.expect("discarded DATA event");
            assert!(matches!(
                event,
                H2StreamEvent::DiscardedData {
                    stream_id: id,
                    flow_control_len: len,
                } if id == stream_id && len == flow_control_len
            ));
            assert_eq!(
                event.into_owned(),
                H2StreamEvent::DiscardedData {
                    stream_id,
                    flow_control_len,
                }
            );

            let (event, consumed, output) = server.accept_event(&frame).unwrap();
            assert_eq!(consumed, frame.len());
            assert!(output.is_empty());
            assert_eq!(
                event,
                Some(H2StreamEvent::DiscardedData {
                    stream_id,
                    flow_control_len,
                })
            );

            let (request, consumed, output) = server.accept(&frame).unwrap();
            assert!(request.is_none());
            assert_eq!(consumed, frame.len());
            assert!(output.is_empty());
        }
    }

    #[test]
    fn h2_client_discarded_data_preserves_borrowed_and_owned_contracts() {
        let mut client = h2_client_after_server_settings();
        let (stream_id, _) = open_stream_bytes(
            &mut client,
            "GET",
            "https",
            "example.com",
            "/download",
            &[],
            false,
        )
        .unwrap();
        let mut peer = H2Server::default();
        let headers = response_headers_bytes(&mut peer, stream_id, 200, &[], false);
        client.accept(&headers).unwrap();
        client.close_stream(stream_id);

        for (frame, flow_control_len) in h2_discarded_data_inputs(stream_id) {
            let (event, consumed, output) = client.accept_ref(&frame).unwrap();
            assert_eq!(consumed, frame.len());
            assert!(output.is_empty());
            let event = event.expect("discarded DATA event");
            assert!(matches!(
                event,
                H2ClientEvent::DiscardedData {
                    stream_id: id,
                    flow_control_len: len,
                } if id == stream_id && len == flow_control_len
            ));
            assert_eq!(
                event.into_owned(),
                H2ClientEvent::DiscardedData {
                    stream_id,
                    flow_control_len,
                }
            );

            let (event, consumed, output) = client.accept(&frame).unwrap();
            assert_eq!(consumed, frame.len());
            assert!(output.is_empty());
            assert_eq!(
                event,
                Some(H2ClientEvent::DiscardedData {
                    stream_id,
                    flow_control_len,
                })
            );
        }
    }

    #[test]
    fn h2_client_ignores_rst_stream_for_known_closed_stream() {
        let mut client = h2_client_after_server_settings();
        let (stream_id, _) =
            open_stream_bytes(&mut client, "GET", "https", "example.com", "/", &[], true).unwrap();
        let mut h2 = H2Server::default();
        let response = response_headers_bytes(&mut h2, stream_id, 200, &[], true);
        client.accept(&response).unwrap();
        let mut reset = Vec::new();
        H2Frame {
            frame_type: H2FrameType::RstStream,
            flags: 0,
            stream_id,
            payload: 8_u32.to_be_bytes().to_vec(),
        }
        .encode(&mut reset);

        assert_eq!(client.accept(&reset), Ok((None, reset.len(), Vec::new())));
    }

    #[test]
    fn h2_closed_stream_tombstones_are_bounded() {
        let mut server = H2Server::with_limits(H2Limits {
            max_closed_stream_tombstones: 1,
            ..H2Limits::default()
        })
        .unwrap();
        let mut client = H2Client::default();
        server.accept_event(&client.connection_preface()).unwrap();
        for path in ["/one", "/two"] {
            let (_stream_id, headers) =
                open_stream_bytes(&mut client, "GET", "https", "example.com", path, &[], true)
                    .unwrap();
            server.accept_event(&headers).unwrap();
        }

        assert_eq!(server.closed_streams.len(), 1);
        assert!(!server.closed_streams.contains(&1));
        assert!(server.closed_streams.contains(&3));
    }

    #[test]
    fn h2_zero_tombstone_capacity_does_not_retain_reset_tolerance() {
        let limits = H2Limits {
            max_closed_stream_tombstones: 0,
            ..H2Limits::default()
        };

        let mut server = H2Server::with_limits(limits).unwrap();
        let mut peer = H2Client::default();
        server.accept_event(&peer.connection_preface()).unwrap();
        let (server_stream, headers) =
            open_stream_bytes(&mut peer, "POST", "https", "example.com", "/", &[], false).unwrap();
        server.accept_event(&headers).unwrap();
        server.accept_event(&h2_reset_frame(server_stream)).unwrap();
        server.close_stream(server_stream.saturating_add(2));
        assert!(server.closed_streams.is_empty());
        assert!(server.closed_stream_order.is_empty());
        assert!(server.reset_tolerant_streams.is_empty());

        let mut client = H2Client::with_limits(limits).unwrap();
        client.accept(&h2_settings_frame(&[])).unwrap();
        let (reset_stream, _) =
            open_stream_bytes(&mut client, "POST", "https", "example.com", "/", &[], false)
                .unwrap();
        client.accept(&h2_reset_frame(reset_stream)).unwrap();
        let (closed_stream, _) =
            open_stream_bytes(&mut client, "POST", "https", "example.com", "/", &[], false)
                .unwrap();
        client.close_stream(closed_stream);
        assert!(client.closed_streams.is_empty());
        assert!(client.closed_stream_order.is_empty());
        assert!(client.reset_tolerant_streams.is_empty());
    }

    #[test]
    fn h2_active_stream_limit_counts_end_stream_requests_until_response_finishes() {
        let mut server = H2Server::with_limits(H2Limits {
            max_active_streams: 1,
            ..H2Limits::default()
        })
        .unwrap();
        let mut client = H2Client::default();
        server.accept_event(&client.connection_preface()).unwrap();

        let (_first, headers) = open_stream_bytes(
            &mut client,
            "GET",
            "https",
            "example.com",
            "/one",
            &[],
            true,
        )
        .unwrap();
        server.accept_event(&headers).unwrap();
        let (_second, headers) = open_stream_bytes(
            &mut client,
            "GET",
            "https",
            "example.com",
            "/two",
            &[],
            true,
        )
        .unwrap();

        let (event, _, output) = server.accept_event(&headers).unwrap();
        assert_eq!(event, None);
        let (reset, consumed) = H2Frame::decode(&output).unwrap();
        assert_eq!(consumed, output.len());
        assert_eq!(reset.frame_type, H2FrameType::RstStream);
        assert_eq!(reset.stream_id, 3);
        assert_eq!(
            reset.payload,
            H2ErrorCode::RefusedStream.as_u32().to_be_bytes()
        );

        server.finish_response_stream(1);
        let (_third, headers) = open_stream_bytes(
            &mut client,
            "GET",
            "https",
            "example.com",
            "/three",
            &[],
            true,
        )
        .unwrap();
        assert!(server.accept_event(&headers).is_ok());
    }

    #[test]
    fn h2_network_owners_accept_explicit_active_stream_limits_above_default() {
        let limits = H2Limits {
            max_active_streams: H2_DEFAULT_MAX_ACTIVE_STREAMS + 1,
            ..H2Limits::default()
        };

        assert!(H2Server::with_limits(limits).is_ok());
        assert!(H2Client::with_limits(limits).is_ok());
    }

    #[test]
    fn h2_client_enforces_shared_header_and_body_limits() {
        let count_limits = HttpLimits::new()
            .set_max_header_bytes(usize::MAX)
            .set_max_headers(0);
        let mut client = H2Client::with_local_flow_control_and_http_limits(
            65_535,
            65_535,
            H2Limits::from_http_limits(count_limits),
            count_limits,
        )
        .unwrap();
        let count_error = client
            .open_stream_with_raw_headers(
                "GET",
                "http",
                "x",
                "/",
                &[H2HeaderField::new(b"x", b"y")],
                true,
            )
            .unwrap_err();
        assert_eq!(count_error.classify().kind(), HttpErrorKind::TooManyHeaders);
        assert_eq!(count_error.classify().limit().unwrap().limit(), 0);
        assert_eq!(count_error.classify().limit().unwrap().actual(), Some(1));

        let byte_limits = HttpLimits::new()
            .set_max_header_bytes(165)
            .set_max_headers(usize::MAX);
        let mut client = H2Client::with_local_flow_control_and_http_limits(
            65_535,
            65_535,
            H2Limits::from_http_limits(byte_limits),
            byte_limits,
        )
        .unwrap();
        let byte_error = client
            .open_stream_with_raw_headers("GET", "http", "x", "/", &[], true)
            .unwrap_err();
        assert_eq!(byte_error.classify().kind(), HttpErrorKind::HeadersTooLarge);
        assert_eq!(byte_error.classify().limit().unwrap().limit(), 165);
        assert_eq!(byte_error.classify().limit().unwrap().actual(), Some(166));

        let body_limits = HttpLimits::new()
            .set_max_header_bytes(usize::MAX)
            .set_max_body_bytes(3);
        let mut client = H2Client::with_local_flow_control_and_http_limits(
            65_535,
            65_535,
            H2Limits::from_http_limits(body_limits),
            body_limits,
        )
        .unwrap();
        let body_error = client
            .open_stream_with_raw_headers(
                "POST",
                "http",
                "x",
                "/",
                &[H2HeaderField::new(b"content-length", b"4")],
                false,
            )
            .unwrap_err();
        assert_eq!(body_error.classify().kind(), HttpErrorKind::BodyTooLarge);
        assert_eq!(body_error.classify().limit().unwrap().limit(), 3);
        assert_eq!(body_error.classify().limit().unwrap().actual(), Some(4));
    }

    #[test]
    fn h2_server_enforces_explicit_active_stream_limit_above_default() {
        let mut server = H2Server::with_limits(H2Limits {
            max_active_streams: H2_DEFAULT_MAX_ACTIVE_STREAMS + 1,
            ..H2Limits::default()
        })
        .unwrap();
        let mut client = H2Client::with_limits(H2Limits {
            max_active_streams: H2_DEFAULT_MAX_ACTIVE_STREAMS + 2,
            ..H2Limits::default()
        })
        .unwrap();
        server.accept_event(&client.connection_preface()).unwrap();

        for index in 0..=H2_DEFAULT_MAX_ACTIVE_STREAMS {
            let (_stream_id, headers) = open_stream_bytes(
                &mut client,
                "GET",
                "https",
                "example.com",
                &format!("/stream/{index}"),
                &[],
                true,
            )
            .unwrap();
            assert!(server.accept_event(&headers).is_ok());
        }
        let (_stream_id, headers) = open_stream_bytes(
            &mut client,
            "GET",
            "https",
            "example.com",
            "/over-limit",
            &[],
            true,
        )
        .unwrap();

        let (event, _, output) = server.accept_event(&headers).unwrap();
        assert_eq!(event, None);
        let (reset, consumed) = H2Frame::decode(&output).unwrap();
        assert_eq!(consumed, output.len());
        assert_eq!(reset.frame_type, H2FrameType::RstStream);
        assert_eq!(
            reset.stream_id,
            2 * H2_DEFAULT_MAX_ACTIVE_STREAMS as u32 + 3
        );
        assert_eq!(
            reset.payload,
            H2ErrorCode::RefusedStream.as_u32().to_be_bytes()
        );
    }

    #[test]
    fn h2_client_enforces_its_local_active_stream_limit() {
        let mut client = H2Client::with_limits(H2Limits {
            max_active_streams: 1,
            ..H2Limits::default()
        })
        .unwrap();

        assert!(
            open_stream_bytes(
                &mut client,
                "POST",
                "https",
                "example.test",
                "/one",
                &[],
                false,
            )
            .is_ok()
        );
        assert_eq!(
            open_stream_bytes(
                &mut client,
                "POST",
                "https",
                "example.test",
                "/two",
                &[],
                false,
            )
            .unwrap_err(),
            ServerError::InvalidFrame
        );
    }

    #[test]
    fn h2_rst_stream_cancels_response_active_stream_after_tombstone_eviction() {
        let mut server = H2Server::with_limits(H2Limits {
            max_closed_stream_tombstones: 1,
            ..H2Limits::default()
        })
        .unwrap();
        let mut client = H2Client::default();
        server.accept_event(&client.connection_preface()).unwrap();
        for path in ["/one", "/two"] {
            let (_stream_id, headers) =
                open_stream_bytes(&mut client, "GET", "https", "example.com", path, &[], true)
                    .unwrap();
            server.accept_event(&headers).unwrap();
        }
        assert!(!server.closed_streams.contains(&1));
        assert!(server.response_active_streams.contains_key(&1));

        let (event, _, _) = server.accept_event(&h2_reset_frame(1)).unwrap();

        assert_eq!(
            event,
            Some(H2StreamEvent::Reset {
                stream_id: 1,
                error_code: 8,
            })
        );
        assert!(!server.response_active_streams.contains_key(&1));
    }

    #[test]
    fn flow_control_window_tracks_signed_credit_and_debt() {
        let mut window = H2FlowControlWindow::new(100).unwrap();

        window.consume(40).unwrap();
        assert_eq!(window.available(), 60);
        window.adjust(-120).unwrap();
        assert_eq!(window.available(), -60);
        window.increase(30).unwrap();
        assert_eq!(window.available(), -30);
        assert_eq!(window.consume(1), Err(ServerError::FlowControlViolation));
    }

    #[test]
    fn flow_receive_window_batches_refunds_and_extra_credit() {
        let mut window = H2ReceiveWindow::new(8);

        window.receive_data(4).unwrap();
        assert_eq!(window.consume_data(1).unwrap(), None);
        assert_eq!(window.consume_data(1).unwrap(), Some(2));
        assert_eq!(window.available(), 6);

        window.receive_data(6).unwrap();
        assert_eq!(window.grant_extra_credit_for_frame(10, 6).unwrap(), Some(4));
        assert_eq!(window.available(), 4);
        assert_eq!(window.consume_data(10).unwrap(), Some(6));
    }

    #[test]
    fn flow_receive_window_rollback_does_not_create_future_credit_debt() {
        let mut window = H2ReceiveWindow::new(10);

        for _ in 0..4 {
            window.receive_data(5).unwrap();
            assert_eq!(window.rollback_received_data(5).unwrap(), Some(5));
            assert_eq!(window.available(), 10);
            assert_eq!(window.pending_update(), 0);
        }

        window.receive_data(10).unwrap();
        assert_eq!(window.consume_data(10).unwrap(), Some(10));
        assert_eq!(window.available(), 10);
    }

    #[test]
    fn flow_receive_window_rollback_removes_adaptive_sample_and_blockage() {
        let start = Instant::now();
        let mut sample_window =
            H2ReceiveWindow::with_adaptive_growth(100, 400, Duration::from_millis(100), start)
                .unwrap();

        sample_window
            .receive_data_at(100, start + Duration::from_millis(10))
            .unwrap();
        sample_window
            .consume_data_at(100, start + Duration::from_millis(10))
            .unwrap();
        sample_window
            .receive_data_at(50, start + Duration::from_millis(20))
            .unwrap();
        sample_window.rollback_received_data(50).unwrap();
        sample_window
            .receive_data_at(100, start + Duration::from_millis(90))
            .unwrap();
        assert_eq!(
            sample_window
                .consume_data_at(100, start + Duration::from_millis(90))
                .unwrap(),
            Some(150)
        );

        let diagnostics = sample_window.diagnostics();
        assert_eq!(diagnostics.current_window_bytes, 150);
        assert_eq!(diagnostics.estimated_bdp_bytes, 125);
        assert_eq!(diagnostics.growth_events, 1);
        assert_eq!(diagnostics.growth_bytes, 50);
        assert_eq!(diagnostics.blocked_time, Duration::from_millis(10));

        let mut blocked_window =
            H2ReceiveWindow::with_adaptive_growth(100, 400, Duration::from_millis(100), start)
                .unwrap();
        blocked_window
            .receive_data_at(100, start + Duration::from_millis(10))
            .unwrap();
        blocked_window
            .consume_data_at(100, start + Duration::from_millis(10))
            .unwrap();
        blocked_window
            .receive_data_at(100, start + Duration::from_millis(20))
            .unwrap();
        blocked_window.rollback_received_data(100).unwrap();
        blocked_window
            .receive_data_at(1, start + Duration::from_millis(90))
            .unwrap();

        let diagnostics = blocked_window.diagnostics();
        assert_eq!(diagnostics.blocked_time, Duration::from_millis(10));
        assert_eq!(diagnostics.last_blocked_time, Duration::from_millis(10));
    }

    #[test]
    fn flow_receive_window_wholly_rolled_back_first_sample_reanchors_epoch() {
        let start = Instant::now();
        let mut window =
            H2ReceiveWindow::with_adaptive_growth(100, 400, Duration::from_millis(100), start)
                .unwrap();

        window
            .receive_data_at(100, start + Duration::from_millis(10))
            .unwrap();
        window.rollback_received_data(100).unwrap();
        window
            .receive_data_at(100, start + Duration::from_millis(90))
            .unwrap();
        window
            .consume_data_at(100, start + Duration::from_millis(150))
            .unwrap();

        let diagnostics = window.diagnostics();
        assert_eq!(diagnostics.estimated_bdp_bytes, 166);
        assert_eq!(diagnostics.received_bytes, 200);
        assert_eq!(diagnostics.consumed_bytes, 100);
    }

    #[test]
    fn flow_receive_window_zero_amount_preserves_pending_first_sample_reanchor() {
        let start = Instant::now();
        let mut window =
            H2ReceiveWindow::with_adaptive_growth(100, 400, Duration::from_millis(100), start)
                .unwrap();

        window
            .receive_data_at(100, start + Duration::from_millis(10))
            .unwrap();
        window.rollback_received_data(100).unwrap();
        window
            .receive_data_at(0, start + Duration::from_millis(50))
            .unwrap();
        window
            .receive_data_at(100, start + Duration::from_millis(90))
            .unwrap();
        window
            .consume_data_at(100, start + Duration::from_millis(150))
            .unwrap();

        let diagnostics = window.diagnostics();
        assert_eq!(diagnostics.estimated_bdp_bytes, 166);
        assert_eq!(diagnostics.received_bytes, 200);
        assert_eq!(diagnostics.consumed_bytes, 100);
        assert_eq!(diagnostics.growth_events, 0);
        assert_eq!(diagnostics.growth_bytes, 0);
    }

    #[test]
    fn flow_receive_window_adapts_after_sustained_fast_turnovers() {
        let start = Instant::now();
        let mut window =
            H2ReceiveWindow::with_adaptive_growth(100, 400, Duration::from_millis(100), start)
                .unwrap();

        window
            .receive_data_at(100, start + Duration::from_millis(10))
            .unwrap();
        assert_eq!(
            window
                .consume_data_at(100, start + Duration::from_millis(10))
                .unwrap(),
            Some(100)
        );
        window
            .receive_data_at(100, start + Duration::from_millis(20))
            .unwrap();
        assert_eq!(
            window
                .consume_data_at(100, start + Duration::from_millis(20))
                .unwrap(),
            Some(200)
        );

        assert_eq!(window.limit(), 200);
        assert_eq!(window.available(), 200);
        assert_eq!(
            window.diagnostics(),
            H2ReceiveWindowDiagnostics {
                current_window_bytes: 200,
                max_window_bytes: 400,
                received_bytes: 200,
                consumed_bytes: 200,
                blocked_time: Duration::from_millis(10),
                last_blocked_time: Duration::from_millis(10),
                rtt_proxy: Duration::from_millis(100),
                estimated_bdp_bytes: 1_000,
                growth_events: 1,
                growth_bytes: 100,
            }
        );
    }

    #[test]
    fn flow_receive_window_starts_first_sample_with_first_data() {
        let start = Instant::now();
        let mut window =
            H2ReceiveWindow::with_adaptive_growth(100, 400, Duration::from_millis(100), start)
                .unwrap();

        window
            .receive_data_at(100, start + Duration::from_millis(50))
            .unwrap();
        window
            .consume_data_at(100, start + Duration::from_millis(150))
            .unwrap();
        window
            .receive_data_at(100, start + Duration::from_millis(160))
            .unwrap();
        window
            .consume_data_at(100, start + Duration::from_millis(250))
            .unwrap();

        assert_eq!(window.limit(), 150);
        assert_eq!(window.diagnostics().growth_events, 1);
    }

    #[test]
    fn flow_receive_window_counts_only_consumed_turnovers() {
        let start = Instant::now();
        let mut window =
            H2ReceiveWindow::with_adaptive_growth(100, 400, Duration::from_millis(100), start)
                .unwrap();
        window.grant_extra_credit(100).unwrap();

        window
            .receive_data_at(200, start + Duration::from_millis(10))
            .unwrap();
        assert_eq!(
            window
                .consume_data_at(100, start + Duration::from_millis(10))
                .unwrap(),
            None
        );

        assert_eq!(window.limit(), 100);
        assert_eq!(window.diagnostics().growth_events, 0);
    }

    #[test]
    fn flow_receive_window_does_not_grow_slow_or_above_cap() {
        let start = Instant::now();
        let mut slow =
            H2ReceiveWindow::with_adaptive_growth(100, 400, Duration::from_millis(100), start)
                .unwrap();
        for elapsed_ms in [10, 160, 310] {
            let now = start + Duration::from_millis(elapsed_ms);
            slow.receive_data_at(100, now).unwrap();
            slow.consume_data_at(100, now).unwrap();
        }
        assert_eq!(slow.limit(), 100);
        assert_eq!(slow.diagnostics().growth_events, 0);

        let mut capped =
            H2ReceiveWindow::with_adaptive_growth(100, 150, Duration::from_millis(100), start)
                .unwrap();
        for elapsed_ms in [10, 20, 30, 40] {
            let now = start + Duration::from_millis(elapsed_ms);
            let amount = capped.limit();
            capped.receive_data_at(amount, now).unwrap();
            capped.consume_data_at(amount, now).unwrap();
        }
        assert_eq!(capped.limit(), 150);
        assert_eq!(capped.diagnostics().growth_bytes, 50);
    }

    #[test]
    fn flow_receive_window_rejects_invalid_adaptive_policy() {
        let now = Instant::now();
        assert_eq!(
            H2ReceiveWindow::with_adaptive_growth(100, 99, Duration::from_millis(1), now),
            Err(ServerError::InvalidFrame)
        );
        assert_eq!(
            H2ReceiveWindow::with_adaptive_growth(100, 200, Duration::ZERO, now),
            Err(ServerError::InvalidFrame)
        );
    }

    #[test]
    fn flow_fair_scheduler_rotates_ready_streams_and_skips_blocked() {
        let mut scheduler = H2FairStreamScheduler::default();
        scheduler.register(1);
        scheduler.register(3);
        scheduler.register(5);

        assert_eq!(scheduler.next_ready(|stream_id| stream_id != 1), Some(3));
        assert_eq!(scheduler.next_ready(|stream_id| stream_id != 1), Some(5));
        scheduler.mark_drained(5);
        assert_eq!(scheduler.next_ready(|stream_id| stream_id != 1), Some(3));
        scheduler.remove(3);
        assert_eq!(scheduler.next_ready(|stream_id| stream_id != 1), None);

        let diagnostics = scheduler.diagnostics();
        assert_eq!(diagnostics.observed_streams, 3);
        assert_eq!(diagnostics.tracked_streams, 2);
        assert_eq!(diagnostics.queued_streams, 1);
        assert_eq!(diagnostics.selection_attempts, 4);
        assert_eq!(diagnostics.selections, 3);
        assert_eq!(diagnostics.no_ready_attempts, 1);
        assert_eq!(diagnostics.skipped_streams, 3);
        assert_eq!(diagnostics.max_scan_depth, 2);
        assert_eq!(diagnostics.min_stream_selections, 0);
        assert_eq!(diagnostics.max_stream_selections, 2);
        assert_eq!(
            diagnostics.stream_selection_buckets,
            [1, 1, 1, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            scheduler.pending_capacity_streams(|stream_id| stream_id == 1),
            1
        );
    }

    #[test]
    fn flow_fair_scheduler_bounds_extreme_initial_capacity() {
        let mut scheduler = H2FairStreamScheduler::with_capacity(usize::MAX);

        scheduler.register(1);

        assert_eq!(scheduler.next_ready(|_| true), Some(1));
    }

    #[test]
    fn flow_fair_scheduler_uses_randomized_hashing() {
        fn assert_random_state(_: &std::collections::hash_map::RandomState) {}

        let scheduler = H2FairStreamScheduler::default();

        assert_random_state(scheduler.stream_slots.hasher());
    }

    #[test]
    fn flow_fair_scheduler_requeue_does_not_duplicate_stale_turns() {
        let mut scheduler = H2FairStreamScheduler::default();
        scheduler.register(1);
        scheduler.register(3);

        assert_eq!(scheduler.next_ready(|_| true), Some(1));
        scheduler.mark_drained(1);
        scheduler.register(1);

        assert_eq!(scheduler.next_ready(|_| true), Some(3));
        assert_eq!(scheduler.next_ready(|_| true), Some(1));
        assert_eq!(scheduler.next_ready(|_| true), Some(3));
        assert_eq!(scheduler.diagnostics().stream_selection_buckets[2], 2);
    }

    #[test]
    fn flow_fair_scheduler_churn_storage_remains_bound_to_live_state() {
        const CYCLES: u32 = 1_000_000;

        let mut scheduler = H2FairStreamScheduler::default();
        for index in 0..CYCLES {
            let stream_id = index.saturating_mul(2).saturating_add(1);
            scheduler.register(stream_id);
            assert_eq!(scheduler.stream_slots.len(), 1);
            assert_eq!(scheduler.slots.len(), 1);
            assert_eq!(scheduler.queued_streams, 1);
            assert_eq!(
                scheduler.slot_stream_id(scheduler.ready_head),
                Some(stream_id)
            );
            assert_eq!(
                scheduler.slot_stream_id(scheduler.ready_tail),
                Some(stream_id)
            );

            scheduler.record_immediate_dispatch();
            scheduler.mark_drained(stream_id);
            assert_eq!(scheduler.stream_slots.len(), 1);
            assert_eq!(scheduler.slots.len(), 1);
            assert_eq!(scheduler.queued_streams, 0);
            assert_eq!(scheduler.ready_head, None);
            assert_eq!(scheduler.ready_tail, None);

            scheduler.remove(stream_id);
            assert!(scheduler.stream_slots.is_empty());
            assert_eq!(scheduler.slots.len(), 1);
            assert_eq!(scheduler.queued_streams, 0);
            assert_eq!(scheduler.ready_head, None);
            assert_eq!(scheduler.ready_tail, None);
        }

        let useful_stream_id = CYCLES.saturating_mul(2).saturating_add(1);
        scheduler.register(useful_stream_id);
        assert_eq!(scheduler.next_ready(|_| true), Some(useful_stream_id));

        let diagnostics = scheduler.diagnostics();
        assert_eq!(diagnostics.observed_streams, CYCLES as usize + 1);
        assert_eq!(diagnostics.tracked_streams, 1);
        assert_eq!(diagnostics.queued_streams, 1);
        assert_eq!(diagnostics.selection_attempts, 1);
        assert_eq!(diagnostics.selections, 1);
        assert_eq!(diagnostics.immediate_dispatches, CYCLES as u64);
        assert_eq!(diagnostics.skipped_streams, 0);
        assert_eq!(diagnostics.max_scan_depth, 1);
        assert_eq!(
            diagnostics.stream_selection_buckets,
            [CYCLES as usize, 1, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn flow_fair_scheduler_slot_storage_tracks_peak_live_streams() {
        const DEFAULT_STREAM_CAP: usize = 100;

        let mut scheduler = H2FairStreamScheduler::default();
        for index in 0..DEFAULT_STREAM_CAP as u32 {
            scheduler.register(index.saturating_mul(2).saturating_add(1));
        }
        assert_eq!(scheduler.stream_slots.len(), DEFAULT_STREAM_CAP);
        assert_eq!(scheduler.slots.len(), DEFAULT_STREAM_CAP);
        assert_eq!(scheduler.queued_streams, DEFAULT_STREAM_CAP);
        let stream_slot_capacity = scheduler.stream_slots.capacity();
        let slot_capacity = scheduler.slots.capacity();

        for index in 0..DEFAULT_STREAM_CAP as u32 {
            scheduler.remove(index.saturating_mul(2).saturating_add(1));
        }
        assert!(scheduler.stream_slots.is_empty());
        assert_eq!(scheduler.slots.len(), DEFAULT_STREAM_CAP);
        assert_eq!(scheduler.queued_streams, 0);

        for index in 0..100_000_u32 {
            let stream_id = 1_001_u32.saturating_add(index.saturating_mul(2));
            scheduler.register(stream_id);
            scheduler.remove(stream_id);
            assert_eq!(scheduler.slots.len(), DEFAULT_STREAM_CAP);
        }
        assert_eq!(scheduler.stream_slots.capacity(), stream_slot_capacity);
        assert_eq!(scheduler.slots.capacity(), slot_capacity);
    }

    #[test]
    fn flow_fair_scheduler_histogram_uses_every_inclusive_boundary() {
        let cases = [
            (1, 0_u64),
            (3, 1),
            (5, 3),
            (7, 7),
            (9, 15),
            (11, 31),
            (13, 63),
            (15, 64),
        ];
        let mut scheduler = H2FairStreamScheduler::default();

        for (stream_id, selections) in cases {
            scheduler.register(stream_id);
            for _ in 0..selections {
                assert_eq!(scheduler.next_ready(|id| id == stream_id), Some(stream_id));
            }
            scheduler.remove(stream_id);
        }

        let diagnostics = scheduler.diagnostics();
        assert_eq!(
            diagnostics.stream_selection_buckets,
            [1, 1, 1, 1, 1, 1, 1, 1]
        );
        assert_eq!(diagnostics.min_stream_selections, 0);
        assert_eq!(diagnostics.max_stream_selections, 64);
    }

    #[test]
    fn flow_fair_scheduler_equality_ignores_diagnostic_history() {
        let mut left = H2FairStreamScheduler::default();
        let mut right = H2FairStreamScheduler::default();
        left.register(1);
        right.register(1);

        assert_eq!(left.next_ready(|_| true), Some(1));
        assert_eq!(right.next_ready(|_| false), None);
        left.record_immediate_dispatch();
        assert_eq!(left, right);

        right.mark_drained(1);
        assert_ne!(left, right);
    }

    #[test]
    fn flow_fair_scheduler_retains_immediate_selection_distribution() {
        let mut scheduler = H2FairStreamScheduler::default();
        scheduler.register(1);
        scheduler.record_immediate_dispatch();
        scheduler.mark_drained(1);

        let active = scheduler.diagnostics();
        assert_eq!(active.observed_streams, 1);
        assert_eq!(active.queued_streams, 0);
        assert_eq!(active.selections, 0);
        assert_eq!(active.immediate_dispatches, 1);
        assert_eq!(active.stream_selection_buckets, [1, 0, 0, 0, 0, 0, 0, 0]);

        scheduler.remove(1);
        let retired = scheduler.diagnostics();
        assert_eq!(retired.observed_streams, 1);
        assert_eq!(retired.tracked_streams, 0);
        assert_eq!(retired.min_stream_selections, 0);
        assert_eq!(retired.max_stream_selections, 0);
        assert_eq!(retired.stream_selection_buckets, [1, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn flow_diagnostics_snapshot_classifies_updates_and_saturates() {
        let mut flow = H2FlowDiagnostics::default();
        flow.record_stream_window_stall();
        flow.record_connection_window_stall();
        flow.record_window_update(1, 7);
        flow.record_window_update(0, 11);

        let snapshot = flow.snapshot(2, H2FairStreamSchedulerDiagnostics::default());
        assert_eq!(snapshot.stream_window_stalls, 1);
        assert_eq!(snapshot.connection_window_stalls, 1);
        assert_eq!(snapshot.stream_window_updates, 1);
        assert_eq!(snapshot.connection_window_updates, 1);
        assert_eq!(snapshot.stream_window_update_bytes, 7);
        assert_eq!(snapshot.connection_window_update_bytes, 11);
        assert_eq!(snapshot.pending_capacity_streams, 2);

        flow.stream_window_stalls = u64::MAX;
        flow.connection_window_stalls = u64::MAX;
        flow.stream_window_updates = u64::MAX;
        flow.connection_window_updates = u64::MAX;
        flow.stream_window_update_bytes = u64::MAX;
        flow.connection_window_update_bytes = u64::MAX;
        flow.record_stream_window_stall();
        flow.record_connection_window_stall();
        flow.record_window_update(1, 1);
        flow.record_window_update(0, 1);

        assert_eq!(
            flow,
            H2FlowDiagnostics {
                stream_window_stalls: u64::MAX,
                connection_window_stalls: u64::MAX,
                stream_window_updates: u64::MAX,
                connection_window_updates: u64::MAX,
                stream_window_update_bytes: u64::MAX,
                connection_window_update_bytes: u64::MAX,
            }
        );
    }

    #[test]
    fn flow_backpressure_state_reports_queue_limits() {
        let limits = H2Limits {
            max_queued_data_bytes: 4,
            max_queued_control_frames: 1,
            ..H2Limits::default()
        };

        let state = H2BackpressureState::new(2, 5, 2, limits);

        assert_eq!(state.pending_streams, 2);
        assert!(state.data_over_limit);
        assert!(state.control_over_limit);
        assert!(!state.should_read_more());
    }

    #[test]
    fn corpus_frame_inputs_cover_unknown_incomplete_and_invalid_control_frames() {
        assert_eq!(H2Frame::decode(&[0, 0, 1]), Err(ServerError::NeedMore));

        let mut unknown = Vec::new();
        H2Frame {
            frame_type: H2FrameType::Unknown(0x21),
            flags: 0,
            stream_id: 0,
            payload: b"abc".to_vec(),
        }
        .encode(&mut unknown);
        let (frame, consumed) = H2Frame::decode(&unknown).unwrap();
        assert_eq!(consumed, unknown.len());
        assert_eq!(frame.frame_type, H2FrameType::Unknown(0x21));

        let mut invalid_settings = Vec::new();
        H2Frame {
            frame_type: H2FrameType::Settings,
            flags: 0,
            stream_id: 0,
            payload: vec![0, 1, 2],
        }
        .encode(&mut invalid_settings);
        let mut server = h2_server_after_preface();
        assert_eq!(
            server.accept_event(&invalid_settings),
            Err(ServerError::InvalidFrame)
        );

        assert_eq!(
            window_update_increment(&0_u32.to_be_bytes()),
            Err(ServerError::InvalidFrame)
        );
    }

    #[test]
    fn corpus_hpack_errors_preserve_distinct_categories() {
        let invalid_cases = [
            (vec![0x80], H2HpackError::HeaderIndexOutOfBounds),
            (vec![0xff], H2HpackError::IntegerDecoding),
        ];

        for (payload, expected) in invalid_cases {
            let mut server = h2_server_after_preface();
            let frame = H2Frame {
                frame_type: H2FrameType::Headers,
                flags: 0x4,
                stream_id: 1,
                payload,
            };
            let H2FrameOutcome::Error(error) = server.accept_frame_typed(frame).0 else {
                panic!("expected HPACK error");
            };
            assert_eq!(error.hpack_error, Some(expected));
        }
    }

    #[test]
    fn h2_rejects_bad_preface_and_frame() {
        let mut server = H2Server::default();
        assert_eq!(server.accept(b"bad"), Err(ServerError::InvalidPreface));
        let mut server = H2Server::default();
        assert_eq!(
            server.accept(&CLIENT_PREFACE[..3]),
            Ok((None, 0, Vec::new()))
        );
        let mut input = b"bad bad bad bad bad bad bad!".to_vec();
        input.truncate(CLIENT_PREFACE.len());
        assert_eq!(server.accept(&input), Err(ServerError::InvalidPreface));
    }

    #[test]
    fn h2_rejects_unexpected_frame_after_settings() {
        let mut input = Vec::new();
        input.extend_from_slice(CLIENT_PREFACE);
        H2Frame {
            frame_type: H2FrameType::Settings,
            flags: 0,
            stream_id: 0,
            payload: Vec::new(),
        }
        .encode(&mut input);
        H2Frame {
            frame_type: H2FrameType::Data,
            flags: 0,
            stream_id: 1,
            payload: b"unexpected".to_vec(),
        }
        .encode(&mut input);

        let mut server = H2Server::default();
        assert_eq!(server.accept(&input), Err(ServerError::InvalidFrame));
    }

    #[test]
    fn h2_server_decoded_frame_paths_require_initial_settings() {
        let mut server = H2Server::default();
        let headers = H2Frame {
            frame_type: H2FrameType::Headers,
            flags: 0x5,
            stream_id: 1,
            payload: encode_hpack_request_headers("GET", "https", "example.com", "/", &[]),
        };

        let H2FrameOutcome::Error(error) = server.accept_frame_typed(headers).0 else {
            panic!("expected decoded HEADERS before SETTINGS to fail");
        };
        assert_eq!(error.scope, H2ErrorScope::Connection);
        assert_eq!(error.code, H2ErrorCode::ProtocolError);

        let mut server = H2Server::default();
        assert_eq!(
            server.accept_complete_header_block(
                1,
                0x5,
                &encode_hpack_request_headers("GET", "https", "example.com", "/", &[])
            ),
            Err(ServerError::InvalidFrame)
        );

        let mut server = H2Server::default();
        assert!(matches!(
            server.classify_frame_typed(H2Frame {
                frame_type: H2FrameType::Headers,
                flags: 0x5,
                stream_id: 1,
                payload: encode_hpack_request_headers("GET", "https", "example.com", "/", &[]),
            }),
            H2FrameOutcome::Error(H2ProtocolError {
                scope: H2ErrorScope::Connection,
                code: H2ErrorCode::ProtocolError,
                ..
            })
        ));
    }

    #[test]
    fn h2_client_data_plans_follow_stream_settings_and_window_updates() {
        let mut client = H2Client::default();
        client
            .accept(&h2_settings_frame(&[H2Setting::new(
                H2SettingId::InitialWindowSize,
                5,
            )]))
            .unwrap();
        let (stream_id, _) =
            open_stream_bytes(&mut client, "POST", "http", "example.com", "/", &[], false).unwrap();

        let plan = client
            .prepare_data_frame(stream_id, 10, true)
            .unwrap()
            .unwrap();
        assert_eq!(plan.payload_len(), 5);
        assert!(!plan.end_stream());
        assert_eq!(plan.header(), &[0, 0, 5, 0, 0, 0, 0, 0, 1]);
        client.commit_data_frame(plan).unwrap();
        assert_eq!(client.prepare_data_frame(stream_id, 5, true).unwrap(), None);

        let update = H2Frame {
            frame_type: H2FrameType::WindowUpdate,
            flags: 0,
            stream_id,
            payload: 3_u32.to_be_bytes().to_vec(),
        };
        client.accept_frame_bytes(update).unwrap();
        assert_eq!(
            client
                .prepare_data_frame(stream_id, 5, true)
                .unwrap()
                .unwrap()
                .payload_len(),
            3
        );

        let settings = H2Frame {
            frame_type: H2FrameType::Settings,
            flags: 0,
            stream_id: 0,
            payload: {
                let mut payload = Vec::new();
                H2Settings::encode_payload(
                    &[H2Setting::new(H2SettingId::InitialWindowSize, 2)],
                    &mut payload,
                );
                payload
            },
        };
        client.accept_frame_bytes(settings).unwrap();
        assert_eq!(
            client.send_capacity(stream_id, 5).unwrap().sendable_bytes,
            0
        );
    }

    #[test]
    fn h2_client_data_plans_enforce_connection_window_and_zero_length_end_stream() {
        let mut client = H2Client::default();
        client
            .accept(&h2_settings_frame(&[H2Setting::new(
                H2SettingId::InitialWindowSize,
                100_000,
            )]))
            .unwrap();
        let (stream_id, _) =
            open_stream_bytes(&mut client, "POST", "http", "example.com", "/", &[], false).unwrap();
        let mut sent = 0;
        while let Some(plan) = client
            .prepare_data_frame(stream_id, 100_000 - sent, false)
            .unwrap()
        {
            sent += plan.payload_len();
            client.commit_data_frame(plan).unwrap();
        }
        assert_eq!(sent, 65_535);
        assert!(
            client
                .send_capacity(stream_id, 1)
                .unwrap()
                .connection_window_blocked
        );

        client
            .accept_frame_bytes(H2Frame {
                frame_type: H2FrameType::WindowUpdate,
                flags: 0,
                stream_id: 0,
                payload: 1_u32.to_be_bytes().to_vec(),
            })
            .unwrap();
        assert_eq!(
            client.send_capacity(stream_id, 1).unwrap().sendable_bytes,
            1
        );

        let mut empty_client = h2_client_after_server_settings();
        let (empty_stream, _) = open_stream_bytes(
            &mut empty_client,
            "POST",
            "http",
            "example.com",
            "/",
            &[],
            false,
        )
        .unwrap();
        let plan = empty_client
            .prepare_data_frame(empty_stream, 0, true)
            .unwrap()
            .unwrap();
        assert_eq!(plan.header(), &[0, 0, 0, 0, 1, 0, 0, 0, 1]);
        empty_client.commit_data_frame(plan).unwrap();
        assert_eq!(
            empty_client.send_capacity(empty_stream, 1),
            Err(ServerError::FlowControlViolation)
        );
    }

    #[test]
    fn h2_window_update_overflow_is_a_scoped_flow_control_error() {
        let mut client = h2_client_after_server_settings();
        let outcome = client
            .accept_frame_bytes_typed(H2Frame {
                frame_type: H2FrameType::WindowUpdate,
                flags: 0,
                stream_id: 0,
                payload: 0x7fff_ffff_u32.to_be_bytes().to_vec(),
            })
            .0;
        assert!(matches!(
            outcome,
            H2FrameOutcome::Error(H2ProtocolError {
                scope: H2ErrorScope::Connection,
                code: H2ErrorCode::FlowControlError,
                ..
            })
        ));
    }

    #[test]
    fn h2_settings_send_window_overflow_is_a_connection_flow_control_error() {
        let mut client = H2Client::default();
        client
            .accept(&h2_settings_frame(&[H2Setting::new(
                H2SettingId::InitialWindowSize,
                5,
            )]))
            .unwrap();
        let (stream_id, _) =
            open_stream_bytes(&mut client, "POST", "http", "example.com", "/", &[], false).unwrap();
        client
            .accept_frame_bytes(H2Frame {
                frame_type: H2FrameType::WindowUpdate,
                flags: 0,
                stream_id,
                payload: (H2_MAX_WINDOW_SIZE - 5).to_be_bytes().to_vec(),
            })
            .unwrap();

        let mut payload = Vec::new();
        H2Settings::encode_payload(
            &[H2Setting::new(H2SettingId::InitialWindowSize, 6)],
            &mut payload,
        );
        assert!(matches!(
            client
                .accept_frame_bytes_typed(H2Frame {
                    frame_type: H2FrameType::Settings,
                    flags: 0,
                    stream_id: 0,
                    payload,
                })
                .0,
            H2FrameOutcome::Error(H2ProtocolError {
                scope: H2ErrorScope::Connection,
                code: H2ErrorCode::FlowControlError,
                ..
            })
        ));
    }

    #[test]
    fn h2_server_data_plans_share_response_stream_lifecycle() {
        let mut peer = H2Client::with_local_flow_control(4, 65_535).unwrap();
        let mut server = H2Server::default();
        server.accept_event(&peer.connection_preface()).unwrap();
        let (stream_id, request) =
            open_stream_bytes(&mut peer, "GET", "http", "example.com", "/", &[], true).unwrap();
        server.accept_event(&request).unwrap();

        let plan = server
            .prepare_data_frame(stream_id, 10, true)
            .unwrap()
            .unwrap();
        assert_eq!(plan.payload_len(), 4);
        assert!(!plan.end_stream());
        server.commit_data_frame(plan).unwrap();
        assert_eq!(server.prepare_data_frame(stream_id, 6, true).unwrap(), None);

        server
            .accept_frame_bytes(H2Frame {
                frame_type: H2FrameType::WindowUpdate,
                flags: 0,
                stream_id,
                payload: 6_u32.to_be_bytes().to_vec(),
            })
            .unwrap();
        let plan = server
            .prepare_data_frame(stream_id, 6, true)
            .unwrap()
            .unwrap();
        assert!(plan.end_stream());
        server.commit_data_frame(plan).unwrap();
        assert_eq!(
            server.send_capacity(stream_id, 1),
            Err(ServerError::FlowControlViolation)
        );
    }

    fn h2_server_after_preface() -> H2Server {
        let mut client = H2Client::default();
        let mut server = H2Server::default();
        let preface = client.connection_preface();
        server.accept_event(&preface).unwrap();
        server
    }

    fn h2_client_after_server_settings() -> H2Client {
        let mut client = H2Client::default();
        client.accept(&h2_settings_frame(&[])).unwrap();
        client
    }

    fn h2_ping_frame() -> Vec<u8> {
        let mut output = Vec::new();
        H2Frame {
            frame_type: H2FrameType::Ping,
            flags: 0,
            stream_id: 0,
            payload: [0; 8].to_vec(),
        }
        .encode(&mut output);
        output
    }

    fn h2_settings_frame(settings: &[H2Setting]) -> Vec<u8> {
        let mut payload = Vec::new();
        H2Settings::encode_payload(settings, &mut payload);
        let mut output = Vec::new();
        H2Frame {
            frame_type: H2FrameType::Settings,
            flags: 0,
            stream_id: 0,
            payload,
        }
        .encode(&mut output);
        output
    }

    fn h2_settings_ack_frame() -> Vec<u8> {
        let mut output = Vec::new();
        H2Frame {
            frame_type: H2FrameType::Settings,
            flags: 0x1,
            stream_id: 0,
            payload: Vec::new(),
        }
        .encode(&mut output);
        output
    }

    fn h2_window_update_frame(stream_id: u32, increment: u32) -> Vec<u8> {
        let mut output = Vec::new();
        H2Frame {
            frame_type: H2FrameType::WindowUpdate,
            flags: 0,
            stream_id,
            payload: increment.to_be_bytes().to_vec(),
        }
        .encode(&mut output);
        output
    }

    fn h2_reset_frame(stream_id: u32) -> Vec<u8> {
        let mut output = Vec::new();
        H2Frame {
            frame_type: H2FrameType::RstStream,
            flags: 0,
            stream_id,
            payload: 8_u32.to_be_bytes().to_vec(),
        }
        .encode(&mut output);
        output
    }

    fn h2_discarded_data_inputs(stream_id: u32) -> [(Vec<u8>, usize); 4] {
        let encode = |flags, payload: Vec<u8>| {
            let flow_control_len = payload.len();
            let mut output = Vec::new();
            H2Frame {
                frame_type: H2FrameType::Data,
                flags,
                stream_id,
                payload,
            }
            .encode(&mut output);
            (output, flow_control_len)
        };
        [
            encode(0, Vec::new()),
            encode(0, b"data".to_vec()),
            encode(0x8, vec![0]),
            encode(0x8, vec![2, b'x', 0, 0]),
        ]
    }

    fn h2_goaway_frame(last_stream_id: u32) -> Vec<u8> {
        let mut output = Vec::new();
        H2Frame {
            frame_type: H2FrameType::Goaway,
            flags: 0,
            stream_id: 0,
            payload: [last_stream_id.to_be_bytes(), 0_u32.to_be_bytes()].concat(),
        }
        .encode(&mut output);
        output
    }

    fn hpack_literal_with_indexing(name: &str, value: &str) -> Vec<u8> {
        let mut block = Vec::new();
        block.push(0x40);
        hpack_push_string(&mut block, name.as_bytes());
        hpack_push_string(&mut block, value.as_bytes());
        block
    }

    fn h2_response_headers_frame(stream_id: u32) -> Vec<u8> {
        let mut server = H2Server::default();
        response_headers_bytes(&mut server, stream_id, 200, &[], false)
    }

    fn h2_priority_frame(stream_id: u32) -> Vec<u8> {
        let mut output = Vec::new();
        H2Frame {
            frame_type: H2FrameType::Priority,
            flags: 0,
            stream_id,
            payload: [0; 5].to_vec(),
        }
        .encode(&mut output);
        output
    }

    fn hex_bytes(input: &str) -> Vec<u8> {
        let input: String = input.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(input.len() % 2, 0);
        input
            .as_bytes()
            .chunks_exact(2)
            .map(|digits| {
                let high = hex_value(digits[0]);
                let low = hex_value(digits[1]);
                (high << 4) | low
            })
            .collect()
    }

    fn hex_value(byte: u8) -> u8 {
        match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => panic!("invalid hex digit"),
        }
    }
}
