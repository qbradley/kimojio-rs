// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! gRPC metadata helpers.
//!
//! Metadata is represented as HTTP/2 headers or trailers with gRPC transport
//! headers filtered out. ASCII metadata must be inserted with [`Metadata::insert`].
//! Binary metadata names must end in `-bin` and are base64 encoded/decoded with
//! [`Metadata::insert_bin`] and [`Metadata::get_bin`].
//!
//! ```
//! use http::HeaderName;
//! use kimojio_stack_grpc::Metadata;
//!
//! let trace = HeaderName::from_static("trace-bin");
//! let mut metadata = Metadata::new();
//! metadata.insert_bin(trace.clone(), b"abc").unwrap();
//! assert_eq!(metadata.get_bin(&trace).unwrap().as_deref(), Some(&b"abc"[..]));
//! ```

use base64::{
    Engine,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
};
use http::{HeaderMap, HeaderName, HeaderValue};
use kimojio_stack_http::Trailers;

use crate::Error;

/// gRPC metadata wrapper.
///
/// Metadata is serialized on the wire as HTTP/2 header or trailer fields, but
/// this type only represents user-visible gRPC metadata. Reserved HTTP/gRPC
/// transport fields such as `content-type`, `te`, `grpc-status`, and
/// `grpc-message` are rejected when inserted directly and filtered when building
/// metadata from HTTP transport fields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Metadata {
    headers: HeaderMap,
}

impl Metadata {
    /// Creates an empty metadata block.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts ASCII metadata.
    ///
    /// Reserved gRPC transport names such as `content-type`, `grpc-status`, and
    /// `te` are rejected. Names ending in `-bin` must use
    /// [`insert_bin`](Self::insert_bin).
    pub fn insert(&mut self, name: HeaderName, value: HeaderValue) -> Result<(), Error> {
        validate_name(&name)?;
        if name.as_str().ends_with("-bin") {
            return Err(Error::Protocol("binary metadata requires insert_bin"));
        }
        self.headers.insert(name, value);
        Ok(())
    }

    /// Inserts binary metadata, base64-encoding `value`.
    ///
    /// The metadata name must end in `-bin`.
    pub fn insert_bin(&mut self, name: HeaderName, value: &[u8]) -> Result<(), Error> {
        validate_name(&name)?;
        if !name.as_str().ends_with("-bin") {
            return Err(Error::Protocol("binary metadata name must end with -bin"));
        }
        let encoded = STANDARD.encode(value);
        let value = HeaderValue::from_str(&encoded)
            .map_err(|_| Error::Protocol("invalid binary metadata"))?;
        self.headers.insert(name, value);
        Ok(())
    }

    /// Returns a raw metadata value.
    pub fn get(&self, name: &HeaderName) -> Option<&HeaderValue> {
        self.headers.get(name)
    }

    /// Returns decoded binary metadata.
    ///
    /// The metadata name must end in `-bin`. Both padded and unpadded base64
    /// encodings are accepted when decoding peer metadata.
    pub fn get_bin(&self, name: &HeaderName) -> Result<Option<Vec<u8>>, Error> {
        let Some(value) = self.headers.get(name) else {
            return Ok(None);
        };
        if !name.as_str().ends_with("-bin") {
            return Err(Error::Protocol("binary metadata name must end with -bin"));
        }
        decode_binary_header(value.as_bytes())
            .map(Some)
            .map_err(|_| Error::Protocol("invalid binary metadata"))
    }

    /// Borrows metadata as HTTP-compatible fields.
    ///
    /// This exposes only user metadata fields, not reserved gRPC transport
    /// fields such as `grpc-status`.
    pub fn as_http_headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Consumes metadata and returns HTTP-compatible fields.
    ///
    /// Use this when adding metadata to HTTP/2 request/response headers or
    /// terminal trailers. The returned map still represents gRPC metadata, not a
    /// general-purpose HTTP header block.
    pub fn into_http_headers(self) -> HeaderMap {
        self.headers
    }

    /// Backwards-compatible alias for [`as_http_headers`](Self::as_http_headers).
    pub fn as_headers(&self) -> &HeaderMap {
        self.as_http_headers()
    }

    /// Backwards-compatible alias for [`into_http_headers`](Self::into_http_headers).
    pub fn into_headers(self) -> HeaderMap {
        self.into_http_headers()
    }

    /// Builds gRPC metadata from HTTP/2 request or response headers.
    ///
    /// Reserved transport names are dropped. Invalid user metadata names return a
    /// protocol error.
    pub fn from_http_headers(headers: HeaderMap) -> Result<Self, Error> {
        Self::from_http_field_map(headers)
    }

    /// Builds gRPC trailing metadata from HTTP trailers.
    ///
    /// Reserved gRPC status trailer fields are filtered out, leaving only
    /// user-visible trailing metadata.
    pub fn from_http_trailers(trailers: Trailers) -> Result<Self, Error> {
        Self::from_http_field_map(trailers.into_map())
    }

    /// Backwards-compatible alias for [`from_http_headers`](Self::from_http_headers).
    pub fn from_headers(headers: HeaderMap) -> Result<Self, Error> {
        Self::from_http_headers(headers)
    }

    fn from_http_field_map(headers: HeaderMap) -> Result<Self, Error> {
        let mut metadata = HeaderMap::new();
        let mut current_name = None;
        for (name, value) in headers {
            if let Some(name) = name {
                current_name = Some(name);
            }
            let Some(name) = current_name.clone() else {
                return Err(Error::Protocol("metadata continuation header unsupported"));
            };
            if is_reserved_transport_name(&name) {
                continue;
            }
            validate_name(&name)?;
            metadata.append(name, value);
        }
        Ok(Self { headers: metadata })
    }
}

fn validate_name(name: &HeaderName) -> Result<(), Error> {
    let name = name.as_str();
    if name.starts_with(':')
        || name == "connection"
        || name == "te"
        || name == "transfer-encoding"
        || name == "upgrade"
        || name == "content-type"
        || name == "grpc-status"
        || name == "grpc-message"
        || name == "grpc-status-details-bin"
    {
        Err(Error::Protocol("reserved metadata name"))
    } else {
        Ok(())
    }
}

fn is_reserved_transport_name(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "content-type" | "te" | "grpc-status" | "grpc-message" | "grpc-status-details-bin"
    )
}

fn decode_binary_header(bytes: &[u8]) -> Result<Vec<u8>, base64::DecodeError> {
    STANDARD
        .decode(bytes)
        .or_else(|_| STANDARD_NO_PAD.decode(bytes))
}
