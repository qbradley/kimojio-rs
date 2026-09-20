//! Owned operations at the transport and application boundaries.

use std::{io::IoSlice, ops::Range, rc::Rc, time::Duration};

use crate::server::h2::headers::H2RawHeaderBlockRef;
use crate::{H2ProtocolError, H2RawHeaderRef, ServerError};

/// A stable view and the total allocation capacity retained by that view.
///
/// Implementations must include all backing allocations, not only visible bytes.
/// Shared ranges report their complete backing allocation conservatively.
/// The bytes and footprint must remain unchanged until the buffer is returned.
pub trait SendBuffer: AsRef<[u8]> {
    fn retained_capacity(&self) -> usize;
}

impl SendBuffer for Vec<u8> {
    fn retained_capacity(&self) -> usize {
        self.capacity()
    }
}

impl SendBuffer for Box<[u8]> {
    fn retained_capacity(&self) -> usize {
        self.len()
    }
}

impl<const N: usize> SendBuffer for [u8; N] {
    fn retained_capacity(&self) -> usize {
        N
    }
}

impl SendBuffer for &'static [u8] {
    fn retained_capacity(&self) -> usize {
        0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct StreamId(pub(crate) u32);

impl StreamId {
    pub const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Token {
    pub(crate) owner: Rc<Owner>,
    pub(crate) sequence: u64,
}

#[derive(Debug)]
pub(crate) struct Owner;

impl PartialEq for Owner {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}
impl Eq for Owner {}

impl Token {
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandError {
    InvalidState,
    InvalidCompletion,
    Capacity,
    SequenceExhausted,
    TimeReversed,
    Protocol(H2ProtocolError),
    Message(ServerError),
}

#[derive(Debug)]
pub struct Rejected<T> {
    pub error: CommandError,
    pub value: T,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IoFailure {
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadOutcome {
    Read(usize),
    Eof,
    Failed(IoFailure),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Progress {
    Exact(usize),
    AtLeast(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteOutcome {
    Written(usize),
    Failed {
        progress: Progress,
        error: IoFailure,
    },
}

#[derive(Debug)]
pub(crate) struct Page {
    pub(crate) bytes: Box<[u8]>,
}

#[derive(Debug)]
pub struct ReadOp {
    pub(crate) token: Token,
    pub(crate) page: Rc<Page>,
}

impl ReadOp {
    pub fn token(&self) -> &Token {
        &self.token
    }
    pub fn buffer_mut(&mut self) -> &mut [u8] {
        &mut Rc::get_mut(&mut self.page)
            .expect("read page is exclusive")
            .bytes
    }
    pub fn complete(self, outcome: ReadOutcome) -> ReadCompletion {
        ReadCompletion { op: self, outcome }
    }
}

#[derive(Debug)]
pub struct ReadCompletion {
    pub(crate) op: ReadOp,
    pub(crate) outcome: ReadOutcome,
}
impl ReadCompletion {
    pub fn into_parts(self) -> (ReadOp, ReadOutcome) {
        (self.op, self.outcome)
    }
}

#[derive(Debug)]
pub(crate) enum WriteStorage<B> {
    Control {
        bytes: Vec<u8>,
        purpose: ControlPurpose,
    },
    Data {
        header: [u8; 9],
        buffer: B,
        range: Range<usize>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ControlPurpose {
    Ordinary,
    LocalSettings,
    ShutdownPing,
}

#[derive(Debug)]
pub struct WriteOp<B: SendBuffer = Vec<u8>> {
    pub(crate) token: Token,
    pub(crate) storage: WriteStorage<B>,
    pub(crate) cursor: usize,
    pub(crate) stream: Option<StreamId>,
    pub(crate) end: bool,
    pub(crate) buffer_end: bool,
}

impl<B: SendBuffer> WriteOp<B> {
    pub fn token(&self) -> &Token {
        &self.token
    }
    /// Remaining slices at the exact transport cursor. Empty slices are valid.
    pub fn slices(&self) -> [IoSlice<'_>; 2] {
        match &self.storage {
            WriteStorage::Control { bytes, .. } => {
                [IoSlice::new(&bytes[self.cursor..]), IoSlice::new(&[])]
            }
            WriteStorage::Data {
                header,
                buffer,
                range,
            } => {
                let payload = &buffer.as_ref()[range.clone()];
                if self.cursor < 9 {
                    [IoSlice::new(&header[self.cursor..]), IoSlice::new(payload)]
                } else {
                    [IoSlice::new(&[]), IoSlice::new(&payload[self.cursor - 9..])]
                }
            }
        }
    }
    pub fn remaining(&self) -> usize {
        match &self.storage {
            WriteStorage::Control { bytes, .. } => bytes.len() - self.cursor,
            WriteStorage::Data { range, .. } => 9 + range.len() - self.cursor,
        }
    }
    pub fn complete(self, outcome: WriteOutcome) -> WriteCompletion<B> {
        WriteCompletion { op: self, outcome }
    }
}

#[derive(Debug)]
pub struct WriteCompletion<B: SendBuffer = Vec<u8>> {
    pub(crate) op: WriteOp<B>,
    pub(crate) outcome: WriteOutcome,
}
impl<B: SendBuffer> WriteCompletion<B> {
    pub fn into_parts(self) -> (WriteOp<B>, WriteOutcome) {
        (self.op, self.outcome)
    }
}

/// A single-use reservation for one bounded application buffer.
///
/// The permit reserves storage, not wire credit. SETTINGS can reduce wire credit
/// after issuance. The engine holds an admitted buffer until credit permits DATA.
#[derive(Debug)]
pub struct SendPermit {
    pub(crate) token: Token,
    pub(crate) stream: StreamId,
    pub(crate) max_bytes: usize,
    pub(crate) max_retained_capacity: usize,
}

impl SendPermit {
    pub fn stream(&self) -> StreamId {
        self.stream
    }
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }
    pub fn max_retained_capacity(&self) -> usize {
        self.max_retained_capacity
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SendStop {
    Finished,
    Reset(u32),
    Unprocessed,
    ConnectionFailed,
}

#[derive(Debug)]
pub struct Sent<B: SendBuffer = Vec<u8>> {
    pub stream: StreamId,
    pub buffer: B,
    /// Exact cumulative DATA payload accepted by the transport for this buffer.
    pub accepted: usize,
    /// False means that further acceptance is unknown after a transport failure.
    pub exact: bool,
    pub result: Result<(), SendStop>,
}

#[derive(Debug)]
pub struct BodyOp {
    pub(crate) token: Token,
    pub(crate) stream: StreamId,
    pub(crate) page: Rc<Page>,
    pub(crate) range: Range<usize>,
}

impl BodyOp {
    pub fn stream(&self) -> StreamId {
        self.stream
    }
    pub fn bytes(&self) -> &[u8] {
        &self.page.bytes[self.range.clone()]
    }
    pub fn release(self) -> BodyRelease {
        BodyRelease { op: self }
    }
}

impl AsRef<[u8]> for BodyOp {
    fn as_ref(&self) -> &[u8] {
        self.bytes()
    }
}

impl SendBuffer for BodyOp {
    fn retained_capacity(&self) -> usize {
        self.page.bytes.len()
    }
}

#[derive(Debug)]
pub struct BodyRelease {
    pub(crate) op: BodyOp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HeadKind {
    Request,
    Informational(u16),
    Response(u16),
    Trailers,
}

#[derive(Clone, Copy, Debug)]
pub struct Head<'a> {
    pub stream: StreamId,
    pub kind: HeadKind,
    pub end_stream: bool,
    pub(crate) fields: H2RawHeaderBlockRef<'a>,
}

impl<'a> Head<'a> {
    pub fn len(&self) -> usize {
        self.fields.len()
    }
    pub fn is_empty(&self) -> bool {
        self.fields.len() == 0
    }
    pub fn field(&self, index: usize) -> Option<H2RawHeaderRef<'a>> {
        self.fields.get(index)
    }
    pub fn fields(&self) -> impl Iterator<Item = H2RawHeaderRef<'a>> + '_ {
        (0..self.len()).map(|index| self.fields.get(index).expect("field index is in bounds"))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamOutcome {
    Complete,
    Reset(u32),
    /// The peer GOAWAY excludes this locally initiated request.
    Unprocessed,
    ConnectionFailed,
    Deadline,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReceiveEnd {
    pub stream: StreamId,
    pub outcome: StreamOutcome,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StreamResult {
    pub stream: StreamId,
    pub outcome: StreamOutcome,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionResult {
    Graceful,
    PeerClosed,
    IoFailed,
    Protocol(H2ProtocolError),
    ResourceExhausted,
}

#[derive(Debug)]
pub struct CancelOp {
    pub(crate) token: Token,
    pub(crate) original: Token,
}

impl CancelOp {
    pub fn original(&self) -> &Token {
        &self.original
    }
    pub fn complete(self) -> CancelCompletion {
        CancelCompletion { op: self }
    }
}
#[derive(Debug)]
pub struct CancelCompletion {
    pub(crate) op: CancelOp,
}

/// A deadline alarm, never an I/O-readiness operation.
///
/// An invalidated alarm can complete late. Its original token still settles,
/// but it cannot trigger the replacement deadline or revive a closed stream.
#[derive(Debug)]
pub struct WakeOp {
    pub(crate) token: Token,
    pub(crate) deadline: Duration,
}
impl WakeOp {
    pub fn token(&self) -> &Token {
        &self.token
    }
    pub fn deadline(&self) -> Duration {
        self.deadline
    }
    pub fn complete(self, now: Duration) -> WakeCompletion {
        WakeCompletion { op: self, now }
    }
}
#[derive(Debug)]
pub struct WakeCompletion {
    pub(crate) op: WakeOp,
    pub(crate) now: Duration,
}

#[derive(Debug)]
pub struct CloseOp {
    pub(crate) token: Token,
}
impl CloseOp {
    pub fn token(&self) -> &Token {
        &self.token
    }
    pub fn complete(self, result: Result<(), IoFailure>) -> CloseCompletion {
        CloseCompletion { op: self, result }
    }
}
#[derive(Debug)]
pub struct CloseCompletion {
    pub(crate) op: CloseOp,
    pub(crate) result: Result<(), IoFailure>,
}

pub trait Ports<B: SendBuffer> {
    type Output;
    fn read(&mut self, op: ReadOp) -> Option<Self::Output>;
    fn write(&mut self, op: WriteOp<B>) -> Option<Self::Output>;
    fn headers(&mut self, head: Head<'_>) -> Option<Self::Output>;
    fn body(&mut self, op: BodyOp) -> Option<Self::Output>;
    fn send_ready(&mut self, permit: SendPermit) -> Option<Self::Output>;
    /// Revokes source demand. An original outstanding write still owns its buffer.
    fn send_stopped(&mut self, stream: StreamId, reason: SendStop) -> Option<Self::Output>;
    fn sent(&mut self, result: Sent<B>) -> Option<Self::Output>;
    fn ended(&mut self, end: ReceiveEnd) -> Option<Self::Output>;
    fn retired(&mut self, result: StreamResult) -> Option<Self::Output>;
    fn cancel(&mut self, op: CancelOp) -> Option<Self::Output>;
    fn wake(&mut self, op: WakeOp) -> Option<Self::Output>;
    fn close(&mut self, op: CloseOp) -> Option<Self::Output>;
    fn closed(&mut self, result: ConnectionResult) -> Option<Self::Output>;
    fn reschedule(&mut self) -> Option<Self::Output>;
}

impl From<ServerError> for CommandError {
    fn from(value: ServerError) -> Self {
        Self::Message(value)
    }
}
impl From<H2ProtocolError> for CommandError {
    fn from(value: H2ProtocolError) -> Self {
        Self::Protocol(value)
    }
}
