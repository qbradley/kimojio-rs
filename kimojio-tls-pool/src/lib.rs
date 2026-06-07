// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Runtime-independent OpenSSL TLS work placement.
//!
//! This crate provides a pool-oriented API for TLS streams. The current surface
//! includes runtime-independent operation placement, callback completion,
//! same-stream serialization, and observable scheduling statistics.

mod config;
mod error;
mod operation;
mod pool;
mod stats;
mod stream;
mod tls;

pub use config::{IdleBehavior, PlacementMode, PoolConfig, PoolConfigError, SizeThresholds};
pub use error::{OperationError, OperationResult, TlsPoolError};
pub use operation::{OperationKind, OperationPlacement};
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
        assert_eq!(stream.stats(), pool.stats());
    }
}
