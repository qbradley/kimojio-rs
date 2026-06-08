// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Conditional request helpers.
//!
//! [`Conditions`] applies ordinary ETag preconditions to request metadata. It is
//! shared by block, snapshot, metadata, and copy request builders.
//!
//! ```
//! use kimojio_stack_storage::{Conditions, OperationClass, RequestParts};
//!
//! let mut request = RequestParts::new(OperationClass::Metadata, "HEAD", "/object");
//! Conditions::if_match("etag").apply(&mut request);
//! assert_eq!(request.metadata.get("if-match"), Some("etag"));
//! ```

use crate::RequestParts;

/// ETag preconditions for storage requests.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Conditions {
    /// Value for `if-match`.
    pub if_match: Option<String>,
    /// Value for `if-none-match`.
    pub if_none_match: Option<String>,
}

impl Conditions {
    /// Creates empty conditions.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a condition requiring a matching ETag.
    pub fn if_match(etag: impl Into<String>) -> Self {
        Self {
            if_match: Some(etag.into()),
            if_none_match: None,
        }
    }

    /// Creates a condition requiring a non-matching ETag.
    pub fn if_none_match(etag: impl Into<String>) -> Self {
        Self {
            if_match: None,
            if_none_match: Some(etag.into()),
        }
    }

    /// Adds or replaces `if-match`.
    pub fn with_if_match(mut self, etag: impl Into<String>) -> Self {
        self.if_match = Some(etag.into());
        self
    }

    /// Adds or replaces `if-none-match`.
    pub fn with_if_none_match(mut self, etag: impl Into<String>) -> Self {
        self.if_none_match = Some(etag.into());
        self
    }

    /// Applies configured conditions to request metadata.
    pub fn apply(&self, request: &mut RequestParts) {
        if let Some(etag) = &self.if_match {
            request.metadata.insert("if-match", etag);
        }
        if let Some(etag) = &self.if_none_match {
            request.metadata.insert("if-none-match", etag);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::OperationClass;

    use super::*;

    #[test]
    fn conditions_apply_etag_headers() {
        let mut request = RequestParts::new(OperationClass::Metadata, "HEAD", "/object");
        Conditions::new()
            .with_if_match("etag-a")
            .with_if_none_match("etag-b")
            .apply(&mut request);

        assert_eq!(request.metadata.get("if-match"), Some("etag-a"));
        assert_eq!(request.metadata.get("if-none-match"), Some("etag-b"));
    }
}
