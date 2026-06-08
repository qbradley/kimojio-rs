// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::{error, fmt};

/// Stable category for storage client failures.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ErrorKind {
    Auth,
    Authorization,
    NotFound,
    AlreadyExists,
    BeingDeleted,
    ConditionNotMet,
    Ownership,
    SequenceNumber,
    Range,
    Timeout,
    Unavailable,
    MetadataFormat,
    Corruption,
    Incomplete,
    Transport,
    UnhandledService,
}

/// Inspectable storage client error.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    service_code: Option<String>,
    request_id: Option<String>,
}

impl Error {
    /// Creates an error with a stable category and message.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            service_code: None,
            request_id: None,
        }
    }

    /// Adds a service error code.
    pub fn with_service_code(mut self, code: impl Into<String>) -> Self {
        self.service_code = Some(code.into());
        self
    }

    /// Adds a service request id.
    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }

    /// Returns the stable category.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Returns the message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the service error code, if present.
    pub fn service_code(&self) -> Option<&str> {
        self.service_code.as_deref()
    }

    /// Returns the service request id, if present.
    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    /// Classifies a storage response status and optional service code.
    pub fn classify_status(status: u16, service_code: Option<&str>) -> ErrorKind {
        match (status, service_code) {
            (401, _) => ErrorKind::Auth,
            (403, _) => ErrorKind::Authorization,
            (404, _) => ErrorKind::NotFound,
            (_, Some(code)) if code.contains("Timeout") || code.contains("OperationTimedOut") => {
                ErrorKind::Timeout
            }
            (_, Some(code)) if code.contains("SequenceNumber") => ErrorKind::SequenceNumber,
            (_, Some(code)) if code.contains("Lease") => ErrorKind::Ownership,
            (409, Some(code)) if code.contains("BeingDeleted") => ErrorKind::BeingDeleted,
            (409, Some(code)) if code.contains("AlreadyExists") => ErrorKind::AlreadyExists,
            (409, _) => ErrorKind::UnhandledService,
            (412, _) => ErrorKind::ConditionNotMet,
            (416, _) => ErrorKind::Range,
            (500..=599, _) => ErrorKind::Unavailable,
            _ => ErrorKind::UnhandledService,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "storage {:?}: {}", self.kind, self.message)?;
        if let Some(code) = &self.service_code {
            write!(f, " code={code}")?;
        }
        if let Some(request_id) = &self.request_id {
            write!(f, " request_id={request_id}")?;
        }
        Ok(())
    }
}

impl error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_preserves_kind_code_and_request_id() {
        let error = Error::new(ErrorKind::ConditionNotMet, "etag mismatch")
            .with_service_code("ConditionNotMet")
            .with_request_id("abc");

        assert_eq!(error.kind(), ErrorKind::ConditionNotMet);
        assert_eq!(error.service_code(), Some("ConditionNotMet"));
        assert_eq!(error.request_id(), Some("abc"));
        assert!(error.to_string().contains("request_id=abc"));
    }

    #[test]
    fn status_and_service_code_classify_errors() {
        assert_eq!(Error::classify_status(401, None), ErrorKind::Auth);
        assert_eq!(Error::classify_status(403, None), ErrorKind::Authorization);
        assert_eq!(Error::classify_status(404, None), ErrorKind::NotFound);
        assert_eq!(
            Error::classify_status(409, Some("BlobAlreadyExists")),
            ErrorKind::AlreadyExists
        );
        assert_eq!(
            Error::classify_status(409, Some("ContainerBeingDeleted")),
            ErrorKind::BeingDeleted
        );
        assert_eq!(
            Error::classify_status(409, Some("LeaseAlreadyPresent")),
            ErrorKind::Ownership
        );
        assert_eq!(
            Error::classify_status(412, Some("LeaseIdMismatchWithBlobOperation")),
            ErrorKind::Ownership
        );
        assert_eq!(
            Error::classify_status(412, None),
            ErrorKind::ConditionNotMet
        );
        assert_eq!(Error::classify_status(416, None), ErrorKind::Range);
        assert_eq!(Error::classify_status(503, None), ErrorKind::Unavailable);
        assert_eq!(
            Error::classify_status(412, Some("SequenceNumberConditionNotMet")),
            ErrorKind::SequenceNumber
        );
        assert_eq!(
            Error::classify_status(500, Some("OperationTimedOut")),
            ErrorKind::Timeout
        );
    }
}
