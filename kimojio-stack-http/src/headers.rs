// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{HeaderMap, HeaderName, HeaderValue};

/// HTTP header block used by requests and responses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Headers {
    map: HeaderMap,
}

impl Headers {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, name: HeaderName, value: HeaderValue) {
        self.map.insert(name, value);
    }

    pub fn get(&self, name: &HeaderName) -> Option<&HeaderValue> {
        self.map.get(name)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn as_map(&self) -> &HeaderMap {
        &self.map
    }

    pub fn into_map(self) -> HeaderMap {
        self.map
    }
}

/// HTTP trailer block used by response trailers and gRPC status metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Trailers {
    map: HeaderMap,
}

impl Trailers {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, name: HeaderName, value: HeaderValue) {
        self.map.insert(name, value);
    }

    pub fn get(&self, name: &HeaderName) -> Option<&HeaderValue> {
        self.map.get(name)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn as_map(&self) -> &HeaderMap {
        &self.map
    }

    pub fn into_map(self) -> HeaderMap {
        self.map
    }
}
