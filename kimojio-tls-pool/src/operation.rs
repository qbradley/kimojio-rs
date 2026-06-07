// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::os::fd::RawFd;
use std::panic::{AssertUnwindSafe, catch_unwind};

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

pub(crate) type OperationFn<T> = Box<dyn FnOnce() -> OperationResult<T> + Send + 'static>;

pub(crate) enum CompletionStatus {
    Succeeded,
    Failed,
    Panicked,
}

pub(crate) struct OperationWork<T> {
    kind: OperationKind,
    size: usize,
    operation: OperationFn<T>,
    callback: crate::CompletionCallback<T>,
}

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
