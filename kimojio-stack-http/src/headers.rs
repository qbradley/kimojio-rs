// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Small wrappers around `http::HeaderMap` for public HTTP headers and trailers.
//!
//! The wrappers keep type names stable across the stack crates while still
//! allowing callers to inspect or take the underlying `HeaderMap` when needed.
//! They model HTTP transport fields only. Higher-level protocols such as gRPC
//! translate these into their own concepts (for example gRPC metadata and
//! status) in their own crates.

use http::{HeaderMap, HeaderName, HeaderValue};

/// HTTP header block used by requests and responses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Headers {
    map: HeaderMap,
}

impl Headers {
    /// Creates an empty header block.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or replaces a header value.
    pub fn insert(&mut self, name: HeaderName, value: HeaderValue) {
        self.map.insert(name, value);
    }

    /// Returns a header value by name.
    pub fn get(&self, name: &HeaderName) -> Option<&HeaderValue> {
        self.map.get(name)
    }

    /// Returns the number of header entries.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Returns whether no headers are present.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Borrows the underlying header map.
    pub fn as_map(&self) -> &HeaderMap {
        &self.map
    }

    /// Consumes the wrapper and returns the underlying header map.
    pub fn into_map(self) -> HeaderMap {
        self.map
    }
}

/// HTTP trailer block used by responses.
///
/// This is a transport-level HTTP trailer map. gRPC status and trailing
/// metadata are encoded into HTTP trailers by `kimojio-stack-grpc`; HTTP callers
/// should not need to understand those gRPC-specific names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Trailers {
    map: HeaderMap,
}

impl Trailers {
    /// Creates an empty trailer block.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or replaces a trailer value.
    pub fn insert(&mut self, name: HeaderName, value: HeaderValue) {
        self.map.insert(name, value);
    }

    /// Returns a trailer value by name.
    pub fn get(&self, name: &HeaderName) -> Option<&HeaderValue> {
        self.map.get(name)
    }

    /// Returns the number of trailer entries.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Returns whether no trailers are present.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Borrows the underlying trailer map.
    pub fn as_map(&self) -> &HeaderMap {
        &self.map
    }

    /// Consumes the wrapper and returns the underlying trailer map.
    pub fn into_map(self) -> HeaderMap {
        self.map
    }
}
