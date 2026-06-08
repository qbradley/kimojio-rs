// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::cell::{RefCell, UnsafeCell};
use std::fmt;
use std::ops::{Deref, DerefMut};

use crate::{RuntimeContext, Waitable, Waiters};

/// A cooperative reader/writer lock for stackful coroutines.
pub struct RwLock<T> {
    state: RefCell<State>,
    value: UnsafeCell<T>,
}

impl<T> RwLock<T> {
    /// Creates a new unlocked reader/writer lock.
    pub fn new(value: T) -> Self {
        Self {
            state: RefCell::new(State {
                readers: 0,
                writer: false,
                waiters: Waiters::default(),
            }),
            value: UnsafeCell::new(value),
        }
    }

    /// Returns a waitable read-acquisition handle.
    pub fn readable(&self) -> ReadLock<'_, T> {
        ReadLock { lock: self }
    }

    /// Returns a waitable write-acquisition handle.
    pub fn writable(&self) -> WriteLock<'_, T> {
        WriteLock { lock: self }
    }

    /// Attempts to acquire a read guard without parking.
    pub fn try_read(&self) -> Option<RwLockReadGuard<'_, T>> {
        let mut state = self.state.borrow_mut();
        if state.writer {
            return None;
        }

        state.readers += 1;
        Some(RwLockReadGuard { lock: self })
    }

    /// Acquires a read guard, parking if a writer holds the lock.
    pub fn read(&self, cx: &RuntimeContext<'_>) -> RwLockReadGuard<'_, T> {
        self.readable().read(cx)
    }

    /// Attempts to acquire a write guard without parking.
    pub fn try_write(&self) -> Option<RwLockWriteGuard<'_, T>> {
        let mut state = self.state.borrow_mut();
        if state.writer || state.readers != 0 {
            return None;
        }

        state.writer = true;
        Some(RwLockWriteGuard { lock: self })
    }

    /// Acquires a write guard, parking if readers or a writer hold the lock.
    pub fn write(&self, cx: &RuntimeContext<'_>) -> RwLockWriteGuard<'_, T> {
        self.writable().write(cx)
    }

    fn read_unlock(&self) {
        let mut state = self.state.borrow_mut();
        state.readers = state
            .readers
            .checked_sub(1)
            .expect("rwlock reader count underflow");
        if state.readers == 0 {
            state.waiters.wake_all();
        }
    }

    fn write_unlock(&self) {
        let mut state = self.state.borrow_mut();
        debug_assert!(state.writer, "unlocking rwlock without writer");
        state.writer = false;
        state.waiters.wake_all();
    }
}

impl<T: fmt::Debug> fmt::Debug for RwLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RwLock").finish_non_exhaustive()
    }
}

impl<T> From<T> for RwLock<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

/// Waitable read acquisition for an [`RwLock`].
pub struct ReadLock<'a, T> {
    lock: &'a RwLock<T>,
}

impl<'a, T> ReadLock<'a, T> {
    /// Attempts to acquire a read guard without parking.
    pub fn try_read(&self) -> Option<RwLockReadGuard<'a, T>> {
        self.lock.try_read()
    }

    /// Acquires a read guard, parking if necessary.
    pub fn read(&self, cx: &RuntimeContext<'_>) -> RwLockReadGuard<'a, T> {
        loop {
            if let Some(guard) = self.try_read() {
                return guard;
            }

            if let Some(waiter) = cx.waiter() {
                self.lock.state.borrow_mut().waiters.push(waiter);
            }
            cx.park();
        }
    }
}

impl<T> Waitable for ReadLock<'_, T> {
    fn is_ready(&self) -> bool {
        !self.lock.state.borrow().writer
    }

    fn add_waiter(&self, cx: &RuntimeContext<'_>) {
        if !self.is_ready()
            && let Some(waiter) = cx.waiter()
        {
            self.lock.state.borrow_mut().waiters.push(waiter);
        }
    }
}

/// Waitable write acquisition for an [`RwLock`].
pub struct WriteLock<'a, T> {
    lock: &'a RwLock<T>,
}

impl<'a, T> WriteLock<'a, T> {
    /// Attempts to acquire a write guard without parking.
    pub fn try_write(&self) -> Option<RwLockWriteGuard<'a, T>> {
        self.lock.try_write()
    }

    /// Acquires a write guard, parking if necessary.
    pub fn write(&self, cx: &RuntimeContext<'_>) -> RwLockWriteGuard<'a, T> {
        loop {
            if let Some(guard) = self.try_write() {
                return guard;
            }

            if let Some(waiter) = cx.waiter() {
                self.lock.state.borrow_mut().waiters.push(waiter);
            }
            cx.park();
        }
    }
}

impl<T> Waitable for WriteLock<'_, T> {
    fn is_ready(&self) -> bool {
        let state = self.lock.state.borrow();
        !state.writer && state.readers == 0
    }

    fn add_waiter(&self, cx: &RuntimeContext<'_>) {
        if !self.is_ready()
            && let Some(waiter) = cx.waiter()
        {
            self.lock.state.borrow_mut().waiters.push(waiter);
        }
    }
}

/// Read guard returned by [`RwLock::read`].
pub struct RwLockReadGuard<'a, T> {
    lock: &'a RwLock<T>,
}

impl<T> Deref for RwLockReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> Drop for RwLockReadGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.read_unlock();
    }
}

/// Write guard returned by [`RwLock::write`].
pub struct RwLockWriteGuard<'a, T> {
    lock: &'a RwLock<T>,
}

impl<T> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for RwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for RwLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.write_unlock();
    }
}

struct State {
    readers: usize,
    writer: bool,
    waiters: Waiters,
}

#[cfg(test)]
mod tests {
    use crate::{Runtime, RwLock};

    #[test]
    fn rwlock_allows_readers_and_excludes_writer() {
        let lock = RwLock::new(5);
        let mut runtime = Runtime::new();

        let output = runtime.block_on(|cx| {
            cx.scope(|scope| {
                let reader = scope.spawn(|cx| {
                    let guard = lock.read(cx);
                    cx.yield_now();
                    *guard
                });
                let writer = scope.spawn(|cx| {
                    let mut guard = lock.write(cx);
                    *guard += 1;
                    *guard
                });

                reader.join(cx) + writer.join(cx)
            })
        });

        assert_eq!(output, 11);
    }
}
