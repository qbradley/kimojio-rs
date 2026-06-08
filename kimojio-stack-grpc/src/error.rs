// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::{error, fmt};

use crate::Status;

/// Stable category for unary gRPC failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Transport,
    Protocol,
    Encode,
    Decode,
    SizeLimit,
    Status,
    UnsupportedCompression,
}

/// Inspectable gRPC error.
#[derive(Debug)]
pub enum Error {
    Transport(kimojio_stack_http::Error),
    Protocol(&'static str),
    Encode(prost::EncodeError),
    Decode(prost::DecodeError),
    SizeLimit { limit: usize, actual: usize },
    Status(Status),
    UnsupportedCompression(u8),
}

impl Error {
    /// Returns the stable error category.
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::Transport(_) => ErrorKind::Transport,
            Self::Protocol(_) => ErrorKind::Protocol,
            Self::Encode(_) => ErrorKind::Encode,
            Self::Decode(_) => ErrorKind::Decode,
            Self::SizeLimit { .. } => ErrorKind::SizeLimit,
            Self::Status(_) => ErrorKind::Status,
            Self::UnsupportedCompression(_) => ErrorKind::UnsupportedCompression,
        }
    }
}

impl From<kimojio_stack_http::Error> for Error {
    fn from(error: kimojio_stack_http::Error) -> Self {
        Self::Transport(error)
    }
}

impl From<prost::EncodeError> for Error {
    fn from(error: prost::EncodeError) -> Self {
        Self::Encode(error)
    }
}

impl From<prost::DecodeError> for Error {
    fn from(error: prost::DecodeError) -> Self {
        Self::Decode(error)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "gRPC transport error: {error}"),
            Self::Protocol(message) => write!(f, "gRPC protocol error: {message}"),
            Self::Encode(error) => write!(f, "gRPC encode error: {error}"),
            Self::Decode(error) => write!(f, "gRPC decode error: {error}"),
            Self::SizeLimit { limit, actual } => {
                write!(
                    f,
                    "gRPC message size limit exceeded: limit {limit}, actual {actual}"
                )
            }
            Self::Status(status) => write!(f, "gRPC status error: {status}"),
            Self::UnsupportedCompression(flag) => {
                write!(f, "unsupported gRPC compression flag: {flag}")
            }
        }
    }
}

impl error::Error for Error {}
