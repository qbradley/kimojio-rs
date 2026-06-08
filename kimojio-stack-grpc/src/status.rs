// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::fmt;

use base64::{
    Engine,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
};
use bytes::Bytes;
use http::{HeaderName, HeaderValue};
use kimojio_stack_http::Trailers;

use crate::{Error, Metadata};

/// Trailer name carrying the numeric gRPC status code.
pub const GRPC_STATUS: HeaderName = HeaderName::from_static("grpc-status");
/// Trailer name carrying the percent-encoded gRPC status message.
pub const GRPC_MESSAGE: HeaderName = HeaderName::from_static("grpc-message");
/// Trailer name carrying base64-encoded rich status details.
pub const GRPC_STATUS_DETAILS_BIN: HeaderName = HeaderName::from_static("grpc-status-details-bin");

/// gRPC status code values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StatusCode {
    Ok = 0,
    Cancelled = 1,
    Unknown = 2,
    InvalidArgument = 3,
    DeadlineExceeded = 4,
    NotFound = 5,
    AlreadyExists = 6,
    PermissionDenied = 7,
    ResourceExhausted = 8,
    FailedPrecondition = 9,
    Aborted = 10,
    OutOfRange = 11,
    Unimplemented = 12,
    Internal = 13,
    Unavailable = 14,
    DataLoss = 15,
    Unauthenticated = 16,
}

impl StatusCode {
    /// Returns the wire numeric gRPC status code.
    pub const fn as_grpc_code(self) -> u8 {
        self as u8
    }

    /// Converts a wire numeric gRPC status code into a status enum.
    pub const fn from_grpc_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Ok),
            1 => Some(Self::Cancelled),
            2 => Some(Self::Unknown),
            3 => Some(Self::InvalidArgument),
            4 => Some(Self::DeadlineExceeded),
            5 => Some(Self::NotFound),
            6 => Some(Self::AlreadyExists),
            7 => Some(Self::PermissionDenied),
            8 => Some(Self::ResourceExhausted),
            9 => Some(Self::FailedPrecondition),
            10 => Some(Self::Aborted),
            11 => Some(Self::OutOfRange),
            12 => Some(Self::Unimplemented),
            13 => Some(Self::Internal),
            14 => Some(Self::Unavailable),
            15 => Some(Self::DataLoss),
            16 => Some(Self::Unauthenticated),
            _ => None,
        }
    }
}

/// gRPC status returned by calls, streams, and handlers.
///
/// Status values are serialized into terminal trailers. `message` is
/// percent-encoded according to gRPC rules, `details` is written to
/// `grpc-status-details-bin`, and custom metadata is merged into the trailer map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    code: StatusCode,
    message: String,
    details: Bytes,
    metadata: Box<Metadata>,
}

impl Status {
    /// Creates a status with no details or custom metadata.
    pub fn new(code: StatusCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: Bytes::new(),
            metadata: Box::new(Metadata::new()),
        }
    }

    /// Creates a status with binary rich details.
    pub fn with_details(code: StatusCode, message: impl Into<String>, details: Bytes) -> Self {
        Self::with_details_and_metadata(code, message, details, Metadata::new())
    }

    /// Creates a status with custom trailing metadata.
    pub fn with_metadata(code: StatusCode, message: impl Into<String>, metadata: Metadata) -> Self {
        Self::with_details_and_metadata(code, message, Bytes::new(), metadata)
    }

    /// Creates a status with details and custom trailing metadata.
    pub fn with_details_and_metadata(
        code: StatusCode,
        message: impl Into<String>,
        details: Bytes,
        metadata: Metadata,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            details,
            metadata: Box::new(metadata),
        }
    }

    /// Creates an OK status with an empty message.
    pub fn ok() -> Self {
        Self::new(StatusCode::Ok, "")
    }

    /// Returns the canonical gRPC status code.
    pub fn code(&self) -> StatusCode {
        self.code
    }

    /// Returns the decoded status message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns binary rich status details.
    pub fn details(&self) -> &[u8] {
        &self.details
    }

    /// Returns custom trailing metadata.
    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    /// Returns mutable custom trailing metadata.
    pub fn metadata_mut(&mut self) -> &mut Metadata {
        &mut self.metadata
    }

    /// Serializes the status into terminal gRPC trailers.
    pub fn to_trailers(&self) -> Result<Trailers, Error> {
        let mut trailers = Trailers::new();
        for (name, value) in self.metadata.as_ref().clone().into_http_headers() {
            let Some(name) = name else {
                return Err(Error::Protocol("metadata continuation header unsupported"));
            };
            trailers.insert(name, value);
        }
        trailers.insert(
            GRPC_STATUS,
            HeaderValue::from_str(&self.code.as_grpc_code().to_string())
                .map_err(|_| Error::Protocol("invalid grpc-status"))?,
        );
        if !self.message.is_empty() {
            trailers.insert(
                GRPC_MESSAGE,
                HeaderValue::from_str(&encode_grpc_message(&self.message))
                    .map_err(|_| Error::Protocol("invalid grpc-message"))?,
            );
        }
        if !self.details.is_empty() {
            let details = STANDARD.encode(&self.details);
            trailers.insert(
                GRPC_STATUS_DETAILS_BIN,
                HeaderValue::from_str(&details)
                    .map_err(|_| Error::Protocol("invalid grpc-status-details-bin"))?,
            );
        }
        Ok(trailers)
    }

    /// Parses status information from terminal gRPC trailers.
    ///
    /// Custom metadata remains available through [`metadata`](Self::metadata);
    /// reserved gRPC status fields are not included in that metadata view.
    pub fn from_trailers(trailers: &Trailers) -> Result<Self, Error> {
        let code = trailers
            .get(&GRPC_STATUS)
            .ok_or(Error::Protocol("missing grpc-status"))?
            .to_str()
            .map_err(|_| Error::Protocol("grpc-status is not ASCII"))?
            .parse::<u8>()
            .map_err(|_| Error::Protocol("invalid grpc-status"))?;
        let code =
            StatusCode::from_grpc_code(code).ok_or(Error::Protocol("unknown grpc-status"))?;
        let message = trailers
            .get(&GRPC_MESSAGE)
            .map(|value| {
                value
                    .to_str()
                    .map_err(|_| Error::Protocol("grpc-message is not ASCII"))
                    .and_then(decode_grpc_message)
            })
            .transpose()?
            .unwrap_or_default();
        let details = trailers
            .get(&GRPC_STATUS_DETAILS_BIN)
            .map(|value| {
                decode_binary_header(value.as_bytes())
                    .map(Bytes::from)
                    .map_err(|_| Error::Protocol("invalid grpc-status-details-bin"))
            })
            .transpose()?
            .unwrap_or_default();
        let metadata = Metadata::from_http_headers(trailers.as_map().clone())?;
        Ok(Self {
            code,
            message,
            details,
            metadata: Box::new(metadata),
        })
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "gRPC status {}: {}",
            self.code.as_grpc_code(),
            self.message
        )
    }
}

fn encode_grpc_message(message: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    let mut encoded = String::with_capacity(message.len());
    for byte in message.bytes() {
        if (0x20..=0x7e).contains(&byte) && byte != b'%' {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[(byte >> 4) as usize]));
            encoded.push(char::from(HEX[(byte & 0x0f) as usize]));
        }
    }
    encoded
}

fn decode_grpc_message(message: &str) -> Result<String, Error> {
    let bytes = message.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let Some(hex) = bytes.get(index + 1..index + 3) else {
                return Err(Error::Protocol("invalid grpc-message percent encoding"));
            };
            let high = hex_value(hex[0])
                .ok_or(Error::Protocol("invalid grpc-message percent encoding"))?;
            let low = hex_value(hex[1])
                .ok_or(Error::Protocol("invalid grpc-message percent encoding"))?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| Error::Protocol("grpc-message is not UTF-8"))
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode_binary_header(bytes: &[u8]) -> Result<Vec<u8>, base64::DecodeError> {
    STANDARD
        .decode(bytes)
        .or_else(|_| STANDARD_NO_PAD.decode(bytes))
}
