// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use bytes::{Bytes, BytesMut};

use crate::error::{Error, LimitKind};

/// Maximum buffered body size for a request or response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyLimits {
    max_len: usize,
}

impl BodyLimits {
    pub const fn new(max_len: usize) -> Self {
        Self { max_len }
    }

    pub const fn max_len(self) -> usize {
        self.max_len
    }

    fn check(self, actual: usize) -> Result<(), Error> {
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
    pub fn empty() -> Self {
        Self {
            bytes: Bytes::new(),
        }
    }

    pub fn from_bytes(bytes: Bytes, limits: BodyLimits) -> Result<Self, Error> {
        limits.check(bytes.len())?;
        Ok(Self { bytes })
    }

    pub fn copy_from_slice(bytes: &[u8], limits: BodyLimits) -> Result<Self, Error> {
        Self::from_bytes(Bytes::copy_from_slice(bytes), limits)
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

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
    pub fn new(limits: BodyLimits) -> Self {
        Self {
            limits,
            bytes: BytesMut::new(),
        }
    }

    pub fn append(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let new_len = self.bytes.len().saturating_add(bytes.len());
        self.limits.check(new_len)?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    pub fn finish(self) -> Body {
        Body {
            bytes: self.bytes.freeze(),
        }
    }
}
