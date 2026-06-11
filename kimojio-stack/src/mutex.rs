// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Cooperative mutual exclusion for stackful coroutines.
//!
//! [`Mutex`] protects data shared by coroutines running on the same
//! [`Runtime`](crate::Runtime). It is not an OS blocking primitive: [`Mutex::lock`]
//! records the current coroutine as a waiter and parks it, allowing the runtime
//! to run other stackful work until the guard is dropped.
//!
//! Use this when the protected data is only accessed from one stack runtime. For
//! cross-thread sharing, put an ordinary `std::sync` primitive at the boundary or
//! use [`channel::cross_thread`](crate::channel::cross_thread).
//!
//! ```
//! use kimojio_stack::{Mutex, Runtime};
//!
//! let value = Mutex::new(0);
//! let mut runtime = Runtime::new();
//! runtime.block_on(|cx| {
//!     cx.scope(|scope| {
//!         scope.spawn(|cx| {
//!             *value.lock(cx) += 1;
//!         });
//!         scope.spawn(|cx| {
//!             *value.lock(cx) += 10;
//!         });
//!     });
//! });
//!
//! assert_eq!(*value.try_lock().unwrap(), 11);
//! ```

use std::cell::{RefCell, UnsafeCell};
use std::fmt;
use std::ops::{Deref, DerefMut};

use crate::{RuntimeContext, WaitRegistration, Waitable, Waiters};

/// A cooperative mutex for stackful coroutines.
pub struct Mutex<T> {
    state: RefCell<State>,
    value: UnsafeCell<T>,
}

impl<T> Mutex<T> {
    /// Creates a new unlocked mutex.
    pub fn new(value: T) -> Self {
        Self {
            state: RefCell::new(State {
                locked: false,
                waiters: Waiters::default(),
            }),
            value: UnsafeCell::new(value),
        }
    }

    /// Attempts to acquire the mutex without parking.
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        let mut state = self.state.borrow_mut();
        if state.locked {
            return None;
        }

        state.locked = true;
        Some(MutexGuard { mutex: self })
    }

    /// Acquires the mutex, parking the current stackful coroutine if necessary.
    pub fn lock(&self, cx: &RuntimeContext<'_>) -> MutexGuard<'_, T> {
        loop {
            if let Some(guard) = self.try_lock() {
                return guard;
            }

            let registration = cx.wait_registration();
            if let Some(waiter) = cx.waiter(&registration) {
                self.state.borrow_mut().waiters.push(waiter);
            }
            cx.park();
        }
    }

    fn unlock(&self) {
        let mut state = self.state.borrow_mut();
        debug_assert!(state.locked, "unlocking an unlocked mutex");
        state.locked = false;
        state.waiters.wake_one();
    }
}

impl<T> Waitable for Mutex<T> {
    fn is_ready(&self) -> bool {
        !self.state.borrow().locked
    }

    fn add_waiter(&self, cx: &RuntimeContext<'_>, registration: &WaitRegistration) {
        if !self.is_ready()
            && let Some(waiter) = cx.waiter(registration)
        {
            self.state.borrow_mut().waiters.push(waiter);
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for Mutex<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mutex").finish_non_exhaustive()
    }
}

impl<T> From<T> for Mutex<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

/// A guard that releases its mutex when dropped.
pub struct MutexGuard<'a, T> {
    mutex: &'a Mutex<T>,
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.mutex.unlock();
    }
}

impl<T: fmt::Debug> fmt::Debug for MutexGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

struct State {
    locked: bool,
    waiters: Waiters,
}

#[cfg(test)]
mod tests {
    use crate::{Mutex, Runtime};

    #[test]
    fn mutex_serializes_stackful_tasks() {
        let mutex = Mutex::new(0);
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let first = scope.spawn(|cx| {
                    let mut guard = mutex.lock(cx);
                    *guard += 1;
                    cx.yield_now();
                    *guard += 1;
                });
                let second = scope.spawn(|cx| {
                    let mut guard = mutex.lock(cx);
                    *guard += 10;
                });

                first.join(cx);
                second.join(cx);
            });
        });

        assert_eq!(*mutex.try_lock().unwrap(), 12);
    }
}
