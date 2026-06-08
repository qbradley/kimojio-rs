// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Cooperative counting semaphore for stackful coroutines.
//!
//! [`Semaphore::acquire`] parks only the current coroutine when no permit is
//! available. Dropping the returned [`SemaphorePermit`] releases one permit and
//! wakes a waiter. The primitive is useful for making concurrency limits
//! explicit around transports, request fan-out, or other scarce resources inside
//! a single stack runtime.
//!
//! ```
//! use kimojio_stack::{Runtime, Semaphore};
//!
//! let limiter = Semaphore::new(1);
//! let mut runtime = Runtime::new();
//! runtime.block_on(|cx| {
//!     cx.scope(|scope| {
//!         scope.spawn(|cx| {
//!             let _permit = limiter.acquire(cx);
//!             cx.yield_now();
//!         });
//!         scope.spawn(|cx| {
//!             let _permit = limiter.acquire(cx);
//!         });
//!     });
//! });
//! assert_eq!(limiter.available_permits(), 1);
//! ```

use std::cell::RefCell;
use std::fmt;

use crate::{RuntimeContext, Waitable, Waiters};

/// A counting semaphore for stackful coroutines.
pub struct Semaphore {
    state: RefCell<State>,
}

impl Semaphore {
    /// Creates a semaphore with `permits` initially available permits.
    pub fn new(permits: usize) -> Self {
        Self {
            state: RefCell::new(State {
                permits,
                waiters: Waiters::default(),
            }),
        }
    }

    /// Returns the number of currently available permits.
    pub fn available_permits(&self) -> usize {
        self.state.borrow().permits
    }

    /// Adds permits and wakes waiting coroutines.
    pub fn add_permits(&self, permits: usize) {
        let mut state = self.state.borrow_mut();
        state.permits = state
            .permits
            .checked_add(permits)
            .expect("semaphore permit count overflow");
        for _ in 0..permits {
            state.waiters.wake_one();
        }
    }

    /// Attempts to acquire one permit without parking.
    pub fn try_acquire(&self) -> Option<SemaphorePermit<'_>> {
        let mut state = self.state.borrow_mut();
        if state.permits == 0 {
            return None;
        }

        state.permits -= 1;
        Some(SemaphorePermit { semaphore: self })
    }

    /// Acquires one permit, parking the current stackful coroutine if necessary.
    pub fn acquire(&self, cx: &RuntimeContext<'_>) -> SemaphorePermit<'_> {
        loop {
            if let Some(permit) = self.try_acquire() {
                return permit;
            }

            if let Some(waiter) = cx.waiter() {
                self.state.borrow_mut().waiters.push(waiter);
            }
            cx.park();
        }
    }

    fn release(&self) {
        self.add_permits(1);
    }
}

impl Waitable for Semaphore {
    fn is_ready(&self) -> bool {
        self.state.borrow().permits != 0
    }

    fn add_waiter(&self, cx: &RuntimeContext<'_>) {
        if !self.is_ready()
            && let Some(waiter) = cx.waiter()
        {
            self.state.borrow_mut().waiters.push(waiter);
        }
    }
}

impl fmt::Debug for Semaphore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Semaphore")
            .field("available_permits", &self.available_permits())
            .finish()
    }
}

/// A semaphore permit that is released when dropped.
pub struct SemaphorePermit<'a> {
    semaphore: &'a Semaphore,
}

impl Drop for SemaphorePermit<'_> {
    fn drop(&mut self) {
        self.semaphore.release();
    }
}

impl fmt::Debug for SemaphorePermit<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SemaphorePermit").finish_non_exhaustive()
    }
}

struct State {
    permits: usize,
    waiters: Waiters,
}

#[cfg(test)]
mod tests {
    use crate::{Runtime, Semaphore};

    #[test]
    fn semaphore_limits_concurrent_stackful_tasks() {
        let semaphore = Semaphore::new(1);
        let mut runtime = Runtime::new();

        let output = runtime.block_on(|cx| {
            cx.scope(|scope| {
                let first = scope.spawn(|cx| {
                    let _permit = semaphore.acquire(cx);
                    cx.yield_now();
                    1
                });
                let second = scope.spawn(|cx| {
                    let _permit = semaphore.acquire(cx);
                    10
                });

                first.join(cx) + second.join(cx)
            })
        });

        assert_eq!(output, 11);
        assert_eq!(semaphore.available_permits(), 1);
    }
}
