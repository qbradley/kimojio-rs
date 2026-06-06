// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::fmt;
use std::time::Duration;

use crate::RuntimeContext;

/// A condition that can wake stackful coroutines when it becomes ready.
///
/// `Waitable` is intentionally readiness-only. Heterogeneous waits can include
/// mutex acquisition, I/O completion, and task completion, so there is no single
/// useful value type for the wait operation itself. Callers wait by reference and
/// then use the specific typed API, such as `try_lock`, `try_get`, or
/// `try_join`, to consume the result.
pub trait Waitable {
    /// Returns whether this condition can be consumed without parking.
    fn is_ready(&self) -> bool;

    #[doc(hidden)]
    fn add_waiter(&self, cx: &RuntimeContext<'_>);
}

/// Backwards-compatible marker for I/O handles.
pub trait IoHandle: Waitable {}

impl<T> IoHandle for T where T: Waitable + ?Sized {}

/// Error returned when waiting for [`Waitable`] values fails.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitError {
    /// The timeout expired before the requested condition became ready.
    TimedOut,
    /// `wait_any` was called without any waitables.
    Empty,
}

/// Backwards-compatible name for errors from `RuntimeContext::join`.
pub type IoJoinError = WaitError;

impl fmt::Display for WaitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimedOut => f.write_str("timed out waiting for stackful condition"),
            Self::Empty => f.write_str("no waitables supplied"),
        }
    }
}

impl std::error::Error for WaitError {}

impl RuntimeContext<'_> {
    /// Waits until every waitable is ready.
    ///
    /// This takes waitables by reference so heterogeneous typed handles can be
    /// waited on together without consuming their values. After this returns,
    /// use each waitable's typed `try_*`/`get`/`join` API to consume its value or
    /// guard.
    pub fn wait_all(
        &self,
        waitables: &[&dyn Waitable],
        timeout: Option<Duration>,
    ) -> Result<(), WaitError> {
        if waitables.iter().all(|waitable| waitable.is_ready()) {
            return Ok(());
        }

        let mut timeout = timeout.map(|duration| self.submit_timeout(duration));

        loop {
            if waitables.iter().all(|waitable| waitable.is_ready()) {
                if let Some(timeout) = timeout.take() {
                    self.cancel_timeout(timeout);
                }
                return Ok(());
            }

            if timeout
                .as_ref()
                .is_some_and(|timeout| timeout.state.is_ready())
            {
                if let Some(timeout) = timeout.take() {
                    self.recycle_io_state(timeout.state);
                }
                return Err(WaitError::TimedOut);
            }

            for waitable in waitables {
                waitable.add_waiter(self);
            }
            if let Some(timeout) = &timeout {
                timeout.state.add_waiter_from(self);
            }
            self.park();
        }
    }

    /// Waits until any waitable is ready and returns its index.
    ///
    /// If multiple waitables are already ready, the lowest index is returned.
    pub fn wait_any(
        &self,
        waitables: &[&dyn Waitable],
        timeout: Option<Duration>,
    ) -> Result<usize, WaitError> {
        if waitables.is_empty() {
            return Err(WaitError::Empty);
        }

        if let Some(index) = first_ready(waitables) {
            return Ok(index);
        }

        let mut timeout = timeout.map(|duration| self.submit_timeout(duration));

        loop {
            if let Some(index) = first_ready(waitables) {
                if let Some(timeout) = timeout.take() {
                    self.cancel_timeout(timeout);
                }
                return Ok(index);
            }

            if timeout
                .as_ref()
                .is_some_and(|timeout| timeout.state.is_ready())
            {
                if let Some(timeout) = timeout.take() {
                    self.recycle_io_state(timeout.state);
                }
                return Err(WaitError::TimedOut);
            }

            for waitable in waitables {
                waitable.add_waiter(self);
            }
            if let Some(timeout) = &timeout {
                timeout.state.add_waiter_from(self);
            }
            self.park();
        }
    }

    /// Alias for [`RuntimeContext::wait_any`].
    pub fn select(
        &self,
        waitables: &[&dyn Waitable],
        timeout: Option<Duration>,
    ) -> Result<usize, WaitError> {
        self.wait_any(waitables, timeout)
    }

    /// Alias for [`RuntimeContext::wait_all`].
    pub fn join(
        &self,
        waitables: &[&dyn Waitable],
        timeout: Option<Duration>,
    ) -> Result<(), WaitError> {
        self.wait_all(waitables, timeout)
    }
}

fn first_ready(waitables: &[&dyn Waitable]) -> Option<usize> {
    waitables.iter().position(|waitable| waitable.is_ready())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rustix::pipe::pipe;

    use crate::{Mutex, Runtime, WaitError, Waitable};

    #[test]
    fn wait_any_can_wait_for_mutex_acquire_or_task_completion() {
        let mutex = Mutex::new(0);
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let guard = mutex.try_lock().unwrap();

            cx.scope(|scope| {
                let unlocker = scope.spawn(move |cx| {
                    cx.yield_now();
                    drop(guard);
                    7
                });

                let waitables: [&dyn Waitable; 2] = [&mutex, &unlocker];
                let index = cx
                    .wait_any(&waitables, Some(Duration::from_secs(1)))
                    .unwrap();
                assert!(index == 0 || index == 1);

                let mut guard = mutex.try_lock().unwrap();
                *guard = 35;
                drop(guard);

                assert_eq!(unlocker.try_join().unwrap().unwrap(), 7);
            });
        });

        assert_eq!(*mutex.try_lock().unwrap(), 35);
    }

    #[test]
    fn wait_all_can_wait_for_io_and_spawned_task_completion() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let (read_fd, write_fd) = pipe().unwrap();
            let mut read = cx.read_async(&read_fd, vec![0]);

            cx.scope(|scope| {
                let writer = scope.spawn(move |cx| {
                    assert_eq!(cx.write(&write_fd, &[42]).unwrap(), 1);
                    cx.close(write_fd).unwrap();
                    5
                });

                let waitables: [&dyn Waitable; 2] = [&read, &writer];
                cx.wait_all(&waitables, Some(Duration::from_secs(1)))
                    .unwrap();

                assert_eq!(writer.try_join().unwrap().unwrap(), 5);
                let read_output = read.try_get().unwrap().unwrap();
                assert_eq!(read_output.buffer[0], 42);
            });

            cx.close(read_fd).unwrap();
        });
    }

    #[test]
    fn wait_any_reports_empty_inputs() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            assert_eq!(cx.wait_any(&[], None), Err(WaitError::Empty));
            assert_eq!(cx.select(&[], None), Err(WaitError::Empty));
        });
    }
}
