// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::os::fd::RawFd;
#[cfg(test)]
use std::panic::{AssertUnwindSafe, catch_unwind};

#[cfg(test)]
use crate::{OperationError, OperationResult};

/// Kind of TLS operation submitted to a stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OperationKind {
    /// Read/decrypt operation.
    Read { fd: RawFd },
    /// Write/encrypt operation.
    Write { fd: RawFd },
    /// Internal operation used by tests and future adapters.
    #[cfg(test)]
    Generic,
}

/// Placement chosen for a submitted operation.
///
/// This describes where ordinary operation work is executed. Readiness-resumed
/// operations may start with an immediate probe and later resume on an executor;
/// inspect [`crate::PoolStatsSnapshot::readiness_waits`] and
/// [`crate::PoolStatsSnapshot::readiness_resumed`] to see that behavior.
///
/// ```
/// use kimojio_tls_pool::OperationPlacement;
///
/// let placement = OperationPlacement::Background { executor: 2 };
/// match placement {
///     OperationPlacement::Immediate => {}
///     OperationPlacement::Background { executor } => assert_eq!(executor, 2),
/// }
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationPlacement {
    /// Operation ran on the submitting thread.
    Immediate,
    /// Operation was routed to a background executor.
    Background {
        /// Executor selected for background execution.
        executor: usize,
    },
}

#[cfg(test)]
pub(crate) type OperationFn<T> = Box<dyn FnOnce() -> OperationResult<T> + Send + 'static>;

pub(crate) enum CompletionStatus {
    Succeeded,
    Failed,
    Panicked,
}

#[cfg(test)]
pub(crate) struct OperationWork<T> {
    kind: OperationKind,
    size: usize,
    operation: OperationFn<T>,
    callback: crate::CompletionCallback<T>,
}

#[cfg(test)]
impl<T> OperationWork<T> {
    pub(crate) fn new(
        kind: OperationKind,
        size: usize,
        operation: OperationFn<T>,
        callback: crate::CompletionCallback<T>,
    ) -> Self {
        Self {
            kind,
            size,
            operation,
            callback,
        }
    }

    pub(crate) fn kind(&self) -> OperationKind {
        self.kind
    }

    pub(crate) fn size(&self) -> usize {
        self.size
    }

    pub(crate) fn into_parts(self) -> (OperationFn<T>, crate::CompletionCallback<T>) {
        (self.operation, self.callback)
    }
}

#[cfg(test)]
pub(crate) fn complete<T>(
    operation: OperationFn<T>,
    callback: crate::CompletionCallback<T>,
) -> CompletionStatus {
    let result = match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(result) => result,
        Err(_) => Err(OperationError::Panic("operation")),
    };
    let success = result.is_ok();

    match catch_unwind(AssertUnwindSafe(|| callback(result))) {
        Ok(()) if success => CompletionStatus::Succeeded,
        Ok(()) => CompletionStatus::Failed,
        Err(_) => CompletionStatus::Panicked,
    }
}
