// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::time::Duration;

/// Operation classification used for retry and observability.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum OperationClass {
    PageWrite,
    PageRead,
    Metadata,
    Lease,
    List,
    Snapshot,
    Copy,
    Block,
    Archive,
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
    pub attempt: u32,
    pub status: Option<u16>,
    pub service_code: Option<String>,
    pub request_id: Option<String>,
    pub elapsed: Option<Duration>,
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
    pub requests: u64,
    pub retries: u64,
    pub allocations: u64,
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
