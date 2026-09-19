//! A server-side WebSocket protocol machine with independently owned I/O.
//!
//! The caller owns execution, scheduling, storage allocation and time.
#![forbid(unsafe_code)]

mod handshake;
mod operations;
mod server;
mod types;

pub use handshake::*;
pub use kimojio_fsm_http1::{
    Acceptance, Buffer, ConnectionId, IoError, IoErrorKind, IoResult, RejectReason, Rejected, Tick,
};
pub use operations::*;
pub use server::Server;
pub use types::*;

#[cfg(doctest)]
#[doc = include_str!("../README.md")]
mod readme {}

/// Callbacks accept each operation exactly once, regardless of return value.
///
/// `Some` suspends with caller output; `None` continues without completing the
/// operation. Do not reenter the borrowed machine. Complete owned operations
/// after `next` returns. Chunk bytes are valid only while their operation lives.
pub trait Ports<B: Buffer, W: AsRef<[u8]> = B> {
    type Output;
    fn read(&mut self, op: ReadOp<B>) -> Option<Self::Output>;
    fn write(&mut self, op: WriteOp<W>) -> Option<Self::Output>;
    fn readiness(&mut self, op: ReadinessOp) -> Option<Self::Output>;
    fn cancel(&mut self, op: CancelOp) -> Option<Self::Output>;
    fn close(&mut self, op: CloseOp) -> Option<Self::Output>;
    fn message_started(&mut self, message: MessageInfo) -> Option<Self::Output>;
    fn chunk(&mut self, op: ChunkOp<B>) -> Option<Self::Output>;
    fn message_finished(&mut self, message: MessageInfo) -> Option<Self::Output>;
    fn message_sent(&mut self, receipt: MessageSent<W>) -> Option<Self::Output>;
    fn peer_closed(&mut self, reason: CloseReason) -> Option<Self::Output>;
    fn deadline_changed(&mut self, deadline: Option<Deadline>) -> Option<Self::Output>;
    fn closed(&mut self, result: ConnectionResult) -> Option<Self::Output>;
}
