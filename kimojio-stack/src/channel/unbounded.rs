// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt;
use std::rc::Rc;

use crate::channel::{RecvError, SendError, TryRecvError};
use crate::{RuntimeContext, Waitable, Waiters};

/// Creates an unbounded channel.
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let inner = Rc::new(RefCell::new(Inner {
        queue: VecDeque::new(),
        senders: 1,
        receivers: 1,
        recv_waiters: Waiters::default(),
    }));

    (
        Sender {
            inner: Rc::clone(&inner),
        },
        Receiver { inner },
    )
}

/// Unbounded channel sender.
pub struct Sender<T> {
    inner: Rc<RefCell<Inner<T>>>,
}

impl<T> Sender<T> {
    /// Sends a value without parking.
    pub fn send(&self, value: T) -> Result<(), SendError<T>> {
        let mut inner = self.inner.borrow_mut();
        if inner.receivers == 0 {
            return Err(SendError(value));
        }

        inner.queue.push_back(value);
        inner.recv_waiters.wake_one();
        Ok(())
    }

    /// Returns whether all receivers have been dropped.
    pub fn is_closed(&self) -> bool {
        self.inner.borrow().receivers == 0
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.inner.borrow_mut().senders += 1;
        Self {
            inner: Rc::clone(&self.inner),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let mut inner = self.inner.borrow_mut();
        inner.senders -= 1;
        if inner.senders == 0 {
            inner.recv_waiters.wake_all();
        }
    }
}

impl<T> Waitable for Sender<T> {
    fn is_ready(&self) -> bool {
        true
    }

    fn add_waiter(&self, _cx: &RuntimeContext<'_>) {}
}

impl<T> fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sender").finish_non_exhaustive()
    }
}

/// Unbounded channel receiver.
pub struct Receiver<T> {
    inner: Rc<RefCell<Inner<T>>>,
}

impl<T> Receiver<T> {
    /// Attempts to receive without parking.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        let mut inner = self.inner.borrow_mut();
        if let Some(value) = inner.queue.pop_front() {
            return Ok(value);
        }

        if inner.senders == 0 {
            Err(TryRecvError::Closed)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    /// Receives a value, parking while the channel is empty and open.
    pub fn recv(&self, cx: &RuntimeContext<'_>) -> Result<T, RecvError> {
        loop {
            match self.try_recv() {
                Ok(value) => return Ok(value),
                Err(TryRecvError::Closed) => return Err(RecvError),
                Err(TryRecvError::Empty) => {}
            }

            if let Some(waiter) = cx.waiter() {
                self.inner.borrow_mut().recv_waiters.push(waiter);
            }
            cx.park();
        }
    }

    /// Returns whether all senders have been dropped.
    pub fn is_closed(&self) -> bool {
        self.inner.borrow().senders == 0
    }
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        self.inner.borrow_mut().receivers += 1;
        Self {
            inner: Rc::clone(&self.inner),
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.inner.borrow_mut().receivers -= 1;
    }
}

impl<T> Waitable for Receiver<T> {
    fn is_ready(&self) -> bool {
        let inner = self.inner.borrow();
        !inner.queue.is_empty() || inner.senders == 0
    }

    fn add_waiter(&self, cx: &RuntimeContext<'_>) {
        if !self.is_ready()
            && let Some(waiter) = cx.waiter()
        {
            self.inner.borrow_mut().recv_waiters.push(waiter);
        }
    }
}

impl<T> fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receiver").finish_non_exhaustive()
    }
}

struct Inner<T> {
    queue: VecDeque<T>,
    senders: usize,
    receivers: usize,
    recv_waiters: Waiters,
}

#[cfg(test)]
mod tests {
    use crate::Runtime;
    use crate::channel;

    #[test]
    fn unbounded_channel_receives_sent_values() {
        let mut runtime = Runtime::new();

        let output = runtime.block_on(|cx| {
            let (tx, rx) = channel::unbounded();
            cx.scope(|scope| {
                let sender = scope.spawn(|_| {
                    tx.send(20).unwrap();
                    tx.send(22).unwrap();
                });
                let receiver = scope.spawn(|cx| rx.recv(cx).unwrap() + rx.recv(cx).unwrap());

                sender.join(cx);
                receiver.join(cx)
            })
        });

        assert_eq!(output, 42);
    }
}
