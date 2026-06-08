// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Diagnostics and counters for storage operations.
//!
//! Operation helpers return diagnostics on both success and failure paths so
//! callers can tag logs/metrics with operation class, attempt status, service
//! code, request ID, elapsed time, and retry classification.

use std::time::Duration;

/// Operation classification used for retry and observability.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum OperationClass {
    /// Page write or clear operation.
    PageWrite,
    /// Page read or written-range operation.
    PageRead,
    /// Metadata/container/properties operation.
    Metadata,
    /// Lease/ownership operation.
    Lease,
    /// Container listing operation.
    List,
    /// Snapshot lifecycle operation.
    Snapshot,
    /// Copy-from-source operation.
    Copy,
    /// Block object operation.
    Block,
    /// Archive object operation.
    Archive,
    /// Config object operation.
    Config,
}

impl OperationClass {
    /// Returns a stable operation dimension.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PageWrite => "page_write",
            Self::PageRead => "page_read",
            Self::Metadata => "metadata",
            Self::Lease => "lease",
            Self::List => "list",
            Self::Snapshot => "snapshot",
            Self::Copy => "copy",
            Self::Block => "block",
            Self::Archive => "archive",
            Self::Config => "config",
        }
    }
}

/// Per-attempt diagnostics.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AttemptDiagnostics {
    /// One-based attempt number.
    pub attempt: u32,
    /// HTTP status, if a response was received.
    pub status: Option<u16>,
    /// Service error code, if supplied.
    pub service_code: Option<String>,
    /// Service or client request ID.
    pub request_id: Option<String>,
    /// Attempt duration, if measured.
    pub elapsed: Option<Duration>,
    /// Whether this attempt looked retryable.
    pub retriable: bool,
}

/// Operation diagnostics accumulated for a request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnostics {
    operation: OperationClass,
    attempts: Vec<AttemptDiagnostics>,
}

impl Diagnostics {
    /// Creates diagnostics for an operation class.
    pub fn new(operation: OperationClass) -> Self {
        Self {
            operation,
            attempts: Vec::new(),
        }
    }

    /// Adds one attempt.
    pub fn push_attempt(&mut self, attempt: AttemptDiagnostics) {
        self.attempts.push(attempt);
    }

    /// Returns the operation class.
    pub fn operation(&self) -> OperationClass {
        self.operation
    }

    /// Returns attempts.
    pub fn attempts(&self) -> &[AttemptDiagnostics] {
        &self.attempts
    }
}

/// Request and allocation counters exposed for migration comparison.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RequestCounts {
    /// Number of request attempts observed.
    pub requests: u64,
    /// Number of those attempts that were retries.
    pub retries: u64,
    /// Number of allocation events observed by external instrumentation.
    pub allocations: u64,
    /// Total allocated bytes observed by external instrumentation.
    pub allocated_bytes: u64,
}

impl RequestCounts {
    /// Records one request attempt.
    pub fn record_request(&mut self, retry: bool) {
        self.requests += 1;
        if retry {
            self.retries += 1;
        }
    }

    /// Records allocation observations.
    pub fn record_allocation(&mut self, bytes: u64) {
        self.allocations += 1;
        self.allocated_bytes += bytes;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_class_formats_stably() {
        assert_eq!(OperationClass::PageWrite.as_str(), "page_write");
        assert_eq!(OperationClass::Block.as_str(), "block");
        assert_eq!(OperationClass::Config.as_str(), "config");
    }

    #[test]
    fn diagnostics_preserve_attempts() {
        let mut diagnostics = Diagnostics::new(OperationClass::Copy);
        diagnostics.push_attempt(AttemptDiagnostics {
            attempt: 2,
            status: Some(503),
            service_code: Some("Busy".into()),
            request_id: Some("req".into()),
            elapsed: Some(Duration::from_millis(5)),
            retriable: true,
        });

        assert_eq!(diagnostics.operation(), OperationClass::Copy);
        assert_eq!(diagnostics.attempts()[0].request_id.as_deref(), Some("req"));
    }
}
