// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::cell::{Cell, Ref, RefCell};
use std::fmt;
use std::rc::Rc;

use crate::{RuntimeContext, WaitRegistration, Waitable, Waiters};

/// Creates a watch channel that retains the latest value.
pub fn channel<T>(value: T) -> (WatchSender<T>, WatchReceiver<T>) {
    let inner = Rc::new(RefCell::new(Inner {
        value,
        version: 0,
        senders: 1,
        receivers: 1,
        waiters: Waiters::default(),
    }));

    (
        WatchSender {
            inner: Rc::clone(&inner),
        },
        WatchReceiver {
            inner,
            seen: Cell::new(0),
        },
    )
}

/// Sender side of a watch channel.
pub struct WatchSender<T> {
    inner: Rc<RefCell<Inner<T>>>,
}

impl<T> WatchSender<T> {
    /// Sends a new value, replacing the previous one.
    pub fn send(&self, value: T) -> Result<(), SendError<T>> {
        let mut inner = self.inner.borrow_mut();
        if inner.receivers == 0 {
            return Err(SendError(value));
        }

        inner.value = value;
        inner.version = inner.version.wrapping_add(1);
        inner.waiters.wake_all();
        Ok(())
    }

    /// Returns whether there is at least one receiver.
    pub fn has_receivers(&self) -> bool {
        self.inner.borrow().receivers != 0
    }
}

impl<T> Clone for WatchSender<T> {
    fn clone(&self) -> Self {
        self.inner.borrow_mut().senders += 1;
        Self {
            inner: Rc::clone(&self.inner),
        }
    }
}

impl<T> Drop for WatchSender<T> {
    fn drop(&mut self) {
        let mut inner = self.inner.borrow_mut();
        inner.senders -= 1;
        if inner.senders == 0 {
            inner.waiters.wake_all();
        }
    }
}

impl<T> fmt::Debug for WatchSender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WatchSender").finish_non_exhaustive()
    }
}

/// Receiver side of a watch channel.
pub struct WatchReceiver<T> {
    inner: Rc<RefCell<Inner<T>>>,
    seen: Cell<u64>,
}

impl<T> WatchReceiver<T> {
    /// Returns a borrow of the current value without marking it seen.
    pub fn borrow(&self) -> Ref<'_, T> {
        Ref::map(self.inner.borrow(), |inner| &inner.value)
    }

    /// Returns a borrow of the current value and marks it seen.
    pub fn borrow_and_update(&self) -> Ref<'_, T> {
        let inner = self.inner.borrow();
        self.seen.set(inner.version);
        Ref::map(inner, |inner| &inner.value)
    }

    /// Returns a clone of the current value and marks it seen.
    pub fn get_cloned(&self) -> T
    where
        T: Clone,
    {
        self.borrow_and_update().clone()
    }

    /// Attempts to observe a change without parking.
    pub fn try_changed(&self) -> Result<bool, RecvError> {
        let inner = self.inner.borrow();
        if inner.version != self.seen.get() {
            self.seen.set(inner.version);
            return Ok(true);
        }

        if inner.senders == 0 {
            return Err(RecvError);
        }

        Ok(false)
    }

    /// Waits until the watched value changes or all senders are dropped.
    pub fn changed(&self, cx: &RuntimeContext<'_>) -> Result<(), RecvError> {
        loop {
            if self.try_changed()? {
                return Ok(());
            }

            let registration = cx.wait_registration();
            if let Some(waiter) = cx.waiter(&registration) {
                self.inner.borrow_mut().waiters.push(waiter);
            }
            cx.park();
        }
    }
}

impl<T> Clone for WatchReceiver<T> {
    fn clone(&self) -> Self {
        self.inner.borrow_mut().receivers += 1;
        Self {
            inner: Rc::clone(&self.inner),
            seen: Cell::new(self.seen.get()),
        }
    }
}

impl<T> Drop for WatchReceiver<T> {
    fn drop(&mut self) {
        self.inner.borrow_mut().receivers -= 1;
    }
}

impl<T> Waitable for WatchReceiver<T> {
    fn is_ready(&self) -> bool {
        let inner = self.inner.borrow();
        inner.version != self.seen.get() || inner.senders == 0
    }

    fn add_waiter(&self, cx: &RuntimeContext<'_>, registration: &WaitRegistration) {
        if !self.is_ready()
            && let Some(waiter) = cx.waiter(registration)
        {
            self.inner.borrow_mut().waiters.push(waiter);
        }
    }
}

impl<T> fmt::Debug for WatchReceiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WatchReceiver")
            .field("ready", &self.is_ready())
            .finish()
    }
}

struct Inner<T> {
    value: T,
    version: u64,
    senders: usize,
    receivers: usize,
    waiters: Waiters,
}

/// Error returned when all watch receivers are dropped.
pub struct SendError<T>(pub T);

impl<T> fmt::Debug for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SendError").finish()
    }
}

impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("watch receivers dropped")
    }
}

impl<T: fmt::Debug> std::error::Error for SendError<T> {}

/// Error returned when all watch senders are dropped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecvError;

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("watch senders dropped")
    }
}

impl std::error::Error for RecvError {}

#[cfg(test)]
mod tests {
    use crate::{Runtime, watch};

    #[test]
    fn watch_observes_latest_value() {
        let (tx, rx) = watch::channel(1);
        let mut runtime = Runtime::new();

        let output = runtime.block_on(|cx| {
            cx.scope(|scope| {
                let receiver = scope.spawn(|cx| {
                    rx.changed(cx).unwrap();
                    rx.get_cloned()
                });
                let sender = scope.spawn(|_| tx.send(42).unwrap());

                sender.join(cx);
                receiver.join(cx)
            })
        });

        assert_eq!(output, 42);
    }
}
