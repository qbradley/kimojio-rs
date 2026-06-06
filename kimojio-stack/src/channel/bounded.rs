// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt;
use std::rc::Rc;

use crate::channel::{RecvError, SendError, TryRecvError, TrySendError};
use crate::{RuntimeContext, Waitable, Waiters};

/// Creates a bounded channel with space for `capacity` messages.
pub fn channel<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    assert!(capacity != 0, "bounded channel capacity must be nonzero");

    let inner = Rc::new(RefCell::new(Inner {
        queue: VecDeque::with_capacity(capacity),
        capacity,
        senders: 1,
        receivers: 1,
        send_waiters: Waiters::default(),
        recv_waiters: Waiters::default(),
    }));

    (
        Sender {
            inner: Rc::clone(&inner),
        },
        Receiver { inner },
    )
}

/// Bounded channel sender.
pub struct Sender<T> {
    inner: Rc<RefCell<Inner<T>>>,
}

impl<T> Sender<T> {
    /// Attempts to send without parking.
    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        let mut inner = self.inner.borrow_mut();
        if inner.receivers == 0 {
            return Err(TrySendError::Closed(value));
        }

        if inner.queue.len() == inner.capacity {
            return Err(TrySendError::Full(value));
        }

        inner.queue.push_back(value);
        inner.recv_waiters.wake_one();
        Ok(())
    }

    /// Sends a value, parking while the bounded channel is full.
    pub fn send(&self, cx: &RuntimeContext<'_>, mut value: T) -> Result<(), SendError<T>> {
        loop {
            match self.try_send(value) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Closed(value)) => return Err(SendError(value)),
                Err(TrySendError::Full(returned)) => value = returned,
            }

            if let Some(waiter) = cx.waiter() {
                self.inner.borrow_mut().send_waiters.push(waiter);
            }
            cx.park();
        }
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
        let inner = self.inner.borrow();
        inner.receivers == 0 || inner.queue.len() < inner.capacity
    }

    fn add_waiter(&self, cx: &RuntimeContext<'_>) {
        if !self.is_ready()
            && let Some(waiter) = cx.waiter()
        {
            self.inner.borrow_mut().send_waiters.push(waiter);
        }
    }
}

impl<T> fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sender").finish_non_exhaustive()
    }
}

/// Bounded channel receiver.
pub struct Receiver<T> {
    inner: Rc<RefCell<Inner<T>>>,
}

impl<T> Receiver<T> {
    /// Attempts to receive without parking.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        let mut inner = self.inner.borrow_mut();
        if let Some(value) = inner.queue.pop_front() {
            inner.send_waiters.wake_one();
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
        let mut inner = self.inner.borrow_mut();
        inner.receivers -= 1;
        if inner.receivers == 0 {
            inner.send_waiters.wake_all();
        }
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
    capacity: usize,
    senders: usize,
    receivers: usize,
    send_waiters: Waiters,
    recv_waiters: Waiters,
}

#[cfg(test)]
mod tests {
    use crate::Runtime;
    use crate::channel;

    #[test]
    fn bounded_channel_blocks_sender_until_capacity_available() {
        let mut runtime = Runtime::new();

        let output = runtime.block_on(|cx| {
            let (tx, rx) = channel::bounded(1);
            tx.try_send(1).unwrap();

            cx.scope(|scope| {
                let sender = scope.spawn(|cx| {
                    tx.send(cx, 2).unwrap();
                    3
                });
                let receiver = scope.spawn(|cx| {
                    assert_eq!(rx.recv(cx).unwrap(), 1);
                    assert_eq!(rx.recv(cx).unwrap(), 2);
                    4
                });

                sender.join(cx).unwrap() + receiver.join(cx).unwrap()
            })
        });

        assert_eq!(output, 7);
    }
}
