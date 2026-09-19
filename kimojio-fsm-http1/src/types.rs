use crate::Buffer;
use std::ops::Range;

pub use httparse::Header;
pub type Headers<'a> = &'a [Header<'a>];

/// The caller must not reuse an identity while an old completion can exist.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ConnectionId {
    pub slot: u64,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ExchangeId {
    pub(crate) connection: ConnectionId,
    pub(crate) sequence: u64,
}

impl ExchangeId {
    pub fn connection(self) -> ConnectionId {
        self.connection
    }
    pub fn sequence(self) -> u64 {
        self.sequence
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum OperationKind {
    Read,
    Write,
    Readable,
    Writable,
    Body,
    Close,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct OperationId {
    pub(crate) connection: ConnectionId,
    pub(crate) sequence: u64,
    pub(crate) kind: OperationKind,
}

impl OperationId {
    pub fn connection(self) -> ConnectionId {
        self.connection
    }
    pub fn sequence(self) -> u64 {
        self.sequence
    }
    pub fn kind(self) -> OperationKind {
        self.kind
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct BodyId {
    pub(crate) connection: ConnectionId,
    pub(crate) sequence: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Version {
    Http10,
    Http11,
}

#[derive(Clone, Copy, Debug)]
pub struct RequestHead<'a> {
    pub method: &'a str,
    pub target: &'a str,
    pub version: Version,
    pub headers: Headers<'a>,
}

#[derive(Clone, Copy, Debug)]
pub struct ResponseHead<'a> {
    pub version: Version,
    pub status: u16,
    pub reason: &'a str,
    pub headers: Headers<'a>,
}

impl<'a> ResponseHead<'a> {
    /// Constructs outgoing metadata. A server selects the actual wire version.
    pub fn new(status: u16, reason: &'a str, headers: Headers<'a>) -> Self {
        Self {
            version: Version::Http11,
            status,
            reason,
            headers,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BodyLength {
    Empty,
    Known(u64),
    Streaming,
}

#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
    pub head: RequestHead<'a>,
    pub body: BodyLength,
    pub expect_continue: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct Response<'a> {
    pub head: ResponseHead<'a>,
    pub body: BodyLength,
}

impl<'a> Response<'a> {
    pub fn new(status: u16, reason: &'a str, headers: Headers<'a>, body: BodyLength) -> Self {
        Self {
            head: ResponseHead::new(status, reason, headers),
            body,
        }
    }
}

pub type InformationalResponse<'a> = ResponseHead<'a>;
pub type UpgradeResponse<'a> = ResponseHead<'a>;

#[derive(Debug)]
pub struct SendBody<B> {
    pub exchange: ExchangeId,
    pub buffer: B,
    pub range: Range<usize>,
    pub end: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RejectReason {
    WrongConnection,
    Stale,
    WrongKind,
    InvalidCount,
    InvalidRange,
    NoCapacity,
    Limit,
    InvalidState,
}

#[derive(Debug)]
pub struct Rejected<T> {
    pub reason: RejectReason,
    pub value: T,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandError {
    InvalidConfig,
    InvalidState,
    StaleExchange,
    InvalidHead,
    InvalidFraming,
    Limit,
    SequenceExhausted,
    TimeRegression,
    StaleDeadline,
    EarlyDeadline,
    NotReady,
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for CommandError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IoErrorKind {
    /// No bytes were accepted. The core waits for readiness before retrying.
    WouldBlock,
    /// No bytes were accepted. The core can retry the operation.
    Interrupted,
    Cancelled,
    /// A cancelled write-all operation with an unknown accepted prefix.
    ///
    /// Unlike `UnknownProgress`, this result confirms cancellation rather than
    /// transport failure. An early final response can still finish before close.
    CancelledUnknownProgress,
    Reset,
    /// Fatal error from an operation that can hide partial accepted bytes.
    ///
    /// The core never retries this operation. This variant is required for
    /// write-all transports that cannot report progress before an error.
    UnknownProgress,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoError {
    pub kind: IoErrorKind,
    pub code: Option<i32>,
}
pub type IoResult<T> = Result<T, IoError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Failure {
    Protocol,
    ExpectationFailed,
    Limit,
    UnexpectedEof,
    WriteZero,
    Transport(IoError),
    Cancelled,
    Application,
    /// The peer's final response stopped an unfinished request body.
    EarlyResponse,
    Timeout,
    SequenceExhausted,
}

pub type ConnectionResult = Result<(), Failure>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExchangeFinished {
    pub exchange: ExchangeId,
    pub result: Result<(), Failure>,
    pub reusable: bool,
}

#[derive(Debug)]
pub struct BodySent<B> {
    pub exchange: ExchangeId,
    pub id: BodyId,
    pub buffer: B,
    /// Payload bytes positively reported by transport, excluding chunk framing.
    ///
    /// `acceptance` states whether this count is exact or a lower bound.
    pub accepted: usize,
    pub acceptance: Acceptance,
    pub result: Result<(), Failure>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Acceptance {
    Exact,
    /// More bytes can have reached the transport than the confirmed count.
    LowerBound,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownMode {
    Graceful,
    Abort,
}

/// Nanoseconds in one caller-supplied monotonic domain.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd)]
pub struct Tick(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Deadline {
    pub(crate) connection: ConnectionId,
    pub(crate) sequence: u64,
    pub at: Tick,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub max_head_bytes: usize,
    pub max_headers: usize,
    pub max_buffer_bytes: usize,
    pub max_body_bytes: u64,
    pub max_chunk_line_bytes: usize,
    pub max_chunk_metadata_bytes: usize,
    pub max_informational_responses: usize,
    pub max_requests: u64,
    pub head_timeout_ns: Option<u64>,
    pub idle_timeout_ns: Option<u64>,
    pub body_timeout_ns: Option<u64>,
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

#[derive(Debug)]
pub struct BufferedInput<B> {
    pub(crate) buffer: B,
    pub(crate) range: Range<usize>,
}

impl<B: Buffer> BufferedInput<B> {
    pub fn bytes(&self) -> &[u8] {
        &self.buffer.as_ref()[self.range.clone()]
    }
    pub fn into_parts(self) -> (B, Range<usize>) {
        (self.buffer, self.range)
    }
}

#[derive(Debug)]
pub struct Handoff<B> {
    pub connection: ConnectionId,
    pub buffered: BufferedInput<B>,
}
