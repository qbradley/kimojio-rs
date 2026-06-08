// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A single-send channel for stackful coroutines.
//!
//! The channel is the stackful equivalent of a one-shot handoff: one
//! [`Sender`] moves exactly one value to one [`Receiver`]. [`Receiver::recv`]
//! parks cooperatively instead of blocking the OS thread, and `Receiver`
//! implements [`Waitable`] so it can participate in
//! [`RuntimeContext::wait_any`](crate::RuntimeContext::wait_any) or
//! [`RuntimeContext::join`](crate::RuntimeContext::join).
//!
//! ```
//! use kimojio_stack::{Runtime, once};
//!
//! let mut runtime = Runtime::new();
//! let value = runtime.block_on(|cx| {
//!     cx.scope(|scope| {
//!         let (tx, rx) = once::channel();
//!         scope.spawn(move |_| tx.send(42).unwrap());
//!         scope.spawn(move |cx| rx.recv(cx).unwrap()).join(cx)
//!     })
//! });
//!
//! assert_eq!(value, 42);
//! ```

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use crate::{RuntimeContext, Waitable, Waiter};

/// Creates a channel that can deliver one value.
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let inner = Rc::new(RefCell::new(Inner {
        value: None,
        sender_alive: true,
        receiver_alive: true,
        receiver_waiter: None,
    }));

    (
        Sender {
            inner: Rc::clone(&inner),
        },
        Receiver { inner },
    )
}

/// Sends one value to a [`Receiver`].
pub struct Sender<T> {
    inner: Rc<RefCell<Inner<T>>>,
}

impl<T> Sender<T> {
    /// Sends `value` to the receiver.
    pub fn send(self, value: T) -> Result<(), SendError<T>> {
        let waiter = {
            let mut inner = self.inner.borrow_mut();
            if !inner.receiver_alive {
                return Err(SendError(value));
            }

            inner.sender_alive = false;
            inner.value = Some(value);
            inner.receiver_waiter.take()
        };

        if let Some(waiter) = waiter {
            waiter.wake();
        }

        Ok(())
    }
}

impl<T> fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sender").finish_non_exhaustive()
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let waiter = {
            let mut inner = self.inner.borrow_mut();
            inner.sender_alive = false;
            inner.receiver_waiter.take()
        };

        if let Some(waiter) = waiter {
            waiter.wake();
        }
    }
}

/// Receives one value from a [`Sender`].
pub struct Receiver<T> {
    inner: Rc<RefCell<Inner<T>>>,
}

impl<T> Receiver<T> {
    /// Blocks cooperatively until the value is sent or the sender is dropped.
    pub fn recv(self, cx: &RuntimeContext<'_>) -> Result<T, RecvError> {
        loop {
            {
                let mut inner = self.inner.borrow_mut();
                if let Some(value) = inner.value.take() {
                    inner.receiver_alive = false;
                    return Ok(value);
                }

                if !inner.sender_alive {
                    inner.receiver_alive = false;
                    return Err(RecvError);
                }

                inner.receiver_waiter = cx.waiter();
            }

            cx.park();
        }
    }
}

impl<T> Waitable for Receiver<T> {
    fn is_ready(&self) -> bool {
        let inner = self.inner.borrow();
        inner.value.is_some() || !inner.sender_alive
    }

    fn add_waiter(&self, cx: &RuntimeContext<'_>) {
        if !self.is_ready() {
            self.inner.borrow_mut().receiver_waiter = cx.waiter();
        }
    }
}

impl<T> fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receiver").finish_non_exhaustive()
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.inner.borrow_mut().receiver_alive = false;
    }
}

struct Inner<T> {
    value: Option<T>,
    sender_alive: bool,
    receiver_alive: bool,
    receiver_waiter: Option<Waiter>,
}

/// Error returned when the receiver has been dropped.
pub struct SendError<T>(pub T);

impl<T> fmt::Debug for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SendError").finish()
    }
}

impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("receiver dropped")
    }
}

impl<T: fmt::Debug> std::error::Error for SendError<T> {}

/// Error returned when the sender is dropped before sending.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecvError;

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("sender dropped")
    }
}

impl std::error::Error for RecvError {}
