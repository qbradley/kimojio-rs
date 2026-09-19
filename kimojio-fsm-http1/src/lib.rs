//! HTTP/1 connections with caller-defined callback suspension.
//!
//! The core performs no I/O and reads no clock. Operations own their buffers.
//! A callback return value controls suspension, not operation completion.
#![forbid(unsafe_code)]

mod codec;
mod connection;
mod operations;
mod types;

pub use connection::{Client, Server};
pub use operations::*;
pub use types::*;

#[cfg(doctest)]
#[doc = include_str!("../README.md")]
mod readme {}

/// Exclusive initialized storage. Moving a buffer must preserve its contents.
///
/// Both slice views must identify the same bytes and retain the same length.
/// An executor must keep submitted operations stable until its I/O completes.
/// No `Send`, shared ownership, or heap allocation is required by this trait.
pub trait Buffer: AsRef<[u8]> + AsMut<[u8]> {}
impl<T: AsRef<[u8]> + AsMut<[u8]>> Buffer for T {}

/// External capabilities and application notifications for one connection.
///
/// Each operation is issued once. Returning `None` accepts the operation and
/// continues progress. Only the corresponding completion settles it.
/// Borrowed metadata expires when the callback returns.
pub trait Ports<B: Buffer, W: AsRef<[u8]> = B> {
    type Output;
    fn read(&mut self, op: ReadOp<B>) -> Option<Self::Output>;
    fn write(&mut self, op: WriteOp<W>) -> Option<Self::Output>;
    fn readiness(&mut self, op: ReadinessOp) -> Option<Self::Output>;
    fn cancel(&mut self, op: CancelOp) -> Option<Self::Output>;
    fn close(&mut self, op: CloseOp) -> Option<Self::Output>;
    fn body(&mut self, op: BodyOp<B>) -> Option<Self::Output>;
    fn trailers(&mut self, exchange: ExchangeId, trailers: Headers<'_>) -> Option<Self::Output>;
    fn incoming_finished(&mut self, exchange: ExchangeId) -> Option<Self::Output>;
    fn send_ready(&mut self, exchange: ExchangeId, capacity: usize) -> Option<Self::Output>;
    /// The machine needs no further producer payload for this exchange.
    ///
    /// The caller can release its body source, including any input stream that
    /// source retains. Owned writes and `body_sent` receipts can still be pending.
    fn source_finished(&mut self, _exchange: ExchangeId) -> Option<Self::Output> {
        None
    }
    fn body_sent(&mut self, result: BodySent<W>) -> Option<Self::Output>;
    fn exchange_finished(&mut self, result: ExchangeFinished) -> Option<Self::Output>;
    fn deadline_changed(&mut self, deadline: Option<Deadline>) -> Option<Self::Output>;
    fn upgrade_ready(&mut self, exchange: ExchangeId) -> Option<Self::Output>;
    fn closed(&mut self, result: ConnectionResult) -> Option<Self::Output>;
}

pub trait ServerPorts<B: Buffer, W: AsRef<[u8]> = B>: Ports<B, W> {
    fn request(&mut self, exchange: ExchangeId, head: RequestHead<'_>) -> Option<Self::Output>;
}

pub trait ClientPorts<B: Buffer, W: AsRef<[u8]> = B>: Ports<B, W> {
    fn response(
        &mut self,
        exchange: ExchangeId,
        head: ResponseHead<'_>,
        informational: bool,
    ) -> Option<Self::Output>;
}
