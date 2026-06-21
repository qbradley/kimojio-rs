// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Structured routing and extraction rejections.

use std::error::Error;
use std::fmt;

use http::StatusCode;
use kimojio_stack_http::Body;

/// Category for a routing or extraction rejection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RejectionKind {
    /// No route matched the request.
    NotFound,
    /// Route existed but not for the requested method.
    MethodNotAllowed,
    /// Path parameters were absent or malformed.
    Path,
    /// Query string extraction failed.
    Query,
    /// Required request header was missing or invalid.
    Header,
    /// Request state was absent or had the wrong type.
    State,
    /// Request extension was absent or had the wrong type.
    Extension,
    /// Request body extraction failed.
    Body,
    /// Handler returned an application rejection.
    Handler,
}

/// Routing, extraction, or handler rejection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Rejection {
    kind: RejectionKind,
    message: String,
}

impl Rejection {
    /// Creates a rejection with `kind` and diagnostic message.
    pub fn new(kind: RejectionKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Creates a not-found rejection.
    pub fn not_found() -> Self {
        Self::new(RejectionKind::NotFound, "route not found")
    }

    /// Creates a method-not-allowed rejection.
    pub fn method_not_allowed() -> Self {
        Self::new(RejectionKind::MethodNotAllowed, "method not allowed")
    }

    /// Returns the rejection category.
    pub const fn kind(&self) -> RejectionKind {
        self.kind
    }

    /// Returns the diagnostic message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the HTTP status associated with this rejection.
    pub const fn status(&self) -> StatusCode {
        match self.kind {
            RejectionKind::NotFound => StatusCode::NOT_FOUND,
            RejectionKind::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
            RejectionKind::Path
            | RejectionKind::Query
            | RejectionKind::Header
            | RejectionKind::Body => StatusCode::BAD_REQUEST,
            RejectionKind::State | RejectionKind::Extension | RejectionKind::Handler => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }
    }

    /// Converts this rejection into a plain-text response.
    pub fn into_response(self) -> http::Response<Body> {
        http::Response::builder()
            .status(self.status())
            .body(
                Body::copy_from_slice(self.message.as_bytes(), Default::default())
                    .expect("rejection message should fit default body limits"),
            )
            .expect("valid rejection response")
    }
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.status(), self.message)
    }
}

impl Error for Rejection {}
