//! Composable, allocation-conscious HTTP/1.0 and HTTP/1.1 client and server state machines.
//!
//! This crate parses and serializes protocol traffic, enforces framing and
//! connection policy, and issues owned operations through [`Ports`]. It never
//! performs I/O or reads a clock: the embedding executor performs each issued
//! operation once, returns its completion, and supplies monotonic [`Tick`]s.
//!
//! Start with [`Client`] or [`Server`], configure [`Config`], and implement
//! [`ClientPorts`] or [`ServerPorts`] to connect the machine to an executor and
//! application. Returning `None` from a port callback accepts the work and lets
//! the drive continue; returning `Some(output)` suspends at that boundary.
//! Only a matching completion settles an accepted operation. See the crate
//! README for a complete driving example and the operation ownership contract.
#![forbid(unsafe_code)]

mod codec;
mod connection;
mod observation;
mod operations;
mod state;
mod types;

#[cfg(feature = "bench-internals")]
#[doc(hidden)]
pub mod benchmark;

pub use connection::{Client, Server};
pub use observation::*;
pub use operations::*;
pub use types::*;

#[cfg(doctest)]
#[doc = include_str!("../README.md")]
mod readme {}

/// Exclusive initialized storage used for receive buffers and body leases.
///
/// Both slice views must identify the same bytes and retain the same length.
/// An executor must keep submitted operations stable until its I/O completes.
/// No `Send`, shared ownership, or heap allocation is required; `Vec<u8>` and
/// fixed arrays work.
pub trait Buffer: AsRef<[u8]> + AsMut<[u8]> {}
impl<T: AsRef<[u8]> + AsMut<[u8]>> Buffer for T {}

/// External capabilities and application notifications for one connection.
///
/// Each operation is issued once. Returning `None` accepts the operation and
/// continues progress. Only the corresponding completion settles it.
/// Borrowed metadata expires when the callback returns.
pub trait Ports<B: Buffer, W: AsRef<[u8]> = B> {
    /// Callback yield value; `None` accepts work and continues driving.
    type Output;
    /// Observes a committed drive boundary without suspending protocol progress.
    ///
    /// This callback must return promptly and must not reenter the machine.
    fn log(&mut self, _connection: ConnectionId, _now: Tick, _event: LogEvent) {}
    /// Starts one read into the owned receive region; complete it exactly once.
    fn read(&mut self, op: ReadOp<B>) -> Option<Self::Output>;
    /// Writes some or all remaining slices and returns the operation completion.
    fn write(&mut self, op: WriteOp<W>) -> Option<Self::Output>;
    /// Waits for readiness after a transport operation reports `WouldBlock`.
    fn readiness(&mut self, op: ReadinessOp) -> Option<Self::Output>;
    /// Requests cancellation but retains the original operation until it settles.
    fn cancel(&mut self, op: CancelOp) -> Option<Self::Output>;
    /// Closes the transport when the core issues its terminal close operation.
    fn close(&mut self, op: CloseOp) -> Option<Self::Output>;
    /// Delivers an exclusive body lease; return it through `release_body`.
    fn body(&mut self, op: BodyOp<B>) -> Option<Self::Output>;
    /// Delivers parsed trailers after the final body data.
    fn trailers(&mut self, exchange: ExchangeId, trailers: Headers<'_>) -> Option<Self::Output>;
    /// Signals that the complete incoming body has been consumed and validated.
    fn incoming_finished(&mut self, exchange: ExchangeId) -> Option<Self::Output>;
    /// Requests up to `capacity` bytes from the outgoing body producer.
    fn send_ready(&mut self, exchange: ExchangeId, capacity: usize) -> Option<Self::Output>;
    /// The machine needs no further producer payload for this exchange.
    ///
    /// The caller can release its body source, including any input stream that
    /// source retains. Owned writes and `body_sent` receipts can still be pending.
    fn source_finished(&mut self, _exchange: ExchangeId) -> Option<Self::Output> {
        None
    }
    /// Returns producer storage with its exact or lower-bound transport receipt.
    fn body_sent(&mut self, result: BodySent<W>) -> Option<Self::Output>;
    /// Reports the final result and reuse eligibility of an exchange.
    fn exchange_finished(&mut self, result: ExchangeFinished) -> Option<Self::Output>;
    /// Arms, replaces, or disarms the executor's timer for this connection.
    fn deadline_changed(&mut self, deadline: Option<Deadline>) -> Option<Self::Output>;
    /// Signals that the application may take the successful protocol handoff.
    fn upgrade_ready(&mut self, exchange: ExchangeId) -> Option<Self::Output>;
    /// Reports terminal connection close after owned operations settle.
    fn closed(&mut self, result: ConnectionResult) -> Option<Self::Output>;
}

/// Port set for a server machine; adds notification for each parsed request.
pub trait ServerPorts<B: Buffer, W: AsRef<[u8]> = B>: Ports<B, W> {
    /// Receives a borrowed request head. Convert metadata before returning if
    /// it must outlive this callback.
    fn request(&mut self, exchange: ExchangeId, head: RequestHead<'_>) -> Option<Self::Output>;
}

/// Port set for a client machine; adds notification for parsed response heads.
pub trait ClientPorts<B: Buffer, W: AsRef<[u8]> = B>: Ports<B, W> {
    /// Receives a borrowed response head; `informational` distinguishes interim
    /// responses from the final head. Convert metadata before returning if it
    /// must outlive this callback.
    fn response(
        &mut self,
        exchange: ExchangeId,
        head: ResponseHead<'_>,
        informational: bool,
    ) -> Option<Self::Output>;
}
