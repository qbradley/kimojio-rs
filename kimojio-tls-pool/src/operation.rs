// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::OperationResult;

/// Placement chosen for a submitted operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationPlacement {
    /// Operation ran on the submitting thread.
    Immediate,
    /// Operation was routed to a background executor.
    Background { executor: usize },
}

pub(crate) type OperationFn<T> = Box<dyn FnOnce() -> OperationResult<T> + Send + 'static>;

pub(crate) fn complete<T>(
    operation: OperationFn<T>,
    callback: crate::CompletionCallback<T>,
) -> bool {
    let result = operation();
    let success = result.is_ok();
    callback(result);
    success
}
