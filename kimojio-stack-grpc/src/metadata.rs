// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{HeaderMap, HeaderName, HeaderValue};

/// Low-level gRPC metadata wrapper over HTTP headers or trailers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Metadata {
    headers: HeaderMap,
}

impl Metadata {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, name: HeaderName, value: HeaderValue) {
        self.headers.insert(name, value);
    }

    pub fn get(&self, name: &HeaderName) -> Option<&HeaderValue> {
        self.headers.get(name)
    }

    pub fn as_headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub fn into_headers(self) -> HeaderMap {
        self.headers
    }
}
