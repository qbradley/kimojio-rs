//! Async HTTP/1.0 and HTTP/1.1 clients and servers over established Kimojio streams.
//!
//! This crate wraps the I/O-independent `kimojio-fsm-http1` protocol machine in
//! cooperative async drivers. Use [`connect`] for an established stream or
//! [`connect_native`] for a socket, then poll its [`Connection::run`] alongside
//! client requests. For servers, pass a stream to [`serve_connection`] and
//! provide a sequential request handler. Bodies are demand-driven: use
//! [`OutgoingBody`] to send data and [`IncomingBody`] to consume it. See the
//! crate README for client, server, forwarding, shutdown, and native-I/O examples.
//!
//! # Client: fetch a response
//!
//! Run under the Kimojio runtime. Establish the socket separately, send an
//! origin-form target (`/`) with a `Host` header, and consume the response body
//! before reusing or shutting down the connection. `connect` does not spawn a
//! task: the application and driver must be polled concurrently.
//!
//! ```no_run
//! use kimojio::{OwnedFdStream, socket_helpers::create_client_socket};
//! use kimojio_http1::{connect, Config, ConnectionId, Error, OutgoingBody, http::Request};
//!
//! #[kimojio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let socket = create_client_socket(&"127.0.0.1:8080".parse()?).await?;
//!     let config = Config::new(ConnectionId { slot: 1, generation: 1 });
//!     let (mut client, connection) = connect(OwnedFdStream::new(socket), config);
//!     let control = client.control();
//!     let application = async {
//!         let result = async {
//!             let request = Request::builder()
//!                 .uri("/")
//!                 .header("host", "127.0.0.1:8080")
//!                 .body(OutgoingBody::empty())
//!                 .map_err(|_| Error::InvalidMetadata)?;
//!             let mut response = client.send(request).await?;
//!             println!("status: {}", response.status());
//!             let bytes = response.body_mut().collect(1024 * 1024).await?;
//!             println!("{}", String::from_utf8_lossy(&bytes));
//!             client.shutdown().await
//!         }.await;
//!         if result.is_err() {
//!             control.abort(); // Still let the driver settle its outstanding I/O.
//!         }
//!         result
//!     };
//!     let (application_result, driver_result) = futures::join!(application, connection.run());
//!     application_result?;
//!     driver_result?;
//!     Ok(())
//! }
//! ```
//!
//! # Server: handle one accepted connection
//!
//! The server helper owns the connection driver and polls the handler alongside
//! I/O. Consume request bodies when keeping the connection reusable; returning
//! an early response normally abandons unread input and closes the connection.
//! This example serves one connection (which can carry multiple requests).
//! A production accept loop should allocate a distinct ID for each connection
//! and run its serving future concurrently with other connections.
//!
//! ```no_run
//! use kimojio::{OwnedFd, OwnedFdStream, operations, socket_helpers::update_accept_socket};
//! use kimojio_http1::{serve_connection, Config, ConnectionId, OutgoingBody, http::Response};
//!
//! #[kimojio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let listener: OwnedFd = std::net::TcpListener::bind("127.0.0.1:8080")?.into();
//!     let socket = operations::accept(&listener).await?;
//!     update_accept_socket(&socket)?;
//!     operations::close(listener).await?;
//!     let config = Config::new(ConnectionId { slot: 1, generation: 1 });
//!     serve_connection(OwnedFdStream::new(socket), config, |mut request| async move {
//!         let _ = request.body_mut().collect(64 * 1024).await?;
//!         Ok(Response::new(OutgoingBody::full(b"hello\n".to_vec())))
//!     }).await?;
//!     Ok(())
//! }
//! ```

mod body;
mod driver;
mod io;
mod io_driver;
mod metadata;
mod observation;
mod receive_lane;
mod timer;
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
    /// The peer, driver, or internal channel closed before the requested result arrived.
    Closed,
    /// A request, response, or body was canceled before completion.
    Cancelled,
    /// A configured message, body, or buffer limit was exceeded.
    Limit,
    /// HTTP metadata could not be represented or conflicts with body policy.
    InvalidMetadata,
    /// The HTTP/1 protocol machine terminated with this failure.
    Protocol(kimojio_fsm_http1::Failure),
    /// A protocol command was rejected synchronously.
    Command(kimojio_fsm_http1::CommandError),
    /// Underlying Kimojio transport operation failed.
    Transport(kimojio::Errno),
    /// Application handler or outgoing body source failed.
    Application(String),
    /// An observation handle was attached to more than one connection.
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
