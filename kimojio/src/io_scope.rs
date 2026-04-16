// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation details for [`io_scope`](super::io_scope).
//!
//! The public API lives in [`operations`](super) — this module contains the
//! future types, guards, and helpers that make io_scope work.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use crate::task::{IoScopeCompletions, TaskState};
use crate::task_ref::wake_task;
use crate::{Completion, CompletionState};

/// Cancels all I/O in `gathered` and returns the remaining incomplete
/// completions that still need to drain. Waits are cancelled immediately.
pub(crate) fn begin_cancel(gathered: &mut IoScopeCompletions) -> Vec<Rc<Completion>> {
    let mut task_state = TaskState::get();

    // Remove already-completed completions.
    retain_incomplete(&mut gathered.completions, &mut task_state);

    // Cancel the remaining ones.
    for completion in &gathered.completions {
        completion.cancel(&mut task_state);
    }

    // Some cancels complete synchronously (e.g. Idle ops), so drain again.
    retain_incomplete(&mut gathered.completions, &mut task_state);

    // Cancel all waits.
    for wait in gathered.waits.drain(..) {
        wait.canceled.set(true);
        wait.waker.use_mut(|waker| {
            if let Some(waker) = waker {
                wake_task(&mut task_state, waker)
            }
        });
    }

    std::mem::take(&mut gathered.completions)
}

/// Synchronously blocks until all completions in `draining` have finished.
/// Only used in Drop paths where returning Pending is not possible.
pub(crate) fn blocking_drain(draining: &mut Vec<Rc<Completion>>) {
    let mut task_state = TaskState::get();
    while !draining.is_empty() {
        task_state = crate::runtime::submit_and_complete_io_all(task_state, true);
        retain_incomplete(draining, &mut task_state);
    }
}

pin_project_lite::pin_project! {
    /// Catches any panics that occur while polling the future.
    pub(crate) struct CatchUnwindFuture<F: Future> {
        #[pin]
        f: F,
    }
}

impl<F: Future> CatchUnwindFuture<F> {
    pub(crate) fn new(f: F) -> Self {
        Self { f }
    }
}

impl<F: Future> Future for CatchUnwindFuture<F> {
    type Output = Result<F::Output, Box<dyn std::any::Any + Send>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        match std::panic::catch_unwind(AssertUnwindSafe(|| this.f.poll(cx))) {
            Ok(Poll::Ready(result)) => Poll::Ready(Ok(result)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

/// Guard that cancels any remaining I/O completions on drop.
/// This ensures that if an `IoScopeFuture` is dropped while pending
/// (e.g., a `select!` branch loses), captured I/O is properly cancelled.
pub(crate) struct IoScopeCompletionsGuard {
    /// Active scope completions (present while the inner future is running).
    pub(crate) scope: Option<IoScopeCompletions>,
    /// Completions that have been cancelled but not yet confirmed by io_uring.
    pub(crate) draining: Vec<Rc<Completion>>,
    /// Panic payload stored when the inner future panicked while draining.
    /// Resumed after the synchronous drain in Drop completes.
    pub(crate) panic_payload: Option<Box<dyn std::any::Any + Send>>,
}

impl Drop for IoScopeCompletionsGuard {
    fn drop(&mut self) {
        // Handle active scope completions (future dropped while Running).
        if let Some(mut completions) = self.scope.take()
            && (!completions.completions.is_empty() || !completions.waits.is_empty())
        {
            // Temporarily install on the task, cancel, and collect draining.
            let task_state = TaskState::get();
            let task = task_state.get_current_task();
            let outer = task.replace_io_scope_completions(Some(completions));
            drop(task_state);

            // Take the gathered completions back from the task.
            let task_state = TaskState::get();
            let task = task_state.get_current_task();
            completions = task.replace_io_scope_completions(outer).unwrap_or_default();
            drop(task_state);

            self.draining = begin_cancel(&mut completions);
        }

        // Synchronously drain any remaining completions.
        if !self.draining.is_empty() {
            blocking_drain(&mut self.draining);
        }

        // If the inner future panicked and the scope was dropped while
        // draining, propagate the panic now that all I/O is cleaned up.
        if let Some(payload) = self.panic_payload.take()
            && !std::thread::panicking()
        {
            std::panic::resume_unwind(payload);
        }
    }
}

pin_project_lite::pin_project! {
    /// Future returned by [`io_scope`](super::io_scope). Installs its
    /// completions list into the task only while the inner future is being
    /// polled, ensuring that sibling futures in combinators like `join!` do
    /// not have their I/O captured by this scope.
    ///
    /// When the inner future completes, any captured I/O is cancelled and
    /// the scope asynchronously waits for the cancellation to finish before
    /// returning Ready.
    ///
    /// Field order matters: `inner` is declared before `completions` so that
    /// the inner future (and any nested scopes) is dropped before this
    /// scope's completions guard runs.
    pub(crate) struct IoScopeFuture<F: Future> {
        #[pin]
        inner: CatchUnwindFuture<F>,
        completions: IoScopeCompletionsGuard,
        result: Option<F::Output>,
    }
}

impl<F: Future> IoScopeFuture<F> {
    pub(crate) fn new(f: F) -> Self {
        Self {
            inner: CatchUnwindFuture::new(f),
            completions: IoScopeCompletionsGuard {
                scope: Some(IoScopeCompletions::default()),
                draining: Vec::new(),
                panic_payload: None,
            },
            result: None,
        }
    }
}

impl<F: Future> Future for IoScopeFuture<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        // Draining state: the inner future completed and we are waiting
        // for cancelled I/O to finish.
        if this.result.is_some() || this.completions.panic_payload.is_some() {
            {
                let mut task_state = TaskState::get();
                retain_incomplete(&mut this.completions.draining, &mut task_state);
            }

            if this.completions.draining.is_empty() {
                // Panic path: resume the panic now that I/O is drained.
                if let Some(payload) = this.completions.panic_payload.take() {
                    std::panic::resume_unwind(payload);
                }
                return Poll::Ready(this.result.take().unwrap());
            }

            // Not done yet. Wake so the runtime processes CQEs and
            // re-polls us.
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }

        // Running state: install this scope's completions into the task
        // for the duration of the inner poll. Drop TaskState before
        // polling to avoid recursive borrow panics.
        let outer = {
            let task_state = TaskState::get();
            let task = task_state.get_current_task();
            task.replace_io_scope_completions(this.completions.scope.take())
        };

        let result = this.inner.poll(cx);

        match result {
            Poll::Ready(result) => {
                // The inner future completed. Restore the outer scope's
                // completions and cancel all I/O captured by this scope.
                let mut gathered = {
                    let task_state = TaskState::get();
                    let task = task_state.get_current_task();
                    task.replace_io_scope_completions(outer).unwrap_or_default()
                };

                let remaining = begin_cancel(&mut gathered);

                // Split the CatchUnwindFuture result into value vs panic.
                let (value, panic) = match result {
                    Ok(val) => (Some(val), None),
                    Err(payload) => (None, Some(payload)),
                };

                if remaining.is_empty() {
                    // All I/O already finished — return immediately.
                    if let Some(payload) = panic {
                        std::panic::resume_unwind(payload);
                    }
                    return Poll::Ready(value.unwrap());
                }

                // Transition to draining state.
                *this.result = value;
                this.completions.panic_payload = panic;
                this.completions.draining = remaining;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Poll::Pending => {
                // Swap our completions back from the task so that sibling
                // futures (e.g., in a join!) don't have their I/O captured
                // by this scope.
                let task_state = TaskState::get();
                let task = task_state.get_current_task();
                this.completions.scope = task.replace_io_scope_completions(outer);

                Poll::Pending
            }
        }
    }
}

fn retain_incomplete(vec: &mut Vec<Rc<Completion>>, task_state: &mut TaskState) {
    let mut index = 0;
    while index < vec.len() {
        let completion = vec.get(index).unwrap();
        let retain = completion.state.use_mut(|state| {
            matches!(
                state,
                CompletionState::Idle { .. } | CompletionState::Submitted { .. }
            )
        });
        if !retain {
            let completion = vec.swap_remove(index);
            task_state.return_completion(completion);
        } else {
            index += 1;
        }
    }
}
