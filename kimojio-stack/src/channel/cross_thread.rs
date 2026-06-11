// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Bounded channels for communication across threads, stackful runtimes, and
//! Tokio-compatible async tasks.
//!
//! This module is for cases where participants do not all run inside the same
//! `kimojio-stack` runtime. It uses a preallocated bounded queue for message
//! storage and endpoint-specific waiters for backpressure:
//!
//! - [`ThreadSender`] and [`ThreadReceiver`] expose blocking OS-thread methods.
//! - [`StackfulSender`] and [`StackfulReceiver`] park only the current stackful
//!   coroutine.
//! - [`AsyncSender`] and [`AsyncReceiver`] expose standard [`Future`]/`Waker`
//!   integration and are compatible with Tokio tasks.
//!
//! Choose endpoint types explicitly with [`bounded`] or the convenience
//! constructors [`thread()`], [`stackful()`], and [`tokio()`].
//!
//! ```
//! use kimojio_stack::channel::cross_thread;
//!
//! let (tx, rx) = cross_thread::thread(1);
//! tx.send_blocking("hello").unwrap();
//! assert_eq!(rx.recv_blocking().unwrap(), "hello");
//! ```
//!
//! Mixed endpoint pairs are available from the builder:
//!
//! ```
//! use kimojio_stack::channel::cross_thread;
//!
//! let (_tx, _rx) = cross_thread::bounded::<u64>(8).thread_to_stackful();
//! ```

use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::hint;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex as StdMutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::thread;

use crossbeam_queue::ArrayQueue;

use crate::channel::{RecvError, SendError, TryRecvError, TrySendError};
use crate::{ExternalWaiter, RuntimeContext};

const ADAPTIVE_SPIN_RETRIES: usize = 128;
const ADAPTIVE_YIELD_RETRIES: usize = 2;

/// Creates a bounded cross-thread channel builder.
pub fn bounded<T>(capacity: usize) -> Builder<T> {
    Builder::new(capacity)
}

/// Creates a bounded cross-thread channel with OS-thread endpoints.
pub fn thread<T>(capacity: usize) -> (ThreadSender<T>, ThreadReceiver<T>) {
    bounded(capacity).thread()
}

/// Creates a bounded cross-thread channel with stackful endpoints.
pub fn stackful<T>(capacity: usize) -> (StackfulSender<T>, StackfulReceiver<T>) {
    bounded(capacity).stackful()
}

/// Creates a bounded cross-thread channel with async endpoints compatible with Tokio tasks.
pub fn tokio<T>(capacity: usize) -> (AsyncSender<T>, AsyncReceiver<T>) {
    bounded(capacity).tokio()
}

/// Builder for selecting cross-thread channel endpoint types.
#[must_use = "call an endpoint selection method such as thread, stackful, or tokio"]
pub struct Builder<T> {
    capacity: usize,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Builder<T> {
    fn new(capacity: usize) -> Self {
        assert!(
            capacity != 0,
            "cross-thread bounded channel capacity must be nonzero"
        );

        Self {
            capacity,
            _marker: PhantomData,
        }
    }

    /// Builds sender and receiver endpoints for blocking OS-thread use.
    pub fn thread(self) -> (ThreadSender<T>, ThreadReceiver<T>) {
        let inner = Arc::new(Inner::new(self.capacity));
        (
            ThreadSender {
                inner: Arc::clone(&inner),
            },
            ThreadReceiver { inner },
        )
    }

    /// Builds sender and receiver endpoints for stackful coroutine use.
    pub fn stackful(self) -> (StackfulSender<T>, StackfulReceiver<T>) {
        let inner = Arc::new(Inner::new(self.capacity));
        (
            StackfulSender {
                inner: Arc::clone(&inner),
            },
            StackfulReceiver { inner },
        )
    }

    /// Builds a thread sender and a stackful receiver.
    pub fn thread_to_stackful(self) -> (ThreadSender<T>, StackfulReceiver<T>) {
        let inner = Arc::new(Inner::new(self.capacity));
        (
            ThreadSender {
                inner: Arc::clone(&inner),
            },
            StackfulReceiver { inner },
        )
    }

    /// Builds a stackful sender and a thread receiver.
    pub fn stackful_to_thread(self) -> (StackfulSender<T>, ThreadReceiver<T>) {
        let inner = Arc::new(Inner::new(self.capacity));
        (
            StackfulSender {
                inner: Arc::clone(&inner),
            },
            ThreadReceiver { inner },
        )
    }

    /// Builds sender and receiver endpoints for async runtimes such as Tokio.
    pub fn tokio(self) -> (AsyncSender<T>, AsyncReceiver<T>) {
        let inner = Arc::new(Inner::new(self.capacity));
        (
            AsyncSender {
                inner: Arc::clone(&inner),
            },
            AsyncReceiver { inner },
        )
    }

    /// Builds an async sender and a stackful receiver.
    pub fn tokio_to_stackful(self) -> (AsyncSender<T>, StackfulReceiver<T>) {
        let inner = Arc::new(Inner::new(self.capacity));
        (
            AsyncSender {
                inner: Arc::clone(&inner),
            },
            StackfulReceiver { inner },
        )
    }

    /// Builds a stackful sender and an async receiver.
    pub fn stackful_to_tokio(self) -> (StackfulSender<T>, AsyncReceiver<T>) {
        let inner = Arc::new(Inner::new(self.capacity));
        (
            StackfulSender {
                inner: Arc::clone(&inner),
            },
            AsyncReceiver { inner },
        )
    }

    /// Builds a thread sender and an async receiver.
    pub fn thread_to_tokio(self) -> (ThreadSender<T>, AsyncReceiver<T>) {
        let inner = Arc::new(Inner::new(self.capacity));
        (
            ThreadSender {
                inner: Arc::clone(&inner),
            },
            AsyncReceiver { inner },
        )
    }

    /// Builds an async sender and a thread receiver.
    pub fn tokio_to_thread(self) -> (AsyncSender<T>, ThreadReceiver<T>) {
        let inner = Arc::new(Inner::new(self.capacity));
        (
            AsyncSender {
                inner: Arc::clone(&inner),
            },
            ThreadReceiver { inner },
        )
    }
}

impl<T> fmt::Debug for Builder<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Builder")
            .field("capacity", &self.capacity)
            .finish()
    }
}

/// Sender endpoint for blocking OS-thread use.
pub struct ThreadSender<T> {
    inner: Arc<Inner<T>>,
}

impl<T> ThreadSender<T> {
    /// Attempts to send without blocking the current OS thread.
    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        self.inner.try_send(value)
    }

    /// Sends a value, blocking the current OS thread while the channel is full.
    pub fn send_blocking(&self, value: T) -> Result<(), SendError<T>> {
        let mut value = match self.inner.try_send_without_notify(value) {
            Ok(()) => {
                self.inner.recv_wait.notify_one();
                return Ok(());
            }
            Err(TrySendError::Closed(value)) => return Err(SendError(value)),
            Err(TrySendError::Full(value)) => value,
        };

        value = match self.inner.try_send_after_adaptive_wait(value) {
            Ok(()) => {
                self.inner.recv_wait.notify_one();
                return Ok(());
            }
            Err(TrySendError::Closed(value)) => return Err(SendError(value)),
            Err(TrySendError::Full(value)) => value,
        };

        let mut wait = self.inner.send_wait.prepare_wait();
        loop {
            match self.inner.try_send_without_notify(value) {
                Ok(()) => {
                    self.inner.send_wait.cancel_wait(wait);
                    self.inner.recv_wait.notify_one();
                    return Ok(());
                }
                Err(TrySendError::Closed(returned)) => {
                    self.inner.send_wait.cancel_wait(wait);
                    return Err(SendError(returned));
                }
                Err(TrySendError::Full(returned)) => value = returned,
            }

            wait = self.inner.send_wait.wait_for_change(wait);
        }
    }

    /// Returns whether all receiver endpoints have been dropped.
    pub fn is_closed(&self) -> bool {
        self.inner.receivers.load(Ordering::Acquire) == 0
    }
}

impl<T> Clone for ThreadSender<T> {
    fn clone(&self) -> Self {
        self.inner.add_sender();
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T> Drop for ThreadSender<T> {
    fn drop(&mut self) {
        self.inner.drop_sender();
    }
}

impl<T> fmt::Debug for ThreadSender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ThreadSender").finish_non_exhaustive()
    }
}

/// Receiver endpoint for blocking OS-thread use.
pub struct ThreadReceiver<T> {
    inner: Arc<Inner<T>>,
}

impl<T> ThreadReceiver<T> {
    /// Attempts to receive without blocking the current OS thread.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.inner.try_recv()
    }

    /// Receives a value, blocking the current OS thread while the channel is empty and open.
    pub fn recv_blocking(&self) -> Result<T, RecvError> {
        match self.inner.try_recv_without_notify() {
            Ok(value) => {
                self.inner.send_wait.notify_one();
                return Ok(value);
            }
            Err(TryRecvError::Closed) => return Err(RecvError),
            Err(TryRecvError::Empty) => {}
        }

        match self.inner.try_recv_after_adaptive_wait() {
            Ok(value) => {
                self.inner.send_wait.notify_one();
                return Ok(value);
            }
            Err(TryRecvError::Closed) => return Err(RecvError),
            Err(TryRecvError::Empty) => {}
        }

        let mut wait = self.inner.recv_wait.prepare_wait();
        loop {
            match self.inner.try_recv_without_notify() {
                Ok(value) => {
                    self.inner.recv_wait.cancel_wait(wait);
                    self.inner.send_wait.notify_one();
                    return Ok(value);
                }
                Err(TryRecvError::Closed) => {
                    self.inner.recv_wait.cancel_wait(wait);
                    return Err(RecvError);
                }
                Err(TryRecvError::Empty) => {}
            }

            wait = self.inner.recv_wait.wait_for_change(wait);
        }
    }

    /// Returns whether all sender endpoints have been dropped.
    pub fn is_closed(&self) -> bool {
        self.inner.senders.load(Ordering::Acquire) == 0
    }
}

impl<T> Clone for ThreadReceiver<T> {
    fn clone(&self) -> Self {
        self.inner.add_receiver();
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T> Drop for ThreadReceiver<T> {
    fn drop(&mut self) {
        self.inner.drop_receiver();
    }
}

impl<T> fmt::Debug for ThreadReceiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ThreadReceiver").finish_non_exhaustive()
    }
}

/// Sender endpoint for stackful coroutine use.
pub struct StackfulSender<T> {
    inner: Arc<Inner<T>>,
}

impl<T> StackfulSender<T> {
    /// Attempts to send without parking the current stackful coroutine.
    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        self.inner.try_send(value)
    }

    /// Sends a value, parking the current stackful coroutine while the channel is full.
    ///
    /// This waiting method is intended for spawned stackful coroutine contexts.
    /// Root runtime code should use [`StackfulSender::try_send`] or spawn a
    /// coroutine before calling `send`.
    pub fn send(&self, cx: &RuntimeContext<'_>, value: T) -> Result<(), SendError<T>> {
        let mut value = match self.inner.try_send_without_notify(value) {
            Ok(()) => {
                self.inner.recv_wait.notify_one();
                return Ok(());
            }
            Err(TrySendError::Closed(value)) => return Err(SendError(value)),
            Err(TrySendError::Full(value)) => value,
        };

        value = match self.inner.try_send_after_adaptive_wait(value) {
            Ok(()) => {
                self.inner.recv_wait.notify_one();
                return Ok(());
            }
            Err(TrySendError::Closed(value)) => return Err(SendError(value)),
            Err(TrySendError::Full(value)) => value,
        };

        loop {
            let Some(registration) = cx.external_wait_registration() else {
                cx.park();
                continue;
            };
            let waiter = registration.waiter();

            let mut wait = self.inner.send_wait.lock();
            match self.inner.try_send_without_notify(value) {
                Ok(()) => {
                    drop(wait);
                    self.inner.recv_wait.notify_one();
                    return Ok(());
                }
                Err(TrySendError::Closed(returned)) => {
                    drop(wait);
                    return Err(SendError(returned));
                }
                Err(TrySendError::Full(returned)) => value = returned,
            }

            self.inner.send_wait.push_stackful_waiter(&mut wait, waiter);
            drop(wait);
            cx.park();
        }
    }

    /// Returns whether all receiver endpoints have been dropped.
    pub fn is_closed(&self) -> bool {
        self.inner.receivers.load(Ordering::Acquire) == 0
    }
}

impl<T> Clone for StackfulSender<T> {
    fn clone(&self) -> Self {
        self.inner.add_sender();
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T> Drop for StackfulSender<T> {
    fn drop(&mut self) {
        self.inner.drop_sender();
    }
}

impl<T> fmt::Debug for StackfulSender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StackfulSender").finish_non_exhaustive()
    }
}

/// Receiver endpoint for stackful coroutine use.
pub struct StackfulReceiver<T> {
    inner: Arc<Inner<T>>,
}

impl<T> StackfulReceiver<T> {
    /// Attempts to receive without parking the current stackful coroutine.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.inner.try_recv()
    }

    /// Receives a value, parking the current stackful coroutine while the channel is empty and open.
    ///
    /// This waiting method is intended for spawned stackful coroutine contexts.
    /// Root runtime code should use [`StackfulReceiver::try_recv`] or spawn a
    /// coroutine before calling `recv`.
    pub fn recv(&self, cx: &RuntimeContext<'_>) -> Result<T, RecvError> {
        match self.inner.try_recv_without_notify() {
            Ok(value) => {
                self.inner.send_wait.notify_one();
                return Ok(value);
            }
            Err(TryRecvError::Closed) => return Err(RecvError),
            Err(TryRecvError::Empty) => {}
        }

        match self.inner.try_recv_after_adaptive_wait() {
            Ok(value) => {
                self.inner.send_wait.notify_one();
                return Ok(value);
            }
            Err(TryRecvError::Closed) => return Err(RecvError),
            Err(TryRecvError::Empty) => {}
        }

        loop {
            let Some(registration) = cx.external_wait_registration() else {
                cx.park();
                continue;
            };
            let waiter = registration.waiter();

            let mut wait = self.inner.recv_wait.lock();
            match self.inner.try_recv_without_notify() {
                Ok(value) => {
                    drop(wait);
                    self.inner.send_wait.notify_one();
                    return Ok(value);
                }
                Err(TryRecvError::Closed) => {
                    drop(wait);
                    return Err(RecvError);
                }
                Err(TryRecvError::Empty) => {}
            }

            self.inner.recv_wait.push_stackful_waiter(&mut wait, waiter);
            drop(wait);
            cx.park();
        }
    }

    /// Returns whether all sender endpoints have been dropped.
    pub fn is_closed(&self) -> bool {
        self.inner.senders.load(Ordering::Acquire) == 0
    }
}

impl<T> Clone for StackfulReceiver<T> {
    fn clone(&self) -> Self {
        self.inner.add_receiver();
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T> Drop for StackfulReceiver<T> {
    fn drop(&mut self) {
        self.inner.drop_receiver();
    }
}

impl<T> fmt::Debug for StackfulReceiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StackfulReceiver").finish_non_exhaustive()
    }
}

/// Sender endpoint for async runtimes such as Tokio.
pub struct AsyncSender<T> {
    inner: Arc<Inner<T>>,
}

impl<T> AsyncSender<T> {
    /// Attempts to send without awaiting.
    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        self.inner.try_send(value)
    }

    /// Sends a value, awaiting while the channel is full.
    pub fn send(&self, value: T) -> SendFuture<T> {
        self.inner.add_sender();
        SendFuture {
            inner: Arc::clone(&self.inner),
            value: Some(value),
            waiter_id: next_async_waiter_id(),
            registered: false,
            sender_active: true,
        }
    }

    /// Returns whether all receiver endpoints have been dropped.
    pub fn is_closed(&self) -> bool {
        self.inner.receivers.load(Ordering::Acquire) == 0
    }
}

impl<T> Clone for AsyncSender<T> {
    fn clone(&self) -> Self {
        self.inner.add_sender();
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T> Drop for AsyncSender<T> {
    fn drop(&mut self) {
        self.inner.drop_sender();
    }
}

impl<T> fmt::Debug for AsyncSender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AsyncSender").finish_non_exhaustive()
    }
}

/// Receiver endpoint for async runtimes such as Tokio.
pub struct AsyncReceiver<T> {
    inner: Arc<Inner<T>>,
}

impl<T> AsyncReceiver<T> {
    /// Attempts to receive without awaiting.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.inner.try_recv()
    }

    /// Receives a value, awaiting while the channel is empty and open.
    pub fn recv(&self) -> RecvFuture<T> {
        self.inner.add_receiver();
        RecvFuture {
            inner: Arc::clone(&self.inner),
            waiter_id: next_async_waiter_id(),
            registered: false,
            receiver_active: true,
        }
    }

    /// Returns whether all sender endpoints have been dropped.
    pub fn is_closed(&self) -> bool {
        self.inner.senders.load(Ordering::Acquire) == 0
    }
}

impl<T> Clone for AsyncReceiver<T> {
    fn clone(&self) -> Self {
        self.inner.add_receiver();
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T> Drop for AsyncReceiver<T> {
    fn drop(&mut self) {
        self.inner.drop_receiver();
    }
}

impl<T> fmt::Debug for AsyncReceiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AsyncReceiver").finish_non_exhaustive()
    }
}

/// Future returned by [`AsyncSender::send`].
#[must_use = "send futures must be awaited to send the value"]
pub struct SendFuture<T> {
    inner: Arc<Inner<T>>,
    value: Option<T>,
    waiter_id: usize,
    registered: bool,
    sender_active: bool,
}

impl<T> SendFuture<T> {
    fn unregister(&mut self) {
        if self.registered {
            self.inner.send_wait.remove_async_waiter(self.waiter_id);
            self.registered = false;
        }
    }

    fn finish_sender(&mut self) {
        if self.sender_active {
            self.sender_active = false;
            self.inner.drop_sender();
        }
    }
}

impl<T> Future for SendFuture<T> {
    type Output = Result<(), SendError<T>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let value = this
            .value
            .take()
            .expect("send future polled after completion");
        let mut value = match this.inner.try_send_without_notify(value) {
            Ok(()) => {
                this.unregister();
                this.finish_sender();
                this.inner.recv_wait.notify_one();
                return Poll::Ready(Ok(()));
            }
            Err(TrySendError::Closed(value)) => {
                this.unregister();
                this.finish_sender();
                return Poll::Ready(Err(SendError(value)));
            }
            Err(TrySendError::Full(value)) => value,
        };

        let mut wait = this.inner.send_wait.lock();
        match this.inner.try_send_without_notify(value) {
            Ok(()) => {
                if this.registered {
                    this.inner
                        .send_wait
                        .remove_async_waiter_locked(&mut wait, this.waiter_id);
                    this.registered = false;
                }
                drop(wait);
                this.finish_sender();
                this.inner.recv_wait.notify_one();
                Poll::Ready(Ok(()))
            }
            Err(TrySendError::Closed(returned)) => {
                if this.registered {
                    this.inner
                        .send_wait
                        .remove_async_waiter_locked(&mut wait, this.waiter_id);
                    this.registered = false;
                }
                drop(wait);
                this.finish_sender();
                Poll::Ready(Err(SendError(returned)))
            }
            Err(TrySendError::Full(returned)) => {
                value = returned;
                this.value = Some(value);
                this.inner.send_wait.push_or_update_async_waiter(
                    &mut wait,
                    this.waiter_id,
                    cx.waker(),
                );
                this.registered = true;
                Poll::Pending
            }
        }
    }
}

impl<T> Unpin for SendFuture<T> {}

impl<T> Drop for SendFuture<T> {
    fn drop(&mut self) {
        self.unregister();
        self.finish_sender();
    }
}

impl<T> fmt::Debug for SendFuture<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SendFuture")
            .field("registered", &self.registered)
            .finish_non_exhaustive()
    }
}

/// Future returned by [`AsyncReceiver::recv`].
#[must_use = "receive futures must be awaited to receive a value"]
pub struct RecvFuture<T> {
    inner: Arc<Inner<T>>,
    waiter_id: usize,
    registered: bool,
    receiver_active: bool,
}

impl<T> RecvFuture<T> {
    fn unregister(&mut self) {
        if self.registered {
            self.inner.recv_wait.remove_async_waiter(self.waiter_id);
            self.registered = false;
        }
    }

    fn finish_receiver(&mut self) {
        if self.receiver_active {
            self.receiver_active = false;
            self.inner.drop_receiver();
        }
    }
}

impl<T> Future for RecvFuture<T> {
    type Output = Result<T, RecvError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match this.inner.try_recv_without_notify() {
            Ok(value) => {
                this.unregister();
                this.finish_receiver();
                this.inner.send_wait.notify_one();
                return Poll::Ready(Ok(value));
            }
            Err(TryRecvError::Closed) => {
                this.unregister();
                this.finish_receiver();
                return Poll::Ready(Err(RecvError));
            }
            Err(TryRecvError::Empty) => {}
        }

        let mut wait = this.inner.recv_wait.lock();
        match this.inner.try_recv_without_notify() {
            Ok(value) => {
                if this.registered {
                    this.inner
                        .recv_wait
                        .remove_async_waiter_locked(&mut wait, this.waiter_id);
                    this.registered = false;
                }
                drop(wait);
                this.finish_receiver();
                this.inner.send_wait.notify_one();
                Poll::Ready(Ok(value))
            }
            Err(TryRecvError::Closed) => {
                if this.registered {
                    this.inner
                        .recv_wait
                        .remove_async_waiter_locked(&mut wait, this.waiter_id);
                    this.registered = false;
                }
                drop(wait);
                this.finish_receiver();
                Poll::Ready(Err(RecvError))
            }
            Err(TryRecvError::Empty) => {
                this.inner.recv_wait.push_or_update_async_waiter(
                    &mut wait,
                    this.waiter_id,
                    cx.waker(),
                );
                this.registered = true;
                Poll::Pending
            }
        }
    }
}

impl<T> Unpin for RecvFuture<T> {}

impl<T> Drop for RecvFuture<T> {
    fn drop(&mut self) {
        self.unregister();
        self.finish_receiver();
    }
}

impl<T> fmt::Debug for RecvFuture<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecvFuture")
            .field("registered", &self.registered)
            .finish_non_exhaustive()
    }
}

static NEXT_ASYNC_WAITER_ID: AtomicUsize = AtomicUsize::new(1);

fn next_async_waiter_id() -> usize {
    let id = NEXT_ASYNC_WAITER_ID.fetch_add(1, Ordering::Relaxed);
    assert!(id != 0, "async waiter id overflow");
    id
}

struct Inner<T> {
    queue: ArrayQueue<T>,
    senders: AtomicUsize,
    receivers: AtomicUsize,
    send_wait: WaitQueue,
    recv_wait: WaitQueue,
}

impl<T> Inner<T> {
    fn new(capacity: usize) -> Self {
        Self {
            queue: ArrayQueue::new(capacity),
            senders: AtomicUsize::new(1),
            receivers: AtomicUsize::new(1),
            send_wait: WaitQueue::default(),
            recv_wait: WaitQueue::default(),
        }
    }

    fn add_sender(&self) {
        self.senders.fetch_add(1, Ordering::Relaxed);
    }

    fn drop_sender(&self) {
        let previous = self.senders.fetch_sub(1, Ordering::AcqRel);
        assert!(previous != 0, "cross-thread sender count underflow");
        if previous == 1 {
            self.recv_wait.notify_all();
        }
    }

    fn add_receiver(&self) {
        self.receivers.fetch_add(1, Ordering::Relaxed);
    }

    fn drop_receiver(&self) {
        let previous = self.receivers.fetch_sub(1, Ordering::AcqRel);
        assert!(previous != 0, "cross-thread receiver count underflow");
        if previous == 1 {
            self.send_wait.notify_all();
        }
    }

    fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        let result = self.try_send_without_notify(value);
        if result.is_ok() {
            self.recv_wait.notify_one();
        }
        result
    }

    fn try_send_without_notify(&self, value: T) -> Result<(), TrySendError<T>> {
        if self.receivers.load(Ordering::Acquire) == 0 {
            return Err(TrySendError::Closed(value));
        }

        match self.queue.push(value) {
            Ok(()) => Ok(()),
            Err(value) if self.receivers.load(Ordering::Acquire) == 0 => {
                Err(TrySendError::Closed(value))
            }
            Err(value) => Err(TrySendError::Full(value)),
        }
    }

    fn try_recv(&self) -> Result<T, TryRecvError> {
        let result = self.try_recv_without_notify();
        if result.is_ok() {
            self.send_wait.notify_one();
        }
        result
    }

    fn try_recv_without_notify(&self) -> Result<T, TryRecvError> {
        if let Some(value) = self.queue.pop() {
            return Ok(value);
        }

        if self.senders.load(Ordering::Acquire) == 0 {
            Err(TryRecvError::Closed)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    fn try_send_after_adaptive_wait(&self, mut value: T) -> Result<(), TrySendError<T>> {
        for _ in 0..ADAPTIVE_SPIN_RETRIES {
            hint::spin_loop();
            match self.try_send_without_notify(value) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Closed(value)) => return Err(TrySendError::Closed(value)),
                Err(TrySendError::Full(returned)) => value = returned,
            }
        }

        for _ in 0..ADAPTIVE_YIELD_RETRIES {
            thread::yield_now();
            match self.try_send_without_notify(value) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Closed(value)) => return Err(TrySendError::Closed(value)),
                Err(TrySendError::Full(returned)) => value = returned,
            }
        }

        Err(TrySendError::Full(value))
    }

    fn try_recv_after_adaptive_wait(&self) -> Result<T, TryRecvError> {
        for _ in 0..ADAPTIVE_SPIN_RETRIES {
            hint::spin_loop();
            match self.try_recv_without_notify() {
                Ok(value) => return Ok(value),
                Err(TryRecvError::Closed) => return Err(TryRecvError::Closed),
                Err(TryRecvError::Empty) => {}
            }
        }

        for _ in 0..ADAPTIVE_YIELD_RETRIES {
            thread::yield_now();
            match self.try_recv_without_notify() {
                Ok(value) => return Ok(value),
                Err(TryRecvError::Closed) => return Err(TryRecvError::Closed),
                Err(TryRecvError::Empty) => {}
            }
        }

        Err(TryRecvError::Empty)
    }
}

#[derive(Default)]
struct WaitQueue {
    state: StdMutex<WaitState>,
    ready: Condvar,
    thread_waiters: AtomicUsize,
    stackful_waiters: AtomicUsize,
    async_waiters: AtomicUsize,
}

impl WaitQueue {
    fn prepare_wait(&self) -> MutexGuard<'_, WaitState> {
        self.thread_waiters.fetch_add(1, Ordering::AcqRel);
        self.lock()
    }

    fn wait_for_change<'a>(
        &self,
        mut guard: MutexGuard<'a, WaitState>,
    ) -> MutexGuard<'a, WaitState> {
        let generation = guard.generation;
        while guard.generation == generation {
            guard = self
                .ready
                .wait(guard)
                .expect("cross-thread channel wait queue mutex poisoned");
        }
        guard
    }

    fn cancel_wait(&self, guard: MutexGuard<'_, WaitState>) {
        drop(guard);
        let previous = self.thread_waiters.fetch_sub(1, Ordering::AcqRel);
        assert!(previous != 0, "cross-thread wait queue count underflow");
    }

    fn push_stackful_waiter(&self, guard: &mut MutexGuard<'_, WaitState>, waiter: ExternalWaiter) {
        self.compact_inactive_stackful_waiters_locked(guard);
        self.stackful_waiters.fetch_add(1, Ordering::AcqRel);
        guard.stackful_waiters.push_back(waiter);
    }

    fn push_or_update_async_waiter(
        &self,
        guard: &mut MutexGuard<'_, WaitState>,
        id: usize,
        waker: &Waker,
    ) {
        if let Some(waiter) = guard
            .async_waiters
            .iter_mut()
            .find(|waiter| waiter.id == id)
        {
            if !waiter.waker.will_wake(waker) {
                waiter.waker = waker.clone();
            }
            return;
        }

        self.async_waiters.fetch_add(1, Ordering::AcqRel);
        guard.async_waiters.push_back(AsyncWaiter {
            id,
            waker: waker.clone(),
        });
    }

    fn remove_async_waiter(&self, id: usize) {
        let mut state = self.lock();
        self.remove_async_waiter_locked(&mut state, id);
    }

    fn remove_async_waiter_locked(&self, guard: &mut MutexGuard<'_, WaitState>, id: usize) {
        let Some(index) = guard
            .async_waiters
            .iter()
            .position(|waiter| waiter.id == id)
        else {
            return;
        };

        guard.async_waiters.remove(index);
        let previous = self.async_waiters.fetch_sub(1, Ordering::AcqRel);
        assert!(
            previous != 0,
            "cross-thread async wait queue count underflow"
        );
    }

    fn notify_one(&self) {
        loop {
            match self.advance_generation_and_pop_wake() {
                WakeOne::Stackful(waiter) => {
                    if waiter.wake_ready() {
                        return;
                    }
                }
                WakeOne::Async(waker) => {
                    waker.wake();
                    return;
                }
                WakeOne::Thread => {
                    self.ready.notify_one();
                    return;
                }
                WakeOne::None => return,
            }
        }
    }

    fn notify_all(&self) {
        let wake = self.advance_generation_and_take_wakes();
        for waiter in wake.stackful {
            waiter.wake_ready();
        }
        for waker in wake.async_wakers {
            waker.wake();
        }
        if wake.notify_threads {
            self.ready.notify_all();
        }
    }

    fn advance_generation_and_pop_wake(&self) -> WakeOne {
        let mut state = self.lock();
        while let Some(waiter) = state.stackful_waiters.pop_front() {
            let previous = self.stackful_waiters.fetch_sub(1, Ordering::AcqRel);
            assert!(
                previous != 0,
                "cross-thread stackful wait queue count underflow"
            );
            if waiter.mark_ready() {
                state.generation = state.generation.wrapping_add(1);
                return WakeOne::Stackful(waiter);
            }
        }

        if let Some(waiter) = state.async_waiters.pop_front() {
            state.generation = state.generation.wrapping_add(1);
            let previous = self.async_waiters.fetch_sub(1, Ordering::AcqRel);
            assert!(
                previous != 0,
                "cross-thread async wait queue count underflow"
            );
            return WakeOne::Async(waiter.waker);
        }

        if self.thread_waiters.load(Ordering::Acquire) != 0 {
            state.generation = state.generation.wrapping_add(1);
            WakeOne::Thread
        } else {
            WakeOne::None
        }
    }

    fn compact_inactive_stackful_waiters_locked(&self, guard: &mut MutexGuard<'_, WaitState>) {
        let before = guard.stackful_waiters.len();
        guard.stackful_waiters.retain(ExternalWaiter::is_active);
        let removed = before - guard.stackful_waiters.len();
        if removed != 0 {
            let previous = self.stackful_waiters.fetch_sub(removed, Ordering::AcqRel);
            assert!(
                previous >= removed,
                "cross-thread stackful wait queue count underflow"
            );
        }
    }

    fn advance_generation_and_take_wakes(&self) -> WakeAll {
        let mut state = self.lock();
        let stackful_waiters = std::mem::take(&mut state.stackful_waiters);
        let stackful_removed = stackful_waiters.len();
        let stackful = stackful_waiters
            .into_iter()
            .filter(ExternalWaiter::mark_ready)
            .collect::<VecDeque<_>>();
        if stackful_removed != 0 {
            let previous = self
                .stackful_waiters
                .fetch_sub(stackful_removed, Ordering::AcqRel);
            assert!(
                previous >= stackful_removed,
                "cross-thread stackful wait queue count underflow"
            );
        }

        let async_wakers = std::mem::take(&mut state.async_waiters)
            .into_iter()
            .map(|waiter| waiter.waker)
            .collect::<Vec<_>>();
        let async_count = async_wakers.len();
        if async_count != 0 {
            let previous = self.async_waiters.fetch_sub(async_count, Ordering::AcqRel);
            assert!(
                previous >= async_count,
                "cross-thread async wait queue count underflow"
            );
        }

        let notify_threads = self.thread_waiters.load(Ordering::Acquire) != 0;
        let stackful_count = stackful.len();
        if stackful_count != 0 || async_count != 0 || notify_threads {
            state.generation = state.generation.wrapping_add(1);
        }

        WakeAll {
            stackful,
            async_wakers,
            notify_threads,
        }
    }

    fn lock(&self) -> MutexGuard<'_, WaitState> {
        self.state
            .lock()
            .expect("cross-thread channel wait queue mutex poisoned")
    }
}

#[derive(Default)]
struct WaitState {
    generation: u64,
    stackful_waiters: VecDeque<ExternalWaiter>,
    async_waiters: VecDeque<AsyncWaiter>,
}

struct AsyncWaiter {
    id: usize,
    waker: Waker,
}

enum WakeOne {
    Stackful(ExternalWaiter),
    Async(Waker),
    Thread,
    None,
}

struct WakeAll {
    stackful: VecDeque<ExternalWaiter>,
    async_wakers: Vec<Waker>,
    notify_threads: bool,
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::hint::black_box;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use crate::Runtime;
    use crate::allocation_tracking;
    use crate::channel::cross_thread;
    use crate::channel::{RecvError, SendError, TryRecvError, TrySendError};

    fn wait_until_blocked(waiters: &AtomicUsize) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while waiters.load(Ordering::Acquire) == 0 {
            assert!(
                Instant::now() < deadline,
                "cross-thread channel operation did not block"
            );
            thread::yield_now();
        }
    }

    async fn wait_until_blocked_async(waiters: &AtomicUsize) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while waiters.load(Ordering::Acquire) == 0 {
            assert!(
                Instant::now() < deadline,
                "cross-thread channel operation did not block"
            );
            tokio::task::yield_now().await;
        }
    }

    #[test]
    #[should_panic(expected = "cross-thread bounded channel capacity must be nonzero")]
    fn threaded_channel_rejects_zero_capacity() {
        let _ = cross_thread::bounded::<i32>(0);
    }

    #[test]
    fn threaded_channel_try_send_try_recv_and_fifo_per_sender() {
        let (tx, rx) = cross_thread::bounded(2).thread();

        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        assert_eq!(tx.try_send(3), Err(TrySendError::Full(3)));
        assert_eq!(rx.try_recv(), Ok(1));
        assert_eq!(rx.try_recv(), Ok(2));
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        drop(tx);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Closed));
    }

    #[test]
    fn cross_runtime_channel_warmed_ready_paths_do_not_allocate() {
        const ITERATIONS: u64 = 128;

        let (tx, rx) = cross_thread::bounded(1).thread();
        tx.try_send(0).unwrap();
        assert_eq!(rx.try_recv(), Ok(0));
        let ((), thread_counts) = allocation_tracking::measure(|| {
            for i in 0..ITERATIONS {
                tx.try_send(i).unwrap();
                black_box(rx.try_recv().unwrap());
            }
        });
        assert_eq!(
            thread_counts.allocating_operations(),
            0,
            "{thread_counts:?}"
        );
        assert_eq!(
            thread_counts.allocated_or_reallocated_bytes(),
            0,
            "{thread_counts:?}"
        );

        let mut runtime = Runtime::new();
        runtime.block_on(|cx| {
            let (tx, rx) = cross_thread::bounded(1).stackful();
            tx.send(cx, 0).unwrap();
            assert_eq!(rx.recv(cx), Ok(0));
            let ((), stackful_counts) = allocation_tracking::measure(|| {
                for i in 0..ITERATIONS {
                    tx.send(cx, i).unwrap();
                    black_box(rx.recv(cx).unwrap());
                }
            });
            assert_eq!(
                stackful_counts.allocating_operations(),
                0,
                "{stackful_counts:?}"
            );
            assert_eq!(
                stackful_counts.allocated_or_reallocated_bytes(),
                0,
                "{stackful_counts:?}"
            );
        });

        let (tx, rx) = cross_thread::bounded(1).tokio();
        tx.try_send(0).unwrap();
        assert_eq!(rx.try_recv(), Ok(0));
        let ((), async_counts) = allocation_tracking::measure(|| {
            for i in 0..ITERATIONS {
                tx.try_send(i).unwrap();
                black_box(rx.try_recv().unwrap());
            }
        });
        assert_eq!(async_counts.allocating_operations(), 0, "{async_counts:?}");
        assert_eq!(
            async_counts.allocated_or_reallocated_bytes(),
            0,
            "{async_counts:?}"
        );
    }

    #[test]
    fn threaded_channel_clone_drop_close_semantics() {
        let (tx, rx) = cross_thread::bounded::<i32>(1).thread();
        let tx_clone = tx.clone();

        drop(tx);
        assert!(!rx.is_closed());
        drop(tx_clone);
        assert!(rx.is_closed());
        assert_eq!(rx.try_recv(), Err(TryRecvError::Closed));

        let (tx, rx) = cross_thread::bounded::<i32>(1).thread();
        let rx_clone = rx.clone();
        drop(rx);
        assert!(!tx.is_closed());
        drop(rx_clone);
        assert!(tx.is_closed());
    }

    #[test]
    fn threaded_channel_final_receiver_drop_closes_sender() {
        let (tx, rx) = cross_thread::bounded(1).thread();
        let rx_clone = rx.clone();

        drop(rx);
        assert!(!tx.is_closed());
        drop(rx_clone);
        assert!(tx.is_closed());
        assert_eq!(tx.try_send(5), Err(TrySendError::Closed(5)));
    }

    #[test]
    fn threaded_channel_blocking_sender_waits_for_capacity() {
        let (tx, rx) = cross_thread::bounded(1).thread();
        tx.try_send(1).unwrap();

        let (started_tx, started_rx) = mpsc::channel();
        let sender = tx.clone();
        let handle = thread::spawn(move || {
            started_tx.send(()).unwrap();
            sender.send_blocking(2).unwrap();
        });

        started_rx.recv().unwrap();
        wait_until_blocked(&tx.inner.send_wait.thread_waiters);
        assert_eq!(rx.try_recv(), Ok(1));
        handle.join().unwrap();
        assert_eq!(rx.try_recv(), Ok(2));
    }

    #[test]
    fn threaded_channel_blocking_receiver_waits_for_value() {
        let (tx, rx) = cross_thread::bounded(1).thread();

        let (started_tx, started_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            started_tx.send(()).unwrap();
            rx.recv_blocking().unwrap()
        });

        started_rx.recv().unwrap();
        wait_until_blocked(&tx.inner.recv_wait.thread_waiters);
        tx.try_send(42).unwrap();
        assert_eq!(handle.join().unwrap(), 42);
    }

    #[test]
    fn threaded_channel_final_receiver_drop_wakes_blocking_sender() {
        let (tx, rx) = cross_thread::bounded(1).thread();
        tx.try_send(1).unwrap();

        let (started_tx, started_rx) = mpsc::channel();
        let sender = tx.clone();
        let handle = thread::spawn(move || {
            started_tx.send(()).unwrap();
            sender.send_blocking(2)
        });

        started_rx.recv().unwrap();
        wait_until_blocked(&tx.inner.send_wait.thread_waiters);
        drop(rx);
        match handle.join().unwrap() {
            Err(SendError(value)) => assert_eq!(value, 2),
            Ok(()) => panic!("send should fail after final receiver drop"),
        }
    }

    #[test]
    fn threaded_channel_final_sender_drop_wakes_blocking_receiver() {
        let (tx, rx) = cross_thread::bounded::<i32>(1).thread();

        let (started_tx, started_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            started_tx.send(()).unwrap();
            rx.recv_blocking()
        });

        started_rx.recv().unwrap();
        wait_until_blocked(&tx.inner.recv_wait.thread_waiters);
        drop(tx);
        assert_eq!(handle.join().unwrap(), Err(RecvError));
    }

    #[test]
    fn cross_runtime_channel_stackful_try_send_try_recv_and_closure() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let (tx, rx) = cross_thread::stackful(1);

            assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
            tx.try_send(1).unwrap();
            assert_eq!(tx.try_send(2), Err(TrySendError::Full(2)));
            assert_eq!(rx.try_recv(), Ok(1));
            tx.send(cx, 3).unwrap();
            assert_eq!(rx.recv(cx), Ok(3));
            drop(tx);
            assert_eq!(rx.recv(cx), Err(RecvError));
        });
    }

    #[test]
    fn cross_runtime_channel_stackful_sender_waits_for_thread_receiver() {
        let (tx, rx) = cross_thread::bounded(1).stackful_to_thread();
        tx.try_send(1).unwrap();
        let observer = tx.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let receiver = thread::spawn(move || {
            started_rx.recv().unwrap();
            wait_until_blocked(&observer.inner.send_wait.stackful_waiters);
            assert_eq!(rx.recv_blocking(), Ok(1));
            assert_eq!(rx.recv_blocking(), Ok(2));
        });

        let mut runtime = Runtime::new();
        let output = runtime.block_on(|cx| {
            cx.scope(|scope| {
                let sender = scope.spawn(move |cx| {
                    started_tx.send(()).unwrap();
                    tx.send(cx, 2).unwrap();
                    5
                });

                sender.join(cx)
            })
        });

        receiver.join().unwrap();
        assert_eq!(output, 5);
    }

    #[test]
    fn cross_runtime_channel_stackful_receiver_waits_for_thread_sender() {
        let (tx, rx) = cross_thread::bounded(1).thread_to_stackful();
        let observer = rx.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let sender = thread::spawn(move || {
            started_rx.recv().unwrap();
            wait_until_blocked(&observer.inner.recv_wait.stackful_waiters);
            tx.send_blocking(42).unwrap();
        });

        let mut runtime = Runtime::new();
        let output = runtime.block_on(|cx| {
            cx.scope(|scope| {
                let receiver = scope.spawn(move |cx| {
                    started_tx.send(()).unwrap();
                    rx.recv(cx).unwrap()
                });

                receiver.join(cx)
            })
        });

        sender.join().unwrap();
        assert_eq!(output, 42);
    }

    #[test]
    fn cross_runtime_channel_final_receiver_drop_wakes_stackful_sender() {
        let (tx, rx) = cross_thread::bounded(1).stackful_to_thread();
        tx.try_send(1).unwrap();
        let observer = tx.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let dropper = thread::spawn(move || {
            started_rx.recv().unwrap();
            wait_until_blocked(&observer.inner.send_wait.stackful_waiters);
            drop(rx);
        });

        let mut runtime = Runtime::new();
        let result = runtime.block_on(|cx| {
            cx.scope(|scope| {
                let sender = scope.spawn(move |cx| {
                    started_tx.send(()).unwrap();
                    tx.send(cx, 2)
                });

                sender.join(cx)
            })
        });

        dropper.join().unwrap();
        match result {
            Err(SendError(value)) => assert_eq!(value, 2),
            Ok(()) => panic!("send should fail after final receiver drop"),
        }
    }

    #[test]
    fn cross_runtime_channel_final_sender_drop_wakes_stackful_receiver() {
        let (tx, rx) = cross_thread::bounded::<i32>(1).thread_to_stackful();
        let observer = rx.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let dropper = thread::spawn(move || {
            started_rx.recv().unwrap();
            wait_until_blocked(&observer.inner.recv_wait.stackful_waiters);
            drop(tx);
        });

        let mut runtime = Runtime::new();
        let result = runtime.block_on(|cx| {
            cx.scope(|scope| {
                let receiver = scope.spawn(move |cx| {
                    started_tx.send(()).unwrap();
                    rx.recv(cx)
                });

                receiver.join(cx)
            })
        });

        dropper.join().unwrap();
        assert_eq!(result, Err(RecvError));
    }

    #[test]
    fn cross_runtime_channel_unrelated_stackful_work_continues_while_receiver_waits() {
        let (tx, rx) = cross_thread::bounded(1).thread_to_stackful();
        let mut runtime = Runtime::new();

        let output = runtime.block_on(|cx| {
            cx.scope(|scope| {
                let receiver = scope.spawn(move |cx| rx.recv(cx).unwrap());
                let other = scope.spawn(|_| 40);

                assert_eq!(other.join(cx), 40);
                tx.try_send(2).unwrap();
                receiver.join(cx)
            })
        });

        assert_eq!(output, 2);
    }

    #[test]
    fn cross_runtime_channel_staged_external_wake_runs_before_scope_cancel() {
        let (tx, rx) = cross_thread::bounded(1).thread_to_stackful();
        let observer = rx.clone();
        let received = Rc::new(Cell::new(false));
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let received = Rc::clone(&received);
                scope.spawn(move |cx| {
                    assert_eq!(rx.recv(cx), Ok(42));
                    received.set(true);
                });

                cx.yield_now();
                wait_until_blocked(&observer.inner.recv_wait.stackful_waiters);
                tx.send_blocking(42).unwrap();
            });
        });

        assert!(received.get());
    }

    #[test]
    fn cross_runtime_channel_canceled_stackful_waiter_is_skipped_on_later_notify() {
        let (tx, rx) = cross_thread::bounded::<i32>(1).thread_to_stackful();
        let observer = rx.clone();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                scope.spawn(move |cx| {
                    let _ = rx.recv(cx);
                });

                cx.yield_now();
                wait_until_blocked(&observer.inner.recv_wait.stackful_waiters);
            });
        });

        assert_eq!(
            observer
                .inner
                .recv_wait
                .stackful_waiters
                .load(Ordering::Acquire),
            1
        );
        drop(tx);
        assert_eq!(
            observer
                .inner
                .recv_wait
                .stackful_waiters
                .load(Ordering::Acquire),
            0
        );
    }

    #[test]
    fn cross_runtime_channel_stackful_waiter_is_claimed_before_wake() {
        let external_wake = std::sync::Arc::new(crate::ExternalWake::new());
        let registration = crate::ExternalWaitRegistration::new(
            &external_wake,
            crate::TaskKey {
                slot: 0,
                generation: 0,
            },
        );
        let wait_queue = super::WaitQueue::default();
        {
            let mut wait = wait_queue.lock();
            wait_queue.push_stackful_waiter(&mut wait, registration.waiter());
        }

        let waiter = match wait_queue.advance_generation_and_pop_wake() {
            super::WakeOne::Stackful(waiter) => waiter,
            _ => panic!("expected stackful waiter"),
        };
        drop(registration);

        assert!(waiter.wake_ready());
        assert_eq!(wait_queue.stackful_waiters.load(Ordering::Acquire), 0);
        assert_eq!(external_wake.inner().waiters, 0);
        assert_eq!(external_wake.take_ready().len(), 1);
    }

    #[test]
    fn cross_runtime_channel_two_stackful_runtimes_exchange_messages() {
        let (tx, rx) = cross_thread::stackful(1);
        let observer = rx.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let receiver = thread::spawn(move || {
            let mut runtime = Runtime::new();
            runtime.block_on(|cx| {
                cx.scope(|scope| {
                    let receiver = scope.spawn(move |cx| {
                        ready_tx.send(()).unwrap();
                        rx.recv(cx).unwrap() + rx.recv(cx).unwrap()
                    });

                    receiver.join(cx)
                })
            })
        });

        ready_rx.recv().unwrap();
        wait_until_blocked(&observer.inner.recv_wait.stackful_waiters);
        drop(observer);

        let sender = thread::spawn(move || {
            let mut runtime = Runtime::new();
            runtime.block_on(|cx| {
                cx.scope(|scope| {
                    let sender = scope.spawn(move |cx| {
                        tx.send(cx, 20).unwrap();
                        tx.send(cx, 22).unwrap();
                    });

                    sender.join(cx);
                });
            });
        });

        sender.join().unwrap();
        assert_eq!(receiver.join().unwrap(), 42);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tokio_channel_try_send_try_recv_and_closure() {
        let (tx, rx) = cross_thread::tokio(1);

        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        tx.try_send(1).unwrap();
        assert_eq!(tx.try_send(2), Err(TrySendError::Full(2)));
        assert_eq!(rx.try_recv(), Ok(1));
        tx.send(3).await.unwrap();
        assert_eq!(rx.recv().await, Ok(3));
        drop(tx);
        assert_eq!(rx.recv().await, Err(RecvError));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tokio_channel_receiver_task_wakes_when_value_sent() {
        let (tx, rx) = cross_thread::tokio(1);
        let observer = rx.clone();
        let receiver = tokio::spawn(async move { rx.recv().await.unwrap() });

        wait_until_blocked_async(&observer.inner.recv_wait.async_waiters).await;
        tx.send(42).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), receiver)
                .await
                .unwrap()
                .unwrap(),
            42
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tokio_channel_sender_task_wakes_when_capacity_available() {
        let (tx, rx) = cross_thread::tokio(1);
        tx.try_send(1).unwrap();
        let observer = tx.clone();
        let sender = tokio::spawn(async move { tx.send(2).await });

        wait_until_blocked_async(&observer.inner.send_wait.async_waiters).await;
        assert_eq!(rx.recv().await, Ok(1));
        tokio::time::timeout(Duration::from_secs(1), sender)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(rx.recv().await, Ok(2));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tokio_channel_final_sender_drop_wakes_async_receiver() {
        let (tx, rx) = cross_thread::tokio::<i32>(1);
        let observer = rx.clone();
        let receiver = tokio::spawn(async move { rx.recv().await });

        wait_until_blocked_async(&observer.inner.recv_wait.async_waiters).await;
        drop(tx);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), receiver)
                .await
                .unwrap()
                .unwrap(),
            Err(RecvError)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tokio_channel_final_receiver_drop_wakes_async_sender() {
        let (tx, rx) = cross_thread::tokio(1);
        tx.try_send(1).unwrap();
        let observer = tx.clone();
        let sender = tokio::spawn(async move { tx.send(2).await });

        wait_until_blocked_async(&observer.inner.send_wait.async_waiters).await;
        drop(rx);
        match tokio::time::timeout(Duration::from_secs(1), sender)
            .await
            .unwrap()
            .unwrap()
        {
            Err(SendError(value)) => assert_eq!(value, 2),
            Ok(()) => panic!("send should fail after final receiver drop"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tokio_channel_async_waiter_drop_unregisters_waker() {
        let (_tx, rx) = cross_thread::tokio::<i32>(1);
        let mut receive = Box::pin(rx.recv());

        assert!(
            tokio::time::timeout(Duration::from_millis(1), &mut receive)
                .await
                .is_err()
        );
        assert_eq!(rx.inner.recv_wait.async_waiters.load(Ordering::Acquire), 1);
        drop(receive);
        assert_eq!(rx.inner.recv_wait.async_waiters.load(Ordering::Acquire), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tokio_channel_pending_send_future_keeps_sender_side_open() {
        let (tx, rx) = cross_thread::tokio(1);
        tx.try_send(1).unwrap();

        let mut send = Box::pin(tx.send(2));
        drop(tx);

        assert!(!rx.is_closed());
        assert!(
            tokio::time::timeout(Duration::from_millis(1), &mut send)
                .await
                .is_err()
        );
        assert!(!rx.is_closed());
        assert_eq!(rx.recv().await, Ok(1));
        send.await.unwrap();
        assert_eq!(rx.recv().await, Ok(2));
        assert!(rx.is_closed());
        assert_eq!(rx.recv().await, Err(RecvError));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tokio_channel_pending_recv_future_keeps_receiver_side_open() {
        let (tx, rx) = cross_thread::tokio(1);
        let mut receive = Box::pin(rx.recv());
        drop(rx);

        assert!(!tx.is_closed());
        assert!(
            tokio::time::timeout(Duration::from_millis(1), &mut receive)
                .await
                .is_err()
        );
        assert!(!tx.is_closed());
        tx.send(5).await.unwrap();
        assert_eq!(receive.await, Ok(5));
        assert!(tx.is_closed());
        match tx.send(6).await {
            Err(SendError(value)) => assert_eq!(value, 6),
            Ok(()) => panic!("send should fail after pending receiver completes"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tokio_channel_dropped_send_future_closes_sender_side() {
        let (tx, rx) = cross_thread::tokio::<i32>(1);
        let mut receive = Box::pin(rx.recv());

        assert!(
            tokio::time::timeout(Duration::from_millis(1), &mut receive)
                .await
                .is_err()
        );
        let send = tx.send(7);
        drop(tx);
        assert!(!rx.is_closed());
        drop(send);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), receive)
                .await
                .unwrap(),
            Err(RecvError)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tokio_channel_tokio_sender_wakes_stackful_receiver() {
        let (tx, rx) = cross_thread::bounded(1).tokio_to_stackful();
        let observer = rx.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let receiver = thread::spawn(move || {
            let mut runtime = Runtime::new();
            runtime.block_on(|cx| {
                cx.scope(|scope| {
                    let receiver = scope.spawn(move |cx| {
                        ready_tx.send(()).unwrap();
                        rx.recv(cx).unwrap()
                    });

                    receiver.join(cx)
                })
            })
        });

        ready_rx.recv().unwrap();
        wait_until_blocked(&observer.inner.recv_wait.stackful_waiters);
        tx.send(55).await.unwrap();
        assert_eq!(receiver.join().unwrap(), 55);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tokio_channel_tokio_sender_wakes_full_stackful_receiver() {
        let (tx, rx) = cross_thread::bounded(1).tokio_to_stackful();
        tx.try_send(1).unwrap();
        let observer = tx.clone();
        let sender = tokio::spawn(async move { tx.send(2).await });

        wait_until_blocked_async(&observer.inner.send_wait.async_waiters).await;
        let receiver = thread::spawn(move || {
            let mut runtime = Runtime::new();
            runtime.block_on(|cx| {
                cx.scope(|scope| {
                    let receiver = scope.spawn(move |cx| {
                        assert_eq!(rx.recv(cx), Ok(1));
                        rx.recv(cx).unwrap()
                    });

                    receiver.join(cx)
                })
            })
        });

        tokio::time::timeout(Duration::from_secs(1), sender)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(receiver.join().unwrap(), 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tokio_channel_stackful_sender_wakes_tokio_receiver() {
        let (tx, rx) = cross_thread::bounded(1).stackful_to_tokio();
        let observer = rx.clone();
        let receiver = tokio::spawn(async move { rx.recv().await.unwrap() });

        wait_until_blocked_async(&observer.inner.recv_wait.async_waiters).await;
        let sender = thread::spawn(move || {
            let mut runtime = Runtime::new();
            runtime.block_on(|cx| {
                cx.scope(|scope| {
                    let sender = scope.spawn(move |cx| tx.send(cx, 66).unwrap());
                    sender.join(cx);
                });
            });
        });

        sender.join().unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), receiver)
                .await
                .unwrap()
                .unwrap(),
            66
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tokio_channel_tokio_receiver_wakes_full_stackful_sender() {
        let (tx, rx) = cross_thread::bounded(1).stackful_to_tokio();
        tx.try_send(1).unwrap();
        let observer = tx.clone();
        let sender = thread::spawn(move || {
            let mut runtime = Runtime::new();
            runtime.block_on(|cx| {
                cx.scope(|scope| {
                    let sender = scope.spawn(move |cx| tx.send(cx, 2).unwrap());
                    sender.join(cx);
                });
            });
        });

        wait_until_blocked(&observer.inner.send_wait.stackful_waiters);
        drop(observer);
        assert_eq!(rx.recv().await, Ok(1));
        assert_eq!(rx.recv().await, Ok(2));
        sender.join().unwrap();
    }
}
