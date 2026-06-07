// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::{error, fmt};

/// Stable category for OpenTelemetry export failures.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ErrorKind {
    Validation,
    SizeLimit,
    Transport,
    Protocol,
    Encode,
    Decode,
    Status,
    UnsupportedCompression,
    Unsupported,
}

/// Inspectable OpenTelemetry export error.
#[derive(Debug)]
pub enum Error {
    Validation(&'static str),
    SizeLimit { limit: usize, actual: usize },
    Grpc(kimojio_stack_grpc::Error),
    Unsupported(&'static str),
}

impl Error {
    /// Returns the stable error category.
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::Validation(_) => ErrorKind::Validation,
            Self::SizeLimit { .. } => ErrorKind::SizeLimit,
            Self::Grpc(error) => match error.kind() {
                kimojio_stack_grpc::ErrorKind::Transport => ErrorKind::Transport,
                kimojio_stack_grpc::ErrorKind::Protocol => ErrorKind::Protocol,
                kimojio_stack_grpc::ErrorKind::Encode => ErrorKind::Encode,
                kimojio_stack_grpc::ErrorKind::Decode => ErrorKind::Decode,
                kimojio_stack_grpc::ErrorKind::SizeLimit => ErrorKind::SizeLimit,
                kimojio_stack_grpc::ErrorKind::Status => ErrorKind::Status,
                kimojio_stack_grpc::ErrorKind::UnsupportedCompression => {
                    ErrorKind::UnsupportedCompression
                }
            },
            Self::Unsupported(_) => ErrorKind::Unsupported,
        }
    }
}

impl From<kimojio_stack_grpc::Error> for Error {
    fn from(error: kimojio_stack_grpc::Error) -> Self {
        Self::Grpc(error)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Validation(field) => write!(f, "invalid OpenTelemetry field: {field}"),
            Self::SizeLimit { limit, actual } => write!(
                f,
                "OpenTelemetry export size limit exceeded: limit {limit}, actual {actual}"
            ),
            Self::Grpc(error) => write!(f, "OpenTelemetry gRPC export error: {error}"),
            Self::Unsupported(feature) => write!(f, "unsupported OpenTelemetry feature: {feature}"),
        }
    }
}

impl error::Error for Error {}
