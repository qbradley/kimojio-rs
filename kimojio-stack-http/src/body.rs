// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Bounded in-memory HTTP bodies.
//!
//! The stack HTTP APIs separate small buffered bodies from incremental body
//! streaming. [`Body`] is intentionally just bytes plus limit checks; use the
//! `*_with_body_chunks` connection APIs when callers need to process large
//! responses without collecting them in memory.
//!
//! ```
//! use kimojio_stack_http::{Body, BodyLimits};
//!
//! let limits = BodyLimits::new(8);
//! let body = Body::copy_from_slice(b"payload", limits).unwrap();
//! assert_eq!(body.as_bytes(), b"payload");
//! assert!(Body::copy_from_slice(b"too large", limits).is_err());
//! ```

use bytes::{Bytes, BytesMut};

use crate::error::{Error, LimitKind};

/// Maximum buffered body size for a request or response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyLimits {
    max_len: usize,
}

impl BodyLimits {
    /// Creates a maximum body-size limit in bytes.
    pub const fn new(max_len: usize) -> Self {
        Self { max_len }
    }

    /// Returns the maximum allowed buffered body size.
    pub const fn max_len(self) -> usize {
        self.max_len
    }

    /// Validates a body length against this limit.
    pub fn check_body_len(self, actual: usize) -> Result<(), Error> {
        if actual > self.max_len {
            Err(Error::size_limit(LimitKind::Body, self.max_len, actual))
        } else {
            Ok(())
        }
    }
}

impl Default for BodyLimits {
    fn default() -> Self {
        Self::new(4 * 1024 * 1024)
    }
}

/// Bounded byte body used by HTTP request and response paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Body {
    bytes: Bytes,
}

impl Body {
    /// Creates an empty body without allocation.
    pub fn empty() -> Self {
        Self {
            bytes: Bytes::new(),
        }
    }

    /// Creates a body from shared bytes after enforcing `limits`.
    pub fn from_bytes(bytes: Bytes, limits: BodyLimits) -> Result<Self, Error> {
        limits.check_body_len(bytes.len())?;
        Ok(Self { bytes })
    }

    /// Copies bytes into a new body after enforcing `limits`.
    pub fn copy_from_slice(bytes: &[u8], limits: BodyLimits) -> Result<Self, Error> {
        Self::from_bytes(Bytes::copy_from_slice(bytes), limits)
    }

    /// Returns the body length in bytes.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Returns whether the body contains no bytes.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Borrows the complete body bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consumes the body and returns its shared byte storage.
    pub fn into_bytes(self) -> Bytes {
        self.bytes
    }
}

/// Incremental bounded body accumulator.
#[derive(Debug, Clone)]
pub struct BodyBuilder {
    limits: BodyLimits,
    bytes: BytesMut,
}

impl BodyBuilder {
    /// Creates an incremental accumulator with the supplied limit.
    pub fn new(limits: BodyLimits) -> Self {
        Self {
            limits,
            bytes: BytesMut::new(),
        }
    }

    /// Appends a chunk, failing before mutation if the new total exceeds limits.
    pub fn append(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let new_len = self.bytes.len().saturating_add(bytes.len());
        self.limits.check_body_len(new_len)?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    /// Finishes the accumulator and freezes the collected bytes.
    pub fn finish(self) -> Body {
        Body {
            bytes: self.bytes.freeze(),
        }
    }
}
