//! Async HTTP/1 client and server connections over established Kimojio streams.
//!
//! The standalone `kimojio-fsm-http1` crate owns framing and connection policy.
//! This crate supplies application bodies, metadata conversion, and native I/O.

mod body;
mod driver;
mod io;
mod io_driver;
mod metadata;
mod observation;
mod transport;

pub use body::{BodyChunk, IncomingBody, IncomingFrame, OutgoingBody, OutgoingFrame};
pub use driver::{
    Client, Config, Connection, NativeConnection, Shutdown, connect, connect_native,
    serve_connection, serve_connection_native, serve_connection_native_with_shutdown,
    serve_connection_with_shutdown,
};
pub use http;
pub use kimojio_fsm_http1::ConnectionId;
#[cfg(feature = "metrics")]
pub use kimojio_fsm_http1::{Counters, MetricsSnapshot, SnapshotPhase};
pub use kimojio_fsm_http1::{LogEvent, Tick};
pub use observation::Observation;

/// A connection, protocol, or application-source failure.
#[derive(Clone, Debug)]
pub enum Error {
    Closed,
    Cancelled,
    Limit,
    InvalidMetadata,
    Protocol(kimojio_fsm_http1::Failure),
    Command(kimojio_fsm_http1::CommandError),
    Transport(kimojio::Errno),
    Application(String),
    ObservationInUse,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for Error {}

impl From<kimojio_fsm_http1::CommandError> for Error {
    fn from(value: kimojio_fsm_http1::CommandError) -> Self {
        Self::Command(value)
    }
}
