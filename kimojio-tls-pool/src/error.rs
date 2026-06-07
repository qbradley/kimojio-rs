// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::fmt;
use std::io;

/// Result type used by TLS pool operations.
pub type OperationResult<T> = Result<T, OperationError>;

/// Error returned by read/write operations.
#[derive(Debug)]
pub enum OperationError {
    /// OpenSSL reported a TLS error.
    Tls(openssl::error::ErrorStack),
    /// The underlying stream reported an I/O error.
    Io(io::Error),
    /// The stream or pool has shut down.
    Shutdown,
    /// Another operation is already in flight for the stream.
    Busy,
}

impl fmt::Display for OperationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tls(error) => write!(f, "TLS error: {error}"),
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::Shutdown => f.write_str("TLS pool operation rejected after shutdown"),
            Self::Busy => f.write_str("TLS stream already has an in-flight operation"),
        }
    }
}

impl std::error::Error for OperationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Tls(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Shutdown | Self::Busy => None,
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
