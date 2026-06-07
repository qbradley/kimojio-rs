// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::{error, fmt};

use kimojio_stack::Errno;

/// Stable category for HTTP transport and protocol failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Io,
    Tls,
    Parse,
    Protocol,
    Unsupported,
    SizeLimit,
    Eof,
    PeerReset,
}

/// Limit category used by [`Error::SizeLimit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitKind {
    StartLine,
    Headers,
    Body,
    Frame,
    Message,
}

/// Inspectable HTTP error used by stackful HTTP components.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Io(Errno),
    Tls(Errno),
    Parse(&'static str),
    Protocol(&'static str),
    Unsupported(&'static str),
    SizeLimit {
        kind: LimitKind,
        limit: usize,
        actual: usize,
    },
    Eof,
    PeerReset,
}

impl Error {
    pub fn io(errno: Errno) -> Self {
        Self::Io(errno)
    }

    pub fn tls(errno: Errno) -> Self {
        Self::Tls(errno)
    }

    pub fn size_limit(kind: LimitKind, limit: usize, actual: usize) -> Self {
        Self::SizeLimit {
            kind,
            limit,
            actual,
        }
    }

    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::Io(_) => ErrorKind::Io,
            Self::Tls(_) => ErrorKind::Tls,
            Self::Parse(_) => ErrorKind::Parse,
            Self::Protocol(_) => ErrorKind::Protocol,
            Self::Unsupported(_) => ErrorKind::Unsupported,
            Self::SizeLimit { .. } => ErrorKind::SizeLimit,
            Self::Eof => ErrorKind::Eof,
            Self::PeerReset => ErrorKind::PeerReset,
        }
    }
}

impl From<Errno> for Error {
    fn from(errno: Errno) -> Self {
        Self::io(errno)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(errno) => write!(f, "HTTP I/O error: {errno:?}"),
            Self::Tls(errno) => write!(f, "HTTP TLS error: {errno:?}"),
            Self::Parse(message) => write!(f, "HTTP parse error: {message}"),
            Self::Protocol(message) => write!(f, "HTTP protocol error: {message}"),
            Self::Unsupported(message) => write!(f, "unsupported HTTP feature: {message}"),
            Self::SizeLimit {
                kind,
                limit,
                actual,
            } => write!(
                f,
                "HTTP {kind:?} size limit exceeded: limit {limit}, actual {actual}"
            ),
            Self::Eof => f.write_str("HTTP transport reached EOF"),
            Self::PeerReset => f.write_str("HTTP peer reset the connection"),
        }
    }
}

impl error::Error for Error {}
