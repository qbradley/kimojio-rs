// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Request body replay model.
//!
//! Storage retries are only safe when the request body can be sent again. This
//! module makes that explicit with [`ReplayBody`]: request builders can reject
//! non-replayable uploads, and [`RetryPolicy`](crate::RetryPolicy) can refuse to
//! retry attempts whose body cannot be reconstructed.
//!
//! ```
//! use kimojio_stack_storage::{BodyReplay, ReplayBody};
//!
//! let body = ReplayBody::from_vec(vec![1, 2, 3]);
//! assert_eq!(body.replay(), BodyReplay::Replayable);
//! assert_eq!(body.as_bytes(), Some(&[1, 2, 3][..]));
//! ```

use std::fmt;

use bytes::Bytes;

/// Replay behavior for a request body.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BodyReplay {
    /// The body bytes can be sent again on retry.
    Replayable,
    /// The body cannot be replayed safely.
    NonReplayable,
}

/// Caller-visible request body representation.
#[derive(Clone, Eq, PartialEq)]
pub enum ReplayBody {
    /// Empty replayable body.
    Empty,
    /// Replayable static byte slice.
    BorrowedStatic(&'static [u8]),
    /// Replayable shared bytes.
    Shared(Bytes),
    /// Replayable bytes owned by this request representation.
    Owned(Bytes),
    /// Body is supplied by an external one-shot stream not represented here.
    NonReplayable { len: Option<usize> },
}

impl fmt::Debug for ReplayBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("Empty"),
            Self::BorrowedStatic(bytes) => f
                .debug_struct("BorrowedStatic")
                .field("len", &bytes.len())
                .finish(),
            Self::Shared(bytes) => f.debug_struct("Shared").field("len", &bytes.len()).finish(),
            Self::Owned(bytes) => f.debug_struct("Owned").field("len", &bytes.len()).finish(),
            Self::NonReplayable { len } => {
                f.debug_struct("NonReplayable").field("len", len).finish()
            }
        }
    }
}

impl ReplayBody {
    /// Creates a replayable body by taking ownership of a caller buffer.
    pub fn from_vec(bytes: Vec<u8>) -> Self {
        Self::Owned(Bytes::from(bytes))
    }

    /// Creates a replayable body from shared bytes.
    pub fn from_bytes(bytes: Bytes) -> Self {
        Self::Shared(bytes)
    }

    /// Returns whether this body can be replayed on retry.
    pub fn replay(&self) -> BodyReplay {
        match self {
            Self::NonReplayable { .. } => BodyReplay::NonReplayable,
            _ => BodyReplay::Replayable,
        }
    }

    /// Returns the known body length, if available.
    pub fn len(&self) -> Option<usize> {
        match self {
            Self::Empty => Some(0),
            Self::BorrowedStatic(bytes) => Some(bytes.len()),
            Self::Shared(bytes) => Some(bytes.len()),
            Self::Owned(bytes) => Some(bytes.len()),
            Self::NonReplayable { len } => *len,
        }
    }

    /// Returns true if the body is known to be empty.
    pub fn is_empty(&self) -> bool {
        self.len() == Some(0)
    }

    /// Returns replayable bytes for body variants that can expose bytes directly.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Empty => Some(&[]),
            Self::BorrowedStatic(bytes) => Some(bytes),
            Self::Shared(bytes) => Some(bytes),
            Self::Owned(bytes) => Some(bytes),
            Self::NonReplayable { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replayable_bodies_expose_lengths_and_bytes() {
        let shared = ReplayBody::Shared(Bytes::from_static(b"abc"));
        let owned = ReplayBody::from_vec(vec![b'x', b'y']);

        assert_eq!(shared.replay(), BodyReplay::Replayable);
        assert_eq!(shared.len(), Some(3));
        assert_eq!(shared.as_bytes(), Some(b"abc".as_slice()));
        assert_eq!(owned.replay(), BodyReplay::Replayable);
        assert_eq!(owned.len(), Some(2));
        assert_eq!(owned.as_bytes(), Some(b"xy".as_slice()));
    }

    #[test]
    fn non_replayable_body_declares_retry_ineligibility() {
        let body = ReplayBody::NonReplayable { len: Some(8) };

        assert_eq!(body.replay(), BodyReplay::NonReplayable);
        assert_eq!(body.len(), Some(8));
        assert_eq!(body.as_bytes(), None);
    }
}
