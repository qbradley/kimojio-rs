// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use base64::{
    Engine,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
};
use http::{HeaderMap, HeaderName, HeaderValue};

use crate::Error;

/// Low-level gRPC metadata wrapper over HTTP headers or trailers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Metadata {
    headers: HeaderMap,
}

impl Metadata {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, name: HeaderName, value: HeaderValue) -> Result<(), Error> {
        validate_name(&name)?;
        if name.as_str().ends_with("-bin") {
            return Err(Error::Protocol("binary metadata requires insert_bin"));
        }
        self.headers.insert(name, value);
        Ok(())
    }

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

    pub fn get(&self, name: &HeaderName) -> Option<&HeaderValue> {
        self.headers.get(name)
    }

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

    pub fn as_headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub fn into_headers(self) -> HeaderMap {
        self.headers
    }

    pub fn from_headers(headers: HeaderMap) -> Result<Self, Error> {
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
