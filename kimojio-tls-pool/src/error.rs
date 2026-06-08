// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::fmt;
use std::io;

/// Result type used by TLS pool read/write operations.
///
/// Public read/write submission methods return `OperationResult<()>` for
/// synchronous submission failures. Their callbacks receive `OperationResult<T>`
/// for the operation result itself.
pub type OperationResult<T> = Result<T, OperationError>;

/// Error returned by read/write operations.
///
/// Submission-time errors are returned directly from methods such as
/// [`crate::TlsStream::write`]. Operation-time errors are delivered through the
/// callback. In either case, callers should treat the error as authoritative;
/// the crate does not silently downgrade TLS or I/O failures to success.
///
/// ```
/// use kimojio_tls_pool::{OperationError, OperationResult};
///
/// fn handle(result: OperationResult<Vec<u8>>) {
///     match result {
///         Ok(bytes) => println!("read {} bytes", bytes.len()),
///         Err(OperationError::Shutdown) => println!("pool is shutting down"),
///         Err(OperationError::ReadTooLarge { requested, max }) => {
///             eprintln!("requested {requested} bytes, max is {max}");
///         }
///         Err(error) => eprintln!("TLS operation failed: {error}"),
///     }
/// }
/// ```
#[derive(Debug)]
pub enum OperationError {
    /// OpenSSL reported a TLS error.
    Tls(openssl::error::ErrorStack),
    /// TLS handshake failed.
    Handshake(String),
    /// The underlying stream reported an I/O error.
    Io(io::Error),
    /// The stream or pool has shut down.
    Shutdown,
    /// A submitted operation or callback panicked.
    ///
    /// Panics are contained for accounting/cleanup, but the operation is still
    /// considered failed.
    Panic(&'static str),
    /// A read request exceeded the configured maximum buffer length.
    ReadTooLarge { requested: usize, max: usize },
    /// Another operation is already in flight for the stream.
    ///
    /// Public read/write APIs currently queue same-stream work instead of
    /// surfacing this error; it is retained for lower-level/internal paths.
    Busy,
    /// Internal stream state was poisoned.
    StatePoisoned,
}

impl fmt::Display for OperationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tls(error) => write!(f, "TLS error: {error}"),
            Self::Handshake(error) => write!(f, "TLS handshake failed: {error}"),
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::Shutdown => f.write_str("TLS pool operation rejected after shutdown"),
            Self::Panic(context) => write!(f, "TLS pool {context} panicked"),
            Self::ReadTooLarge { requested, max } => {
                write!(
                    f,
                    "read buffer length {requested} exceeds configured maximum {max}"
                )
            }
            Self::Busy => f.write_str("TLS stream already has an in-flight operation"),
            Self::StatePoisoned => f.write_str("TLS stream state was poisoned"),
        }
    }
}

impl std::error::Error for OperationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Tls(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Handshake(_)
            | Self::Shutdown
            | Self::Panic(_)
            | Self::ReadTooLarge { .. }
            | Self::Busy
            | Self::StatePoisoned => None,
        }
    }
}

impl From<openssl::error::ErrorStack> for OperationError {
    fn from(value: openssl::error::ErrorStack) -> Self {
        Self::Tls(value)
    }
}

impl From<io::Error> for OperationError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

/// Error returned by pool-level operations.
///
/// Pool-level operations include pool construction and TLS stream creation.
/// Once a [`crate::TlsStream`] exists, read/write failures are reported as
/// [`OperationError`].
///
/// ```no_run
/// use std::os::unix::net::UnixStream;
///
/// use kimojio_tls_pool::{PoolConfig, TlsPool, TlsPoolError};
/// use openssl::ssl::SslConnector;
///
/// # fn connector() -> SslConnector { unimplemented!() }
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let pool = TlsPool::new(PoolConfig::new(1))?;
/// let (client_io, server_io) = UnixStream::pair()?;
/// drop(server_io);
///
/// match pool.client(&connector(), "localhost", client_io) {
///     Ok(_stream) => {}
///     Err(TlsPoolError::Operation(error)) => eprintln!("handshake failed: {error}"),
///     Err(TlsPoolError::Config(error)) => eprintln!("bad config: {error}"),
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub enum TlsPoolError {
    /// Invalid pool configuration.
    Config(crate::PoolConfigError),
    /// TLS operation failed.
    Operation(OperationError),
}

impl fmt::Display for TlsPoolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(error) => write!(f, "invalid TLS pool config: {error}"),
            Self::Operation(error) => write!(f, "TLS pool operation failed: {error}"),
        }
    }
}

impl std::error::Error for TlsPoolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Config(error) => Some(error),
            Self::Operation(error) => Some(error),
        }
    }
}

impl From<crate::PoolConfigError> for TlsPoolError {
    fn from(value: crate::PoolConfigError) -> Self {
        Self::Config(value)
    }
}

impl From<OperationError> for TlsPoolError {
    fn from(value: OperationError) -> Self {
        Self::Operation(value)
    }
}
