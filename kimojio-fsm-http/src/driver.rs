use std::collections::{HashMap, VecDeque};

use crate::persistence::{comma_list_tokens, http1_response_is_persistent};
use crate::server::{
    H2HeaderValidationRole, forbidden_trailer, http1_header_count, validate_decoded_header_fields,
};
use crate::{
    CLIENT_PREFACE, H2ByteClientEventRef, H2ByteStreamEventRef, H2Client, H2DataFramePlan,
    H2ErrorCode, H2ErrorScope, H2FairStreamScheduler, H2Frame, H2HeaderField, H2Limits,
    H2OutboundCommit, H2ProtocolError, H2Server, Http1BodyKind, Http1ConnectionDecoder,
    Http1ConnectionEvent, Http1HeaderScratch, Http1MessageHead, Http1ResponseContext,
    Http1ResponseParts, Http1ResponsePlan, Http1Server, Http1Version, HttpLimits, ParseHeader,
    ServerError, hpack_field_size, http1_request_is_persistent, http1_version, is_framing_header,
    project_h2_request_head, project_h2_response_head,
};

/// The wire protocol a connection is using.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum HttpProtocol {
    /// HTTP/1.x.
    Http1,
    /// HTTP/2.
    Http2,
}

impl HttpProtocol {
    /// Maps an ALPN protocol identifier to its HTTP protocol.
    pub fn from_alpn(identifier: Option<&[u8]>) -> Result<Self, ServerError> {
        match identifier {
            // RFC 9113 Sections 3.1 and 3.2 reserve `h2` for HTTP/2 over TLS.
            Some(b"h2") => Ok(Self::Http2),
            // RFC 9112 Section 12.4 registers `http/1.1` as the HTTP/1.1 ALPN ID.
            Some(b"http/1.1") => Ok(Self::Http1),
            // RFC 9113 Sections 3.2 and 3.3 require TLS HTTP/2 to negotiate `h2`.
            None => Ok(Self::Http1),
            // RFC 9113 Section 3.2 forbids `h2c` with TLS, and RFC 7301
            // Section 3.2 makes every selected ALPN protocol definitive.
            // Guessing another protocol here would create protocol confusion.
            Some(_) => Err(ServerError::UnsupportedAlpnProtocol),
        }
    }

    /// The ALPN protocol identifier for this protocol.
    pub const fn alpn_identifier(self) -> &'static [u8] {
        match self {
            Self::Http1 => b"http/1.1",
            Self::Http2 => b"h2",
        }
    }
}

/// How a server connection learns its wire protocol.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProtocolSelection<'a> {
    /// Detect HTTP/2 from the cleartext connection preface, else HTTP/1.
    Detect,
    /// A protocol already known out of band (prior knowledge).
    Known(HttpProtocol),
    /// The ALPN protocol identifier the TLS handshake selected, or `None` when
    /// the peer selected nothing.
    Alpn(Option<&'a [u8]>),
}

/// Whether request bodies are buffered or streamed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RequestBodyMode {
    /// Enforce the configured total request-body limit.
    Buffered,
    /// Surface request chunks without enforcing a total request-body limit.
    Streaming,
}

/// A wire-level HTTP version reported by a connection driver.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum HttpVersion {
    /// HTTP/1.0.
    Http10,
    /// HTTP/1.1.
    Http11,
    /// HTTP/2.
    Http2,
}

/// The expectation declared by an inbound request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RequestExpectation {
    /// No expectation was declared.
    None,
    /// Every non-empty `Expect` list element was `100-continue`.
    ///
    /// HTTP/1.1 request bodies are gated. HTTP/1.0 and HTTP/2 report the field
    /// but proceed without an informational-response handshake.
    Continue,
    /// At least one expectation is not supported.
    Unsupported,
}

/// Verdict after processing one HTTP/2 frame while no exchange is active.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ClientIdleStatus {
    /// The input does not yet contain a complete frame.
    NeedInput,
    /// A connection-level control frame was processed and reuse remains safe.
    Reusable,
    /// The frame is not valid idle connection-level traffic.
    NotReusable,
}

/// One unit of progress requested by a driver.
#[derive(Debug)]
#[non_exhaustive]
pub enum Step<'a, E> {
    /// More inbound bytes are required before progress can continue.
    NeedInput,
    /// These bytes must be written to the transport before continuing.
    Write(&'a [u8]),
    /// A protocol event is ready.
    Event(E),
    /// The exchange is complete.
    Done,
}

/// A borrowed header field used at the driver boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HeaderRef<'a> {
    name: &'a [u8],
    value: &'a [u8],
    sensitive: bool,
}

impl<'a> HeaderRef<'a> {
    /// Creates a non-sensitive borrowed header field.
    pub const fn new(name: &'a [u8], value: &'a [u8]) -> Self {
        Self {
            name,
            value,
            sensitive: false,
        }
    }

    /// Marks whether HTTP/2 compression should avoid indexing this field.
    pub const fn with_sensitive(mut self, sensitive: bool) -> Self {
        self.sensitive = sensitive;
        self
    }

    /// Returns the field name.
    pub const fn name(self) -> &'a [u8] {
        self.name
    }

    /// Returns the field value.
    pub const fn value(self) -> &'a [u8] {
        self.value
    }

    /// Returns whether the field carries sensitive data.
    pub const fn sensitive(self) -> bool {
        self.sensitive
    }
}

/// A uniformly iterable borrowed HTTP/1 or HTTP/2 header block.
#[derive(Clone, Copy, Debug)]
pub struct HeaderBlock<'a> {
    inner: HeaderBlockInner<'a>,
}

#[derive(Clone, Copy, Debug)]
enum HeaderBlockInner<'a> {
    Http1(&'a [ParseHeader<'a>]),
    Http2(&'a [H2HeaderField]),
}

impl<'a> HeaderBlock<'a> {
    fn http1(headers: &'a [ParseHeader<'a>]) -> Self {
        Self {
            inner: HeaderBlockInner::Http1(headers),
        }
    }

    fn http2(headers: &'a [H2HeaderField]) -> Self {
        Self {
            inner: HeaderBlockInner::Http2(headers),
        }
    }

    /// Returns the number of fields.
    pub fn len(self) -> usize {
        match self.inner {
            HeaderBlockInner::Http1(headers) => headers.len(),
            HeaderBlockInner::Http2(headers) => headers.len(),
        }
    }

    /// Returns whether the block contains no fields.
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// Iterates over fields in wire order.
    pub fn iter(self) -> HeaderIter<'a> {
        HeaderIter {
            block: self,
            index: 0,
        }
    }
}

/// An iterator over a [`HeaderBlock`].
#[derive(Clone, Debug)]
pub struct HeaderIter<'a> {
    block: HeaderBlock<'a>,
    index: usize,
}

impl<'a> Iterator for HeaderIter<'a> {
    type Item = HeaderRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let header = match self.block.inner {
            HeaderBlockInner::Http1(headers) => {
                let header = headers.get(self.index)?;
                HeaderRef::new(header.name.as_bytes(), header.value)
            }
            HeaderBlockInner::Http2(headers) => {
                let header = headers.get(self.index)?;
                HeaderRef::new(&header.name, &header.value).with_sensitive(header.sensitive)
            }
        };
        self.index += 1;
        Some(header)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.block.len().saturating_sub(self.index);
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for HeaderIter<'_> {}

fn classify_request_expectation<'a>(
    headers: impl IntoIterator<Item = HeaderRef<'a>>,
) -> RequestExpectation {
    let mut expectation = RequestExpectation::None;
    for header in headers
        .into_iter()
        .filter(|header| header.name().eq_ignore_ascii_case(b"expect"))
    {
        for token in comma_list_tokens(header.value()) {
            if !token.eq_ignore_ascii_case(b"100-continue") {
                return RequestExpectation::Unsupported;
            }
            expectation = RequestExpectation::Continue;
        }
    }
    expectation
}

fn request_expectation(headers: HeaderBlock<'_>) -> RequestExpectation {
    classify_request_expectation(headers.iter())
}

/// Applies RFC 9110 section 10.1.1's rule that a `100-continue` expectation in
/// an HTTP/1.0 request must be ignored.
///
/// Reporting `Continue` there would promise a handshake that never happens:
/// HTTP/1.0 neither gates the body nor receives an informational response, so a
/// caller acting on it would wait for an exchange that has already moved on.
/// HTTP/2 is left alone, because it may legitimately carry an informational
/// response even though this driver does not gate its body. An unsupported
/// expectation is still reported, because refusing one is version independent.
fn expectation_for_version(
    expectation: RequestExpectation,
    version: HttpVersion,
) -> RequestExpectation {
    if expectation == RequestExpectation::Continue && version == HttpVersion::Http10 {
        return RequestExpectation::None;
    }
    expectation
}

/// A request description used to start a client exchange.
#[derive(Clone, Copy, Debug)]
pub struct ClientRequest<'a> {
    /// Request method.
    pub method: &'a str,
    /// URI scheme used by HTTP/2.
    pub scheme: &'a str,
    /// URI authority used by HTTP/2.
    pub authority: &'a str,
    /// HTTP request target or HTTP/2 path.
    pub target: &'a str,
    /// Request header fields.
    pub headers: &'a [HeaderRef<'a>],
    /// Body length, or `None` when it will be streamed to completion.
    pub body_len: Option<usize>,
}

/// A response description used to start a server response.
#[derive(Clone, Copy, Debug)]
pub struct ConnectionResponse<'a> {
    /// Numeric response status.
    pub status: u16,
    /// HTTP/1 reason phrase.
    pub reason: &'a str,
    /// Response header fields.
    pub headers: &'a [HeaderRef<'a>],
    /// Body length, or `None` when it will be streamed to completion.
    pub body_len: Option<usize>,
}

/// A connection-local identifier for one request/response exchange.
///
/// HTTP/2 exchanges correspond to streams. HTTP/1 exchanges receive synthetic
/// monotonic identifiers so adapters can use one protocol-neutral path.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ExchangeId(u64);

impl ExchangeId {
    const fn http2(stream_id: u32) -> Self {
        Self(stream_id as u64)
    }

    /// Returns the connection-local numeric identifier.
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// A stream failure that the driver converted into an outbound `RST_STREAM`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HandledStreamError {
    exchange_id: ExchangeId,
    error: ServerError,
    protocol_error: H2ProtocolError,
}

impl HandledStreamError {
    /// Returns the exchange that was reset.
    pub const fn exchange_id(&self) -> ExchangeId {
        self.exchange_id
    }

    /// Returns the compatibility error produced while processing the stream.
    pub const fn error(&self) -> &ServerError {
        &self.error
    }

    /// Returns the HTTP/2 stream error sent to the peer.
    pub const fn protocol_error(&self) -> H2ProtocolError {
        self.protocol_error
    }
}

/// A protocol-neutral event emitted while receiving a request.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub enum ServerEvent<'a> {
    /// A complete request head is ready.
    RequestHead {
        /// Exchange receiving this request.
        exchange_id: ExchangeId,
        /// Request method token.
        method: &'a [u8],
        /// Request target or HTTP/2 path.
        target: &'a [u8],
        /// HTTP/2 authority to synthesize as `host`, when needed.
        authority: Option<&'a [u8]>,
        /// Request wire version.
        version: HttpVersion,
        /// Regular request fields in wire order.
        headers: HeaderBlock<'a>,
        /// Validated content length, when declared.
        content_length: Option<usize>,
        /// Parsed request expectation.
        expectation: RequestExpectation,
    },
    /// A zero-copy request body chunk is ready.
    RequestBody {
        /// Exchange receiving this body chunk.
        exchange_id: ExchangeId,
        /// Bytes borrowed directly from the caller's input buffer.
        chunk: &'a [u8],
    },
    /// Request trailer fields are ready.
    RequestTrailers {
        /// Exchange receiving these trailers.
        exchange_id: ExchangeId,
        /// Trailer fields in wire order.
        headers: HeaderBlock<'a>,
    },
    /// The complete request has been received.
    RequestComplete {
        /// Exchange whose request is complete.
        exchange_id: ExchangeId,
    },
}

/// A protocol-neutral event emitted while receiving a response.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub enum ClientEvent<'a> {
    /// A final response head is ready.
    ResponseHead {
        /// Exchange receiving this response.
        exchange_id: ExchangeId,
        /// Numeric response status.
        status: u16,
        /// Response wire version.
        version: HttpVersion,
        /// Regular response fields in wire order.
        headers: HeaderBlock<'a>,
        /// Validated content length, when declared.
        content_length: Option<usize>,
    },
    /// A zero-copy response body chunk is ready.
    ResponseBody {
        /// Exchange receiving this body chunk.
        exchange_id: ExchangeId,
        /// Bytes borrowed directly from the caller's input buffer.
        chunk: &'a [u8],
    },
    /// Response trailer fields are ready.
    ResponseTrailers {
        /// Exchange receiving these trailers.
        exchange_id: ExchangeId,
        /// Trailer fields in wire order.
        headers: HeaderBlock<'a>,
    },
    /// The complete response has been received.
    ResponseComplete {
        /// Exchange whose response is complete.
        exchange_id: ExchangeId,
    },
}

/// A prepared zero-copy outbound body chunk.
#[derive(Clone, Copy, Debug)]
pub struct BodyChunk<'a> {
    header: &'a [u8],
    payload_len: usize,
    footer: &'a [u8],
}

impl<'a> BodyChunk<'a> {
    /// Returns the protocol framing bytes to write before the payload.
    pub const fn header(self) -> &'a [u8] {
        self.header
    }

    /// Returns the caller-owned payload prefix to write.
    pub const fn payload_len(self) -> usize {
        self.payload_len
    }

    /// Returns the protocol framing bytes to write after the payload.
    pub const fn footer(self) -> &'a [u8] {
        self.footer
    }
}

#[derive(Clone, Copy, Debug)]
enum WriteKind {
    Buffer,
    H2Block {
        commit: H2OutboundCommit,
        finishing_exchange: Option<ExchangeId>,
    },
}

#[derive(Clone, Copy, Debug)]
enum PendingStep {
    Input(usize),
    Write { input: usize, kind: WriteKind },
}

impl PendingStep {
    const fn input(self) -> usize {
        match self {
            Self::Input(input) | Self::Write { input, .. } => input,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum ServerPendingBody {
    Http1 {
        exchange_id: ExchangeId,
        payload_len: usize,
        footer: &'static [u8],
        finishes_body: bool,
    },
    Http2 {
        exchange_id: ExchangeId,
        plan: H2DataFramePlan,
        finishes_body: bool,
    },
}

impl ServerPendingBody {
    const fn exchange_id(&self) -> ExchangeId {
        match self {
            Self::Http1 { exchange_id, .. } | Self::Http2 { exchange_id, .. } => *exchange_id,
        }
    }
}

enum PendingBody {
    Http1 {
        exchange_id: ExchangeId,
        payload_len: usize,
        footer: &'static [u8],
        finishes_body: bool,
    },
    Http2 {
        exchange_id: ExchangeId,
        plan: H2DataFramePlan,
    },
}

impl PendingBody {
    const fn exchange_id(&self) -> ExchangeId {
        match self {
            Self::Http1 { exchange_id, .. } | Self::Http2 { exchange_id, .. } => *exchange_id,
        }
    }
}

fn h2_limits(limits: HttpLimits) -> H2Limits {
    let mut h2 = H2Limits::from_http_limits(limits);
    if h2.max_queued_data_bytes == 0 {
        h2.max_queued_data_bytes = 1;
    }
    h2
}

fn receive_window(limits: HttpLimits) -> u32 {
    limits.max_body_bytes().clamp(65_535, 2_147_483_647) as u32
}

fn streaming_body_limits(limits: HttpLimits) -> HttpLimits {
    limits.set_max_body_bytes(usize::MAX)
}

fn new_h2_server(limits: HttpLimits, stream_request_bodies: bool) -> H2Server {
    let window = receive_window(limits);
    let mut server = H2Server::with_local_flow_control_and_http_limits(
        window,
        window,
        h2_limits(limits),
        limits,
    )
    .expect("HttpLimits always produce valid HTTP/2 limits");
    if stream_request_bodies {
        server.stream_request_bodies();
    }
    server
}

fn new_h2_client(limits: HttpLimits) -> H2Client {
    let window = receive_window(limits);
    H2Client::with_local_flow_control_and_http_limits(window, window, h2_limits(limits), limits)
        .expect("HttpLimits always produce valid HTTP/2 limits")
}

fn parse_headers<'a>(headers: &'a [HeaderRef<'a>]) -> Result<Vec<ParseHeader<'a>>, ServerError> {
    headers
        .iter()
        .map(|header| {
            let name = std::str::from_utf8(header.name).map_err(|_| ServerError::InvalidHeader)?;
            Ok(ParseHeader {
                name,
                value: header.value,
            })
        })
        .collect()
}

fn h2_headers(headers: &[HeaderRef<'_>], filter_framing: bool) -> Vec<H2HeaderField> {
    headers
        .iter()
        .filter(|header| !filter_framing || !is_framing_header(header.name))
        .map(|header| H2HeaderField {
            name: header.name.to_vec(),
            value: header.value.to_vec(),
            sensitive: header.sensitive,
        })
        .collect()
}

#[derive(Debug)]
enum ResponseTrailers {
    Http1(Vec<u8>),
    Http2(Vec<H2HeaderField>),
}

fn valid_http1_field_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name.iter().all(|byte| {
            byte.is_ascii_alphanumeric()
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
        })
}

fn valid_http1_field_value(value: &[u8]) -> bool {
    value
        .iter()
        .all(|byte| *byte == b'\t' || *byte >= b' ' && *byte != 0x7f)
}

fn http1_trailer_bytes(headers: &[HeaderRef<'_>]) -> Result<Vec<u8>, ServerError> {
    let mut output = Vec::new();
    output.extend_from_slice(b"0\r\n");
    for header in headers {
        let name = std::str::from_utf8(header.name()).map_err(|_| ServerError::InvalidHeader)?;
        if !valid_http1_field_name(header.name())
            || !valid_http1_field_value(header.value())
            || forbidden_trailer(name)
        {
            return Err(ServerError::InvalidHeader);
        }
        output.extend_from_slice(header.name());
        output.extend_from_slice(b": ");
        output.extend_from_slice(header.value());
        output.extend_from_slice(b"\r\n");
    }
    output.extend_from_slice(b"\r\n");
    Ok(output)
}

fn h2_trailer_fields(headers: &[HeaderRef<'_>]) -> Result<Vec<H2HeaderField>, ServerError> {
    for header in headers {
        let name = std::str::from_utf8(header.name()).map_err(|_| ServerError::InvalidHeader)?;
        if forbidden_trailer(name) {
            return Err(ServerError::InvalidHeader);
        }
    }
    let fields = h2_headers(headers, false);
    validate_decoded_header_fields(&fields, H2HeaderValidationRole::Trailers)
        .map_err(|_| ServerError::InvalidHeader)?;
    Ok(fields)
}

fn enforce_outbound_trailer_limits(
    header_count: usize,
    header_bytes: usize,
    trailer_count: usize,
    trailer_bytes: usize,
    limits: HttpLimits,
) -> Result<(), ServerError> {
    let actual_count = header_count.saturating_add(trailer_count);
    if actual_count > limits.max_headers() {
        return Err(ServerError::TooManyHeaders {
            limit: limits.max_headers(),
            actual: actual_count,
        });
    }
    let actual_bytes = header_bytes.saturating_add(trailer_bytes);
    if actual_bytes > limits.max_header_bytes() {
        return Err(ServerError::HeaderTooLarge {
            limit: limits.max_header_bytes(),
            actual: actual_bytes,
        });
    }
    Ok(())
}

enum ServerInner {
    Detect,
    Http1 {
        decoder: Http1ConnectionDecoder,
        scratch: Http1HeaderScratch,
    },
    Http2(Box<H2Server>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ServerStage {
    Reading,
    Responding,
    Done,
}

#[derive(Debug)]
struct ServerExchangeState {
    stream_id: Option<u32>,
    stage: ServerStage,
    request_complete: bool,
    request_persistent: bool,
    request_method: Vec<u8>,
    request_version: HttpVersion,
    expectation: RequestExpectation,
    continue_pending: bool,
    automatic_rejection_pending: bool,
    request_events_suppressed: bool,
    outbound_body_len: Option<usize>,
    outbound_body_sent: usize,
    outbound_header_count: usize,
    outbound_header_bytes: usize,
    outbound_trailers: Option<ResponseTrailers>,
    finishing_header: Option<H2OutboundCommit>,
    response_closes_connection: bool,
}

impl ServerExchangeState {
    fn new(
        stream_id: Option<u32>,
        request_method: &[u8],
        request_version: HttpVersion,
        request_persistent: bool,
        expectation: RequestExpectation,
        body_expected: bool,
    ) -> Self {
        Self {
            stream_id,
            stage: ServerStage::Reading,
            request_complete: false,
            request_persistent,
            request_method: request_method.to_vec(),
            request_version,
            expectation,
            continue_pending: expectation == RequestExpectation::Continue
                && request_version == HttpVersion::Http11
                && body_expected,
            automatic_rejection_pending: expectation == RequestExpectation::Unsupported,
            request_events_suppressed: false,
            outbound_body_len: Some(0),
            outbound_body_sent: 0,
            outbound_header_count: 0,
            outbound_header_bytes: 0,
            outbound_trailers: None,
            finishing_header: None,
            response_closes_connection: true,
        }
    }

    /// Whether this exchange can be retired.
    ///
    /// The second arm covers a rejected expectation, where the response was
    /// sent without the request body ever arriving. Such an exchange is
    /// finished only because the connection is closing: the peer may still put
    /// the body it was refused on the wire, and those bytes would otherwise be
    /// read as the head of the next request. That is why the arm requires
    /// `response_closes_connection` - an abandoned unread body must never be
    /// followed by connection reuse.
    fn is_finished(&self) -> bool {
        self.stage == ServerStage::Done
            && (self.request_complete
                || (self.stream_id.is_none()
                    && self.request_events_suppressed
                    && self.response_closes_connection))
    }
}

/// An I/O-free server-side connection driver.
///
/// The driver detects HTTP/1 versus HTTP/2, owns protocol state and parser
/// scratch, and exposes only transport reads, writes, events, and zero-copy
/// outbound body plans.
pub struct ServerConnection {
    inner: ServerInner,
    protocol: Option<HttpProtocol>,
    limits: HttpLimits,
    stream_request_bodies: bool,
    pending: Option<PendingStep>,
    output: Vec<u8>,
    pending_body: Option<ServerPendingBody>,
    complete_pending: Option<ExchangeId>,
    exchanges: HashMap<ExchangeId, ServerExchangeState>,
    http1_exchange: Option<ExchangeId>,
    next_http1_exchange_id: u64,
    http1_request_count: usize,
    response_scheduler: H2FairStreamScheduler,
    scheduled_body: Option<ExchangeId>,
    buffer_finishes: Option<ExchangeId>,
    reset_pending: Option<ExchangeId>,
    response_resets: VecDeque<ExchangeId>,
    handled_stream_error: Option<HandledStreamError>,
    terminal_h2_error: Option<H2ProtocolError>,
    shutdown_started: bool,
}

impl ServerConnection {
    /// Creates a server driver with explicit message limits.
    pub fn new(limits: HttpLimits) -> Self {
        Self::with_protocol_selection(ProtocolSelection::Detect, limits, RequestBodyMode::Buffered)
            .expect("cleartext protocol detection cannot fail during construction")
    }

    /// Creates a server driver that surfaces request bodies without a total-size limit.
    ///
    /// Header, input-buffer, flow-control, and queued-DATA bounds remain in
    /// force; only the buffered-body total is inapplicable to streamed bytes.
    pub fn new_streaming(limits: HttpLimits) -> Self {
        Self::with_protocol_selection(
            ProtocolSelection::Detect,
            limits,
            RequestBodyMode::Streaming,
        )
        .expect("cleartext protocol detection cannot fail during construction")
    }

    /// Creates a server driver for a protocol already selected by the transport.
    ///
    /// HTTP/2 still consumes and validates the client connection preface; this
    /// constructor only skips HTTP/1 versus HTTP/2 sniffing.
    pub fn new_with_protocol(protocol: HttpProtocol, limits: HttpLimits) -> Self {
        Self::with_protocol_selection(
            ProtocolSelection::Known(protocol),
            limits,
            RequestBodyMode::Buffered,
        )
        .expect("a known HTTP protocol cannot fail during construction")
    }

    /// Creates a protocol-selected server driver that streams request bodies.
    pub fn new_with_protocol_streaming(protocol: HttpProtocol, limits: HttpLimits) -> Self {
        Self::with_protocol_selection(
            ProtocolSelection::Known(protocol),
            limits,
            RequestBodyMode::Streaming,
        )
        .expect("a known HTTP protocol cannot fail during construction")
    }

    /// Creates a server driver with explicit protocol selection and request-body mode.
    ///
    /// HTTP/2 selections still consume and validate the client connection
    /// preface after cleartext prior knowledge or TLS ALPN negotiation.
    pub fn with_protocol_selection(
        selection: ProtocolSelection<'_>,
        limits: HttpLimits,
        body_mode: RequestBodyMode,
    ) -> Result<Self, ServerError> {
        let stream_request_bodies = body_mode == RequestBodyMode::Streaming;
        let protocol = match selection {
            ProtocolSelection::Detect => None,
            ProtocolSelection::Known(protocol) => Some(protocol),
            ProtocolSelection::Alpn(identifier) => Some(HttpProtocol::from_alpn(identifier)?),
        };
        let inbound_limits = if stream_request_bodies {
            streaming_body_limits(limits)
        } else {
            limits
        };
        let inner = match protocol {
            None => ServerInner::Detect,
            Some(HttpProtocol::Http1) => ServerInner::Http1 {
                decoder: Http1ConnectionDecoder::request(inbound_limits),
                scratch: Http1HeaderScratch::new(limits.max_headers()),
            },
            Some(HttpProtocol::Http2) => {
                ServerInner::Http2(Box::new(new_h2_server(limits, stream_request_bodies)))
            }
        };
        Ok(Self {
            inner,
            protocol,
            limits,
            stream_request_bodies,
            pending: None,
            output: Vec::new(),
            pending_body: None,
            complete_pending: None,
            exchanges: HashMap::new(),
            http1_exchange: None,
            next_http1_exchange_id: 0,
            http1_request_count: 0,
            response_scheduler: H2FairStreamScheduler::with_capacity(
                h2_limits(limits).max_active_streams,
            ),
            scheduled_body: None,
            buffer_finishes: None,
            reset_pending: None,
            response_resets: VecDeque::new(),
            handled_stream_error: None,
            terminal_h2_error: None,
            shutdown_started: false,
        })
    }

    /// Returns the detected wire protocol, or `None` while the preface remains ambiguous.
    pub const fn protocol(&self) -> Option<HttpProtocol> {
        self.protocol
    }

    /// Returns the input byte count associated with the current step.
    ///
    /// The caller removes this many bytes from its input buffer only after it
    /// has synchronously handled an event, successfully completed a write, or
    /// handled a recoverable peer reset.
    pub fn consumed(&self) -> usize {
        self.pending.map_or(0, PendingStep::input)
    }

    /// Returns bytes for the currently pending [`Step::Write`].
    pub fn pending_write(&self) -> Option<&[u8]> {
        match self.pending? {
            PendingStep::Write {
                kind: WriteKind::Buffer,
                ..
            } => Some(&self.output),
            PendingStep::Write {
                kind: WriteKind::H2Block { commit, .. },
                ..
            } => {
                let ServerInner::Http2(server) = &self.inner else {
                    return None;
                };
                server
                    .next_outbound_block()
                    .filter(|block| block.commit() == commit)
                    .map(|block| block.bytes())
            }
            PendingStep::Input(_) => None,
        }
    }

    /// Takes a stream failure already converted into `RST_STREAM`.
    ///
    /// Owners use this to discard request/response state outside the driver
    /// without treating the failure as connection-fatal.
    pub fn take_handled_stream_error(&mut self) -> Option<HandledStreamError> {
        self.handled_stream_error.take()
    }

    /// Stages the protocol-appropriate termination frame for a failed exchange.
    ///
    /// Returns `true` when bytes were staged and must be flushed with
    /// [`Self::pending_write`] before closing the transport.
    pub fn begin_shutdown(&mut self) -> Result<bool, ServerError> {
        if self.shutdown_started {
            return Ok(false);
        }
        let ServerInner::Http2(server) = &mut self.inner else {
            self.shutdown_started = true;
            return Ok(false);
        };
        let last_stream_id = server.highest_processed_stream_id();
        let output = match self.terminal_h2_error {
            Some(error) => match error.scope() {
                H2ErrorScope::Connection => {
                    server.goaway_frame_with_code(last_stream_id, error.error_code())?
                }
                H2ErrorScope::Stream(stream_id) => {
                    server.rst_stream_frame_with_code(stream_id, error.error_code())?
                }
            },
            None => server.goaway_frame_with_code(last_stream_id, H2ErrorCode::NoError)?,
        };
        self.pending = Some(PendingStep::Write {
            input: 0,
            kind: WriteKind::Buffer,
        });
        self.pending_body = None;
        self.output = output;
        self.complete_pending = None;
        self.scheduled_body = None;
        self.buffer_finishes = None;
        self.response_resets.clear();
        self.shutdown_started = true;
        Ok(true)
    }

    /// Discards an exchange the peer canceled with RST_STREAM.
    ///
    /// Resetting a stream ends that exchange only, so the connection stays
    /// usable for other streams. This leaves the driver where a finished
    /// exchange leaves it, ready for [`Self::begin_next_exchange`].
    ///
    /// Returns the canceled exchange identifier, or `None` when the connection
    /// cannot continue, which is the case for HTTP/1 and a connection that
    /// already failed.
    pub fn cancel_exchange(&mut self) -> Result<Option<ExchangeId>, ServerError> {
        if self.shutdown_started || self.terminal_h2_error.is_some() {
            return Ok(None);
        }
        if !matches!(self.inner, ServerInner::Http2(_)) {
            return Ok(None);
        }
        let exchange_id = self
            .reset_pending
            .take()
            .ok_or(ServerError::InvalidOutboundState)?;
        let state = self
            .exchanges
            .get_mut(&exchange_id)
            .ok_or(ServerError::InvalidOutboundState)?;
        let stream_id = state.stream_id.ok_or(ServerError::InvalidOutboundState)?;
        if let ServerInner::Http2(server) = &mut self.inner {
            server.finish_response_stream(stream_id);
        }
        self.response_scheduler.remove(stream_id);
        if self.scheduled_body == Some(exchange_id) {
            self.scheduled_body = None;
        }
        self.response_resets
            .retain(|pending| *pending != exchange_id);
        state.request_complete = true;
        state.finishing_header = None;
        state.stage = ServerStage::Done;
        Ok(Some(exchange_id))
    }

    /// Stops surfacing request-body events for an exchange the application no
    /// longer consumes.
    ///
    /// HTTP/1 responses for an incomplete request are already forced to close
    /// the connection. HTTP/2 keeps sibling streams usable while discarding
    /// the abandoned stream's remaining DATA under its own framing.
    pub fn abandon_request(&mut self, exchange_id: ExchangeId) -> Result<(), ServerError> {
        if self.pending.is_some() || self.pending_body.is_some() {
            return Err(ServerError::InvalidOutboundState);
        }
        let state = self
            .exchanges
            .get_mut(&exchange_id)
            .ok_or(ServerError::InvalidOutboundState)?;
        if state.request_complete {
            return Ok(());
        }
        state.continue_pending = false;
        state.request_events_suppressed = true;
        Ok(())
    }

    /// Resets an HTTP/2 exchange whose response source can no longer continue.
    ///
    /// The reset is emitted in connection wire order after already-queued
    /// response headers. HTTP/1 cannot isolate an exchange and returns `false`.
    pub fn abandon_response(&mut self, exchange_id: ExchangeId) -> Result<bool, ServerError> {
        if self.pending.is_some() || self.pending_body.is_some() {
            return Err(ServerError::InvalidOutboundState);
        }
        if !matches!(self.inner, ServerInner::Http2(_)) {
            return Ok(false);
        }
        let state = self
            .exchanges
            .get_mut(&exchange_id)
            .ok_or(ServerError::InvalidOutboundState)?;
        if state.stage != ServerStage::Responding {
            return Err(ServerError::InvalidOutboundState);
        }
        state.continue_pending = false;
        state.request_events_suppressed = true;
        if !self.response_resets.contains(&exchange_id) {
            self.response_resets.push_back(exchange_id);
        }
        if let Some(stream_id) = state.stream_id {
            self.response_scheduler.remove(stream_id);
        }
        if self.scheduled_body == Some(exchange_id) {
            self.scheduled_body = None;
        }
        Ok(true)
    }

    /// Returns whether an exchange has completed both inbound and outbound
    /// protocol work and may be passed to [`Self::begin_next_exchange`].
    pub fn exchange_is_finished(&self, exchange_id: ExchangeId) -> bool {
        self.exchanges
            .get(&exchange_id)
            .is_some_and(ServerExchangeState::is_finished)
    }

    /// Retires one completed exchange while preserving every sibling stream.
    ///
    /// Calling this after [`Step::Done`] lets the connection accept a later
    /// HTTP/1 request or later HTTP/2 streams. It may also retire a completed
    /// HTTP/2 stream while siblings remain active.
    ///
    /// Returns `false` when the connection cannot carry another exchange, in
    /// which case the caller must shut it down.
    pub fn begin_next_exchange(&mut self, exchange_id: ExchangeId) -> Result<bool, ServerError> {
        if self.shutdown_started || self.terminal_h2_error.is_some() {
            return Ok(false);
        }
        if matches!(self.inner, ServerInner::Detect) {
            return Ok(false);
        }
        if self.pending.is_some() || self.pending_body.is_some() {
            return Err(ServerError::InvalidOutboundState);
        }
        let (stream_id, response_closes_connection) = {
            let state = self
                .exchanges
                .get(&exchange_id)
                .ok_or(ServerError::InvalidOutboundState)?;
            if !state.is_finished() || state.finishing_header.is_some() {
                return Err(ServerError::InvalidOutboundState);
            }
            (state.stream_id, state.response_closes_connection)
        };

        if let ServerInner::Http1 { decoder, .. } = &mut self.inner {
            if self.http1_exchange != Some(exchange_id) || stream_id.is_some() {
                return Err(ServerError::InvalidOutboundState);
            }
            let persists = !response_closes_connection;
            if persists {
                decoder.begin_next_message()?;
            }
            self.exchanges.remove(&exchange_id);
            self.http1_exchange = None;
            if self.complete_pending == Some(exchange_id) {
                self.complete_pending = None;
            }
            if self.buffer_finishes == Some(exchange_id) {
                self.buffer_finishes = None;
            }
            Ok(persists)
        } else {
            self.exchanges.remove(&exchange_id);
            if let Some(stream_id) = stream_id {
                self.response_scheduler.remove(stream_id);
            }
            if self.scheduled_body == Some(exchange_id) {
                self.scheduled_body = None;
            }
            if self.complete_pending == Some(exchange_id) {
                self.complete_pending = None;
            }
            if self.reset_pending == Some(exchange_id) {
                self.reset_pending = None;
            }
            self.response_resets
                .retain(|pending| *pending != exchange_id);
            Ok(true)
        }
    }

    /// Acknowledges the current step and its associated input consumption.
    pub fn consume(&mut self, amount: usize) -> Result<(), ServerError> {
        let pending = self
            .pending
            .take()
            .ok_or(ServerError::InvalidOutboundState)?;
        if pending.input() != amount {
            self.pending = Some(pending);
            return Err(ServerError::InvalidOutboundState);
        }
        match pending {
            PendingStep::Input(_) => {}
            PendingStep::Write {
                kind: WriteKind::Buffer,
                ..
            } => {
                self.output.clear();
                if let Some(exchange_id) = self.buffer_finishes.take() {
                    let state = self
                        .exchanges
                        .get_mut(&exchange_id)
                        .ok_or(ServerError::InvalidOutboundState)?;
                    state.stage = ServerStage::Done;
                }
            }
            PendingStep::Write {
                kind:
                    WriteKind::H2Block {
                        commit,
                        finishing_exchange,
                    },
                ..
            } => {
                let ServerInner::Http2(server) = &mut self.inner else {
                    return Err(ServerError::InvalidOutboundState);
                };
                server.acknowledge_outbound_block(commit)?;
                if let Some(exchange_id) = finishing_exchange {
                    let state = self
                        .exchanges
                        .get_mut(&exchange_id)
                        .ok_or(ServerError::InvalidOutboundState)?;
                    if state.finishing_header != Some(commit) {
                        return Err(ServerError::InvalidOutboundState);
                    }
                    let stream_id = state.stream_id.ok_or(ServerError::InvalidOutboundState)?;
                    server.finish_response_stream(stream_id);
                    state.finishing_header = None;
                    state.stage = ServerStage::Done;
                }
            }
        }
        Ok(())
    }

    /// Advances the connection and callback-scopes all borrowed protocol data.
    pub fn step<R>(
        &mut self,
        input: &[u8],
        on_step: impl for<'step> FnOnce(Step<'step, ServerEvent<'step>>) -> R,
    ) -> Result<R, ServerError> {
        if self.pending.is_some() || self.pending_body.is_some() {
            return Err(ServerError::InvalidOutboundState);
        }
        if !self.exchanges.is_empty()
            && self
                .exchanges
                .values()
                .all(ServerExchangeState::is_finished)
        {
            return Ok(on_step(Step::Done));
        }
        // An expectation the caller never answered is answered here, so that
        // ignoring `RequestHead::expectation` degrades to the pre-`Expect`
        // behaviour instead of hanging the peer.
        self.prepare_default_expectation_response()?;
        if !self.output.is_empty() {
            self.pending = Some(PendingStep::Write {
                input: 0,
                kind: WriteKind::Buffer,
            });
            return Ok(on_step(Step::Write(&self.output)));
        }
        if let ServerInner::Http2(server) = &self.inner
            && let Some(block) = server.next_outbound_block()
        {
            let commit = block.commit();
            let finishing_exchange = self.exchanges.iter().find_map(|(exchange_id, state)| {
                (state.finishing_header == Some(commit)).then_some(*exchange_id)
            });
            self.pending = Some(PendingStep::Write {
                input: 0,
                kind: WriteKind::H2Block {
                    commit,
                    finishing_exchange,
                },
            });
            return Ok(on_step(Step::Write(block.bytes())));
        }
        if let Some(exchange_id) = self.response_resets.pop_front() {
            let state = self
                .exchanges
                .get_mut(&exchange_id)
                .ok_or(ServerError::InvalidOutboundState)?;
            let stream_id = state.stream_id.ok_or(ServerError::InvalidOutboundState)?;
            let ServerInner::Http2(server) = &mut self.inner else {
                return Err(ServerError::InvalidOutboundState);
            };
            self.output =
                server.rst_stream_frame_with_code(stream_id, H2ErrorCode::InternalError)?;
            server.close_stream(stream_id);
            state.request_complete = true;
            state.finishing_header = None;
            self.buffer_finishes = Some(exchange_id);
            self.pending = Some(PendingStep::Write {
                input: 0,
                kind: WriteKind::Buffer,
            });
            return Ok(on_step(Step::Write(&self.output)));
        }
        if let Some(exchange_id) = self.complete_pending.take() {
            let state = self
                .exchanges
                .get_mut(&exchange_id)
                .ok_or(ServerError::InvalidOutboundState)?;
            state.request_complete = true;
            self.pending = Some(PendingStep::Input(0));
            return if state.request_events_suppressed {
                Ok(on_step(Step::NeedInput))
            } else {
                Ok(on_step(Step::Event(ServerEvent::RequestComplete {
                    exchange_id,
                })))
            };
        }
        if let Some(exchange_id) = self.http1_exchange
            && self
                .exchanges
                .get(&exchange_id)
                .is_some_and(|state| state.request_events_suppressed)
        {
            self.pending = Some(PendingStep::Input(0));
            return Ok(on_step(Step::NeedInput));
        }

        if matches!(self.inner, ServerInner::Detect) {
            if input.len() < CLIENT_PREFACE.len() && CLIENT_PREFACE.starts_with(input) {
                self.pending = Some(PendingStep::Input(0));
                return Ok(on_step(Step::NeedInput));
            }
            if input.starts_with(CLIENT_PREFACE) {
                self.protocol = Some(HttpProtocol::Http2);
                self.inner = ServerInner::Http2(Box::new(new_h2_server(
                    self.limits,
                    self.stream_request_bodies,
                )));
            } else {
                self.protocol = Some(HttpProtocol::Http1);
                let limits = if self.stream_request_bodies {
                    streaming_body_limits(self.limits)
                } else {
                    self.limits
                };
                self.inner = ServerInner::Http1 {
                    decoder: Http1ConnectionDecoder::request(limits),
                    scratch: Http1HeaderScratch::new(self.limits.max_headers()),
                };
            }
        }

        match &mut self.inner {
            ServerInner::Detect => unreachable!("protocol detection selected a decoder"),
            ServerInner::Http1 { decoder, scratch } => {
                let pending = &mut self.pending;
                let exchanges = &mut self.exchanges;
                let http1_exchange = &mut self.http1_exchange;
                let next_http1_exchange_id = &mut self.next_http1_exchange_id;
                let http1_request_count = &mut self.http1_request_count;
                scratch.with_input(input, |input, headers| {
                    let event = decoder.next_event(input, headers)?;
                    match event {
                        Http1ConnectionEvent::NeedInput => {
                            *pending = Some(PendingStep::Input(0));
                            Ok(on_step(Step::NeedInput))
                        }
                        Http1ConnectionEvent::Head {
                            head,
                            body,
                            consumed,
                            ..
                        } => {
                            if http1_exchange.is_some() {
                                return Err(ServerError::MalformedMessage);
                            }
                            let Http1MessageHead::Request(head) = head else {
                                return Err(ServerError::MalformedMessage);
                            };
                            let wire_version = http1_version(head.version)?;
                            let version = match wire_version {
                                Http1Version::Http10 => HttpVersion::Http10,
                                Http1Version::Http11 => HttpVersion::Http11,
                            };
                            let body_expected = !matches!(
                                body,
                                Http1BodyKind::Empty | Http1BodyKind::ContentLength(0)
                            );
                            let headers = HeaderBlock::http1(head.headers);
                            let expectation =
                                expectation_for_version(request_expectation(headers), version);
                            *next_http1_exchange_id = next_http1_exchange_id
                                .checked_add(1)
                                .ok_or(ServerError::InvalidOutboundState)?;
                            let exchange_id = ExchangeId(*next_http1_exchange_id);
                            *http1_request_count = http1_request_count
                                .checked_add(1)
                                .ok_or(ServerError::InvalidOutboundState)?;
                            *http1_exchange = Some(exchange_id);
                            exchanges.insert(
                                exchange_id,
                                ServerExchangeState::new(
                                    None,
                                    head.method.as_bytes(),
                                    version,
                                    http1_request_is_persistent(wire_version, head.headers),
                                    expectation,
                                    body_expected,
                                ),
                            );
                            *pending = Some(PendingStep::Input(consumed));
                            let content_length = match body {
                                Http1BodyKind::ContentLength(length) => Some(length),
                                Http1BodyKind::Empty
                                | Http1BodyKind::Chunked
                                | Http1BodyKind::Eof => None,
                            };
                            Ok(on_step(Step::Event(ServerEvent::RequestHead {
                                exchange_id,
                                method: head.method.as_bytes(),
                                target: head.target.as_bytes(),
                                authority: None,
                                version,
                                headers,
                                content_length,
                                expectation,
                            })))
                        }
                        Http1ConnectionEvent::Body { chunk, consumed } => {
                            let exchange_id =
                                http1_exchange.ok_or(ServerError::MalformedMessage)?;
                            *pending = Some(PendingStep::Input(consumed));
                            let state = exchanges
                                .get(&exchange_id)
                                .ok_or(ServerError::InvalidOutboundState)?;
                            // Defence in depth for HTTP/1. Rejecting an
                            // expectation also forces the connection closed, so
                            // `is_finished` reports the exchange complete and
                            // `step` returns `Done` before body bytes reach
                            // here - which is why no test can distinguish this
                            // branch today. It exists so a future keep-alive
                            // rejection cannot silently start surfacing a body
                            // the caller already refused. The HTTP/2 twin below
                            // is reachable, because a rejected stream leaves
                            // its connection alive.
                            if state.request_events_suppressed {
                                Ok(on_step(Step::NeedInput))
                            } else {
                                Ok(on_step(Step::Event(ServerEvent::RequestBody {
                                    exchange_id,
                                    chunk,
                                })))
                            }
                        }
                        Http1ConnectionEvent::Trailers { fields, consumed } => {
                            *pending = Some(PendingStep::Input(consumed));
                            let exchange_id =
                                http1_exchange.ok_or(ServerError::MalformedMessage)?;
                            let state = exchanges
                                .get(&exchange_id)
                                .ok_or(ServerError::InvalidOutboundState)?;
                            if fields.is_empty() || state.request_events_suppressed {
                                Ok(on_step(Step::NeedInput))
                            } else {
                                Ok(on_step(Step::Event(ServerEvent::RequestTrailers {
                                    exchange_id,
                                    headers: HeaderBlock::http1(fields),
                                })))
                            }
                        }
                        Http1ConnectionEvent::ProtocolSwitch { .. } => {
                            Err(ServerError::UnsupportedTransferEncoding)
                        }
                        Http1ConnectionEvent::Complete => {
                            let exchange_id =
                                http1_exchange.ok_or(ServerError::MalformedMessage)?;
                            let state = exchanges
                                .get_mut(&exchange_id)
                                .ok_or(ServerError::InvalidOutboundState)?;
                            state.request_complete = true;
                            *pending = Some(PendingStep::Input(0));
                            if state.request_events_suppressed {
                                Ok(on_step(Step::NeedInput))
                            } else {
                                Ok(on_step(Step::Event(ServerEvent::RequestComplete {
                                    exchange_id,
                                })))
                            }
                        }
                    }
                })
            }
            ServerInner::Http2(server) => {
                let (event, consumed, output) = match server.accept_event_bytes_ref(input) {
                    Ok(progress) => progress,
                    Err(error) => {
                        if let Some(protocol_error) = server.take_reported_protocol_error() {
                            self.terminal_h2_error.get_or_insert(protocol_error);
                        }
                        return Err(error);
                    }
                };
                let handled_protocol_error = server.take_handled_stream_error();
                let handled_compat_error = server.take_handled_compat_error();
                let handled_stream_id =
                    handled_protocol_error.and_then(|error| match error.scope() {
                        H2ErrorScope::Stream(stream_id) => Some(stream_id),
                        H2ErrorScope::Connection => None,
                    });
                self.output = output;
                if let Some(stream_id) = handled_stream_id {
                    let exchange_id = ExchangeId::http2(stream_id);
                    if let (Some(protocol_error), Some(error)) =
                        (handled_protocol_error, handled_compat_error)
                    {
                        self.handled_stream_error = Some(HandledStreamError {
                            exchange_id,
                            error,
                            protocol_error,
                        });
                    }
                    self.exchanges.remove(&exchange_id);
                    self.response_scheduler.remove(stream_id);
                    if self.scheduled_body == Some(exchange_id) {
                        self.scheduled_body = None;
                    }
                    if self.complete_pending == Some(exchange_id) {
                        self.complete_pending = None;
                    }
                    if self.buffer_finishes == Some(exchange_id) {
                        self.buffer_finishes = None;
                    }
                    if self.reset_pending == Some(exchange_id) {
                        self.reset_pending = None;
                    }
                    self.response_resets
                        .retain(|pending| *pending != exchange_id);
                }
                match event {
                    Some(H2ByteStreamEventRef::RequestHeaders {
                        stream_id,
                        headers,
                        end_stream,
                    }) => {
                        let limits = if self.stream_request_bodies {
                            streaming_body_limits(self.limits)
                        } else {
                            self.limits
                        };
                        let head = project_h2_request_head(&headers, limits)?;
                        let exchange_id = ExchangeId::http2(stream_id);
                        if self.exchanges.contains_key(&exchange_id) {
                            return Err(ServerError::InvalidFrame);
                        }
                        let headers = HeaderBlock::http2(head.fields());
                        let expectation = request_expectation(headers);
                        self.exchanges.insert(
                            exchange_id,
                            ServerExchangeState::new(
                                Some(stream_id),
                                head.method(),
                                HttpVersion::Http2,
                                true,
                                expectation,
                                !end_stream,
                            ),
                        );
                        self.complete_pending = end_stream.then_some(exchange_id);
                        self.pending = Some(PendingStep::Input(consumed));
                        Ok(on_step(Step::Event(ServerEvent::RequestHead {
                            exchange_id,
                            method: head.method(),
                            target: head.path(),
                            authority: head.effective_host(),
                            version: HttpVersion::Http2,
                            headers,
                            content_length: head.content_length(),
                            expectation,
                        })))
                    }
                    Some(H2ByteStreamEventRef::Data {
                        stream_id,
                        payload,
                        end_stream,
                        ..
                    }) => {
                        let exchange_id = ExchangeId::http2(stream_id);
                        let state = self
                            .exchanges
                            .get(&exchange_id)
                            .ok_or(ServerError::InvalidFrame)?;
                        self.complete_pending = end_stream.then_some(exchange_id);
                        self.pending = Some(PendingStep::Input(consumed));
                        if state.request_events_suppressed {
                            Ok(on_step(Step::NeedInput))
                        } else {
                            Ok(on_step(Step::Event(ServerEvent::RequestBody {
                                exchange_id,
                                chunk: payload,
                            })))
                        }
                    }
                    Some(H2ByteStreamEventRef::Trailers { stream_id, headers }) => {
                        let exchange_id = ExchangeId::http2(stream_id);
                        let state = self
                            .exchanges
                            .get(&exchange_id)
                            .ok_or(ServerError::InvalidFrame)?;
                        self.complete_pending = Some(exchange_id);
                        self.pending = Some(PendingStep::Input(consumed));
                        if headers.is_empty() || state.request_events_suppressed {
                            Ok(on_step(Step::NeedInput))
                        } else {
                            Ok(on_step(Step::Event(ServerEvent::RequestTrailers {
                                exchange_id,
                                headers: HeaderBlock::http2(&headers),
                            })))
                        }
                    }
                    Some(H2ByteStreamEventRef::Reset {
                        stream_id,
                        error_code,
                    }) if self.exchanges.contains_key(&ExchangeId::http2(stream_id)) => {
                        self.reset_pending = Some(ExchangeId::http2(stream_id));
                        self.pending = Some(PendingStep::Input(consumed));
                        Err(ServerError::PeerReset {
                            stream_id,
                            error_code,
                        })
                    }
                    Some(H2ByteStreamEventRef::Goaway {
                        last_stream_id,
                        error_code,
                    }) => Err(ServerError::PeerGoaway {
                        last_stream_id,
                        error_code,
                    }),
                    Some(
                        H2ByteStreamEventRef::Settings { .. }
                        | H2ByteStreamEventRef::Ping { .. }
                        | H2ByteStreamEventRef::WindowUpdate { .. }
                        | H2ByteStreamEventRef::DiscardedData { .. }
                        | H2ByteStreamEventRef::Reset { .. },
                    )
                    | None => self.emit_h2_progress(consumed, on_step),
                }
            }
        }
    }

    /// Answers any expectation the caller left unanswered.
    ///
    /// RFC 9110 section 10.1.1 requires a server that receives an expectation to
    /// respond with either `100 Continue` or a final status. Answering neither
    /// hangs the peer, which is worse than not supporting the mechanism at all,
    /// so a caller that ignores `ServerEvent::RequestHead`'s expectation must
    /// still end up with a reply on the wire. Running this from [`Self::step`]
    /// is what makes ignoring the field degrade to the pre-`Expect` behaviour
    /// rather than to a stall.
    ///
    /// Removing this call, or making it conditional on the caller having opted
    /// in, reintroduces that stall.
    /// `server_defaults_to_continue_when_caller_ignores_expectation` in
    /// `tests/expect_continue.rs` pins it.
    fn prepare_default_expectation_response(&mut self) -> Result<(), ServerError> {
        if let Some(exchange_id) = self.exchanges.iter().find_map(|(exchange_id, state)| {
            (state.stage == ServerStage::Reading && state.automatic_rejection_pending)
                .then_some(*exchange_id)
        }) {
            self.prepare_response(
                exchange_id,
                ConnectionResponse {
                    status: 417,
                    reason: "Expectation Failed",
                    headers: &[],
                    body_len: Some(0),
                },
            )?;
            return Ok(());
        }

        if let Some(exchange_id) = self.exchanges.iter().find_map(|(exchange_id, state)| {
            (state.stage == ServerStage::Reading && state.continue_pending).then_some(*exchange_id)
        }) {
            self.prepare_continue(exchange_id)?;
        }
        Ok(())
    }

    /// Stages `100 Continue` when the request's wire version uses the handshake.
    ///
    /// The return value is `false` for HTTP/1.0 and HTTP/2, where the request
    /// body is not gated and no informational response is emitted.
    pub fn prepare_continue(&mut self, exchange_id: ExchangeId) -> Result<bool, ServerError> {
        if self.pending.is_some() || self.pending_body.is_some() {
            return Err(ServerError::InvalidOutboundState);
        }
        let state = self
            .exchanges
            .get(&exchange_id)
            .ok_or(ServerError::InvalidOutboundState)?;
        if state.stage != ServerStage::Reading {
            return Err(ServerError::InvalidOutboundState);
        }
        // Answering an exchange that never carried the expectation is a
        // no-op rather than an error, so a caller may call this unconditionally.
        // HTTP/1.0 reports no expectation at all (RFC 9110 section 10.1.1), and
        // HTTP/2 is never gated.
        if state.expectation != RequestExpectation::Continue {
            return Ok(false);
        }
        if state.request_version != HttpVersion::Http11 || !state.continue_pending {
            return Ok(false);
        }
        if !matches!(self.inner, ServerInner::Http1 { .. })
            || self.http1_exchange != Some(exchange_id)
            || !self.output.is_empty()
        {
            return Err(ServerError::InvalidOutboundState);
        }
        let output = Http1Server::response_head_bytes_with_raw_headers_and_limits(
            1,
            100,
            "Continue",
            &[],
            None,
            0,
            self.limits,
        )?;
        let state = self
            .exchanges
            .get_mut(&exchange_id)
            .ok_or(ServerError::InvalidOutboundState)?;
        state.continue_pending = false;
        self.output = output;
        Ok(true)
    }

    fn emit_h2_progress<R>(
        &mut self,
        consumed: usize,
        on_step: impl for<'step> FnOnce(Step<'step, ServerEvent<'step>>) -> R,
    ) -> Result<R, ServerError> {
        if self.output.is_empty() {
            self.pending = Some(PendingStep::Input(consumed));
            Ok(on_step(Step::NeedInput))
        } else {
            self.pending = Some(PendingStep::Write {
                input: consumed,
                kind: WriteKind::Buffer,
            });
            Ok(on_step(Step::Write(&self.output)))
        }
    }

    /// Prepares response headers and records the optional caller-owned body length.
    ///
    /// A final response prepared while `100-continue` remains unanswered
    /// suppresses that request's subsequent body events.
    ///
    /// The return value reports whether any body bytes must be written.
    pub fn prepare_response(
        &mut self,
        exchange_id: ExchangeId,
        response: ConnectionResponse<'_>,
    ) -> Result<bool, ServerError> {
        if self.pending.is_some() || self.pending_body.is_some() {
            return Err(ServerError::InvalidOutboundState);
        }
        let state = self
            .exchanges
            .get(&exchange_id)
            .ok_or(ServerError::InvalidOutboundState)?;
        if state.stage != ServerStage::Reading {
            return Err(ServerError::InvalidOutboundState);
        }
        if state.expectation == RequestExpectation::Unsupported && response.status != 417 {
            return Err(ServerError::InvalidOutboundState);
        }
        let method = state.request_method.as_slice();
        let request_version = state.request_version;
        let request_complete = state.request_complete;
        let request_persistent = state.request_persistent;
        let stream_id = state.stream_id;
        let suppress_request_events = response.status >= 200
            && !request_complete
            && state.expectation != RequestExpectation::None;
        let shutdown_started = self.shutdown_started;
        let below_http1_request_limit =
            self.http1_request_count < self.limits.max_requests_per_connection();
        let body_allowed = !matches!(response.status, 100..=199 | 204 | 304) && method != b"HEAD";
        let send_body = body_allowed && response.body_len != Some(0);
        let (
            finishing_header,
            response_closes_connection,
            outbound_header_count,
            outbound_header_bytes,
        ) = match &mut self.inner {
            ServerInner::Http1 { .. } => {
                if self.http1_exchange != Some(exchange_id) || stream_id.is_some() {
                    return Err(ServerError::InvalidOutboundState);
                }
                let version = match request_version {
                    HttpVersion::Http10 => Http1Version::Http10,
                    HttpVersion::Http11 => Http1Version::Http11,
                    HttpVersion::Http2 => return Err(ServerError::InvalidOutboundState),
                };
                let headers = parse_headers(response.headers)?;
                let plan = Http1ResponsePlan::new(
                    Http1ResponseContext {
                        method,
                        version,
                        keep_alive: request_complete
                            && request_persistent
                            && below_http1_request_limit
                            && !shutdown_started,
                    },
                    Http1ResponseParts {
                        status: response.status,
                        reason: response.reason,
                        headers: &headers,
                        body_len: response.body_len,
                    },
                    self.limits,
                )?;
                let closes_connection = plan.closes_connection();
                self.output = plan.head().to_vec();
                self.buffer_finishes = (!send_body).then_some(exchange_id);
                (
                    None,
                    closes_connection,
                    http1_header_count(plan.head()),
                    plan.head().len(),
                )
            }
            ServerInner::Http2(server) => {
                let stream_id = stream_id.ok_or(ServerError::InvalidOutboundState)?;
                if response.body_len.is_none() {
                    server.stream_response_body(stream_id)?;
                }
                let mut headers = h2_headers(response.headers, true);
                if !matches!(response.status, 100..=199 | 204)
                    && let Some(body_len) = response.body_len
                {
                    headers.push(H2HeaderField::new(
                        b"content-length",
                        body_len.to_string().as_bytes(),
                    ));
                }
                let commit = match server.response_headers_frame_with_raw_headers_and_body_length(
                    stream_id,
                    response.status,
                    &headers,
                    response.body_len.unwrap_or(0),
                    !send_body,
                ) {
                    Ok(commit) => commit,
                    Err(error) => {
                        self.terminal_h2_error.get_or_insert(error);
                        return Err(error.into());
                    }
                };
                let status = response.status.to_string();
                let header_count = headers
                    .iter()
                    .filter(|header| !header.name.starts_with(b":"))
                    .count();
                let header_bytes = headers.iter().fold(
                    hpack_field_size(b":status", status.as_bytes()),
                    |total, header| {
                        total.saturating_add(hpack_field_size(&header.name, &header.value))
                    },
                );
                if send_body {
                    self.response_scheduler.register(stream_id);
                    (None, false, header_count, header_bytes)
                } else {
                    (Some(commit), false, header_count, header_bytes)
                }
            }
            ServerInner::Detect => return Err(ServerError::InvalidOutboundState),
        };
        let state = self
            .exchanges
            .get_mut(&exchange_id)
            .ok_or(ServerError::InvalidOutboundState)?;
        state.stage = ServerStage::Responding;
        state.outbound_body_len = if send_body {
            response.body_len
        } else {
            Some(0)
        };
        state.outbound_body_sent = 0;
        state.outbound_header_count = outbound_header_count;
        state.outbound_header_bytes = outbound_header_bytes;
        state.outbound_trailers = None;
        state.finishing_header = finishing_header;
        state.response_closes_connection = response_closes_connection;
        if response.status == 417 && state.expectation == RequestExpectation::Unsupported {
            state.automatic_rejection_pending = false;
        }
        if suppress_request_events {
            state.continue_pending = false;
            state.request_events_suppressed = true;
        }
        Ok(send_body)
    }

    /// Stores response trailers to emit when this response body ends.
    ///
    /// Call this after [`Self::prepare_response`] and before preparing the final
    /// body chunk. Earlier body chunks may already have been committed, and a
    /// later call replaces the previously stored trailer block. Responses that
    /// already ended, including bodyless responses, reject the call.
    ///
    /// RFC 9112 section 7.1.2 permits HTTP/1 trailers only on an HTTP/1.1
    /// chunked response. HTTP/2 emits the fields as a terminal HEADERS block as
    /// required by RFC 9113 section 8.1.
    pub fn set_response_trailers(
        &mut self,
        exchange_id: ExchangeId,
        headers: &[HeaderRef<'_>],
    ) -> Result<(), ServerError> {
        if self.pending.is_some() || self.pending_body.is_some() {
            return Err(ServerError::InvalidOutboundState);
        }
        let state = self
            .exchanges
            .get(&exchange_id)
            .ok_or(ServerError::InvalidOutboundState)?;
        if state.stage != ServerStage::Responding
            || state.finishing_header.is_some()
            || state.outbound_body_len == Some(0)
        {
            return Err(ServerError::InvalidOutboundState);
        }

        let trailers = match &self.inner {
            ServerInner::Http1 { .. } => {
                if self.http1_exchange != Some(exchange_id)
                    || state.stream_id.is_some()
                    || state.request_version != HttpVersion::Http11
                    || state.outbound_body_len.is_some()
                {
                    return Err(ServerError::InvalidOutboundState);
                }
                let bytes = http1_trailer_bytes(headers)?;
                enforce_outbound_trailer_limits(
                    state.outbound_header_count,
                    state.outbound_header_bytes,
                    headers.len(),
                    bytes.len(),
                    self.limits,
                )?;
                ResponseTrailers::Http1(bytes)
            }
            ServerInner::Http2(_) => {
                state.stream_id.ok_or(ServerError::InvalidOutboundState)?;
                let fields = h2_trailer_fields(headers)?;
                let trailer_bytes = fields.iter().fold(0usize, |total, header| {
                    total.saturating_add(hpack_field_size(&header.name, &header.value))
                });
                enforce_outbound_trailer_limits(
                    state.outbound_header_count,
                    state.outbound_header_bytes,
                    fields.len(),
                    trailer_bytes,
                    self.limits,
                )?;
                ResponseTrailers::Http2(fields)
            }
            ServerInner::Detect => return Err(ServerError::InvalidOutboundState),
        };
        let state = self
            .exchanges
            .get_mut(&exchange_id)
            .ok_or(ServerError::InvalidOutboundState)?;
        state.outbound_trailers = Some(trailers);
        Ok(())
    }

    /// Plans the next zero-copy response-body write.
    ///
    /// For a known-length body, `remaining` must be the complete unsent suffix
    /// length. For a streaming body, it is the currently available byte count,
    /// and zero stages the protocol end marker. A `false` return means HTTP/2
    /// send windows are blocked, protocol headers must be written first, or
    /// another exchange has the next fair send opportunity.
    pub fn prepare_body_chunk(
        &mut self,
        exchange_id: ExchangeId,
        remaining: usize,
    ) -> Result<bool, ServerError> {
        if self.pending.is_some() || self.pending_body.is_some() {
            return Err(ServerError::InvalidOutboundState);
        }
        let state = self
            .exchanges
            .get(&exchange_id)
            .ok_or(ServerError::InvalidOutboundState)?;
        if state.stage != ServerStage::Responding {
            return Err(ServerError::InvalidOutboundState);
        }
        if let Some(body_len) = state.outbound_body_len {
            let expected_remaining = body_len.saturating_sub(state.outbound_body_sent);
            if remaining != expected_remaining {
                return Err(ServerError::InvalidOutboundState);
            }
            if remaining == 0 {
                return Ok(false);
            }
        }
        if state.outbound_body_len.is_some() {
            let projected_body_len = state.outbound_body_sent.saturating_add(remaining);
            if projected_body_len > self.limits.max_body_bytes() {
                return Err(ServerError::BodyTooLarge {
                    limit: self.limits.max_body_bytes(),
                    actual: projected_body_len,
                });
            }
        }
        let stream_id = state.stream_id;
        let body_len = state.outbound_body_len;
        match &self.inner {
            ServerInner::Http1 { .. } => {
                if self.http1_exchange != Some(exchange_id) || stream_id.is_some() {
                    return Err(ServerError::InvalidOutboundState);
                }
                let streaming_end = body_len.is_none() && remaining == 0;
                let chunked = body_len.is_none() && state.request_version == HttpVersion::Http11;
                let footer = if chunked && !streaming_end {
                    b"\r\n".as_slice()
                } else {
                    b"".as_slice()
                };
                if chunked {
                    if streaming_end {
                        match state.outbound_trailers.as_ref() {
                            Some(ResponseTrailers::Http1(bytes)) => {
                                self.output.extend_from_slice(bytes);
                            }
                            Some(ResponseTrailers::Http2(_)) => {
                                return Err(ServerError::InvalidOutboundState);
                            }
                            None => self.output.extend_from_slice(b"0\r\n\r\n"),
                        }
                    } else {
                        Http1Server::push_chunked_body_prefix(&mut self.output, remaining);
                    }
                }
                self.pending_body = Some(ServerPendingBody::Http1 {
                    exchange_id,
                    payload_len: remaining,
                    footer,
                    finishes_body: body_len.is_some() || streaming_end,
                });
                Ok(true)
            }
            ServerInner::Http2(server) => {
                if !self.output.is_empty() || server.next_outbound_block().is_some() {
                    return Ok(false);
                }
                let stream_id = stream_id.ok_or(ServerError::InvalidOutboundState)?;
                if self.scheduled_body.is_none() {
                    let exchanges = &self.exchanges;
                    let selected = self.response_scheduler.next_ready(|candidate| {
                        let candidate_id = ExchangeId::http2(candidate);
                        let Some(state) = exchanges.get(&candidate_id) else {
                            return false;
                        };
                        let pending_bytes = match state.outbound_body_len {
                            Some(body_len) => body_len.saturating_sub(state.outbound_body_sent),
                            None if candidate == stream_id => remaining,
                            None => 0,
                        };
                        let streaming_end = state.outbound_body_len.is_none()
                            && candidate == stream_id
                            && remaining == 0;
                        state.stage == ServerStage::Responding
                            && (pending_bytes != 0 || streaming_end)
                            && server
                                .send_capacity(candidate, pending_bytes)
                                .is_ok_and(|capacity| capacity.sendable_bytes != 0 || streaming_end)
                    });
                    self.scheduled_body = selected.map(ExchangeId::http2);
                }
                if self.scheduled_body != Some(exchange_id) {
                    return Ok(false);
                }
                let finishes_body = body_len.is_some() || remaining == 0;
                let has_trailers = match state.outbound_trailers.as_ref() {
                    Some(ResponseTrailers::Http2(_)) => true,
                    Some(ResponseTrailers::Http1(_)) => {
                        return Err(ServerError::InvalidOutboundState);
                    }
                    None => false,
                };
                // RFC 9113 section 8.1 makes the trailing HEADERS block the
                // stream terminator. Keeping END_STREAM off the final DATA
                // frame prevents the peer from treating the trailers as a
                // second, invalid message on an already closed stream.
                let plan = if finishes_body && has_trailers {
                    server.prepare_data_frame_before_trailers(stream_id, remaining)?
                } else {
                    server.prepare_data_frame(stream_id, remaining, finishes_body)?
                };
                let Some(plan) = plan else {
                    self.scheduled_body = None;
                    return Ok(false);
                };
                let finishes_body = finishes_body && plan.payload_len() == remaining;
                self.pending_body = Some(ServerPendingBody::Http2 {
                    exchange_id,
                    plan,
                    finishes_body,
                });
                Ok(true)
            }
            ServerInner::Detect => Err(ServerError::InvalidOutboundState),
        }
    }

    /// Borrows framing bytes for the body chunk prepared by [`Self::prepare_body_chunk`].
    pub fn body_chunk(&self, exchange_id: ExchangeId) -> Option<BodyChunk<'_>> {
        match self.pending_body.as_ref()? {
            ServerPendingBody::Http1 {
                exchange_id: pending_exchange,
                payload_len,
                footer,
                ..
            } if *pending_exchange == exchange_id => Some(BodyChunk {
                header: &self.output,
                payload_len: *payload_len,
                footer,
            }),
            ServerPendingBody::Http2 {
                exchange_id: pending_exchange,
                plan,
                ..
            } if *pending_exchange == exchange_id => Some(BodyChunk {
                header: plan.header(),
                payload_len: plan.payload_len(),
                footer: &[],
            }),
            ServerPendingBody::Http1 { .. } | ServerPendingBody::Http2 { .. } => None,
        }
    }

    /// Commits a successfully written response-body chunk.
    pub fn commit_body_chunk(&mut self, exchange_id: ExchangeId) -> Result<(), ServerError> {
        let pending = self
            .pending_body
            .take()
            .ok_or(ServerError::InvalidOutboundState)?;
        if pending.exchange_id() != exchange_id {
            self.pending_body = Some(pending);
            return Err(ServerError::InvalidOutboundState);
        }
        let (payload_len, finishes_body) = match pending {
            ServerPendingBody::Http1 {
                payload_len,
                finishes_body,
                ..
            } => {
                self.output.clear();
                (payload_len, finishes_body)
            }
            ServerPendingBody::Http2 {
                plan,
                finishes_body,
                ..
            } => {
                let payload_len = plan.payload_len();
                let ServerInner::Http2(server) = &mut self.inner else {
                    self.pending_body = Some(pending);
                    return Err(ServerError::InvalidOutboundState);
                };
                if let Err(error) = server.commit_data_frame(plan) {
                    self.pending_body = Some(pending);
                    return Err(error);
                }
                self.scheduled_body = None;
                (payload_len, finishes_body)
            }
        };
        let state = self
            .exchanges
            .get_mut(&exchange_id)
            .ok_or(ServerError::InvalidOutboundState)?;
        state.outbound_body_sent = state.outbound_body_sent.saturating_add(payload_len);
        if !finishes_body {
            return Ok(());
        }
        if let Some(body_len) = state.outbound_body_len
            && state.outbound_body_sent != body_len
        {
            return Err(ServerError::InvalidOutboundState);
        }
        let stream_id = state.stream_id;
        let trailers = state.outbound_trailers.take();
        if let Some(stream_id) = stream_id {
            self.response_scheduler.remove(stream_id);
        }
        match (stream_id, trailers) {
            (Some(stream_id), Some(ResponseTrailers::Http2(fields))) => {
                let ServerInner::Http2(server) = &mut self.inner else {
                    return Err(ServerError::InvalidOutboundState);
                };
                let commit = match server.trailers_frame_with_raw_headers(stream_id, &fields) {
                    Ok(commit) => commit,
                    Err(error) => {
                        self.terminal_h2_error.get_or_insert(error);
                        return Err(error.into());
                    }
                };
                let state = self
                    .exchanges
                    .get_mut(&exchange_id)
                    .ok_or(ServerError::InvalidOutboundState)?;
                state.finishing_header = Some(commit);
            }
            (None, Some(ResponseTrailers::Http1(_))) | (_, None) => {
                state.stage = ServerStage::Done;
            }
            (Some(_), Some(ResponseTrailers::Http1(_)))
            | (None, Some(ResponseTrailers::Http2(_))) => {
                return Err(ServerError::InvalidOutboundState);
            }
        }
        Ok(())
    }
}

enum ClientInner {
    Http1 {
        decoder: Option<Http1ConnectionDecoder>,
        scratch: Http1HeaderScratch,
    },
    Http2(Box<H2Client>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClientStage {
    Sending,
    WaitingForContinue,
    Reading,
    Done,
}

#[derive(Debug)]
struct ClientExchangeState {
    stage: ClientStage,
    stream_id: Option<u32>,
    final_head_seen: bool,
    stream_response_body: bool,
    outbound_body_len: Option<usize>,
    outbound_body_sent: usize,
    outbound_body_complete: bool,
    expect_continue: bool,
    body_suppressed: bool,
    exchange_reusable: bool,
    request_header: Option<H2OutboundCommit>,
    locally_reset: bool,
    /// Bytes this streaming body last offered, so the fair scheduler can weigh a
    /// sibling on a call that belongs to another exchange. A streaming body
    /// declares no length, so this is the only evidence the driver has that a
    /// sibling has anything to send.
    streaming_offered: Option<usize>,
}

impl ClientExchangeState {
    fn new(
        protocol: HttpProtocol,
        stream_id: Option<u32>,
        stream_response_body: bool,
        outbound_body_len: Option<usize>,
        expect_continue: bool,
        request_header: Option<H2OutboundCommit>,
    ) -> Self {
        Self {
            stage: ClientStage::Sending,
            stream_id,
            final_head_seen: false,
            stream_response_body,
            outbound_body_len,
            outbound_body_sent: 0,
            outbound_body_complete: outbound_body_len == Some(0),
            expect_continue,
            body_suppressed: false,
            exchange_reusable: matches!(protocol, HttpProtocol::Http2),
            request_header,
            locally_reset: false,
            streaming_offered: None,
        }
    }

    fn is_finished(&self) -> bool {
        self.stage == ClientStage::Done
    }
}

/// An I/O-free client-side connection driver.
///
/// The driver owns HTTP/1 or HTTP/2 protocol state and exposes a single
/// callback-scoped receive loop plus zero-copy request-body planning.
pub struct ClientConnection {
    protocol: HttpProtocol,
    inner: ClientInner,
    limits: HttpLimits,
    pending: Option<PendingStep>,
    output: Vec<u8>,
    pending_body: Option<PendingBody>,
    complete_pending: Option<ExchangeId>,
    exchanges: HashMap<ExchangeId, ClientExchangeState>,
    http1_exchange: Option<ExchangeId>,
    next_http1_exchange_id: u64,
    request_scheduler: H2FairStreamScheduler,
    scheduled_body: Option<ExchangeId>,
    reset_pending: Option<ExchangeId>,
    request_resets: VecDeque<ExchangeId>,
    buffer_finishes: Option<ExchangeId>,
    output_exchange: Option<ExchangeId>,
    input_exchange: Option<ExchangeId>,
}

impl ClientConnection {
    /// Creates a client driver for one selected wire protocol.
    pub fn new(protocol: HttpProtocol, limits: HttpLimits) -> Self {
        let inner = match protocol {
            HttpProtocol::Http1 => ClientInner::Http1 {
                decoder: None,
                scratch: Http1HeaderScratch::new(limits.max_headers()),
            },
            HttpProtocol::Http2 => ClientInner::Http2(Box::new(new_h2_client(limits))),
        };
        Self {
            protocol,
            inner,
            limits,
            pending: None,
            output: Vec::new(),
            pending_body: None,
            complete_pending: None,
            exchanges: HashMap::new(),
            http1_exchange: None,
            next_http1_exchange_id: 0,
            request_scheduler: H2FairStreamScheduler::with_capacity(
                h2_limits(limits).max_active_streams,
            ),
            scheduled_body: None,
            reset_pending: None,
            request_resets: VecDeque::new(),
            buffer_finishes: None,
            output_exchange: None,
            input_exchange: None,
        }
    }

    /// Returns the selected wire protocol.
    pub const fn protocol(&self) -> HttpProtocol {
        self.protocol
    }

    /// Returns the peer- and locally-bounded HTTP/2 stream capacity.
    ///
    /// HTTP/1 connections report one.
    pub fn max_active_exchanges(&self) -> usize {
        match &self.inner {
            ClientInner::Http1 { .. } => 1,
            ClientInner::Http2(client) => client.active_stream_limit(),
        }
    }

    /// Returns the exchange whose request bytes are in the pending write.
    ///
    /// Connection prefaces are attributed to the request that caused them.
    /// Connection-level control writes and stream resets return `None`.
    pub fn pending_write_exchange(&self) -> Option<ExchangeId> {
        match self.pending? {
            PendingStep::Write {
                kind:
                    WriteKind::H2Block {
                        finishing_exchange, ..
                    },
                ..
            } => finishing_exchange,
            PendingStep::Write {
                kind: WriteKind::Buffer,
                ..
            } => self.output_exchange,
            PendingStep::Input(_) => None,
        }
    }

    /// Returns the active exchange named by the HTTP/2 frame inspected during
    /// the most recent [`Self::step`] call.
    ///
    /// Attribution is available from a complete frame header even when the
    /// payload is incomplete or frame processing returns an error. HTTP/1,
    /// connection-level frames, and unknown streams return `None`.
    pub const fn input_exchange(&self) -> Option<ExchangeId> {
        self.input_exchange
    }

    /// Returns whether an HTTP/1.1 request is waiting for `100 Continue`.
    ///
    /// A transport adapter can use this state to start its own short deadline.
    pub fn is_waiting_for_continue(&self, exchange_id: ExchangeId) -> bool {
        self.exchanges
            .get(&exchange_id)
            .is_some_and(|state| state.stage == ClientStage::WaitingForContinue)
    }

    /// Releases a request body after the adapter's continue wait expires.
    ///
    /// The driver owns no clock. Callers bound the wait externally, then call
    /// this method before resuming request-body writes.
    pub fn proceed_with_body(&mut self, exchange_id: ExchangeId) -> Result<(), ServerError> {
        if self.pending.is_some() || self.pending_body.is_some() {
            return Err(ServerError::InvalidOutboundState);
        }
        let state = self
            .exchanges
            .get_mut(&exchange_id)
            .ok_or(ServerError::InvalidOutboundState)?;
        if state.stage != ClientStage::WaitingForContinue || state.body_suppressed {
            return Err(ServerError::InvalidOutboundState);
        }
        state.expect_continue = false;
        state.stage = ClientStage::Sending;
        Ok(())
    }

    /// Retires one completed exchange while preserving every sibling stream.
    ///
    /// A peer-reset HTTP/2 exchange may also be retired after the caller
    /// consumes the reset frame reported by [`Self::step`].
    ///
    /// Returns whether the connection can accept another exchange immediately.
    /// A `false` HTTP/1 result requires closing the transport. HTTP/2 can also
    /// return `false` while surviving sibling streams occupy the peer's current
    /// capacity; those siblings remain usable.
    ///
    /// This is the return-to-pool safety gate for HTTP/1: a transport adapter
    /// must also probe for unread bytes when checking the connection out,
    /// because peer bytes can arrive while the connection is idle after this
    /// method returns.
    pub fn begin_next_exchange(&mut self, exchange_id: ExchangeId) -> Result<bool, ServerError> {
        if self.pending.is_some() || self.pending_body.is_some() || !self.output.is_empty() {
            return Err(ServerError::InvalidOutboundState);
        }
        // A declared body that was never sent leaves the peer's framing state
        // unknowable. If it intends to read and discard that body, it will
        // swallow the head of the next request on this connection, which is a
        // request smuggling condition. The peer's keep-alive signal cannot
        // settle the question because it does not say whether the body is
        // still outstanding, so an unsent body always retires the connection.
        // This mirrors the server, which refuses to retire an exchange whose
        // request body was abandoned unless the response also closes.
        let state = self
            .exchanges
            .get(&exchange_id)
            .ok_or(ServerError::InvalidOutboundState)?;
        let peer_reset = self.reset_pending == Some(exchange_id);
        if !peer_reset
            && !state.locally_reset
            && (!state.is_finished()
                || !state.final_head_seen
                || !state.outbound_body_complete
                || !state.exchange_reusable
                || state.request_header.is_some())
        {
            return Ok(false);
        }
        match &mut self.inner {
            ClientInner::Http1 { decoder, .. } => {
                if self.http1_exchange != Some(exchange_id) || state.stream_id.is_some() {
                    return Err(ServerError::InvalidOutboundState);
                }
                decoder
                    .as_mut()
                    .ok_or(ServerError::InvalidOutboundState)?
                    .begin_next_message()?;
                self.exchanges.remove(&exchange_id);
                self.http1_exchange = None;
                if self.complete_pending == Some(exchange_id) {
                    self.complete_pending = None;
                }
                Ok(true)
            }
            ClientInner::Http2(client) => {
                let stream_id = state.stream_id.ok_or(ServerError::InvalidOutboundState)?;
                self.exchanges.remove(&exchange_id);
                self.request_scheduler.remove(stream_id);
                if self.scheduled_body == Some(exchange_id) {
                    self.scheduled_body = None;
                }
                if self.complete_pending == Some(exchange_id) {
                    self.complete_pending = None;
                }
                if self.reset_pending == Some(exchange_id) {
                    self.reset_pending = None;
                }
                self.request_resets
                    .retain(|pending| *pending != exchange_id);
                if self.buffer_finishes == Some(exchange_id) {
                    self.buffer_finishes = None;
                }
                Ok(client.can_open_stream())
            }
        }
    }

    /// Returns whether an exchange has finished and can be retired.
    pub fn exchange_is_retireable(&self, exchange_id: ExchangeId) -> bool {
        let Some(state) = self.exchanges.get(&exchange_id) else {
            return false;
        };
        self.reset_pending == Some(exchange_id)
            || state.locally_reset
            || (state.is_finished()
                && state.final_head_seen
                && state.outbound_body_complete
                && state.exchange_reusable
                && state.request_header.is_none())
    }

    /// Resets one locally abandoned HTTP/2 exchange.
    ///
    /// The reset is emitted after already-queued request headers. HTTP/1
    /// cannot isolate one exchange and returns `false`.
    pub fn abandon_exchange(&mut self, exchange_id: ExchangeId) -> Result<bool, ServerError> {
        if self.pending.is_some() || self.pending_body.is_some() {
            return Err(ServerError::InvalidOutboundState);
        }
        if !matches!(self.inner, ClientInner::Http2(_)) {
            return Ok(false);
        }
        let state = self
            .exchanges
            .get(&exchange_id)
            .ok_or(ServerError::InvalidOutboundState)?;
        if state.locally_reset {
            return Ok(true);
        }
        if !self.request_resets.contains(&exchange_id) {
            self.request_resets.push_back(exchange_id);
        }
        if let Some(stream_id) = state.stream_id {
            self.request_scheduler.remove(stream_id);
        }
        if self.scheduled_body == Some(exchange_id) {
            self.scheduled_body = None;
        }
        Ok(true)
    }

    /// Processes one frame received while an HTTP/2 connection is idle.
    ///
    /// Only SETTINGS, PING, and WINDOW_UPDATE are accepted as idle control
    /// traffic. The returned bytes contain any mandatory acknowledgement and
    /// must be written before the connection is reused. Application events,
    /// GOAWAY, and unclassified frames return [`ClientIdleStatus::NotReusable`].
    /// Any returned error also requires the caller to discard the connection.
    pub fn process_idle_input(
        &mut self,
        input: &[u8],
    ) -> Result<(ClientIdleStatus, usize, Vec<u8>), ServerError> {
        if !self.exchanges.is_empty()
            || self.http1_exchange.is_some()
            || self.pending.is_some()
            || self.pending_body.is_some()
            || !self.output.is_empty()
        {
            return Err(ServerError::InvalidOutboundState);
        }
        let ClientInner::Http2(client) = &mut self.inner else {
            return Err(ServerError::InvalidOutboundState);
        };
        let (event, consumed, output) = client.accept_bytes_ref(input)?;
        let status = match event {
            Some(
                H2ByteClientEventRef::Settings { .. }
                | H2ByteClientEventRef::Ping { .. }
                | H2ByteClientEventRef::WindowUpdate { .. },
            ) => ClientIdleStatus::Reusable,
            Some(
                H2ByteClientEventRef::ResponseHeaders { .. }
                | H2ByteClientEventRef::Data { .. }
                | H2ByteClientEventRef::DiscardedData { .. }
                | H2ByteClientEventRef::Trailers { .. }
                | H2ByteClientEventRef::Reset { .. }
                | H2ByteClientEventRef::Goaway { .. },
            ) => ClientIdleStatus::NotReusable,
            None if consumed == 0 => ClientIdleStatus::NeedInput,
            None => ClientIdleStatus::NotReusable,
        };
        Ok((status, consumed, output))
    }

    /// Prepares request headers and records the optional caller-owned body length.
    pub fn prepare_request(
        &mut self,
        request: ClientRequest<'_>,
    ) -> Result<ExchangeId, ServerError> {
        self.prepare_request_inner(request, false)
    }

    /// Prepares a request whose response body will be consumed incrementally.
    pub fn prepare_request_streaming_response(
        &mut self,
        request: ClientRequest<'_>,
    ) -> Result<ExchangeId, ServerError> {
        self.prepare_request_inner(request, true)
    }

    fn prepare_request_inner(
        &mut self,
        request: ClientRequest<'_>,
        stream_response_body: bool,
    ) -> Result<ExchangeId, ServerError> {
        if self.pending.is_some() || self.pending_body.is_some() {
            return Err(ServerError::InvalidOutboundState);
        }
        let expectation = classify_request_expectation(request.headers.iter().copied());
        let (exchange_id, state) = match &mut self.inner {
            ClientInner::Http1 { decoder, .. } => {
                if self.http1_exchange.is_some() {
                    return Err(ServerError::InvalidOutboundState);
                }
                let mut headers = parse_headers(request.headers)?;
                let content_length = request.body_len.map(|body_len| body_len.to_string());
                if let Some(content_length) = content_length.as_ref()
                    && !headers
                        .iter()
                        .any(|header| header.name.eq_ignore_ascii_case("content-length"))
                {
                    headers.push(ParseHeader {
                        name: "content-length",
                        value: content_length.as_bytes(),
                    });
                } else if request.body_len.is_none() {
                    headers.retain(|header| {
                        !header.name.eq_ignore_ascii_case("content-length")
                            && !header.name.eq_ignore_ascii_case("transfer-encoding")
                    });
                    headers.push(ParseHeader {
                        name: "transfer-encoding",
                        value: b"chunked",
                    });
                }
                self.output = Http1Server::request_head_bytes_with_raw_headers_and_limits(
                    request.method,
                    request.target,
                    1,
                    &headers,
                    request.body_len.unwrap_or(0),
                    self.limits,
                )?;
                if let Some(decoder) = decoder {
                    decoder.set_response_request_method(request.method)?;
                    decoder.set_max_body_bytes(if stream_response_body {
                        usize::MAX
                    } else {
                        self.limits.max_body_bytes()
                    })?;
                } else {
                    *decoder = Some(Http1ConnectionDecoder::response(
                        request.method,
                        if stream_response_body {
                            streaming_body_limits(self.limits)
                        } else {
                            self.limits
                        },
                    ));
                }
                let next_http1_exchange_id = self
                    .next_http1_exchange_id
                    .checked_add(1)
                    .ok_or(ServerError::InvalidOutboundState)?;
                let exchange_id = ExchangeId(next_http1_exchange_id);
                self.next_http1_exchange_id = next_http1_exchange_id;
                self.http1_exchange = Some(exchange_id);
                self.output_exchange = Some(exchange_id);
                let outbound_body_complete = request.body_len == Some(0);
                let expect_continue =
                    expectation == RequestExpectation::Continue && !outbound_body_complete;
                (
                    exchange_id,
                    ClientExchangeState::new(
                        self.protocol,
                        None,
                        stream_response_body,
                        request.body_len,
                        expect_continue,
                        None,
                    ),
                )
            }
            ClientInner::Http2(client) => {
                let mut headers = h2_headers(request.headers, false);
                if request.body_len.is_none() {
                    headers.retain(|header| {
                        !header.name.eq_ignore_ascii_case(b"content-length")
                            && !header.name.eq_ignore_ascii_case(b"transfer-encoding")
                    });
                }
                let mut fields = vec![
                    H2HeaderField::new(b":method", request.method.as_bytes()),
                    H2HeaderField::new(b":scheme", request.scheme.as_bytes()),
                    H2HeaderField::new(b":authority", request.authority.as_bytes()),
                    H2HeaderField::new(b":path", request.target.as_bytes()),
                ];
                fields.extend(headers.iter().cloned());
                project_h2_request_head(&fields, self.limits)?;
                let preface = client.connection_preface();
                let wrote_preface = !preface.is_empty();
                self.output.extend_from_slice(&preface);
                let (stream_id, commit) = client
                    .open_stream_with_raw_headers(
                        request.method,
                        request.scheme,
                        request.authority,
                        request.target,
                        &headers,
                        request.body_len == Some(0),
                    )
                    .map_err(ServerError::from)?;
                client.stream_body(stream_id, request.body_len.is_none(), stream_response_body)?;
                let exchange_id = ExchangeId::http2(stream_id);
                if wrote_preface {
                    self.output_exchange = Some(exchange_id);
                }
                if request.body_len != Some(0) {
                    self.request_scheduler.register(stream_id);
                }
                (
                    exchange_id,
                    ClientExchangeState::new(
                        self.protocol,
                        Some(stream_id),
                        stream_response_body,
                        request.body_len,
                        false,
                        Some(commit),
                    ),
                )
            }
        };
        if self.exchanges.insert(exchange_id, state).is_some() {
            return Err(ServerError::InvalidOutboundState);
        }
        Ok(exchange_id)
    }

    /// Returns the input byte count associated with the current step.
    pub fn consumed(&self) -> usize {
        self.pending.map_or(0, PendingStep::input)
    }

    /// Returns bytes for the currently pending [`Step::Write`].
    pub fn pending_write(&self) -> Option<&[u8]> {
        match self.pending? {
            PendingStep::Write {
                kind: WriteKind::Buffer,
                ..
            } => Some(&self.output),
            PendingStep::Write {
                kind: WriteKind::H2Block { commit, .. },
                ..
            } => {
                let ClientInner::Http2(client) = &self.inner else {
                    return None;
                };
                client
                    .next_outbound_block()
                    .filter(|block| block.commit() == commit)
                    .map(|block| block.bytes())
            }
            PendingStep::Input(_) => None,
        }
    }

    /// Acknowledges the current step and its associated input consumption.
    pub fn consume(&mut self, amount: usize) -> Result<(), ServerError> {
        let pending = self
            .pending
            .take()
            .ok_or(ServerError::InvalidOutboundState)?;
        if pending.input() != amount {
            self.pending = Some(pending);
            return Err(ServerError::InvalidOutboundState);
        }
        match pending {
            PendingStep::Input(_) => {}
            PendingStep::Write {
                kind: WriteKind::Buffer,
                ..
            } => {
                self.output.clear();
                self.output_exchange = None;
                if let Some(exchange_id) = self.buffer_finishes.take() {
                    let state = self
                        .exchanges
                        .get_mut(&exchange_id)
                        .ok_or(ServerError::InvalidOutboundState)?;
                    state.locally_reset = true;
                    state.request_header = None;
                    state.stage = ClientStage::Done;
                }
                if let Some(exchange_id) = self.http1_exchange {
                    let state = self
                        .exchanges
                        .get_mut(&exchange_id)
                        .ok_or(ServerError::InvalidOutboundState)?;
                    if state.stage == ClientStage::Sending {
                        if state.expect_continue && !state.outbound_body_complete {
                            state.stage = ClientStage::WaitingForContinue;
                        } else if state.outbound_body_complete {
                            state.stage = ClientStage::Reading;
                        }
                    }
                }
            }
            PendingStep::Write {
                kind:
                    WriteKind::H2Block {
                        commit,
                        finishing_exchange,
                    },
                ..
            } => {
                let ClientInner::Http2(client) = &mut self.inner else {
                    return Err(ServerError::InvalidOutboundState);
                };
                client.acknowledge_outbound_block(commit)?;
                let exchange_id = finishing_exchange.ok_or(ServerError::InvalidOutboundState)?;
                let state = self
                    .exchanges
                    .get_mut(&exchange_id)
                    .ok_or(ServerError::InvalidOutboundState)?;
                if state.request_header != Some(commit) {
                    return Err(ServerError::InvalidOutboundState);
                }
                state.request_header = None;
                if state.outbound_body_complete {
                    state.stage = ClientStage::Reading;
                }
            }
        }
        Ok(())
    }

    /// Advances the connection and callback-scopes all borrowed protocol data.
    ///
    /// [`ServerError::PeerReset`] is stream-scoped: its stream identifier names
    /// the affected exchange. The caller consumes the reported input, retires
    /// that exchange with [`Self::begin_next_exchange`], and may continue
    /// driving sibling HTTP/2 streams. Other errors make all active exchanges
    /// non-reusable.
    pub fn step<R>(
        &mut self,
        input: &[u8],
        on_step: impl for<'step> FnOnce(Step<'step, ClientEvent<'step>>) -> R,
    ) -> Result<R, ServerError> {
        self.input_exchange = None;
        if self.pending.is_some() || self.pending_body.is_some() {
            return Err(ServerError::InvalidOutboundState);
        }
        if self.exchanges.is_empty() {
            return Err(ServerError::InvalidOutboundState);
        }
        let result = self.step_active(input, on_step);
        if let Err(error) = &result {
            if let ServerError::PeerReset { stream_id, .. } = error {
                if let Some(state) = self.exchanges.get_mut(&ExchangeId::http2(*stream_id)) {
                    state.exchange_reusable = false;
                }
            } else {
                for state in self.exchanges.values_mut() {
                    state.exchange_reusable = false;
                }
            }
        }
        result
    }

    fn step_active<R>(
        &mut self,
        input: &[u8],
        on_step: impl for<'step> FnOnce(Step<'step, ClientEvent<'step>>) -> R,
    ) -> Result<R, ServerError> {
        if !self.output.is_empty() {
            self.pending = Some(PendingStep::Write {
                input: 0,
                kind: WriteKind::Buffer,
            });
            return Ok(on_step(Step::Write(&self.output)));
        }
        if let ClientInner::Http2(client) = &self.inner
            && let Some(block) = client.next_outbound_block()
        {
            let commit = block.commit();
            let exchange_id = self
                .exchanges
                .iter()
                .find_map(|(exchange_id, state)| {
                    (state.request_header == Some(commit)).then_some(*exchange_id)
                })
                .ok_or(ServerError::InvalidOutboundState)?;
            self.pending = Some(PendingStep::Write {
                input: 0,
                kind: WriteKind::H2Block {
                    commit,
                    finishing_exchange: Some(exchange_id),
                },
            });
            return Ok(on_step(Step::Write(block.bytes())));
        }
        if let Some(exchange_id) = self.request_resets.pop_front() {
            let state = self
                .exchanges
                .get(&exchange_id)
                .ok_or(ServerError::InvalidOutboundState)?;
            let stream_id = state.stream_id.ok_or(ServerError::InvalidOutboundState)?;
            let ClientInner::Http2(client) = &mut self.inner else {
                return Err(ServerError::InvalidOutboundState);
            };
            self.output = client.reset_stream(stream_id, H2ErrorCode::Cancel)?;
            self.output_exchange = None;
            self.buffer_finishes = Some(exchange_id);
            self.pending = Some(PendingStep::Write {
                input: 0,
                kind: WriteKind::Buffer,
            });
            return Ok(on_step(Step::Write(&self.output)));
        }
        // A finished response can still leave protocol output queued, such as
        // the local reset for an early HTTP/2 response. Flush it before `Done`
        // so the exchange can become retireable instead of stalling its caller.
        if self
            .exchanges
            .values()
            .all(ClientExchangeState::is_finished)
        {
            return Ok(on_step(Step::Done));
        }
        if let Some(exchange_id) = self.http1_exchange {
            let state = self
                .exchanges
                .get_mut(&exchange_id)
                .ok_or(ServerError::InvalidOutboundState)?;
            if state.stage == ClientStage::Sending {
                if !state.outbound_body_complete {
                    self.pending = Some(PendingStep::Input(0));
                    return Ok(on_step(Step::NeedInput));
                } else {
                    state.stage = ClientStage::Reading;
                }
            }
        }
        if let Some(exchange_id) = self.complete_pending.take() {
            let state = self
                .exchanges
                .get_mut(&exchange_id)
                .ok_or(ServerError::InvalidOutboundState)?;
            if !state.final_head_seen {
                return Err(ServerError::MalformedMessage);
            }
            if self.http1_exchange == Some(exchange_id) && !input.is_empty() {
                state.exchange_reusable = false;
            }
            state.stage = ClientStage::Done;
            if let Some(stream_id) = state.stream_id {
                self.request_scheduler.remove(stream_id);
                if self.scheduled_body == Some(exchange_id) {
                    self.scheduled_body = None;
                }
            }
            self.pending = Some(PendingStep::Input(0));
            return Ok(on_step(Step::Event(ClientEvent::ResponseComplete {
                exchange_id,
            })));
        }

        if self.protocol == HttpProtocol::Http2 {
            self.input_exchange = H2Frame::decode_header(input)
                .ok()
                .filter(|head| head.stream_id != 0)
                .map(|head| ExchangeId::http2(head.stream_id))
                .filter(|exchange_id| self.exchanges.contains_key(exchange_id));
        }

        match &mut self.inner {
            ClientInner::Http1 { decoder, scratch } => {
                let exchange_id = self
                    .http1_exchange
                    .ok_or(ServerError::InvalidOutboundState)?;
                let decoder = decoder.as_mut().ok_or(ServerError::InvalidOutboundState)?;
                let pending = &mut self.pending;
                let state = self
                    .exchanges
                    .get_mut(&exchange_id)
                    .ok_or(ServerError::InvalidOutboundState)?;
                scratch.with_input(input, |input, headers| {
                    let event = decoder.next_event(input, headers)?;
                    match event {
                        Http1ConnectionEvent::NeedInput => {
                            *pending = Some(PendingStep::Input(0));
                            Ok(on_step(Step::NeedInput))
                        }
                        Http1ConnectionEvent::Head {
                            head,
                            body,
                            consumed,
                            informational,
                        } => {
                            let Http1MessageHead::Response(head) = head else {
                                return Err(ServerError::MalformedMessage);
                            };
                            *pending = Some(PendingStep::Input(consumed));
                            if informational {
                                if head.status == 100
                                    && state.stage == ClientStage::WaitingForContinue
                                {
                                    state.expect_continue = false;
                                    state.stage = ClientStage::Sending;
                                }
                                return Ok(on_step(Step::NeedInput));
                            }
                            if state.final_head_seen {
                                return Err(ServerError::MalformedMessage);
                            }
                            state.final_head_seen = true;
                            if state.stage == ClientStage::WaitingForContinue {
                                state.expect_continue = false;
                                state.body_suppressed = true;
                                state.stage = ClientStage::Reading;
                            }
                            let wire_version = http1_version(head.version)?;
                            state.exchange_reusable =
                                http1_response_is_persistent(wire_version, head.headers)
                                    && !matches!(body, Http1BodyKind::Eof);
                            let version = match wire_version {
                                Http1Version::Http10 => HttpVersion::Http10,
                                Http1Version::Http11 => HttpVersion::Http11,
                            };
                            let content_length = match body {
                                Http1BodyKind::ContentLength(length) => Some(length),
                                Http1BodyKind::Empty
                                | Http1BodyKind::Chunked
                                | Http1BodyKind::Eof => None,
                            };
                            Ok(on_step(Step::Event(ClientEvent::ResponseHead {
                                exchange_id,
                                status: head.status,
                                version,
                                headers: HeaderBlock::http1(head.headers),
                                content_length,
                            })))
                        }
                        Http1ConnectionEvent::Body { chunk, consumed } => {
                            if !state.final_head_seen {
                                return Err(ServerError::MalformedMessage);
                            }
                            *pending = Some(PendingStep::Input(consumed));
                            Ok(on_step(Step::Event(ClientEvent::ResponseBody {
                                exchange_id,
                                chunk,
                            })))
                        }
                        Http1ConnectionEvent::Trailers { fields, consumed } => {
                            *pending = Some(PendingStep::Input(consumed));
                            if fields.is_empty() {
                                Ok(on_step(Step::NeedInput))
                            } else {
                                Ok(on_step(Step::Event(ClientEvent::ResponseTrailers {
                                    exchange_id,
                                    headers: HeaderBlock::http1(fields),
                                })))
                            }
                        }
                        Http1ConnectionEvent::ProtocolSwitch { .. } => {
                            Err(ServerError::UnsupportedTransferEncoding)
                        }
                        Http1ConnectionEvent::Complete => {
                            if !state.final_head_seen {
                                return Err(ServerError::MalformedMessage);
                            }
                            if !input.is_empty() {
                                state.exchange_reusable = false;
                            }
                            state.stage = ClientStage::Done;
                            *pending = Some(PendingStep::Input(0));
                            Ok(on_step(Step::Event(ClientEvent::ResponseComplete {
                                exchange_id,
                            })))
                        }
                    }
                })
            }
            ClientInner::Http2(client) => {
                let (event, consumed, output) = client.accept_bytes_ref(input)?;
                self.output = output;
                self.output_exchange = None;
                match event {
                    Some(H2ByteClientEventRef::ResponseHeaders {
                        stream_id,
                        headers,
                        end_stream,
                    }) => {
                        let exchange_id = self.ensure_stream(stream_id)?;
                        let stream_response_body = self
                            .exchanges
                            .get(&exchange_id)
                            .ok_or(ServerError::InvalidFrame)?
                            .stream_response_body;
                        let limits = if stream_response_body {
                            streaming_body_limits(self.limits)
                        } else {
                            self.limits
                        };
                        let head = project_h2_response_head(&headers, limits)?;
                        self.pending = Some(PendingStep::Input(consumed));
                        if head.status() == 101 {
                            return Err(ServerError::UnsupportedMethod);
                        }
                        if (100..=199).contains(&head.status()) {
                            return Ok(on_step(Step::NeedInput));
                        }
                        let state = self
                            .exchanges
                            .get_mut(&exchange_id)
                            .ok_or(ServerError::InvalidFrame)?;
                        if state.final_head_seen {
                            return Err(ServerError::MalformedMessage);
                        }
                        state.final_head_seen = true;
                        self.complete_pending = end_stream.then_some(exchange_id);
                        Ok(on_step(Step::Event(ClientEvent::ResponseHead {
                            exchange_id,
                            status: head.status(),
                            version: HttpVersion::Http2,
                            headers: HeaderBlock::http2(head.fields()),
                            content_length: head.content_length(),
                        })))
                    }
                    Some(H2ByteClientEventRef::Data {
                        stream_id,
                        payload,
                        end_stream,
                        ..
                    }) => {
                        let exchange_id = self.ensure_stream(stream_id)?;
                        let state = self
                            .exchanges
                            .get(&exchange_id)
                            .ok_or(ServerError::InvalidFrame)?;
                        if !state.final_head_seen {
                            return Err(ServerError::MalformedMessage);
                        }
                        self.complete_pending = end_stream.then_some(exchange_id);
                        self.pending = Some(PendingStep::Input(consumed));
                        Ok(on_step(Step::Event(ClientEvent::ResponseBody {
                            exchange_id,
                            chunk: payload,
                        })))
                    }
                    Some(H2ByteClientEventRef::Trailers { stream_id, headers }) => {
                        let exchange_id = self.ensure_stream(stream_id)?;
                        self.complete_pending = Some(exchange_id);
                        self.pending = Some(PendingStep::Input(consumed));
                        if headers.is_empty() {
                            Ok(on_step(Step::NeedInput))
                        } else {
                            Ok(on_step(Step::Event(ClientEvent::ResponseTrailers {
                                exchange_id,
                                headers: HeaderBlock::http2(&headers),
                            })))
                        }
                    }
                    Some(H2ByteClientEventRef::Reset {
                        stream_id,
                        error_code,
                    }) if self.exchanges.contains_key(&ExchangeId::http2(stream_id)) => {
                        let exchange_id = ExchangeId::http2(stream_id);
                        self.reset_pending = Some(exchange_id);
                        self.pending = Some(PendingStep::Input(consumed));
                        self.request_scheduler.remove(stream_id);
                        if self.scheduled_body == Some(exchange_id) {
                            self.scheduled_body = None;
                        }
                        Err(ServerError::PeerReset {
                            stream_id,
                            error_code,
                        })
                    }
                    Some(H2ByteClientEventRef::Goaway {
                        last_stream_id,
                        error_code,
                    }) => Err(ServerError::PeerGoaway {
                        last_stream_id,
                        error_code,
                    }),
                    Some(
                        H2ByteClientEventRef::Settings { .. }
                        | H2ByteClientEventRef::Ping { .. }
                        | H2ByteClientEventRef::WindowUpdate { .. }
                        | H2ByteClientEventRef::DiscardedData { .. }
                        | H2ByteClientEventRef::Reset { .. },
                    )
                    | None => self.emit_h2_progress(consumed, on_step),
                }
            }
        }
    }

    fn ensure_stream(&self, stream_id: u32) -> Result<ExchangeId, ServerError> {
        let exchange_id = ExchangeId::http2(stream_id);
        self.exchanges
            .get(&exchange_id)
            .filter(|state| state.stream_id == Some(stream_id))
            .map(|_| exchange_id)
            .ok_or(ServerError::InvalidFrame)
    }

    fn emit_h2_progress<R>(
        &mut self,
        consumed: usize,
        on_step: impl for<'step> FnOnce(Step<'step, ClientEvent<'step>>) -> R,
    ) -> Result<R, ServerError> {
        if self.output.is_empty() {
            self.pending = Some(PendingStep::Input(consumed));
            Ok(on_step(Step::NeedInput))
        } else {
            self.pending = Some(PendingStep::Write {
                input: consumed,
                kind: WriteKind::Buffer,
            });
            Ok(on_step(Step::Write(&self.output)))
        }
    }

    /// Marks transport EOF and completes a valid HTTP/1 close-delimited response.
    ///
    /// Returns `true` when EOF completed the response. HTTP/2 and premature
    /// HTTP/1 EOF return an error.
    pub fn finish_eof(&mut self) -> Result<bool, ServerError> {
        if self.pending.is_some() || self.pending_body.is_some() {
            return Err(ServerError::InvalidOutboundState);
        }
        let exchange_id = self
            .http1_exchange
            .ok_or(ServerError::InvalidOutboundState)?;
        let ClientInner::Http1 { decoder, .. } = &mut self.inner else {
            return Err(ServerError::MalformedMessage);
        };
        let result = decoder
            .as_mut()
            .ok_or(ServerError::InvalidOutboundState)?
            .finish_eof();
        match result {
            Ok(completed) => {
                self.complete_pending = completed.then_some(exchange_id);
                Ok(completed)
            }
            Err(error) => {
                let state = self
                    .exchanges
                    .get_mut(&exchange_id)
                    .ok_or(ServerError::InvalidOutboundState)?;
                state.exchange_reusable = false;
                Err(error)
            }
        }
    }

    /// Declares how many request-body bytes an exchange currently has ready.
    ///
    /// A streaming body declares no length, so the driver cannot tell whether a
    /// stream it is not currently being asked about has anything to send. An
    /// adapter driving concurrent streaming uploads must call this for every
    /// active exchange - passing zero for those with nothing ready - before
    /// [`Self::prepare_body_chunk`], or the fair scheduler cannot see the
    /// siblings and will keep serving whichever exchange it is asked about
    /// first. Known-length bodies are unaffected, since their outstanding count
    /// is already derivable.
    ///
    /// `concurrent_streaming_request_bodies_rotate_fairly` pins the rotation
    /// this enables.
    pub fn note_request_body_available(&mut self, exchange_id: ExchangeId, available: usize) {
        if let Some(state) = self.exchanges.get_mut(&exchange_id)
            && state.outbound_body_len.is_none()
        {
            state.streaming_offered = Some(available);
        }
    }

    /// Plans the next zero-copy request-body write.
    ///
    /// For a known-length body, `remaining` must be the complete unsent suffix
    /// length. For a streaming body, it is the currently available byte count,
    /// and zero stages the protocol end marker. A `false` return means the
    /// client is waiting for `100 Continue`, a final response suppressed the
    /// body, or HTTP/2 output or send windows are blocking it.
    pub fn prepare_body_chunk(
        &mut self,
        exchange_id: ExchangeId,
        remaining: usize,
    ) -> Result<bool, ServerError> {
        if self.pending.is_some() || self.pending_body.is_some() {
            return Err(ServerError::InvalidOutboundState);
        }
        let state = self
            .exchanges
            .get(&exchange_id)
            .ok_or(ServerError::InvalidOutboundState)?;
        if let Some(body_len) = state.outbound_body_len {
            let expected_remaining = body_len.saturating_sub(state.outbound_body_sent);
            if remaining != expected_remaining {
                return Err(ServerError::InvalidOutboundState);
            }
            if remaining == 0 {
                return Ok(false);
            }
        }
        if state.outbound_body_len.is_some() {
            let projected_body_len = state.outbound_body_sent.saturating_add(remaining);
            if projected_body_len > self.limits.max_body_bytes() {
                return Err(ServerError::BodyTooLarge {
                    limit: self.limits.max_body_bytes(),
                    actual: projected_body_len,
                });
            }
        }
        if state.body_suppressed
            || state.stage == ClientStage::WaitingForContinue
            || (state.expect_continue && !self.output.is_empty())
        {
            return Ok(false);
        }
        if state.stage != ClientStage::Sending {
            return Err(ServerError::InvalidOutboundState);
        }
        let stream_id = state.stream_id;
        let outbound_body_len = state.outbound_body_len;
        match &self.inner {
            ClientInner::Http1 { .. } => {
                if self.http1_exchange != Some(exchange_id) || stream_id.is_some() {
                    return Err(ServerError::InvalidOutboundState);
                }
                let streaming_end = outbound_body_len.is_none() && remaining == 0;
                let chunked = outbound_body_len.is_none();
                let footer = if chunked && !streaming_end {
                    b"\r\n".as_slice()
                } else {
                    b"".as_slice()
                };
                if chunked {
                    if streaming_end {
                        self.output.extend_from_slice(b"0\r\n\r\n");
                    } else {
                        Http1Server::push_chunked_body_prefix(&mut self.output, remaining);
                    }
                }
                self.pending_body = Some(PendingBody::Http1 {
                    exchange_id,
                    payload_len: remaining,
                    footer,
                    finishes_body: outbound_body_len.is_some() || streaming_end,
                });
                Ok(true)
            }
            ClientInner::Http2(client) => {
                if !self.output.is_empty() || client.next_outbound_block().is_some() {
                    return Ok(false);
                }
                let stream_id = stream_id.ok_or(ServerError::InvalidOutboundState)?;
                // Record what this streaming body is offering before selecting.
                // A later call for a sibling reads it, which is what lets the
                // scheduler see that this stream is still waiting its turn.
                if outbound_body_len.is_none()
                    && let Some(state) = self.exchanges.get_mut(&exchange_id)
                {
                    state.streaming_offered = Some(remaining);
                }
                if self.scheduled_body.is_none() {
                    let exchanges = &self.exchanges;
                    let selected = self.request_scheduler.next_ready(|candidate| {
                        let candidate_id = ExchangeId::http2(candidate);
                        let Some(state) = exchanges.get(&candidate_id) else {
                            return false;
                        };
                        let pending_bytes = match state.outbound_body_len {
                            Some(body_len) => body_len.saturating_sub(state.outbound_body_sent),
                            None if candidate == stream_id => remaining,
                            // A sibling streaming body is weighed by what it
                            // last offered. Treating it as having nothing made
                            // it permanently ineligible, so the scheduler could
                            // never rotate away from whichever stream the
                            // adapter happened to offer first, and that stream
                            // took the whole connection.
                            // `concurrent_streaming_request_bodies_rotate_fairly`
                            // pins the rotation.
                            None => state.streaming_offered.unwrap_or(0),
                        };
                        let streaming_end = state.outbound_body_len.is_none()
                            && candidate == stream_id
                            && remaining == 0;
                        state.stage == ClientStage::Sending
                            && (pending_bytes != 0 || streaming_end)
                            && client
                                .send_capacity(candidate, pending_bytes)
                                .is_ok_and(|capacity| capacity.sendable_bytes != 0 || streaming_end)
                    });
                    self.scheduled_body = selected.map(ExchangeId::http2);
                }
                if self.scheduled_body != Some(exchange_id) {
                    return Ok(false);
                }
                let end_stream = outbound_body_len.is_some() || remaining == 0;
                let Some(plan) = client.prepare_data_frame(stream_id, remaining, end_stream)?
                else {
                    self.scheduled_body = None;
                    return Ok(false);
                };
                self.pending_body = Some(PendingBody::Http2 { exchange_id, plan });
                Ok(true)
            }
        }
    }

    /// Borrows framing bytes for the body chunk prepared by [`Self::prepare_body_chunk`].
    pub fn body_chunk(&self, exchange_id: ExchangeId) -> Option<BodyChunk<'_>> {
        match self.pending_body.as_ref()? {
            PendingBody::Http1 {
                exchange_id: pending_exchange,
                payload_len,
                footer,
                ..
            } if *pending_exchange == exchange_id => Some(BodyChunk {
                header: &self.output,
                payload_len: *payload_len,
                footer,
            }),
            PendingBody::Http2 {
                exchange_id: pending_exchange,
                plan,
            } if *pending_exchange == exchange_id => Some(BodyChunk {
                header: plan.header(),
                payload_len: plan.payload_len(),
                footer: &[],
            }),
            PendingBody::Http1 { .. } | PendingBody::Http2 { .. } => None,
        }
    }

    /// Commits a successfully written request-body chunk.
    pub fn commit_body_chunk(&mut self, exchange_id: ExchangeId) -> Result<(), ServerError> {
        let pending = self
            .pending_body
            .take()
            .ok_or(ServerError::InvalidOutboundState)?;
        if pending.exchange_id() != exchange_id {
            self.pending_body = Some(pending);
            return Err(ServerError::InvalidOutboundState);
        }
        let (payload_len, finishes_body) = match pending {
            PendingBody::Http1 {
                payload_len,
                finishes_body,
                ..
            } => {
                self.output.clear();
                (payload_len, finishes_body)
            }
            PendingBody::Http2 { plan, .. } => {
                let payload_len = plan.payload_len();
                let finishes_body = plan.end_stream();
                let ClientInner::Http2(client) = &mut self.inner else {
                    self.pending_body = Some(pending);
                    return Err(ServerError::InvalidOutboundState);
                };
                if let Err(error) = client.commit_data_frame(plan) {
                    self.pending_body = Some(pending);
                    return Err(error);
                }
                self.scheduled_body = None;
                (payload_len, finishes_body)
            }
        };
        let state = self
            .exchanges
            .get_mut(&exchange_id)
            .ok_or(ServerError::InvalidOutboundState)?;
        state.outbound_body_sent = state.outbound_body_sent.saturating_add(payload_len);
        // The offer this write consumed is spent. Leaving it set would keep
        // claiming turns for a stream whose adapter has nothing more ready yet.
        state.streaming_offered = None;
        if finishes_body {
            if let Some(body_len) = state.outbound_body_len
                && state.outbound_body_sent != body_len
            {
                return Err(ServerError::InvalidOutboundState);
            }
            state.outbound_body_complete = true;
            state.expect_continue = false;
            state.stage = ClientStage::Reading;
            if let Some(stream_id) = state.stream_id {
                self.request_scheduler.remove(stream_id);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{H2ByteStreamEvent, H2Frame, H2FrameType, HttpErrorKind};

    #[derive(Debug, Eq, PartialEq)]
    enum ServerObserved {
        NeedInput,
        Write(Vec<u8>),
        Head {
            exchange_id: ExchangeId,
            method: Vec<u8>,
            target: Vec<u8>,
            version: HttpVersion,
        },
        Body {
            exchange_id: ExchangeId,
            chunk: Vec<u8>,
        },
        Complete {
            exchange_id: ExchangeId,
        },
        Done,
    }

    fn server_step(
        connection: &mut ServerConnection,
        input: &[u8],
    ) -> Result<ServerObserved, ServerError> {
        connection.step(input, |step| match step {
            Step::NeedInput => ServerObserved::NeedInput,
            Step::Write(bytes) => ServerObserved::Write(bytes.to_vec()),
            Step::Event(ServerEvent::RequestHead {
                exchange_id,
                method,
                target,
                version,
                ..
            }) => ServerObserved::Head {
                exchange_id,
                method: method.to_vec(),
                target: target.to_vec(),
                version,
            },
            Step::Event(ServerEvent::RequestBody { exchange_id, chunk }) => ServerObserved::Body {
                exchange_id,
                chunk: chunk.to_vec(),
            },
            Step::Event(ServerEvent::RequestTrailers { .. }) => {
                panic!("unexpected request trailers")
            }
            Step::Event(ServerEvent::RequestComplete { exchange_id }) => {
                ServerObserved::Complete { exchange_id }
            }
            Step::Done => ServerObserved::Done,
        })
    }

    fn take_client_block(client: &mut H2Client, commit: H2OutboundCommit) -> Vec<u8> {
        let block = client.next_outbound_block().unwrap();
        assert_eq!(block.commit(), commit);
        let bytes = block.bytes().to_vec();
        client.acknowledge_outbound_block(commit).unwrap();
        bytes
    }

    fn h2_request(body: &[u8]) -> Vec<u8> {
        let mut client = H2Client::default();
        let mut bytes = client.connection_preface();
        let content_length = body.len().to_string();
        let headers = [H2HeaderField::new(
            b"content-length",
            content_length.as_bytes(),
        )];
        let (stream_id, commit) = client
            .open_stream_with_raw_headers(
                "POST",
                "http",
                "example.test",
                "/submit",
                &headers,
                body.is_empty(),
            )
            .unwrap();
        bytes.extend_from_slice(&take_client_block(&mut client, commit));
        if !body.is_empty() {
            bytes.extend_from_slice(&client.data_frame(stream_id, body, true));
        }
        bytes
    }

    fn h2_open_request() -> Vec<u8> {
        let mut client = H2Client::default();
        let mut bytes = client.connection_preface();
        let headers = [H2HeaderField::new(b"content-length", b"3")];
        let (_, commit) = client
            .open_stream_with_raw_headers(
                "POST",
                "http",
                "example.test",
                "/submit",
                &headers,
                false,
            )
            .unwrap();
        bytes.extend_from_slice(&take_client_block(&mut client, commit));
        bytes
    }

    fn collect_request_events(
        connection: &mut ServerConnection,
        input: &[u8],
        expected_completions: usize,
    ) -> (Vec<ServerObserved>, usize) {
        let mut events = Vec::new();
        let mut offset = 0;
        let mut completions = 0;
        for _ in 0..64 {
            let observed = server_step(connection, &input[offset..]).unwrap();
            let consumed = connection.consumed();
            let retain = match &observed {
                ServerObserved::Write(_) | ServerObserved::NeedInput => false,
                ServerObserved::Complete { .. } => {
                    completions += 1;
                    true
                }
                ServerObserved::Head { .. } | ServerObserved::Body { .. } => true,
                ServerObserved::Done => panic!("requests completed before all events arrived"),
            };
            if retain {
                events.push(observed);
            }
            connection.consume(consumed).unwrap();
            offset += consumed;
            if completions == expected_completions {
                return (events, offset);
            }
        }
        panic!("request event collection did not complete");
    }

    fn assert_connections_observe_same_request(
        mut existing: ServerConnection,
        mut unified: ServerConnection,
        input: &[u8],
    ) {
        let existing_protocol_before = existing.protocol();
        let unified_protocol_before = unified.protocol();
        let existing_observation = collect_request_events(&mut existing, input, 1);
        let unified_observation = collect_request_events(&mut unified, input, 1);

        assert_eq!(existing_protocol_before, unified_protocol_before);
        assert_eq!(existing_observation, unified_observation);
        assert_eq!(existing.protocol(), unified.protocol());
    }

    #[test]
    fn http_protocol_maps_supported_alpn_identifiers() {
        assert_eq!(
            HttpProtocol::from_alpn(Some(b"h2")),
            Ok(HttpProtocol::Http2)
        );
        assert_eq!(
            HttpProtocol::from_alpn(Some(b"http/1.1")),
            Ok(HttpProtocol::Http1)
        );
        assert_eq!(HttpProtocol::from_alpn(None), Ok(HttpProtocol::Http1));
    }

    #[test]
    fn http_protocol_rejects_unsupported_alpn_identifiers() {
        for identifier in [
            b"spdy/3".as_slice(),
            b"h2c".as_slice(),
            b"".as_slice(),
            b"http/1.0".as_slice(),
        ] {
            assert_eq!(
                HttpProtocol::from_alpn(Some(identifier)),
                Err(ServerError::UnsupportedAlpnProtocol)
            );
        }
    }

    #[test]
    fn http_protocol_alpn_identifiers_round_trip() {
        for protocol in [HttpProtocol::Http1, HttpProtocol::Http2] {
            assert_eq!(
                HttpProtocol::from_alpn(Some(protocol.alpn_identifier())),
                Ok(protocol)
            );
        }
    }

    #[test]
    fn unified_protocol_detection_recognizes_cleartext_http1_and_http2() {
        let mut http1 = ServerConnection::with_protocol_selection(
            ProtocolSelection::Detect,
            HttpLimits::new(),
            RequestBodyMode::Buffered,
        )
        .unwrap();
        let http1_request = b"GET /detected HTTP/1.1\r\nhost: example.test\r\n\r\n";
        let (events, consumed) = collect_request_events(&mut http1, http1_request, 1);
        assert_eq!(consumed, http1_request.len());
        assert!(matches!(
            events.first(),
            Some(ServerObserved::Head {
                target,
                version: HttpVersion::Http11,
                ..
            }) if target == b"/detected"
        ));
        assert_eq!(http1.protocol(), Some(HttpProtocol::Http1));

        let mut http2 = ServerConnection::with_protocol_selection(
            ProtocolSelection::Detect,
            HttpLimits::new(),
            RequestBodyMode::Buffered,
        )
        .unwrap();
        let http2_request = h2_request(&[]);
        let (events, consumed) = collect_request_events(&mut http2, &http2_request, 1);
        assert_eq!(consumed, http2_request.len());
        assert!(matches!(
            events.first(),
            Some(ServerObserved::Head {
                target,
                version: HttpVersion::Http2,
                ..
            }) if target == b"/submit"
        ));
        assert_eq!(http2.protocol(), Some(HttpProtocol::Http2));
    }

    #[test]
    fn known_and_alpn_http2_selections_consume_the_client_preface() {
        for selection in [
            ProtocolSelection::Known(HttpProtocol::Http2),
            ProtocolSelection::Alpn(Some(b"h2")),
        ] {
            let mut connection = ServerConnection::with_protocol_selection(
                selection,
                HttpLimits::new(),
                RequestBodyMode::Buffered,
            )
            .unwrap();
            let request = h2_request(b"body");
            let (events, consumed) = collect_request_events(&mut connection, &request, 1);

            assert_eq!(consumed, request.len());
            assert!(matches!(
                events.first(),
                Some(ServerObserved::Head {
                    version: HttpVersion::Http2,
                    ..
                })
            ));
            assert!(events.iter().any(
                |event| matches!(event, ServerObserved::Body { chunk, .. } if chunk == b"body")
            ));
            assert_eq!(connection.protocol(), Some(HttpProtocol::Http2));
        }
    }

    #[test]
    fn alpn_http1_and_no_selection_use_http1() {
        for identifier in [Some(b"http/1.1".as_slice()), None] {
            let mut connection = ServerConnection::with_protocol_selection(
                ProtocolSelection::Alpn(identifier),
                HttpLimits::new(),
                RequestBodyMode::Buffered,
            )
            .unwrap();
            let request = b"GET /alpn HTTP/1.1\r\nhost: example.test\r\n\r\n";
            let (events, consumed) = collect_request_events(&mut connection, request, 1);

            assert_eq!(consumed, request.len());
            assert!(matches!(
                events.first(),
                Some(ServerObserved::Head {
                    target,
                    version: HttpVersion::Http11,
                    ..
                }) if target == b"/alpn"
            ));
            assert_eq!(connection.protocol(), Some(HttpProtocol::Http1));
        }
    }

    #[test]
    fn unified_request_body_mode_controls_total_body_limit() {
        let limits = HttpLimits::new().set_max_body_bytes(2);
        let request = b"POST /body HTTP/1.1\r\nhost: example.test\r\ncontent-length: 3\r\n\r\nabc";
        let mut buffered = ServerConnection::with_protocol_selection(
            ProtocolSelection::Known(HttpProtocol::Http1),
            limits,
            RequestBodyMode::Buffered,
        )
        .unwrap();
        assert!(matches!(
            server_step(&mut buffered, request),
            Err(ServerError::BodyTooLarge {
                limit: 2,
                actual: 3
            })
        ));

        let mut streaming = ServerConnection::with_protocol_selection(
            ProtocolSelection::Known(HttpProtocol::Http1),
            limits,
            RequestBodyMode::Streaming,
        )
        .unwrap();
        let (events, consumed) = collect_request_events(&mut streaming, request, 1);
        assert_eq!(consumed, request.len());
        assert!(
            events.iter().any(
                |event| matches!(event, ServerObserved::Body { chunk, .. } if chunk == b"abc")
            )
        );
    }

    #[test]
    fn existing_server_constructors_match_unified_selection() {
        let limits = HttpLimits::new();
        let http1_request =
            b"POST /legacy HTTP/1.1\r\nhost: example.test\r\ncontent-length: 3\r\n\r\nold";
        assert_connections_observe_same_request(
            ServerConnection::new(limits),
            ServerConnection::with_protocol_selection(
                ProtocolSelection::Detect,
                limits,
                RequestBodyMode::Buffered,
            )
            .unwrap(),
            http1_request,
        );
        assert_connections_observe_same_request(
            ServerConnection::new_streaming(limits),
            ServerConnection::with_protocol_selection(
                ProtocolSelection::Detect,
                limits,
                RequestBodyMode::Streaming,
            )
            .unwrap(),
            http1_request,
        );
        assert_connections_observe_same_request(
            ServerConnection::new_with_protocol(HttpProtocol::Http1, limits),
            ServerConnection::with_protocol_selection(
                ProtocolSelection::Known(HttpProtocol::Http1),
                limits,
                RequestBodyMode::Buffered,
            )
            .unwrap(),
            http1_request,
        );

        let http2_request = h2_request(b"new");
        assert_connections_observe_same_request(
            ServerConnection::new_with_protocol_streaming(HttpProtocol::Http2, limits),
            ServerConnection::with_protocol_selection(
                ProtocolSelection::Known(HttpProtocol::Http2),
                limits,
                RequestBodyMode::Streaming,
            )
            .unwrap(),
            &http2_request,
        );
    }

    #[test]
    fn server_can_start_with_a_transport_selected_protocol() {
        let http1 = ServerConnection::new_with_protocol(HttpProtocol::Http1, HttpLimits::new());
        assert_eq!(http1.protocol(), Some(HttpProtocol::Http1));

        let http2 = ServerConnection::new_with_protocol(HttpProtocol::Http2, HttpLimits::new());
        assert_eq!(http2.protocol(), Some(HttpProtocol::Http2));
    }

    #[test]
    fn server_shutdown_stages_connection_error_goaway_with_last_stream() {
        let request = h2_request(&[]);
        let mut connection = ServerConnection::new(HttpLimits::new());

        assert!(matches!(
            server_step(&mut connection, &request).unwrap(),
            ServerObserved::Head { .. }
        ));
        connection.consume(connection.consumed()).unwrap();
        assert!(matches!(
            server_step(&mut connection, &[]).unwrap(),
            ServerObserved::Write(_)
        ));
        connection.consume(0).unwrap();
        assert_eq!(
            server_step(&mut connection, &[]).unwrap(),
            ServerObserved::Complete {
                exchange_id: ExchangeId(1),
            }
        );
        connection.consume(0).unwrap();

        let oversized_frame_head = [0, 0x40, 1, 0, 0, 0, 0, 0, 0];
        assert_eq!(
            server_step(&mut connection, &oversized_frame_head),
            Err(ServerError::InvalidFrame)
        );
        assert!(connection.begin_shutdown().unwrap());

        let (frame, consumed) = H2Frame::decode(connection.pending_write().unwrap()).unwrap();
        assert_eq!(consumed, 17);
        assert_eq!(frame.frame_type, H2FrameType::Goaway);
        assert_eq!(frame.stream_id, 0);
        assert_eq!(&frame.payload[..4], &1_u32.to_be_bytes());
        assert_eq!(
            &frame.payload[4..],
            &H2ErrorCode::FrameSizeError.as_u32().to_be_bytes()
        );
    }

    /// Drives a connection through one complete request and response so the
    /// stage machine reaches `Done`.
    fn serve_one_exchange(connection: &mut ServerConnection, request: &[u8]) -> ExchangeId {
        let ServerObserved::Head { exchange_id, .. } = server_step(connection, request).unwrap()
        else {
            panic!("request must start with a head");
        };
        connection.consume(connection.consumed()).unwrap();
        loop {
            match server_step(connection, &[]).unwrap() {
                ServerObserved::Complete {
                    exchange_id: completed,
                } if completed == exchange_id => break,
                ServerObserved::Write(_) => connection.consume(0).unwrap(),
                other => panic!("unexpected step while reading the request: {other:?}"),
            }
        }
        connection.consume(0).unwrap();

        connection
            .prepare_response(
                exchange_id,
                ConnectionResponse {
                    status: 200,
                    reason: "OK",
                    headers: &[],
                    body_len: Some(0),
                },
            )
            .unwrap();
        loop {
            match server_step(connection, &[]).unwrap() {
                ServerObserved::Done => break,
                ServerObserved::Write(_) => connection.consume(0).unwrap(),
                other => panic!("unexpected step while responding: {other:?}"),
            }
        }
        exchange_id
    }

    /// HTTP/2 multiplexes, so a finished exchange must leave the connection
    /// able to carry the next one.
    #[test]
    fn server_begins_another_exchange_over_http2() {
        let mut peer = H2Client::default();
        let mut first = peer.connection_preface();
        let (_, commit) = peer
            .open_stream_with_raw_headers("GET", "http", "example.test", "/first", &[], true)
            .unwrap();
        first.extend_from_slice(&take_client_block(&mut peer, commit));

        let mut connection = ServerConnection::new(HttpLimits::new());
        let first_exchange = serve_one_exchange(&mut connection, &first);

        assert!(connection.begin_next_exchange(first_exchange).unwrap());

        let (_, commit) = peer
            .open_stream_with_raw_headers("GET", "http", "example.test", "/second", &[], true)
            .unwrap();
        let second = take_client_block(&mut peer, commit);

        assert!(matches!(
            server_step(&mut connection, &second).unwrap(),
            ServerObserved::Head { .. }
        ));
    }

    /// HTTP/1.1 is persistent by default, so a clean exchange leaves the
    /// decoder ready for the next request.
    #[test]
    fn server_begins_another_exchange_over_http1() {
        let mut connection =
            ServerConnection::new_with_protocol(HttpProtocol::Http1, HttpLimits::new());
        let exchange_id = serve_one_exchange(
            &mut connection,
            b"GET / HTTP/1.1\r\nhost: example.test\r\n\r\n",
        );

        assert!(connection.begin_next_exchange(exchange_id).unwrap());
        assert!(matches!(
            server_step(
                &mut connection,
                b"GET /next HTTP/1.1\r\nhost: example.test\r\n\r\n"
            )
            .unwrap(),
            ServerObserved::Head { target, .. } if target == b"/next"
        ));
    }

    /// A shut-down connection is going away, so it must not accept more work.
    #[test]
    fn server_refuses_another_exchange_after_shutdown() {
        let mut connection = ServerConnection::new(HttpLimits::new());
        let exchange_id = serve_one_exchange(&mut connection, &h2_request(&[]));
        connection.begin_shutdown().unwrap();

        assert!(!connection.begin_next_exchange(exchange_id).unwrap());
    }

    /// An exchange still in flight is not finished, so reuse would drop a
    /// half-written response on the floor.
    #[test]
    fn server_refuses_another_exchange_before_the_current_one_finishes() {
        let mut connection = ServerConnection::new(HttpLimits::new());
        let ServerObserved::Head { exchange_id, .. } =
            server_step(&mut connection, &h2_request(&[])).unwrap()
        else {
            panic!("request must start with a head");
        };

        assert_eq!(
            connection.begin_next_exchange(exchange_id),
            Err(ServerError::InvalidOutboundState)
        );
    }

    /// RST_STREAM cancels one stream, not the connection, so the driver has to
    /// offer a way back to a reusable state.
    #[test]
    fn server_recovers_from_a_peer_reset_exchange() {
        let mut connection = ServerConnection::new(HttpLimits::new());
        let mut peer = H2Client::default();
        let mut input = peer.connection_preface();
        let (stream_id, commit) = peer
            .open_stream_with_raw_headers("GET", "http", "example.test", "/", &[], true)
            .unwrap();
        input.extend_from_slice(&take_client_block(&mut peer, commit));
        H2Frame {
            frame_type: H2FrameType::RstStream,
            flags: 0,
            stream_id,
            payload: H2ErrorCode::Cancel.as_u32().to_be_bytes().to_vec(),
        }
        .encode(&mut input);

        let mut offset = 0;
        loop {
            match server_step(&mut connection, &input[offset..]) {
                Err(ServerError::PeerReset { .. }) => {
                    let consumed = connection.consumed();
                    connection.consume(consumed).unwrap();
                    offset += consumed;
                    break;
                }
                Ok(observed) => {
                    let consumed = connection.consumed();
                    connection.consume(consumed).unwrap();
                    offset += consumed;
                    assert!(
                        !matches!(observed, ServerObserved::NeedInput),
                        "the reset must arrive before the driver runs out of input"
                    );
                }
                other => panic!("expected a peer reset, got {other:?}"),
            }
        }
        assert_eq!(offset, input.len());

        let reset_exchange = connection.cancel_exchange().unwrap().unwrap();
        assert!(connection.begin_next_exchange(reset_exchange).unwrap());

        let (_, commit) = peer
            .open_stream_with_raw_headers("GET", "http", "example.test", "/after", &[], true)
            .unwrap();
        let second = take_client_block(&mut peer, commit);

        assert!(matches!(
            server_step(&mut connection, &second).unwrap(),
            ServerObserved::Head { .. }
        ));
    }

    #[test]
    fn server_routes_interleaved_http2_request_bodies_by_exchange() {
        let mut peer = H2Client::default();
        let mut input = peer.connection_preface();
        let headers = [H2HeaderField::new(b"content-length", b"2")];
        let (first_stream, first_headers) = peer
            .open_stream_with_raw_headers("POST", "http", "example.test", "/first", &headers, false)
            .unwrap();
        input.extend_from_slice(&take_client_block(&mut peer, first_headers));
        let (second_stream, second_headers) = peer
            .open_stream_with_raw_headers(
                "POST",
                "http",
                "example.test",
                "/second",
                &headers,
                false,
            )
            .unwrap();
        input.extend_from_slice(&take_client_block(&mut peer, second_headers));
        input.extend_from_slice(&peer.data_frame(first_stream, b"a", false));
        input.extend_from_slice(&peer.data_frame(second_stream, b"b", false));
        input.extend_from_slice(&peer.data_frame(first_stream, b"c", true));
        input.extend_from_slice(&peer.data_frame(second_stream, b"d", true));

        let mut connection = ServerConnection::new(HttpLimits::new());
        let (events, consumed) = collect_request_events(&mut connection, &input, 2);

        assert_eq!(consumed, input.len());
        assert_eq!(
            events,
            vec![
                ServerObserved::Head {
                    exchange_id: ExchangeId::http2(first_stream),
                    method: b"POST".to_vec(),
                    target: b"/first".to_vec(),
                    version: HttpVersion::Http2,
                },
                ServerObserved::Head {
                    exchange_id: ExchangeId::http2(second_stream),
                    method: b"POST".to_vec(),
                    target: b"/second".to_vec(),
                    version: HttpVersion::Http2,
                },
                ServerObserved::Body {
                    exchange_id: ExchangeId::http2(first_stream),
                    chunk: b"a".to_vec(),
                },
                ServerObserved::Body {
                    exchange_id: ExchangeId::http2(second_stream),
                    chunk: b"b".to_vec(),
                },
                ServerObserved::Body {
                    exchange_id: ExchangeId::http2(first_stream),
                    chunk: b"c".to_vec(),
                },
                ServerObserved::Complete {
                    exchange_id: ExchangeId::http2(first_stream),
                },
                ServerObserved::Body {
                    exchange_id: ExchangeId::http2(second_stream),
                    chunk: b"d".to_vec(),
                },
                ServerObserved::Complete {
                    exchange_id: ExchangeId::http2(second_stream),
                },
            ]
        );
    }

    #[test]
    fn server_schedules_concurrent_http2_response_data_fairly() {
        let mut peer = H2Client::default();
        let mut input = peer.connection_preface();
        let (first_stream, first_headers) = peer
            .open_stream_with_raw_headers("GET", "http", "example.test", "/first", &[], true)
            .unwrap();
        input.extend_from_slice(&take_client_block(&mut peer, first_headers));
        let (second_stream, second_headers) = peer
            .open_stream_with_raw_headers("GET", "http", "example.test", "/second", &[], true)
            .unwrap();
        input.extend_from_slice(&take_client_block(&mut peer, second_headers));

        let first = ExchangeId::http2(first_stream);
        let second = ExchangeId::http2(second_stream);
        let mut connection = ServerConnection::new(HttpLimits::new());
        let (_, consumed) = collect_request_events(&mut connection, &input, 2);
        assert_eq!(consumed, input.len());

        for exchange_id in [second, first] {
            assert!(
                connection
                    .prepare_response(
                        exchange_id,
                        ConnectionResponse {
                            status: 200,
                            reason: "OK",
                            headers: &[],
                            body_len: Some(20_000),
                        },
                    )
                    .unwrap()
            );
        }
        for expected_stream in [second_stream, first_stream] {
            let ServerObserved::Write(bytes) = server_step(&mut connection, &[]).unwrap() else {
                panic!("queued response headers must be written first");
            };
            let (frame, _) = H2Frame::decode(&bytes).unwrap();
            assert_eq!(frame.frame_type, H2FrameType::Headers);
            assert_eq!(frame.stream_id, expected_stream);
            connection.consume(0).unwrap();
        }

        assert!(!connection.prepare_body_chunk(first, 20_000).unwrap());
        assert!(connection.prepare_body_chunk(second, 20_000).unwrap());
        assert!(connection.body_chunk(first).is_none());
        assert_eq!(connection.body_chunk(second).unwrap().payload_len(), 16_384);
        connection.commit_body_chunk(second).unwrap();

        assert!(!connection.prepare_body_chunk(second, 3_616).unwrap());
        assert!(connection.prepare_body_chunk(first, 20_000).unwrap());
        assert_eq!(connection.body_chunk(first).unwrap().payload_len(), 16_384);
        connection.commit_body_chunk(first).unwrap();

        assert!(!connection.prepare_body_chunk(first, 3_616).unwrap());
        assert!(connection.prepare_body_chunk(second, 3_616).unwrap());
        assert_eq!(connection.body_chunk(second).unwrap().payload_len(), 3_616);
        connection.commit_body_chunk(second).unwrap();

        assert!(connection.prepare_body_chunk(first, 3_616).unwrap());
        assert_eq!(connection.body_chunk(first).unwrap().payload_len(), 3_616);
        connection.commit_body_chunk(first).unwrap();

        assert_eq!(
            server_step(&mut connection, &[]).unwrap(),
            ServerObserved::Done
        );
    }

    #[test]
    fn server_reset_cancels_only_its_http2_exchange() {
        let mut peer = H2Client::default();
        let mut input = peer.connection_preface();
        let (first_stream, first_headers) = peer
            .open_stream_with_raw_headers("GET", "http", "example.test", "/first", &[], true)
            .unwrap();
        input.extend_from_slice(&take_client_block(&mut peer, first_headers));
        let (second_stream, second_headers) = peer
            .open_stream_with_raw_headers("GET", "http", "example.test", "/second", &[], true)
            .unwrap();
        input.extend_from_slice(&take_client_block(&mut peer, second_headers));
        H2Frame {
            frame_type: H2FrameType::RstStream,
            flags: 0,
            stream_id: first_stream,
            payload: H2ErrorCode::Cancel.as_u32().to_be_bytes().to_vec(),
        }
        .encode(&mut input);

        let mut connection = ServerConnection::new(HttpLimits::new());
        let (_, offset) = collect_request_events(&mut connection, &input, 2);
        assert_eq!(
            server_step(&mut connection, &input[offset..]),
            Err(ServerError::PeerReset {
                stream_id: first_stream,
                error_code: H2ErrorCode::Cancel.as_u32(),
            })
        );
        let consumed = connection.consumed();
        assert_eq!(consumed, input.len() - offset);
        connection.consume(consumed).unwrap();
        let reset_exchange = connection.cancel_exchange().unwrap().unwrap();
        assert!(connection.begin_next_exchange(reset_exchange).unwrap());

        let empty_response = ConnectionResponse {
            status: 204,
            reason: "No Content",
            headers: &[],
            body_len: Some(0),
        };
        assert_eq!(
            connection.prepare_response(ExchangeId::http2(first_stream), empty_response),
            Err(ServerError::InvalidOutboundState)
        );
        assert!(
            !connection
                .prepare_response(ExchangeId::http2(second_stream), empty_response)
                .unwrap()
        );
        assert!(matches!(
            server_step(&mut connection, &[]).unwrap(),
            ServerObserved::Write(_)
        ));
        connection.consume(0).unwrap();
        assert_eq!(
            server_step(&mut connection, &[]).unwrap(),
            ServerObserved::Done
        );
    }

    #[test]
    fn server_handles_stream_error_without_connection_shutdown() {
        let mut peer = H2Client::default();
        let mut input = peer.connection_preface();
        H2Frame {
            frame_type: H2FrameType::Priority,
            flags: 0,
            stream_id: 1,
            payload: [1_u32.to_be_bytes().as_slice(), &[0]].concat(),
        }
        .encode(&mut input);
        let mut connection =
            ServerConnection::new_with_protocol(HttpProtocol::Http2, HttpLimits::new());

        let ServerObserved::Write(output) = server_step(&mut connection, &input).unwrap() else {
            panic!("stream error must stage RST_STREAM");
        };
        let mut offset = 0;
        let mut reset = None;
        while offset < output.len() {
            let (frame, consumed) = H2Frame::decode(&output[offset..]).unwrap();
            offset += consumed;
            if frame.frame_type == H2FrameType::RstStream {
                reset = Some(frame);
            }
        }
        let reset = reset.expect("stream error reset");
        assert_eq!(reset.stream_id, 1);
        assert_eq!(
            reset.payload,
            H2ErrorCode::ProtocolError.as_u32().to_be_bytes()
        );
        assert_eq!(connection.terminal_h2_error, None);
    }

    #[test]
    fn server_shutdown_ignores_http1() {
        let mut connection =
            ServerConnection::new_with_protocol(HttpProtocol::Http1, HttpLimits::new());

        assert!(!connection.begin_shutdown().unwrap());
        assert!(!connection.begin_shutdown().unwrap());
        assert_eq!(connection.pending_write(), None);
    }

    #[test]
    fn server_shutdown_without_error_stages_idempotent_no_error_goaway() {
        let mut connection =
            ServerConnection::new_with_protocol(HttpProtocol::Http2, HttpLimits::new());

        assert!(connection.begin_shutdown().unwrap());
        let first = connection.pending_write().unwrap().to_vec();
        assert!(!connection.begin_shutdown().unwrap());
        assert_eq!(connection.pending_write(), Some(first.as_slice()));

        let (frame, consumed) = H2Frame::decode(&first).unwrap();
        assert_eq!(consumed, 17);
        assert_eq!(frame.frame_type, H2FrameType::Goaway);
        assert_eq!(frame.stream_id, 0);
        assert_eq!(&frame.payload[..4], &0_u32.to_be_bytes());
        assert_eq!(
            &frame.payload[4..],
            &H2ErrorCode::NoError.as_u32().to_be_bytes()
        );
    }

    #[test]
    fn server_detects_http1_and_emits_ordered_events() {
        let mut connection = ServerConnection::new(HttpLimits::new());
        let input = b"POST /submit HTTP/1.1\r\nhost: example.test\r\ncontent-length: 3\r\n\r\nabc";
        let mut offset = 0;

        assert_eq!(
            server_step(&mut connection, &input[offset..]).unwrap(),
            ServerObserved::Head {
                exchange_id: ExchangeId(1),
                method: b"POST".to_vec(),
                target: b"/submit".to_vec(),
                version: HttpVersion::Http11,
            }
        );
        assert_eq!(connection.protocol(), Some(HttpProtocol::Http1));
        offset += connection.consumed();
        connection.consume(connection.consumed()).unwrap();

        assert_eq!(
            server_step(&mut connection, &input[offset..]).unwrap(),
            ServerObserved::Body {
                exchange_id: ExchangeId(1),
                chunk: b"abc".to_vec(),
            }
        );
        offset += connection.consumed();
        connection.consume(connection.consumed()).unwrap();

        assert_eq!(
            server_step(&mut connection, &input[offset..]).unwrap(),
            ServerObserved::Complete {
                exchange_id: ExchangeId(1),
            }
        );
        connection.consume(connection.consumed()).unwrap();
        assert_eq!(offset, input.len());
    }

    #[test]
    fn server_detects_split_http2_preface_emits_control_and_ordered_events() {
        let bytes = h2_request(b"abc");
        let mut connection = ServerConnection::new(HttpLimits::new());

        assert_eq!(
            server_step(&mut connection, &bytes[..3]).unwrap(),
            ServerObserved::NeedInput
        );
        assert_eq!(connection.protocol(), None);
        connection.consume(0).unwrap();

        let mut offset = 0;
        assert_eq!(
            server_step(&mut connection, &bytes[offset..]).unwrap(),
            ServerObserved::Head {
                exchange_id: ExchangeId(1),
                method: b"POST".to_vec(),
                target: b"/submit".to_vec(),
                version: HttpVersion::Http2,
            }
        );
        assert_eq!(connection.protocol(), Some(HttpProtocol::Http2));
        offset += connection.consumed();
        connection.consume(connection.consumed()).unwrap();

        let ServerObserved::Write(control) =
            server_step(&mut connection, &bytes[offset..]).unwrap()
        else {
            panic!("HTTP/2 settings must surface a control write");
        };
        assert!(!control.is_empty());
        connection.consume(0).unwrap();

        assert_eq!(
            server_step(&mut connection, &bytes[offset..]).unwrap(),
            ServerObserved::Body {
                exchange_id: ExchangeId(1),
                chunk: b"abc".to_vec(),
            }
        );
        offset += connection.consumed();
        connection.consume(connection.consumed()).unwrap();

        assert_eq!(
            server_step(&mut connection, &bytes[offset..]).unwrap(),
            ServerObserved::Complete {
                exchange_id: ExchangeId(1),
            }
        );
        connection.consume(0).unwrap();
        assert_eq!(offset, bytes.len());
    }

    #[test]
    fn server_classifies_peer_reset_and_goaway() {
        let request = h2_open_request();
        let mut reset = ServerConnection::new(HttpLimits::new());
        assert!(matches!(
            server_step(&mut reset, &request).unwrap(),
            ServerObserved::Head { .. }
        ));
        let consumed = reset.consumed();
        reset.consume(consumed).unwrap();
        assert!(matches!(
            server_step(&mut reset, &request[consumed..]).unwrap(),
            ServerObserved::Write(_)
        ));
        reset.consume(0).unwrap();
        let reset_frame = [0, 0, 4, 3, 0, 0, 0, 0, 1, 0, 0, 0, 8];
        let error = server_step(&mut reset, &reset_frame).unwrap_err();
        assert_eq!(error.classify().kind(), HttpErrorKind::PeerReset);

        let mut peer = H2Client::default();
        let mut goaway_input = peer.connection_preface();
        goaway_input.extend_from_slice(&[
            0, 0, 8, 7, 0, 0, 0, 0, 0, // frame head
            0, 0, 0, 0, // last stream ID
            0, 0, 0, 0, // NO_ERROR
        ]);
        let mut goaway = ServerConnection::new(HttpLimits::new());
        let error = server_step(&mut goaway, &goaway_input).unwrap_err();
        assert_eq!(error.classify().kind(), HttpErrorKind::PeerGoaway);
    }

    #[derive(Debug, Eq, PartialEq)]
    enum ClientObserved {
        NeedInput,
        Write(Vec<u8>),
        Head {
            exchange_id: ExchangeId,
            status: u16,
            version: HttpVersion,
        },
        Body {
            exchange_id: ExchangeId,
            bytes: Vec<u8>,
        },
        Complete {
            exchange_id: ExchangeId,
        },
        Done,
    }

    fn client_step(
        connection: &mut ClientConnection,
        input: &[u8],
    ) -> Result<ClientObserved, ServerError> {
        connection.step(input, |step| match step {
            Step::NeedInput => ClientObserved::NeedInput,
            Step::Write(bytes) => ClientObserved::Write(bytes.to_vec()),
            Step::Event(ClientEvent::ResponseHead {
                exchange_id,
                status,
                version,
                ..
            }) => ClientObserved::Head {
                exchange_id,
                status,
                version,
            },
            Step::Event(ClientEvent::ResponseBody { exchange_id, chunk }) => ClientObserved::Body {
                exchange_id,
                bytes: chunk.to_vec(),
            },
            Step::Event(ClientEvent::ResponseTrailers { .. }) => {
                panic!("unexpected response trailers")
            }
            Step::Event(ClientEvent::ResponseComplete { exchange_id }) => {
                ClientObserved::Complete { exchange_id }
            }
            Step::Done => ClientObserved::Done,
        })
    }

    fn prepare_get(connection: &mut ClientConnection) -> ExchangeId {
        let headers = [HeaderRef::new(b"content-length", b"0")];
        connection
            .prepare_request(ClientRequest {
                method: "GET",
                scheme: "http",
                authority: "example.test",
                target: "/",
                headers: &headers,
                body_len: Some(0),
            })
            .unwrap()
    }

    fn drain_client_output(connection: &mut ClientConnection) -> Vec<u8> {
        let mut output = Vec::new();
        loop {
            match client_step(connection, &[]).unwrap() {
                ClientObserved::Write(bytes) => {
                    output.extend_from_slice(&bytes);
                    connection.consume(connection.consumed()).unwrap();
                }
                ClientObserved::NeedInput => {
                    connection.consume(connection.consumed()).unwrap();
                    return output;
                }
                event => panic!("unexpected output-drain event: {event:?}"),
            }
        }
    }

    #[test]
    fn client_drives_complete_http1_exchange() {
        let mut connection = ClientConnection::new(HttpProtocol::Http1, HttpLimits::new());
        let exchange_id = prepare_get(&mut connection);
        let request = drain_client_output(&mut connection);
        assert!(request.starts_with(b"GET / HTTP/1.1\r\n"));

        let response = b"HTTP/1.1 200 OK\r\ncontent-length: 3\r\n\r\nabc";
        let mut offset = 0;
        assert_eq!(
            client_step(&mut connection, &response[offset..]).unwrap(),
            ClientObserved::Head {
                exchange_id,
                status: 200,
                version: HttpVersion::Http11,
            }
        );
        offset += connection.consumed();
        connection.consume(connection.consumed()).unwrap();
        assert_eq!(
            client_step(&mut connection, &response[offset..]).unwrap(),
            ClientObserved::Body {
                exchange_id,
                bytes: b"abc".to_vec(),
            }
        );
        offset += connection.consumed();
        connection.consume(connection.consumed()).unwrap();
        assert_eq!(
            client_step(&mut connection, &response[offset..]).unwrap(),
            ClientObserved::Complete { exchange_id }
        );
        connection.consume(0).unwrap();
        assert_eq!(offset, response.len());
    }

    #[test]
    fn client_drives_complete_http2_exchange() {
        let mut connection = ClientConnection::new(HttpProtocol::Http2, HttpLimits::new());
        let exchange_id = prepare_get(&mut connection);
        let request = drain_client_output(&mut connection);

        let mut server = H2Server::default();
        let mut request_offset = 0;
        let mut stream_id = None;
        let mut response = Vec::new();
        while request_offset < request.len() {
            let (event, consumed, control) = server
                .accept_event_bytes(&request[request_offset..])
                .unwrap();
            request_offset += consumed;
            response.extend_from_slice(&control);
            if let Some(H2ByteStreamEvent::RequestHeaders {
                stream_id: observed,
                ..
            }) = event
            {
                stream_id = Some(observed);
                break;
            }
            assert_ne!(consumed, 0);
        }
        let stream_id = stream_id.unwrap();
        let fields = [H2HeaderField::new(b"content-length", b"3")];
        let commit = server
            .response_headers_frame_with_raw_headers(stream_id, 200, &fields, false)
            .unwrap();
        let block = server.next_outbound_block().unwrap();
        assert_eq!(block.commit(), commit);
        response.extend_from_slice(block.bytes());
        server.acknowledge_outbound_block(commit).unwrap();
        response.extend_from_slice(&server.data_frame(stream_id, b"abc", true));

        let mut offset = 0;
        let mut observed = Vec::new();
        while !observed.contains(&ClientObserved::Complete { exchange_id }) {
            let event = client_step(&mut connection, &response[offset..]).unwrap();
            let consumed = connection.consumed();
            if matches!(event, ClientObserved::Write(_)) {
                assert!(!connection.pending_write().unwrap().is_empty());
            } else {
                observed.push(event);
            }
            connection.consume(consumed).unwrap();
            offset += consumed;
        }
        assert!(observed.contains(&ClientObserved::Head {
            exchange_id,
            status: 200,
            version: HttpVersion::Http2,
        }));
        assert!(observed.contains(&ClientObserved::Body {
            exchange_id,
            bytes: b"abc".to_vec(),
        }));
    }

    #[test]
    fn client_resets_one_abandoned_http2_exchange_and_keeps_its_sibling() {
        let mut connection = ClientConnection::new(HttpProtocol::Http2, HttpLimits::new());
        let first = prepare_get(&mut connection);
        let second = prepare_get(&mut connection);
        assert!(connection.abandon_exchange(first).unwrap());
        let request = drain_client_output(&mut connection);

        let mut server = H2Server::default();
        let mut offset = 0;
        let mut request_streams = Vec::new();
        let mut reset_stream = None;
        while offset < request.len() {
            let (event, consumed, _) = server.accept_event_bytes(&request[offset..]).unwrap();
            assert_ne!(consumed, 0);
            offset += consumed;
            match event {
                Some(H2ByteStreamEvent::RequestHeaders { stream_id, .. }) => {
                    request_streams.push(stream_id);
                }
                Some(H2ByteStreamEvent::Reset { stream_id, .. }) => {
                    reset_stream = Some(stream_id);
                }
                _ => {}
            }
        }
        assert_eq!(request_streams.len(), 2);
        assert_eq!(reset_stream, Some(first.as_u64() as u32));
        assert!(connection.exchange_is_retireable(first));
        connection.begin_next_exchange(first).unwrap();
        assert!(!connection.exchange_is_retireable(second));

        let third = prepare_get(&mut connection);
        assert_ne!(third, second);
    }

    fn prepared_h2_client() -> ClientConnection {
        let mut connection = ClientConnection::new(HttpProtocol::Http2, HttpLimits::new());
        let _ = prepare_get(&mut connection);
        let _ = drain_client_output(&mut connection);
        let settings = [0, 0, 0, 4, 0, 0, 0, 0, 0];
        assert!(matches!(
            client_step(&mut connection, &settings).unwrap(),
            ClientObserved::Write(_)
        ));
        connection.consume(connection.consumed()).unwrap();
        connection
    }

    #[test]
    fn client_classifies_peer_reset_and_goaway() {
        let mut reset = prepared_h2_client();
        let reset_frame = [0, 0, 4, 3, 0, 0, 0, 0, 1, 0, 0, 0, 8];
        let error = client_step(&mut reset, &reset_frame).unwrap_err();
        assert_eq!(error.classify().kind(), HttpErrorKind::PeerReset);

        let mut goaway = prepared_h2_client();
        let goaway_frame = [
            0, 0, 8, 7, 0, 0, 0, 0, 0, // frame head
            0, 0, 0, 1, // last stream ID
            0, 0, 0, 0, // NO_ERROR
        ];
        let error = client_step(&mut goaway, &goaway_frame).unwrap_err();
        assert_eq!(error.classify().kind(), HttpErrorKind::PeerGoaway);
    }

    #[test]
    fn client_attributes_partial_http2_frame_to_exchange() {
        let mut connection = prepared_h2_client();
        let partial_data_frame = [0, 0, 3, 0, 0, 0, 0, 0, 1];

        assert_eq!(
            client_step(&mut connection, &partial_data_frame).unwrap(),
            ClientObserved::NeedInput
        );
        assert_eq!(connection.input_exchange(), Some(ExchangeId(1)));
    }
}
