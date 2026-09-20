//! Concurrent HTTP/2 clients and servers over established native Kimojio descriptors.
//!
//! Poll [`NativeConnection::run`] alongside application requests. This crate
//! supplies neither connection establishment nor TLS, ALPN, pooling, or retries.
#![doc = include_str!("../README.md")]

mod body;
mod driver;
mod informational;
mod io;
mod metadata;
mod observation;

#[cfg(test)]
mod native_tests;

pub use body::{BodyChunk, IncomingBody, IncomingFrame, OutgoingBody, OutgoingFrame};
pub use driver::{
    Client, Config, NativeConnection, Shutdown, connect_native, serve_connection_native,
    serve_connection_native_with_shutdown,
};
pub use http;
pub use informational::InformationalSender;
pub use kimojio_fsm_http2::{ConnectionResult, ReceiveEnd, SendStop, StreamId, StreamOutcome};
pub use observation::{RequestObserver, SendFailure, StreamReport};

/// A local application failure or an authoritative protocol outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Closed,
    Cancelled,
    Limit,
    InvalidMetadata,
    BufferTooLarge {
        bytes: usize,
        capacity: usize,
        max_bytes: usize,
        max_capacity: usize,
    },
    Command(kimojio_fsm_http2::CommandError),
    Stream(StreamOutcome),
    Connection(ConnectionResult),
    Transport(kimojio::Errno),
    Send {
        accepted: usize,
        exact: bool,
        reason: SendStop,
    },
    Application(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for Error {}

impl From<kimojio_fsm_http2::CommandError> for Error {
    fn from(value: kimojio_fsm_http2::CommandError) -> Self {
        Self::Command(value)
    }
}
