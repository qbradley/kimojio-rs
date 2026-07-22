use crate::HttpLimits;

const CONTENT_LENGTH_PREFIX: &[u8] = b"content-length: ";

/// HTTP header name/value pair borrowed from caller-owned storage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Header<'a> {
    /// Header field name.
    pub name: &'a str,
    /// Header field value.
    pub value: &'a str,
}

impl<'a> Header<'a> {
    /// Creates a borrowed header pair.
    pub const fn new(name: &'a str, value: &'a str) -> Self {
        Self { name, value }
    }
}

/// Request metadata used to emit one HTTP/1.1 request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RequestHead<'a> {
    /// HTTP method token.
    pub method: &'a str,
    /// Request target, including path and optional query.
    pub target: &'a str,
    /// Header fields emitted before the request body.
    pub headers: &'a [Header<'a>],
}

impl<'a> RequestHead<'a> {
    /// Creates borrowed request metadata.
    pub const fn new(method: &'a str, target: &'a str, headers: &'a [Header<'a>]) -> Self {
        Self {
            method,
            target,
            headers,
        }
    }
}

/// Parsed response head borrowed from caller-provided input and header scratch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResponseHead<'headers, 'input> {
    /// HTTP minor version: `0` for HTTP/1.0 and `1` for HTTP/1.1.
    pub version: u8,
    /// HTTP status code.
    pub status: u16,
    /// Optional reason phrase.
    pub reason: &'input str,
    /// Parsed response headers backed by caller-provided scratch storage.
    pub headers: &'headers [httparse::Header<'input>],
}

/// Observable state of the HTTP client FSM.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientState {
    /// Request bytes are still available through [`HttpClient::poll_outbound`].
    SendingRequest,
    /// Request output is drained and response bytes may be fed.
    ReadingResponse,
    /// Response processing reached a terminal state.
    Complete,
    /// A fatal protocol/resource error occurred; callers should drop this FSM.
    Failed,
}

/// Event exposed by the HTTP client FSM.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientEvent<'headers, 'input> {
    /// No event is ready; callers should feed more input when transport data is available.
    NeedInput,
    /// A response head is ready. The caller should discard `consumed` input bytes.
    ResponseHead {
        head: ResponseHead<'headers, 'input>,
        consumed: usize,
    },
    /// A borrowed response body chunk is ready. The caller should discard
    /// `consumed` input bytes after processing the chunk.
    BodyChunk {
        chunk: &'input [u8],
        consumed: usize,
    },
    /// Response processing is complete.
    Complete,
}

/// Outbound data or state exposed by the HTTP client FSM.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outbound<'a> {
    /// Bytes are ready for the caller to write to its transport.
    Bytes(&'a [u8]),
    /// The request needs more caller-provided body bytes.
    NeedBody { remaining: usize },
    /// No outbound bytes remain.
    Done,
}

/// HTTP client state-machine error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Error {
    /// Response headers exceeded the configured byte limit.
    HeaderTooLarge { limit: usize, actual: usize },
    /// Response or request body exceeded the configured byte limit.
    BodyTooLarge { limit: usize, actual: usize },
    /// Request header name or method token was invalid.
    InvalidHeaderName,
    /// Header value was invalid.
    InvalidHeaderValue,
    /// Request target was invalid.
    InvalidRequestTarget,
    /// Response status code was invalid.
    InvalidStatus,
    /// Response status code was absent.
    MissingStatus,
    /// Response content-length metadata was invalid or inconsistent.
    InvalidContentLength,
    /// Response parsing failed.
    Parse,
    /// Caller-provided response header scratch was too small.
    TooManyHeaders { limit: usize, actual: usize },
    /// Response used an unsupported transfer encoding.
    UnsupportedTransferEncoding,
    /// Response chunked body framing was invalid.
    InvalidChunkedBody,
    /// Caller attempted to feed input after terminal state.
    TerminalState,
    /// Caller consumed more outbound bytes than were available.
    OutputUnderflow,
    /// Caller attempted to advance while a previous input event has not been acknowledged.
    EventPending,
    /// Caller acknowledged a different byte count than the pending event exposed.
    InputConsumeMismatch { expected: usize, actual: usize },
}

impl Error {
    /// Returns stable, allocation-free semantic information about this failure.
    pub fn classify(&self) -> crate::HttpErrorInfo {
        use crate::{HttpErrorInfo, HttpErrorKind, HttpErrorScope, LimitViolation};

        let (kind, detail, limit) = match *self {
            Self::HeaderTooLarge { limit, actual } => (
                HttpErrorKind::HeadersTooLarge,
                "HTTP/1 response headers exceed the configured limit",
                Some(LimitViolation::new(limit, Some(actual))),
            ),
            Self::TooManyHeaders { limit, actual } => (
                HttpErrorKind::TooManyHeaders,
                "HTTP/1 response contains too many headers",
                Some(LimitViolation::new(limit, Some(actual))),
            ),
            Self::BodyTooLarge { limit, actual } => (
                HttpErrorKind::BodyTooLarge,
                "HTTP/1 body exceeds the configured limit",
                Some(LimitViolation::new(limit, Some(actual))),
            ),
            Self::InvalidHeaderName => (
                HttpErrorKind::InvalidHeader,
                "HTTP/1 header name is invalid",
                None,
            ),
            Self::InvalidHeaderValue => (
                HttpErrorKind::InvalidHeader,
                "HTTP/1 header value is invalid",
                None,
            ),
            Self::InvalidRequestTarget => (
                HttpErrorKind::MalformedMessage,
                "HTTP/1 request target is invalid",
                None,
            ),
            Self::InvalidStatus => (
                HttpErrorKind::MalformedMessage,
                "HTTP/1 response status is invalid",
                None,
            ),
            Self::MissingStatus => (
                HttpErrorKind::MalformedMessage,
                "HTTP/1 response status is missing",
                None,
            ),
            Self::InvalidContentLength => (
                HttpErrorKind::InvalidContentLength,
                "HTTP/1 response content-length is invalid",
                None,
            ),
            Self::Parse => (
                HttpErrorKind::MalformedMessage,
                "HTTP/1 response parsing failed",
                None,
            ),
            Self::UnsupportedTransferEncoding => (
                HttpErrorKind::UnsupportedFeature,
                "HTTP/1 transfer encoding is unsupported",
                None,
            ),
            Self::InvalidChunkedBody => (
                HttpErrorKind::InvalidFraming,
                "HTTP/1 chunked body framing is invalid",
                None,
            ),
            Self::TerminalState => (
                HttpErrorKind::InvalidState,
                "HTTP/1 input followed terminal state",
                None,
            ),
            Self::OutputUnderflow => (
                HttpErrorKind::InvalidState,
                "HTTP/1 output accounting underflowed",
                None,
            ),
            Self::EventPending => (
                HttpErrorKind::InvalidState,
                "HTTP/1 event was not acknowledged",
                None,
            ),
            Self::InputConsumeMismatch { .. } => (
                HttpErrorKind::InvalidState,
                "HTTP/1 input acknowledgement did not match the pending event",
                None,
            ),
        };
        HttpErrorInfo::new(kind, HttpErrorScope::Message, detail, limit)
    }
}

/// I/O-independent HTTP/1.1 client state machine.
///
/// The state machine stores borrowed request metadata. It does not allocate
/// request output or response input buffers. Callers drain outbound slices,
/// maintain their own inbound buffer, and provide scratch header storage when
/// advancing response parsing.
pub struct HttpClient<'a> {
    request: RequestHead<'a>,
    body: RequestBody<'a>,
    state: ClientState,
    output_segment: OutputSegment,
    output_offset: usize,
    header_index: usize,
    content_length: [u8; 20],
    content_length_len: usize,
    emit_content_length: bool,
    response_started: bool,
    response_body: ResponseBody,
    response_header_count: usize,
    response_header_bytes: usize,
    received_body_bytes: usize,
    pending_consume: PendingConsume,
    limits: HttpLimits,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestBody<'a> {
    Borrowed(&'a [u8]),
    Streaming { content_length: usize, sent: usize },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutputSegment {
    Method,
    MethodSpace,
    Target,
    Version,
    HeaderName,
    HeaderSeparator,
    HeaderValue,
    HeaderCrLf,
    ContentLengthPrefix,
    ContentLengthValue,
    ContentLengthCrLf,
    EndHeaders,
    Body,
    Done,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingConsume {
    None,
    Head(usize),
    Body {
        input_bytes: usize,
        body_bytes: usize,
        completes: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResponseBody {
    ContentLength(usize),
    Chunked,
    Eof,
}

impl ResponseBody {
    const fn declared_len(self) -> Option<usize> {
        match self {
            Self::ContentLength(len) => Some(len),
            Self::Chunked | Self::Eof => None,
        }
    }
}

impl<'a> HttpClient<'a> {
    /// Creates a response-only HTTP/1.1 client state machine.
    ///
    /// Callers that already emitted request bytes through another short-lived
    /// client can use this to parse the matching response without retaining
    /// borrowed request metadata beyond request construction.
    pub fn response_reader(
        method: &'static str,
        limits: HttpLimits,
    ) -> Result<HttpClient<'static>, Error> {
        validate_token(method).map_err(|_| Error::InvalidHeaderName)?;
        Ok(HttpClient {
            request: RequestHead::new(method, "/", &[]),
            body: RequestBody::Borrowed(b""),
            state: ClientState::ReadingResponse,
            output_segment: OutputSegment::Done,
            output_offset: 0,
            header_index: 0,
            content_length: [0; 20],
            content_length_len: 0,
            emit_content_length: false,
            response_started: false,
            response_body: ResponseBody::ContentLength(0),
            response_header_count: 0,
            response_header_bytes: 0,
            received_body_bytes: 0,
            pending_consume: PendingConsume::None,
            limits,
        })
    }

    /// Creates a known-length HTTP/1.1 request state machine.
    pub fn request_with_body(head: RequestHead<'a>, body: &'a [u8]) -> Result<Self, Error> {
        Self::request_with_body_limits(head, body, HttpLimits::new())
    }

    /// Creates a known-length streaming-body HTTP/1.1 request state machine.
    ///
    /// The caller supplies body chunks later through [`Self::poll_outbound`].
    pub fn request_with_streaming_body(
        head: RequestHead<'a>,
        content_length: usize,
    ) -> Result<Self, Error> {
        Self::request_with_streaming_body_limits(head, content_length, HttpLimits::new())
    }

    /// Creates a request state machine with explicit parser limits.
    pub fn request_with_body_limits(
        head: RequestHead<'a>,
        body: &'a [u8],
        limits: HttpLimits,
    ) -> Result<Self, Error> {
        if body.len() > limits.max_body_bytes() {
            return Err(Error::BodyTooLarge {
                limit: limits.max_body_bytes(),
                actual: body.len(),
            });
        }
        Self::new(head, RequestBody::Borrowed(body), body.len(), limits)
    }

    /// Creates a streaming-body request state machine with explicit parser limits.
    pub fn request_with_streaming_body_limits(
        head: RequestHead<'a>,
        content_length: usize,
        limits: HttpLimits,
    ) -> Result<Self, Error> {
        if content_length > limits.max_body_bytes() {
            return Err(Error::BodyTooLarge {
                limit: limits.max_body_bytes(),
                actual: content_length,
            });
        }
        Self::new(
            head,
            RequestBody::Streaming {
                content_length,
                sent: 0,
            },
            content_length,
            limits,
        )
    }

    fn new(
        head: RequestHead<'a>,
        body: RequestBody<'a>,
        content_length: usize,
        limits: HttpLimits,
    ) -> Result<Self, Error> {
        validate_token(head.method).map_err(|_| Error::InvalidHeaderName)?;
        validate_target(head.target)?;
        for header in head.headers {
            validate_header(header)?;
        }
        let header_count = head.headers.len().saturating_add(usize::from(
            !head
                .headers
                .iter()
                .any(|header| header.name.eq_ignore_ascii_case("content-length")),
        ));
        if header_count > limits.max_headers() {
            return Err(Error::TooManyHeaders {
                limit: limits.max_headers(),
                actual: header_count,
            });
        }
        let mut content_length_bytes = [0_u8; 20];
        let content_length_len = write_decimal(content_length, &mut content_length_bytes);
        let emit_content_length = !head
            .headers
            .iter()
            .any(|header| header.name.eq_ignore_ascii_case("content-length"));
        let head_bytes = request_head_size(head, emit_content_length.then_some(content_length_len));
        if head_bytes > limits.max_header_bytes() {
            return Err(Error::HeaderTooLarge {
                limit: limits.max_header_bytes(),
                actual: head_bytes,
            });
        }
        Ok(Self {
            request: head,
            body,
            state: ClientState::SendingRequest,
            output_segment: OutputSegment::Method,
            output_offset: 0,
            header_index: 0,
            content_length: content_length_bytes,
            content_length_len,
            emit_content_length,
            response_started: false,
            response_body: ResponseBody::ContentLength(0),
            response_header_count: 0,
            response_header_bytes: 0,
            received_body_bytes: 0,
            pending_consume: PendingConsume::None,
            limits,
        })
    }

    pub fn state(&self) -> ClientState {
        self.state
    }

    /// Returns outbound bytes or a streaming-body request state.
    ///
    /// `body_chunk` is used only when a streaming request is ready to send body
    /// bytes. The returned body slice is borrowed directly from `body_chunk`.
    pub fn poll_outbound<'chunk>(
        &'chunk self,
        body_chunk: Option<&'chunk [u8]>,
    ) -> Result<Outbound<'chunk>, Error> {
        if self.output_segment == OutputSegment::Body {
            return self.body_outbound(body_chunk);
        }
        Ok(self
            .current_output_segment()
            .map(|segment| &segment[self.output_offset..])
            .filter(|segment| !segment.is_empty())
            .map_or(Outbound::Done, Outbound::Bytes))
    }

    /// Marks `amount` outbound bytes as written.
    pub fn consume_outbound(&mut self, amount: usize) -> Result<(), Error> {
        if self.output_segment == OutputSegment::Body {
            return self.consume_body_outbound(amount);
        }
        let segment_len = self
            .current_output_segment()
            .map_or(0, <[u8]>::len)
            .saturating_sub(self.output_offset);
        if amount > segment_len {
            return Err(Error::OutputUnderflow);
        }
        self.output_offset += amount;
        if self.output_offset == self.current_output_segment().map_or(0, <[u8]>::len) {
            self.output_offset = 0;
            self.advance_output_segment();
        }
        Ok(())
    }

    /// Advances response parsing using caller-owned input and header scratch.
    pub fn next_event<'headers, 'input>(
        &mut self,
        input: &'input [u8],
        headers: &'headers mut [httparse::Header<'input>],
    ) -> Result<ClientEvent<'headers, 'input>, Error> {
        match self.state {
            ClientState::SendingRequest => Ok(ClientEvent::NeedInput),
            ClientState::Failed => Err(Error::TerminalState),
            ClientState::Complete if input.is_empty() => {
                if self.pending_consume == PendingConsume::None {
                    Ok(ClientEvent::Complete)
                } else {
                    Err(Error::EventPending)
                }
            }
            ClientState::Complete => Err(Error::TerminalState),
            ClientState::ReadingResponse => self.next_response_event(input, headers),
        }
    }

    /// Acknowledges input bytes exposed by the previous response-head or body event.
    pub fn consume_input(&mut self, amount: usize) -> Result<(), Error> {
        match self.pending_consume {
            PendingConsume::None => {
                if amount == 0 {
                    Ok(())
                } else {
                    Err(Error::InputConsumeMismatch {
                        expected: 0,
                        actual: amount,
                    })
                }
            }
            PendingConsume::Head(expected) => {
                if amount != expected {
                    return Err(Error::InputConsumeMismatch {
                        expected,
                        actual: amount,
                    });
                }
                self.pending_consume = PendingConsume::None;
                if self.response_body == ResponseBody::ContentLength(0) {
                    self.state = ClientState::Complete;
                }
                Ok(())
            }
            PendingConsume::Body {
                input_bytes,
                body_bytes,
                completes,
            } => {
                if amount != input_bytes {
                    return Err(Error::InputConsumeMismatch {
                        expected: input_bytes,
                        actual: amount,
                    });
                }
                self.pending_consume = PendingConsume::None;
                if let ResponseBody::ContentLength(remaining) = &mut self.response_body {
                    *remaining = remaining.saturating_sub(body_bytes);
                }
                self.received_body_bytes = self.received_body_bytes.saturating_add(body_bytes);
                if completes || self.response_body == ResponseBody::ContentLength(0) {
                    self.state = ClientState::Complete;
                }
                Ok(())
            }
        }
    }

    /// Marks an EOF-delimited response complete after the transport reaches EOF.
    pub fn finish_eof(&mut self) -> Result<bool, Error> {
        if self.pending_consume != PendingConsume::None {
            return Err(Error::EventPending);
        }
        match self.response_body {
            ResponseBody::Eof if self.response_started => {
                self.state = ClientState::Complete;
                Ok(true)
            }
            ResponseBody::ContentLength(0) if self.response_started => {
                self.state = ClientState::Complete;
                Ok(true)
            }
            _ => {
                self.state = ClientState::Failed;
                Err(Error::Parse)
            }
        }
    }

    fn next_response_event<'headers, 'input>(
        &mut self,
        input: &'input [u8],
        headers: &'headers mut [httparse::Header<'input>],
    ) -> Result<ClientEvent<'headers, 'input>, Error> {
        if self.pending_consume != PendingConsume::None {
            return Err(Error::EventPending);
        }
        if !self.response_started {
            let Some(head_len) = find_header_end(input) else {
                if input.len() >= self.limits.max_header_bytes() {
                    return Err(Error::HeaderTooLarge {
                        limit: self.limits.max_header_bytes(),
                        actual: input.len(),
                    });
                }
                return Ok(ClientEvent::NeedInput);
            };
            if head_len > self.limits.max_header_bytes() {
                return Err(Error::HeaderTooLarge {
                    limit: self.limits.max_header_bytes(),
                    actual: head_len,
                });
            }
            let header_count = http1_header_count(&input[..head_len]);
            if header_count > self.limits.max_headers() {
                return Err(Error::TooManyHeaders {
                    limit: self.limits.max_headers(),
                    actual: header_count,
                });
            }
            let head = self.fail_on_err(parse_response_head(
                &input[..head_len],
                headers,
                self.limits,
            ))?;
            let declared_length = self.fail_on_err(content_length(head.headers))?;
            if let Some(actual) = declared_length
                && actual > self.limits.max_body_bytes()
            {
                self.state = ClientState::Failed;
                return Err(Error::BodyTooLarge {
                    limit: self.limits.max_body_bytes(),
                    actual,
                });
            }
            let response_body = self.fail_on_err(response_body_mode(
                self.request.method,
                head.status,
                head.headers,
            ))?;
            if response_body
                .declared_len()
                .is_some_and(|len| len > self.limits.max_body_bytes())
            {
                self.state = ClientState::Failed;
                return Err(Error::BodyTooLarge {
                    limit: self.limits.max_body_bytes(),
                    actual: response_body.declared_len().unwrap_or(usize::MAX),
                });
            }
            self.response_started = true;
            self.response_body = response_body;
            self.response_header_count = header_count;
            self.response_header_bytes = head_len;
            self.pending_consume = PendingConsume::Head(head_len);
            return Ok(ClientEvent::ResponseHead {
                head,
                consumed: head_len,
            });
        }
        match self.response_body {
            ResponseBody::ContentLength(0) => {
                self.state = ClientState::Complete;
                Ok(ClientEvent::Complete)
            }
            ResponseBody::ContentLength(remaining) => {
                if input.is_empty() {
                    return Ok(ClientEvent::NeedInput);
                }
                let amount = input.len().min(remaining);
                self.pending_consume = PendingConsume::Body {
                    input_bytes: amount,
                    body_bytes: amount,
                    completes: amount == remaining,
                };
                Ok(ClientEvent::BodyChunk {
                    chunk: &input[..amount],
                    consumed: amount,
                })
            }
            ResponseBody::Chunked => self.next_chunked_event(input),
            ResponseBody::Eof => {
                if input.is_empty() {
                    return Ok(ClientEvent::NeedInput);
                }
                self.check_body_addition(input.len())?;
                self.pending_consume = PendingConsume::Body {
                    input_bytes: input.len(),
                    body_bytes: input.len(),
                    completes: false,
                };
                Ok(ClientEvent::BodyChunk {
                    chunk: input,
                    consumed: input.len(),
                })
            }
        }
    }

    fn next_chunked_event<'headers, 'input>(
        &mut self,
        input: &'input [u8],
    ) -> Result<ClientEvent<'headers, 'input>, Error> {
        if input.is_empty() {
            return Ok(ClientEvent::NeedInput);
        }
        let Some(line_end) = find_crlf(input) else {
            return Ok(ClientEvent::NeedInput);
        };
        let size = parse_chunk_size(&input[..line_end]).inspect_err(|_| {
            self.state = ClientState::Failed;
        })?;
        let data_start = line_end + 2;
        if size == 0 {
            let trailer = &input[data_start..];
            if trailer.len() < 2 {
                return Ok(ClientEvent::NeedInput);
            }
            if trailer.starts_with(b"\r\n") {
                self.state = ClientState::Complete;
                return Ok(ClientEvent::Complete);
            }
            if let Some(trailer_len) = find_header_end(trailer) {
                let trailer_count = http1_trailer_count(&trailer[..trailer_len]);
                let actual_count = self.response_header_count.saturating_add(trailer_count);
                if actual_count > self.limits.max_headers() {
                    return Err(Error::TooManyHeaders {
                        limit: self.limits.max_headers(),
                        actual: actual_count,
                    });
                }
                let actual_bytes = self
                    .response_header_bytes
                    .saturating_add(data_start)
                    .saturating_add(trailer_len);
                if actual_bytes > self.limits.max_header_bytes() {
                    return Err(Error::HeaderTooLarge {
                        limit: self.limits.max_header_bytes(),
                        actual: actual_bytes,
                    });
                }
                self.state = ClientState::Complete;
                return Ok(ClientEvent::Complete);
            }
            let actual_bytes = self.response_header_bytes.saturating_add(input.len());
            if actual_bytes >= self.limits.max_header_bytes() {
                return Err(Error::HeaderTooLarge {
                    limit: self.limits.max_header_bytes(),
                    actual: actual_bytes,
                });
            }
            return Ok(ClientEvent::NeedInput);
        }
        let total = data_start
            .checked_add(size)
            .and_then(|end| end.checked_add(2))
            .ok_or(Error::InvalidChunkedBody)
            .inspect_err(|_| {
                self.state = ClientState::Failed;
            })?;
        if input.len() < total {
            return Ok(ClientEvent::NeedInput);
        }
        if &input[data_start + size..total] != b"\r\n" {
            self.state = ClientState::Failed;
            return Err(Error::InvalidChunkedBody);
        }
        self.check_body_addition(size)?;
        self.pending_consume = PendingConsume::Body {
            input_bytes: total,
            body_bytes: size,
            completes: false,
        };
        Ok(ClientEvent::BodyChunk {
            chunk: &input[data_start..data_start + size],
            consumed: total,
        })
    }

    fn check_body_addition(&mut self, additional: usize) -> Result<(), Error> {
        let actual = self.received_body_bytes.saturating_add(additional);
        if actual > self.limits.max_body_bytes() {
            self.state = ClientState::Failed;
            Err(Error::BodyTooLarge {
                limit: self.limits.max_body_bytes(),
                actual,
            })
        } else {
            Ok(())
        }
    }

    fn fail_on_err<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if result.is_err() {
            self.state = ClientState::Failed;
        }
        result
    }

    fn body_outbound<'chunk>(
        &'chunk self,
        body_chunk: Option<&'chunk [u8]>,
    ) -> Result<Outbound<'chunk>, Error> {
        match self.body {
            RequestBody::Borrowed(body) => {
                if self.output_offset < body.len() {
                    Ok(Outbound::Bytes(&body[self.output_offset..]))
                } else {
                    Ok(Outbound::Done)
                }
            }
            RequestBody::Streaming {
                content_length,
                sent,
            } => {
                let remaining = content_length.saturating_sub(sent);
                if remaining == 0 {
                    return Ok(Outbound::Done);
                }
                let Some(chunk) = body_chunk.filter(|chunk| !chunk.is_empty()) else {
                    return Ok(Outbound::NeedBody { remaining });
                };
                Ok(Outbound::Bytes(&chunk[..chunk.len().min(remaining)]))
            }
        }
    }

    fn current_output_segment(&self) -> Option<&[u8]> {
        match self.output_segment {
            OutputSegment::Method => Some(self.request.method.as_bytes()),
            OutputSegment::MethodSpace => Some(b" "),
            OutputSegment::Target => Some(self.request.target.as_bytes()),
            OutputSegment::Version => Some(b" HTTP/1.1\r\n"),
            OutputSegment::HeaderName => self
                .request
                .headers
                .get(self.header_index)
                .map(|header| header.name.as_bytes()),
            OutputSegment::HeaderSeparator => Some(b": "),
            OutputSegment::HeaderValue => self
                .request
                .headers
                .get(self.header_index)
                .map(|header| header.value.as_bytes()),
            OutputSegment::HeaderCrLf => Some(b"\r\n"),
            OutputSegment::ContentLengthPrefix => Some(CONTENT_LENGTH_PREFIX),
            OutputSegment::ContentLengthValue => {
                Some(&self.content_length[..self.content_length_len])
            }
            OutputSegment::ContentLengthCrLf => Some(b"\r\n"),
            OutputSegment::EndHeaders => Some(b"\r\n"),
            OutputSegment::Body => match self.body {
                RequestBody::Borrowed(body) => Some(body),
                RequestBody::Streaming { .. } => None,
            },
            OutputSegment::Done => None,
        }
    }

    fn advance_output_segment(&mut self) {
        loop {
            self.output_segment = match self.output_segment {
                OutputSegment::Method => OutputSegment::MethodSpace,
                OutputSegment::MethodSpace => OutputSegment::Target,
                OutputSegment::Target => OutputSegment::Version,
                OutputSegment::Version => {
                    if self.request.headers.is_empty() {
                        self.next_after_headers()
                    } else {
                        OutputSegment::HeaderName
                    }
                }
                OutputSegment::HeaderName => OutputSegment::HeaderSeparator,
                OutputSegment::HeaderSeparator => OutputSegment::HeaderValue,
                OutputSegment::HeaderValue => OutputSegment::HeaderCrLf,
                OutputSegment::HeaderCrLf => {
                    self.header_index += 1;
                    if self.header_index < self.request.headers.len() {
                        OutputSegment::HeaderName
                    } else {
                        self.next_after_headers()
                    }
                }
                OutputSegment::ContentLengthPrefix => OutputSegment::ContentLengthValue,
                OutputSegment::ContentLengthValue => OutputSegment::ContentLengthCrLf,
                OutputSegment::ContentLengthCrLf => OutputSegment::EndHeaders,
                OutputSegment::EndHeaders => OutputSegment::Body,
                OutputSegment::Body => OutputSegment::Done,
                OutputSegment::Done => OutputSegment::Done,
            };
            if self.output_segment == OutputSegment::Done {
                self.state = ClientState::ReadingResponse;
                return;
            }
            if self.output_segment == OutputSegment::Body {
                if self.body_has_remaining() {
                    return;
                }
                continue;
            }
            if self
                .current_output_segment()
                .is_some_and(|segment| !segment.is_empty())
            {
                return;
            }
        }
    }

    fn next_after_headers(&self) -> OutputSegment {
        if self.emit_content_length {
            OutputSegment::ContentLengthPrefix
        } else {
            OutputSegment::EndHeaders
        }
    }

    fn body_has_remaining(&self) -> bool {
        match self.body {
            RequestBody::Borrowed(body) => self.output_offset < body.len(),
            RequestBody::Streaming {
                content_length,
                sent,
            } => sent < content_length,
        }
    }

    fn consume_body_outbound(&mut self, amount: usize) -> Result<(), Error> {
        match &mut self.body {
            RequestBody::Borrowed(body) => {
                let remaining = body.len().saturating_sub(self.output_offset);
                if amount > remaining {
                    return Err(Error::OutputUnderflow);
                }
                self.output_offset += amount;
                if self.output_offset == body.len() {
                    self.output_offset = 0;
                    self.advance_output_segment();
                }
                Ok(())
            }
            RequestBody::Streaming {
                content_length,
                sent,
            } => {
                let remaining = content_length.saturating_sub(*sent);
                if amount > remaining {
                    return Err(Error::OutputUnderflow);
                }
                *sent += amount;
                if *sent == *content_length {
                    self.advance_output_segment();
                }
                Ok(())
            }
        }
    }
}

fn validate_header(header: &Header<'_>) -> Result<(), Error> {
    validate_token(header.name).map_err(|_| Error::InvalidHeaderName)?;
    if header
        .value
        .bytes()
        .any(|byte| matches!(byte, b'\r' | b'\n'))
    {
        return Err(Error::InvalidHeaderValue);
    }
    Ok(())
}

fn request_head_size(head: RequestHead<'_>, content_length_digits: Option<usize>) -> usize {
    let mut bytes = head
        .method
        .len()
        .saturating_add(1)
        .saturating_add(head.target.len())
        .saturating_add(b" HTTP/1.1\r\n".len())
        .saturating_add(2);
    for header in head.headers {
        bytes = bytes
            .saturating_add(header.name.len())
            .saturating_add(2)
            .saturating_add(header.value.len())
            .saturating_add(2);
    }
    if let Some(digits) = content_length_digits {
        bytes = bytes
            .saturating_add(CONTENT_LENGTH_PREFIX.len())
            .saturating_add(digits)
            .saturating_add(2);
    }
    bytes
}

fn http1_header_count(head: &[u8]) -> usize {
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

fn validate_target(target: &str) -> Result<(), Error> {
    if target.is_empty() || target.bytes().any(|byte| matches!(byte, b'\r' | b'\n')) {
        Err(Error::InvalidRequestTarget)
    } else {
        Ok(())
    }
}

fn validate_token(value: &str) -> Result<(), ()> {
    if value.is_empty() {
        return Err(());
    }
    if value.bytes().all(|byte| {
        matches!(
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
                | b'0'..=b'9'
                | b'A'..=b'Z'
                | b'a'..=b'z'
        )
    }) {
        Ok(())
    } else {
        Err(())
    }
}

fn write_decimal(value: usize, output: &mut [u8; 20]) -> usize {
    let mut value = value;
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
    for index in 0..len {
        output[index] = reversed[len - 1 - index];
    }
    len
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

fn find_crlf(bytes: &[u8]) -> Option<usize> {
    bytes.windows(2).position(|window| window == b"\r\n")
}

fn parse_response_head<'headers, 'input>(
    bytes: &'input [u8],
    headers: &'headers mut [httparse::Header<'input>],
    limits: HttpLimits,
) -> Result<ResponseHead<'headers, 'input>, Error> {
    let scratch_len = headers.len();
    let mut response = httparse::Response::new(headers);
    match response.parse(bytes) {
        Ok(httparse::Status::Complete(_)) => {}
        Ok(httparse::Status::Partial) => return Err(Error::Parse),
        Err(httparse::Error::TooManyHeaders) => {
            return Err(Error::TooManyHeaders {
                limit: limits.max_headers(),
                actual: scratch_len.saturating_add(1),
            });
        }
        Err(_) => return Err(Error::Parse),
    }
    let version = response.version.ok_or(Error::Parse)?;
    let status = response.code.ok_or(Error::MissingStatus)?;
    if !(100..=999).contains(&status) {
        return Err(Error::InvalidStatus);
    }
    Ok(ResponseHead {
        version,
        status,
        reason: response.reason.unwrap_or(""),
        headers: response.headers,
    })
}

fn content_length(headers: &[httparse::Header<'_>]) -> Result<Option<usize>, Error> {
    let mut parsed = None;
    for value in headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case("content-length"))
        .map(|header| header.value)
    {
        let value = crate::parse_content_length(value).ok_or(Error::InvalidContentLength)?;
        if parsed.replace(value).is_some_and(|prior| prior != value) {
            return Err(Error::InvalidContentLength);
        }
    }
    Ok(parsed)
}

fn response_body_mode(
    request_method: &str,
    status: u16,
    headers: &[httparse::Header<'_>],
) -> Result<ResponseBody, Error> {
    if request_method.eq_ignore_ascii_case("HEAD") || matches!(status, 100..=199 | 204 | 304) {
        return Ok(ResponseBody::ContentLength(0));
    }
    if let Some(transfer_encoding) = header_value(headers, "transfer-encoding") {
        let value = std::str::from_utf8(transfer_encoding)
            .map_err(|_| Error::UnsupportedTransferEncoding)?
            .trim();
        if value.eq_ignore_ascii_case("chunked") {
            return Ok(ResponseBody::Chunked);
        }
        return Err(Error::UnsupportedTransferEncoding);
    }
    Ok(content_length(headers)?.map_or(ResponseBody::Eof, ResponseBody::ContentLength))
}

fn header_value<'a>(headers: &'a [httparse::Header<'_>], name: &str) -> Option<&'a [u8]> {
    headers
        .iter()
        .rev()
        .find(|header| header.name.eq_ignore_ascii_case(name))
        .map(|header| header.value)
}

fn parse_chunk_size(bytes: &[u8]) -> Result<usize, Error> {
    let size = bytes
        .split(|byte| *byte == b';')
        .next()
        .unwrap_or(bytes)
        .iter()
        .copied()
        .take_while(|byte| !byte.is_ascii_whitespace());
    let mut value = 0usize;
    let mut saw_digit = false;
    for byte in size {
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return Err(Error::InvalidChunkedBody),
        };
        value = value
            .checked_mul(16)
            .and_then(|value| value.checked_add(digit as usize))
            .ok_or(Error::InvalidChunkedBody)?;
        saw_digit = true;
    }
    if saw_digit {
        Ok(value)
    } else {
        Err(Error::InvalidChunkedBody)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: [Header<'static>; 1] = [Header::new("host", "127.0.0.1")];

    fn get_client() -> HttpClient<'static> {
        HttpClient::request_with_body(
            RequestHead::new("GET", "/account/container/blob", &HOST),
            b"",
        )
        .unwrap()
    }

    fn drain_request(client: &mut HttpClient<'_>) -> Vec<u8> {
        let mut output = Vec::new();
        loop {
            match client.poll_outbound(None).unwrap() {
                Outbound::Bytes(segment) => {
                    output.extend_from_slice(segment);
                    client.consume_outbound(segment.len()).unwrap();
                }
                Outbound::NeedBody { .. } => panic!("fixed-body request should not need chunks"),
                Outbound::Done => break,
            }
        }
        output
    }

    #[test]
    fn emits_request_and_drains_output_without_internal_output_vec() {
        let mut client = get_client();
        let output = drain_request(&mut client);
        assert_eq!(
            std::str::from_utf8(&output).unwrap(),
            "GET /account/container/blob HTTP/1.1\r\nhost: 127.0.0.1\r\ncontent-length: 0\r\n\r\n"
        );
        assert_eq!(client.poll_outbound(None).unwrap(), Outbound::Done);
        assert_eq!(client.state(), ClientState::ReadingResponse);
    }

    #[test]
    fn emits_body_as_borrowed_segment() {
        let mut client =
            HttpClient::request_with_body(RequestHead::new("PUT", "/upload", &HOST), b"hello")
                .unwrap();
        let mut saw_body_segment = false;
        loop {
            match client.poll_outbound(None).unwrap() {
                Outbound::Bytes(segment) => {
                    if segment == b"hello" {
                        saw_body_segment = true;
                    }
                    client.consume_outbound(segment.len()).unwrap();
                }
                Outbound::NeedBody { .. } => panic!("borrowed body should not need chunks"),
                Outbound::Done => break,
            }
        }
        assert!(saw_body_segment);
    }

    #[test]
    fn streaming_body_requests_chunks_without_internal_body_buffer() {
        let mut client =
            HttpClient::request_with_streaming_body(RequestHead::new("PUT", "/upload", &HOST), 5)
                .unwrap();
        let mut output = Vec::new();
        loop {
            match client.poll_outbound(None).unwrap() {
                Outbound::Bytes(bytes) => {
                    output.extend_from_slice(bytes);
                    client.consume_outbound(bytes.len()).unwrap();
                }
                Outbound::NeedBody { remaining } => {
                    assert_eq!(remaining, 5);
                    break;
                }
                Outbound::Done => panic!("body should be pending"),
            }
        }
        assert!(
            std::str::from_utf8(&output)
                .unwrap()
                .contains("content-length: 5\r\n")
        );

        let first_source = b"he";
        let Outbound::Bytes(first) = client.poll_outbound(Some(first_source)).unwrap() else {
            panic!("expected first body chunk");
        };
        assert_eq!(first, b"he");
        assert_eq!(first.as_ptr(), first_source.as_ptr());
        client.consume_outbound(first.len()).unwrap();

        let second_source = b"llo extra";
        let Outbound::Bytes(second) = client.poll_outbound(Some(second_source)).unwrap() else {
            panic!("expected second body chunk");
        };
        assert_eq!(second, b"llo");
        assert_eq!(second.as_ptr(), second_source.as_ptr());
        client.consume_outbound(second.len()).unwrap();
        assert_eq!(client.poll_outbound(None).unwrap(), Outbound::Done);
        assert_eq!(client.state(), ClientState::ReadingResponse);
    }

    #[test]
    fn parses_complete_response_head_body_and_complete_from_caller_buffer() {
        let mut client = get_client();
        drain_request(&mut client);
        let mut headers = [httparse::EMPTY_HEADER; 8];
        let mut input =
            b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\nx-ms-request-id: abc\r\n\r\nhello".as_slice();

        let ClientEvent::ResponseHead { head, consumed } =
            client.next_event(input, &mut headers).unwrap()
        else {
            panic!("expected response head");
        };
        assert_eq!(head.status, 200);
        assert_eq!(head.headers[1].name, "x-ms-request-id");
        assert_eq!(head.headers[1].value, b"abc");
        client.consume_input(consumed).unwrap();
        input = &input[consumed..];

        let ClientEvent::BodyChunk { chunk, consumed } =
            client.next_event(input, &mut headers).unwrap()
        else {
            panic!("expected body chunk");
        };
        assert_eq!(chunk, b"hello");
        assert_eq!(chunk.as_ptr(), input.as_ptr());
        client.consume_input(consumed).unwrap();
        input = &input[consumed..];

        assert_eq!(
            client.next_event(input, &mut headers).unwrap(),
            ClientEvent::Complete
        );
    }

    #[test]
    fn response_events_apply_backpressure_until_consumed() {
        let mut client = get_client();
        drain_request(&mut client);
        let mut headers = [httparse::EMPTY_HEADER; 8];
        let input = b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello";
        assert!(matches!(
            client.next_event(input, &mut headers).unwrap(),
            ClientEvent::ResponseHead { .. }
        ));
        assert_eq!(
            client.next_event(input, &mut headers),
            Err(Error::EventPending)
        );
    }

    #[test]
    fn head_response_with_content_length_completes_without_body() {
        let mut client =
            HttpClient::request_with_body(RequestHead::new("HEAD", "/blob", &HOST), b"").unwrap();
        drain_request(&mut client);
        let mut headers = [httparse::EMPTY_HEADER; 8];
        let input = b"HTTP/1.1 200 OK\r\ncontent-length: 123\r\n\r\n";

        let ClientEvent::ResponseHead { head, consumed } =
            client.next_event(input, &mut headers).unwrap()
        else {
            panic!("expected response head");
        };
        assert_eq!(head.status, 200);
        assert_eq!(consumed, input.len());
        client.consume_input(consumed).unwrap();
        assert_eq!(
            client.next_event(b"", &mut headers).unwrap(),
            ClientEvent::Complete
        );
    }

    #[test]
    fn no_body_status_ignores_content_length() {
        let mut client = get_client();
        drain_request(&mut client);
        let mut headers = [httparse::EMPTY_HEADER; 8];
        let input = b"HTTP/1.1 204 No Content\r\ncontent-length: 123\r\n\r\n";

        let ClientEvent::ResponseHead { consumed, .. } =
            client.next_event(input, &mut headers).unwrap()
        else {
            panic!("expected response head");
        };
        client.consume_input(consumed).unwrap();
        assert_eq!(
            client.next_event(b"", &mut headers).unwrap(),
            ClientEvent::Complete
        );
    }

    #[test]
    fn no_body_response_still_enforces_declared_body_limit() {
        let mut client =
            HttpClient::response_reader("HEAD", HttpLimits::new().set_max_body_bytes(3)).unwrap();
        let mut headers = [httparse::EMPTY_HEADER; 8];
        assert_eq!(
            client.next_event(
                b"HTTP/1.1 200 OK\r\ncontent-length: 4\r\n\r\n",
                &mut headers
            ),
            Err(Error::BodyTooLarge {
                limit: 3,
                actual: 4
            })
        );
    }

    #[test]
    fn preserves_partial_response_head_until_complete() {
        let mut client = get_client();
        drain_request(&mut client);
        let mut headers = [httparse::EMPTY_HEADER; 8];
        assert_eq!(
            client
                .next_event(b"HTTP/1.1 200 OK\r\ncontent", &mut headers)
                .unwrap(),
            ClientEvent::NeedInput
        );
        assert!(matches!(
            client
                .next_event(
                    b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n",
                    &mut headers
                )
                .unwrap(),
            ClientEvent::ResponseHead { consumed: 38, .. }
        ));
        client.consume_input(38).unwrap();
        assert_eq!(
            client.next_event(b"", &mut headers).unwrap(),
            ClientEvent::Complete
        );
    }

    #[test]
    fn splits_content_length_body_across_caller_buffers() {
        let mut client = get_client();
        drain_request(&mut client);
        let mut headers = [httparse::EMPTY_HEADER; 8];
        let mut input = b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhe".as_slice();
        let ClientEvent::ResponseHead { consumed, .. } =
            client.next_event(input, &mut headers).unwrap()
        else {
            panic!("expected response head");
        };
        client.consume_input(consumed).unwrap();
        input = &input[consumed..];
        let ClientEvent::BodyChunk { chunk, consumed } =
            client.next_event(input, &mut headers).unwrap()
        else {
            panic!("expected first chunk");
        };
        assert_eq!(chunk, b"he");
        assert_eq!(consumed, 2);
        client.consume_input(consumed).unwrap();
        assert_eq!(
            client.next_event(b"", &mut headers).unwrap(),
            ClientEvent::NeedInput
        );
        let ClientEvent::BodyChunk { chunk, consumed } =
            client.next_event(b"llo", &mut headers).unwrap()
        else {
            panic!("expected second chunk");
        };
        assert_eq!(chunk, b"llo");
        assert_eq!(consumed, 3);
        client.consume_input(consumed).unwrap();
        assert_eq!(
            client.next_event(b"", &mut headers).unwrap(),
            ClientEvent::Complete
        );
    }

    #[test]
    fn reports_protocol_errors() {
        let mut client = get_client();
        drain_request(&mut client);
        let mut headers = [httparse::EMPTY_HEADER; 8];
        assert_eq!(
            client.next_event(b"not http\r\n\r\n", &mut headers),
            Err(Error::Parse)
        );
        assert_eq!(client.state(), ClientState::Failed);
    }

    #[test]
    fn enforces_body_limits_for_borrowed_streaming_and_response_bodies() {
        assert_eq!(
            HttpClient::request_with_body_limits(
                RequestHead::new("PUT", "/upload", &HOST),
                b"abcd",
                HttpLimits::new().set_max_body_bytes(3),
            )
            .err(),
            Some(Error::BodyTooLarge {
                limit: 3,
                actual: 4
            })
        );
        assert_eq!(
            HttpClient::request_with_streaming_body_limits(
                RequestHead::new("PUT", "/upload", &HOST),
                4,
                HttpLimits::new().set_max_body_bytes(3),
            )
            .err(),
            Some(Error::BodyTooLarge {
                limit: 3,
                actual: 4
            })
        );

        let mut client = HttpClient::request_with_body_limits(
            RequestHead::new("GET", "/small", &HOST),
            b"",
            HttpLimits::new().set_max_body_bytes(3),
        )
        .unwrap();
        drain_request(&mut client);
        let mut headers = [httparse::EMPTY_HEADER; 8];
        assert_eq!(
            client.next_event(
                b"HTTP/1.1 200 OK\r\ncontent-length: 4\r\n\r\n",
                &mut headers
            ),
            Err(Error::BodyTooLarge {
                limit: 3,
                actual: 4
            })
        );
        assert_eq!(client.state(), ClientState::Failed);
    }

    #[test]
    fn rejects_feed_after_terminal_state() {
        let mut client = get_client();
        drain_request(&mut client);
        let mut headers = [httparse::EMPTY_HEADER; 8];
        let input = b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n";
        let ClientEvent::ResponseHead { consumed, .. } =
            client.next_event(input, &mut headers).unwrap()
        else {
            panic!("expected response head");
        };
        client.consume_input(consumed).unwrap();
        assert_eq!(
            client.next_event(b"", &mut headers).unwrap(),
            ClientEvent::Complete
        );
        assert_eq!(
            client.next_event(b"extra", &mut headers),
            Err(Error::TerminalState)
        );
    }

    #[test]
    fn enforces_header_limit() {
        let mut client =
            HttpClient::response_reader("GET", HttpLimits::new().set_max_header_bytes(8)).unwrap();
        let mut headers = [httparse::EMPTY_HEADER; 8];
        assert_eq!(
            client.next_event(b"HTTP/1.1 200 OK\r\n", &mut headers),
            Err(Error::HeaderTooLarge {
                limit: 8,
                actual: 17
            })
        );
    }

    #[test]
    fn enforces_response_header_count_limit() {
        let mut client =
            HttpClient::response_reader("GET", HttpLimits::new().set_max_headers(1)).unwrap();
        let mut headers = [httparse::EMPTY_HEADER; 4];
        assert_eq!(
            client.next_event(
                b"HTTP/1.1 200 OK\r\nx-one: 1\r\nx-two: 2\r\n\r\n",
                &mut headers
            ),
            Err(Error::TooManyHeaders {
                limit: 1,
                actual: 2
            })
        );
    }

    #[test]
    fn enforces_accumulated_chunked_body_limit() {
        let mut client =
            HttpClient::response_reader("GET", HttpLimits::new().set_max_body_bytes(3)).unwrap();
        let mut headers = [httparse::EMPTY_HEADER; 4];
        let head = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n";
        let ClientEvent::ResponseHead { consumed, .. } =
            client.next_event(head, &mut headers).unwrap()
        else {
            panic!("expected response head");
        };
        client.consume_input(consumed).unwrap();
        assert_eq!(
            client.next_event(b"4\r\nbody\r\n", &mut headers),
            Err(Error::BodyTooLarge {
                limit: 3,
                actual: 4
            })
        );
    }

    #[test]
    fn rejects_mismatched_input_consumption() {
        let mut client = get_client();
        drain_request(&mut client);
        let mut headers = [httparse::EMPTY_HEADER; 8];
        let input = b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n";
        let ClientEvent::ResponseHead { consumed, .. } =
            client.next_event(input, &mut headers).unwrap()
        else {
            panic!("expected response head");
        };
        assert_eq!(
            client.consume_input(consumed - 1),
            Err(Error::InputConsumeMismatch {
                expected: consumed,
                actual: consumed - 1
            })
        );
    }
}
