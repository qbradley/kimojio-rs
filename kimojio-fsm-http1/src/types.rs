//! HTTP/1 message metadata, identities, outcomes, limits, and timer values.
//!
//! Metadata borrows caller-owned header storage. The connection validates and
//! serializes it when a command is accepted; keep it alive through that call.
use crate::Buffer;
use std::ops::Range;

/// One parsed HTTP field, re-exported from `httparse` for the public API.
pub use httparse::Header;
/// A borrowed list of HTTP fields whose names and values live for `'a`.
pub type Headers<'a> = &'a [Header<'a>];

/// Identifies one logical connection, including its generation.
///
/// Allocate IDs so a late completion from an earlier connection cannot match
/// a newer one. Do not reuse an ID while any operation or completion can remain.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ConnectionId {
    /// Caller-selected slot, commonly an index into a connection table.
    pub slot: u64,
    /// Incremented whenever the slot is assigned to a new connection.
    pub generation: u64,
}

/// Identifies one request/response exchange within a connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ExchangeId {
    pub(crate) connection: ConnectionId,
    pub(crate) sequence: u64,
}

impl ExchangeId {
    /// Returns the connection that owns this exchange.
    pub fn connection(self) -> ConnectionId {
        self.connection
    }
    /// Returns the monotonically assigned sequence within that connection.
    pub fn sequence(self) -> u64 {
        self.sequence
    }
}

/// Kind of asynchronous operation represented by an [`OperationId`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum OperationKind {
    /// Read bytes from the transport into core-owned receive storage.
    Read,
    /// Write the remaining encoded bytes to the transport.
    Write,
    /// Wait until the transport can be read without blocking.
    Readable,
    /// Wait until the transport can be written without blocking.
    Writable,
    /// Deliver received body bytes to the application for consumption.
    Body,
    /// Close the transport after outstanding ownership has settled.
    Close,
}

/// Unique token tying an issued operation to its eventual completion.
///
/// Return the same token from the operation object; do not manufacture or reuse
/// tokens. The core rejects stale, mismatched, or duplicate completions.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct OperationId {
    pub(crate) connection: ConnectionId,
    pub(crate) sequence: u64,
    pub(crate) kind: OperationKind,
}

impl OperationId {
    /// Returns the connection that issued this operation.
    pub fn connection(self) -> ConnectionId {
        self.connection
    }
    /// Returns this operation's sequence number within the connection.
    pub fn sequence(self) -> u64 {
        self.sequence
    }
    /// Returns the operation's lane and completion category.
    pub fn kind(self) -> OperationKind {
        self.kind
    }
}

/// Identifies one producer body submission until its receipt is returned.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct BodyId {
    pub(crate) connection: ConnectionId,
    pub(crate) sequence: u64,
}

/// HTTP version supported by the HTTP/1 connection machine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Version {
    /// HTTP/1.0 persistence defaults to closing unless explicitly enabled.
    Http10,
    /// HTTP/1.1 persistence defaults to keeping the connection alive.
    Http11,
}

/// Borrowed request-line and header metadata parsed from or sent to a peer.
#[derive(Clone, Copy, Debug)]
pub struct RequestHead<'a> {
    /// Method token, such as `GET` or `POST`.
    pub method: &'a str,
    /// Request target, such as `/items` or an absolute URI.
    pub target: &'a str,
    /// HTTP version used for this message.
    pub version: Version,
    /// Borrowed header fields; the core validates framing-sensitive fields.
    pub headers: Headers<'a>,
}

/// Borrowed status-line and header metadata for a response.
#[derive(Clone, Copy, Debug)]
pub struct ResponseHead<'a> {
    /// Version selected for the wire response; servers normally use the request version.
    pub version: Version,
    /// Three-digit HTTP status code.
    pub status: u16,
    /// Borrowed reason phrase (may be empty).
    pub reason: &'a str,
    /// Borrowed response fields.
    pub headers: Headers<'a>,
}

impl<'a> ResponseHead<'a> {
    /// Constructs outgoing response metadata using HTTP/1.1 by default.
    ///
    /// A server machine selects the actual wire version from the request.
    /// Header names and values are validated when the response is accepted.
    pub fn new(status: u16, reason: &'a str, headers: Headers<'a>) -> Self {
        Self {
            version: Version::Http11,
            status,
            reason,
            headers,
        }
    }
}

/// Framing expectation for a message body.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BodyLength {
    /// The message has no payload.
    Empty,
    /// Exactly this many payload bytes are sent or received.
    Known(u64),
    /// Length is not known in advance; HTTP/1.1 uses chunked framing.
    Streaming,
}

/// Client request command, including framing and `100-continue` policy.
#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
    /// Request line and headers.
    pub head: RequestHead<'a>,
    /// Declared request-body framing.
    pub body: BodyLength,
    /// Whether the body waits for `100 Continue` or its configured fallback.
    pub expect_continue: bool,
}

/// Final response command with its declared body framing.
#[derive(Clone, Copy, Debug)]
pub struct Response<'a> {
    /// Status line and response fields.
    pub head: ResponseHead<'a>,
    /// Declared response-body framing.
    pub body: BodyLength,
}

impl<'a> Response<'a> {
    /// Builds a response with an HTTP/1.1 default version and explicit framing.
    pub fn new(status: u16, reason: &'a str, headers: Headers<'a>, body: BodyLength) -> Self {
        Self {
            head: ResponseHead::new(status, reason, headers),
            body,
        }
    }
}

/// A non-final response head, typically status `100` or `103`.
pub type InformationalResponse<'a> = ResponseHead<'a>;
/// Successful protocol-switch response head; accepted upgrades transfer the transport.
pub type UpgradeResponse<'a> = ResponseHead<'a>;

/// Producer payload offered for an exchange's outgoing body.
#[derive(Debug)]
pub struct SendBody<B> {
    /// Exchange that requested producer data.
    pub exchange: ExchangeId,
    /// Owned storage retained until a [`BodySent`] receipt is delivered.
    pub buffer: B,
    /// Payload bytes within `buffer` to send.
    pub range: Range<usize>,
    /// Marks this payload as the final producer frame.
    pub end: bool,
}

/// Why a command or completion was rejected without consuming its value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RejectReason {
    /// The token belongs to another connection.
    WrongConnection,
    /// The token refers to an operation or exchange that has already retired.
    Stale,
    /// The token's operation kind does not match the expected callback.
    WrongKind,
    /// A reported byte count exceeds the operation's remaining bytes.
    InvalidCount,
    /// The requested buffer range is malformed or out of bounds.
    InvalidRange,
    /// The connection has no producer or receive capacity for this command.
    NoCapacity,
    /// A configured byte, message, or count limit would be exceeded.
    Limit,
    /// The command is not legal in the connection's current protocol state.
    InvalidState,
}

/// Rejected input returned intact so the caller can inspect or recover it.
#[derive(Debug)]
pub struct Rejected<T> {
    /// Reason admission failed.
    pub reason: RejectReason,
    /// Original command or completion, not consumed by the machine.
    pub value: T,
}

/// Error returned synchronously when a command cannot be applied.
///
/// Unlike [`Failure`], this does not necessarily fail the connection. A command
/// error is returned immediately and leaves protocol state unchanged unless noted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandError {
    /// Configuration contains a zero or inconsistent required limit.
    InvalidConfig,
    /// This command is not permitted in the current connection state.
    InvalidState,
    /// The exchange has retired or belongs to another connection.
    StaleExchange,
    /// Request or response metadata is syntactically invalid.
    InvalidHead,
    /// Declared body length conflicts with HTTP framing fields or status semantics.
    InvalidFraming,
    /// Accepting the command would exceed a configured resource limit.
    Limit,
    /// An operation, exchange, or deadline sequence cannot advance without wrapping.
    SequenceExhausted,
    /// Caller-supplied monotonic time moved backwards.
    TimeRegression,
    /// Deadline token is no longer the currently armed deadline.
    StaleDeadline,
    /// The supplied time precedes the currently armed deadline.
    EarlyDeadline,
    /// Requested protocol action is not currently ready.
    NotReady,
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for CommandError {}

/// Transport error class controlling retry, cancellation, and progress accounting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IoErrorKind {
    /// No bytes were accepted. The core waits for readiness before retrying.
    WouldBlock,
    /// No bytes were accepted. The core can retry the operation.
    Interrupted,
    /// Operation was canceled before it accepted any bytes.
    Cancelled,
    /// A cancelled write-all operation with an unknown accepted prefix.
    ///
    /// Unlike `UnknownProgress`, this result confirms cancellation rather than
    /// transport failure. An early final response can still finish before close.
    CancelledUnknownProgress,
    /// Peer reset or otherwise forcibly terminated the transport.
    Reset,
    /// Fatal error from an operation that can hide partial accepted bytes.
    ///
    /// The core never retries this operation. This variant is required for
    /// write-all transports that cannot report progress before an error.
    UnknownProgress,
    /// An error not covered by the more specific transport classifications.
    Other,
}

/// Transport completion error supplied by the executor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoError {
    /// Semantics used by the protocol machine to retry or terminate the operation.
    pub kind: IoErrorKind,
    /// Optional platform error code for diagnostics.
    pub code: Option<i32>,
}
/// Result type for transport operation completions.
pub type IoResult<T> = Result<T, IoError>;

/// Terminal reason reported for an exchange or connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Failure {
    /// Malformed, unsupported, or otherwise invalid HTTP/1 wire data.
    Protocol,
    /// The peer did not authorize a request body gated by `Expect: 100-continue`.
    ExpectationFailed,
    /// A configured head, body, metadata, or request-count limit was exceeded.
    Limit,
    /// The peer closed before the current message was complete.
    UnexpectedEof,
    /// A transport write reported success without making progress.
    WriteZero,
    /// An operation failed at the transport layer.
    Transport(IoError),
    /// The application or executor canceled the exchange.
    Cancelled,
    /// Application callback or body producer failed.
    Application,
    /// The peer's final response stopped an unfinished request body.
    EarlyResponse,
    /// A protocol deadline expired.
    Timeout,
    /// A sequence space was exhausted before the connection could continue safely.
    SequenceExhausted,
}

/// Terminal result delivered when the connection has fully closed.
pub type ConnectionResult = Result<(), Failure>;

/// Final outcome of one exchange, delivered after its owned work settles.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExchangeFinished {
    /// Exchange that has retired.
    pub exchange: ExchangeId,
    /// Success or the reason this exchange failed.
    pub result: Result<(), Failure>,
    /// Whether the connection can accept another exchange after this result.
    pub reusable: bool,
}

/// Receipt returning producer storage after its body frame settles.
#[derive(Debug)]
pub struct BodySent<B> {
    /// Exchange that owned the outgoing body.
    pub exchange: ExchangeId,
    /// Submission identity supplied in the corresponding `SendBody`.
    pub id: BodyId,
    /// Original producer allocation, returned without copying.
    pub buffer: B,
    /// Payload bytes positively reported by transport, excluding chunk framing.
    ///
    /// `acceptance` states whether this count is exact or a lower bound.
    /// Payload bytes positively confirmed by transport, excluding framing.
    pub accepted: usize,
    /// Whether `accepted` is exact or only a lower bound.
    pub acceptance: Acceptance,
    /// Success or failure of this producer submission.
    pub result: Result<(), Failure>,
}

/// Precision of the payload-byte acceptance count in a [`BodySent`] receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Acceptance {
    /// Transport progress is known exactly.
    Exact,
    /// More bytes can have reached the transport than the confirmed count.
    LowerBound,
}

/// How to stop accepting work on a connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownMode {
    /// Finish permitted work and close once protocol and operations settle.
    Graceful,
    /// Cancel active work and close as soon as owned operations settle.
    Abort,
}

/// Nanoseconds in one caller-supplied monotonic domain.
/// Monotonic nanosecond timestamp supplied by the caller.
///
/// All timestamps for a connection must use the same clock origin. The core
/// never reads a clock; drive methods compare these values to armed deadlines.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd)]
pub struct Tick(pub u64);

/// Versioned deadline token emitted by `deadline_changed`.
///
/// Pass it back with [`crate::Client::expire`] or [`crate::Server::expire`];
/// obsolete tokens are rejected instead of expiring a newer timer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Deadline {
    pub(crate) connection: ConnectionId,
    pub(crate) sequence: u64,
    /// Absolute expiration time in the connection's caller-supplied clock domain.
    pub at: Tick,
}

/// Per-connection protocol limits and timeout policy.
///
/// Start from [`Default`] and override limits to fit the application. Set a
/// timeout to `None` to disable that phase's deadline.
#[derive(Clone, Debug)]
pub struct Config {
    /// Maximum bytes in a start line and header section.
    pub max_head_bytes: usize,
    /// Maximum number of parsed or generated header fields.
    pub max_headers: usize,
    /// Maximum receive buffer, delivered body allocation, or producer frame size.
    pub max_buffer_bytes: usize,
    /// Maximum total payload bytes in one body.
    pub max_body_bytes: u64,
    /// Maximum bytes in one chunk-size line, including extensions.
    pub max_chunk_line_bytes: usize,
    /// Maximum cumulative chunk metadata, including trailers.
    pub max_chunk_metadata_bytes: usize,
    /// Maximum informational responses accepted for one exchange.
    pub max_informational_responses: usize,
    /// Maximum requests accepted on one connection.
    pub max_requests: u64,
    /// Nanoseconds allowed to receive a message head.
    pub head_timeout_ns: Option<u64>,
    /// Nanoseconds an idle keep-alive connection may remain open.
    pub idle_timeout_ns: Option<u64>,
    /// Nanoseconds allowed for body progress.
    pub body_timeout_ns: Option<u64>,
    /// Nanoseconds to wait for `100 Continue` before applying fallback policy.
    pub continue_timeout_ns: Option<u64>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_head_bytes: 16 * 1024,
            max_headers: 128,
            max_buffer_bytes: 64 * 1024,
            max_body_bytes: 1024 * 1024 * 1024,
            max_chunk_line_bytes: 1024,
            max_chunk_metadata_bytes: 1024 * 1024,
            max_informational_responses: 16,
            max_requests: 1000,
            head_timeout_ns: Some(30_000_000_000),
            idle_timeout_ns: Some(60_000_000_000),
            body_timeout_ns: Some(30_000_000_000),
            continue_timeout_ns: Some(1_000_000_000),
        }
    }
}

/// Bytes already read beyond an HTTP upgrade boundary.
///
/// The upgraded protocol must process this prefix before reading more from the
/// transferred transport. Use [`Self::into_parts`] to take ownership of storage.
#[derive(Debug)]
pub struct BufferedInput<B> {
    pub(crate) buffer: B,
    pub(crate) range: Range<usize>,
}

impl<B: Buffer> BufferedInput<B> {
    /// Views the unconsumed bytes without transferring their storage.
    pub fn bytes(&self) -> &[u8] {
        &self.buffer.as_ref()[self.range.clone()]
    }
    /// Returns the original buffer and the range containing the buffered prefix.
    pub fn into_parts(self) -> (B, Range<usize>) {
        (self.buffer, self.range)
    }
}

/// Successful protocol upgrade transfer from HTTP/1 to another protocol.
#[derive(Debug)]
pub struct Handoff<B> {
    /// Identity of the connection whose transport is being transferred.
    pub connection: ConnectionId,
    /// Input prefix received after the upgrade request and before handoff.
    pub buffered: BufferedInput<B>,
}
