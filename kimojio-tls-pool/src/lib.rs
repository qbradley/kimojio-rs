// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Runtime-independent OpenSSL TLS work placement.
//!
//! `kimojio-tls-pool` is a low-level TLS execution layer for applications that
//! want to keep TLS encryption/decryption work observable and schedulable
//! without committing to a particular async runtime. A [`TlsPool`] owns worker
//! threads, a small readiness reactor, placement policy, and statistics. The
//! pool creates [`TlsStream`] handles that expose callback-completing read and
//! write operations.
//!
//! # What this crate gives you
//!
//! - OpenSSL client/server TLS streams over connected fd-backed transports.
//! - Immediate, background-only, and adaptive operation placement modes.
//! - Same-stream serialization so OpenSSL stream state is never accessed
//!   concurrently.
//! - Readiness-aware progress: socket-blocked reads and writes park without
//!   occupying a pool worker, then resume on an executor when the fd is ready.
//! - Per-pool and per-stream statistics for understanding placement, queueing,
//!   readiness, completion, and failure behavior.
//!
//! # Success path
//!
//! The usual flow is:
//!
//! 1. Build a [`PoolConfig`] and create a [`TlsPool`].
//! 2. Create connected client/server transports, such as `UnixStream`,
//!    `TcpStream`, or another `Read + Write + AsFd + Send + 'static` type.
//! 3. Use [`TlsPool::client`] or [`TlsPool::server`] to perform the OpenSSL
//!    handshake and obtain a [`TlsStream`].
//! 4. Submit [`TlsStream::read`], [`TlsStream::write`],
//!    [`TlsStream::write_shared`], or [`TlsStream::write_batch`] operations.
//! 5. Treat callbacks as notifications: hand results back to your own channel,
//!    queue, reactor, or runtime adapter, then return promptly.
//!
//! ```no_run
//! use std::os::unix::net::UnixStream;
//! use std::thread;
//!
//! use kimojio_tls_pool::{PlacementMode, PoolConfig, TlsPool};
//! use openssl::ssl::{SslAcceptor, SslConnector};
//!
//! # fn tls_contexts() -> (SslAcceptor, SslConnector) { unimplemented!() }
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let (acceptor, connector) = tls_contexts();
//! let pool = TlsPool::new(
//!     PoolConfig::new(4).with_placement_mode(PlacementMode::Adaptive),
//! )?;
//!
//! let (client_io, server_io) = UnixStream::pair()?;
//! let server_pool = pool.clone();
//! let server_thread = thread::spawn(move || server_pool.server(&acceptor, server_io));
//!
//! let client = pool.client(&connector, "localhost", client_io)?;
//! let server = server_thread.join().unwrap()?;
//! # let _ = (client, server);
//! # Ok(())
//! # }
//! ```
//!
//! # Execution and callback model
//!
//! Operations submitted to the same [`TlsStream`] are serialized. This is
//! intentional: OpenSSL TLS stream state is mutable and must not be driven by
//! multiple threads at once. Different streams can make progress concurrently.
//!
//! Callbacks run on the thread that completes the operation. Immediate
//! operations may call back on the submitting thread. Background and
//! readiness-resumed operations call back on a pool executor. Callback panics
//! are contained so stream/executor accounting can recover, but callbacks should
//! still be small, nonblocking notification handlers. In particular, do not
//! synchronously wait inside a callback for another operation on the same stream:
//! same-stream queued work cannot start until the callback returns.
//!
//! ```no_run
//! use std::sync::mpsc;
//! use std::time::Duration;
//!
//! use kimojio_tls_pool::{OperationResult, TlsStream};
//!
//! # fn connected_tls_stream() -> TlsStream { unimplemented!() }
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let stream = connected_tls_stream();
//! let (tx, rx) = mpsc::channel::<OperationResult<usize>>();
//!
//! stream.write(b"hello".to_vec(), Box::new(move |result| {
//!     // Forward the completion to application-owned coordination.
//!     tx.send(result).unwrap();
//! }))?;
//!
//! assert_eq!(rx.recv_timeout(Duration::from_secs(5))??, 5);
//! # Ok(())
//! # }
//! ```
//!
//! # Important contracts and caveats
//!
//! - Transports must be fd-backed (`AsFd`) because the pool waits for socket
//!   readiness. The pool sets the fd to nonblocking mode after handshake and
//!   assumes exclusive control over that fd's blocking-mode state while the
//!   [`TlsStream`] is alive.
//! - The core crate has no tokio, kimojio, or kimojio-stack dependency. Runtime
//!   adapters should sit above this crate.
//! - The default maximum read buffer is 32 KiB. Larger reads complete through
//!   the callback with [`OperationError::ReadTooLarge`].
//! - `write_shared` and `write_batch` are the preferred high-throughput write
//!   APIs when payloads can be shared as `Arc<[u8]>`.

mod config;
mod error;
mod operation;
mod policy;
mod pool;
mod stats;
mod stream;
mod tls;

pub use config::{IdleBehavior, PlacementMode, PoolConfig, PoolConfigError, SizeThresholds};
pub use error::{OperationError, OperationResult, TlsPoolError};
pub use operation::OperationPlacement;
pub use pool::TlsPool;
pub use stats::{ExecutorStatsSnapshot, PoolStats, PoolStatsSnapshot};
pub use stream::{CompletionCallback, StreamStatsSnapshot, TlsStream};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_pool_uses_valid_config() {
        let pool = TlsPool::default();
        assert_eq!(pool.config().executor_count(), 1);
        assert_eq!(pool.config().placement_mode(), PlacementMode::Adaptive);
    }

    #[test]
    fn placeholder_stream_shares_pool_stats() {
        let pool = TlsPool::default();
        let stream = pool.stream();
        assert_eq!(stream.stats().submitted, pool.stats().submitted);
    }
}
