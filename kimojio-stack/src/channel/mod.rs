// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Bounded and unbounded channels for stackful coroutines.
//!
//! The `bounded` and `unbounded` modules provide multi-producer/multi-consumer
//! channels that park stackful coroutines when a send or receive cannot make
//! progress. They do not block the OS thread and they do not start helper
//! workers. The `cross_thread` module is the explicit boundary for communicating
//! with other runtimes, OS threads, or Tokio-compatible tasks.
//!
//! ```
//! use kimojio_stack::{Runtime, channel};
//!
//! let mut runtime = Runtime::new();
//! runtime.block_on(|cx| {
//!     cx.scope(|scope| {
//!         let (tx, rx) = channel::bounded(1);
//!         scope.spawn(move |cx| tx.send(cx, "hello").unwrap());
//!         assert_eq!(rx.recv(cx).unwrap(), "hello");
//!     });
//! });
//! ```

use std::fmt;

pub mod bounded;
pub mod cross_thread;
pub mod unbounded;

pub use bounded::channel as bounded;
pub use unbounded::channel as unbounded;

/// Error returned when sending to a closed channel.
pub struct SendError<T>(pub T);

impl<T> fmt::Debug for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SendError").finish()
    }
}

impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("channel closed")
    }
}

impl<T: fmt::Debug> std::error::Error for SendError<T> {}

/// Error returned by nonblocking send.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrySendError<T> {
    /// The channel is full.
    Full(T),
    /// The channel is closed.
    Closed(T),
}

impl<T> fmt::Display for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full(_) => f.write_str("channel full"),
            Self::Closed(_) => f.write_str("channel closed"),
        }
    }
}

impl<T: fmt::Debug> std::error::Error for TrySendError<T> {}

/// Error returned when receiving from a closed and empty channel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecvError;

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("channel closed")
    }
}

impl std::error::Error for RecvError {}

/// Error returned by nonblocking receive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TryRecvError {
    /// The channel is open but empty.
    Empty,
    /// The channel is closed and empty.
    Closed,
}

impl fmt::Display for TryRecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("channel empty"),
            Self::Closed => f.write_str("channel closed"),
        }
    }
}

impl std::error::Error for TryRecvError {}
