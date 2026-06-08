// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::RequestParts;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Conditions {
    pub if_match: Option<String>,
    pub if_none_match: Option<String>,
}

impl Conditions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn if_match(etag: impl Into<String>) -> Self {
        Self {
            if_match: Some(etag.into()),
            if_none_match: None,
        }
    }

    pub fn if_none_match(etag: impl Into<String>) -> Self {
        Self {
            if_match: None,
            if_none_match: Some(etag.into()),
        }
    }

    pub fn with_if_match(mut self, etag: impl Into<String>) -> Self {
        self.if_match = Some(etag.into());
        self
    }

    pub fn with_if_none_match(mut self, etag: impl Into<String>) -> Self {
        self.if_none_match = Some(etag.into());
        self
    }

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
