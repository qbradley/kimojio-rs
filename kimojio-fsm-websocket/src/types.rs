use crate::*;
use std::ops::Range;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum OperationKind {
    Read,
    Write,
    Readable,
    Writable,
    Chunk,
    Close,
}

/// WS identities have a separate type from HTTP identities.
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
pub struct MessageId {
    pub(crate) connection: ConnectionId,
    pub(crate) sequence: u64,
}

impl MessageId {
    pub fn connection(self) -> ConnectionId {
        self.connection
    }
    pub fn sequence(self) -> u64 {
        self.sequence
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageKind {
    Text,
    Binary,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MessageInfo {
    pub id: MessageId,
    pub kind: MessageKind,
    /// Zero in `message_started`; the complete length in `message_finished`.
    pub length: u64,
}

#[derive(Debug)]
pub struct SendMessage<W> {
    pub kind: MessageKind,
    pub buffer: W,
    pub range: Range<usize>,
}

/// Resources recovered when a handoff cannot fit the WebSocket configuration.
#[derive(Debug)]
pub struct UpgradeInput<B> {
    pub connection: ConnectionId,
    pub buffer: B,
    pub range: Range<usize>,
}

#[derive(Debug)]
pub struct MessageSent<W> {
    pub id: MessageId,
    pub buffer: W,
    pub accepted: usize,
    pub acceptance: Acceptance,
    pub result: Result<(), Failure>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Failure {
    Protocol,
    InvalidUtf8,
    Limit,
    UnexpectedEof,
    WriteZero,
    Transport(IoError),
    Cancelled,
    Application,
    Closing,
    Timeout,
    SequenceExhausted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConnectionResult {
    pub result: Result<(), Failure>,
    pub peer_close: Option<CloseReason>,
    pub clean: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandError {
    InvalidConfig,
    InvalidState,
    InvalidClose,
    Limit,
    SequenceExhausted,
    TimeRegression,
    StaleDeadline,
    EarlyDeadline,
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for CommandError {}

/// Inline close payload; `None` means no status code was supplied.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CloseReason {
    pub(crate) bytes: [u8; 125],
    pub(crate) len: u8,
}

impl CloseReason {
    pub fn empty() -> Self {
        Self {
            bytes: [0; 125],
            len: 0,
        }
    }
    /// Constructs a server-originated close. Code 1010 is client-only.
    pub fn new(code: u16, reason: &str) -> Result<Self, CommandError> {
        if !valid_close_code(code) || code == 1010 || reason.len() > 123 {
            return Err(CommandError::InvalidClose);
        }
        let mut result = Self::empty();
        result.bytes[..2].copy_from_slice(&code.to_be_bytes());
        result.bytes[2..2 + reason.len()].copy_from_slice(reason.as_bytes());
        result.len = (2 + reason.len()) as u8;
        Ok(result)
    }
    pub fn code(&self) -> Option<u16> {
        (self.len >= 2).then(|| u16::from_be_bytes([self.bytes[0], self.bytes[1]]))
    }
    pub fn reason(&self) -> &str {
        std::str::from_utf8(&self.bytes[usize::from(self.len.min(2))..usize::from(self.len)])
            .expect("validated close reason")
    }
    pub(crate) fn payload(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

pub(crate) fn valid_close_code(code: u16) -> bool {
    matches!(code, 1000..=1003 | 1007..=1014 | 3000..=4999)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Deadline {
    pub(crate) connection: ConnectionId,
    pub(crate) sequence: u64,
    pub at: Tick,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub max_buffer_bytes: usize,
    pub max_message_bytes: u64,
    pub max_frame_bytes: u64,
    pub max_fragments: u64,
    pub outgoing_frame_bytes: usize,
    pub idle_timeout_ns: Option<u64>,
    pub frame_timeout_ns: Option<u64>,
    pub message_timeout_ns: Option<u64>,
    pub write_timeout_ns: Option<u64>,
    pub close_timeout_ns: Option<u64>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_buffer_bytes: 1024 * 1024,
            max_message_bytes: 1024 * 1024,
            max_frame_bytes: 1024 * 1024,
            max_fragments: 65536,
            outgoing_frame_bytes: 16 * 1024,
            idle_timeout_ns: Some(60_000_000_000),
            frame_timeout_ns: Some(30_000_000_000),
            message_timeout_ns: Some(30_000_000_000),
            write_timeout_ns: Some(30_000_000_000),
            close_timeout_ns: Some(5_000_000_000),
        }
    }
}
