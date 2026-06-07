// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Runtime-independent OpenSSL TLS work placement.
//!
//! This crate provides a pool-oriented API for TLS streams. The initial public
//! surface defines configuration, placement policy, operation results, and
//! statistics; worker execution and TLS stream operations are implemented in
//! later phases.

mod config;
mod error;
mod stats;

use std::sync::Arc;

pub use config::{IdleBehavior, PlacementMode, PoolConfig, PoolConfigError, SizeThresholds};
pub use error::{OperationError, OperationResult, TlsPoolError};
pub use stats::{ExecutorStatsSnapshot, PoolStats, PoolStatsSnapshot};

/// Callback invoked when a submitted TLS operation completes.
pub type CompletionCallback<T> = Box<dyn FnOnce(OperationResult<T>) + Send + 'static>;

/// User-facing pool that owns TLS operation placement policy and shared stats.
#[derive(Clone, Debug)]
pub struct TlsPool {
    config: PoolConfig,
    stats: Arc<PoolStats>,
}

impl TlsPool {
    /// Creates a pool handle from a validated configuration.
    pub fn new(config: PoolConfig) -> Result<Self, PoolConfigError> {
        config.validate()?;
        Ok(Self {
            config,
            stats: Arc::new(PoolStats::new(0)),
        })
    }

    /// Returns the pool configuration.
    pub fn config(&self) -> &PoolConfig {
        &self.config
    }

    /// Returns a snapshot of pool statistics.
    pub fn stats(&self) -> PoolStatsSnapshot {
        self.stats.snapshot()
    }
}

impl Default for TlsPool {
    fn default() -> Self {
        Self::new(PoolConfig::default()).expect("default TLS pool config should be valid")
    }
}

/// User-facing TLS stream handle created by a pool.
#[derive(Debug)]
pub struct TlsStream {
    stats: Arc<PoolStats>,
}

impl TlsStream {
    /// Creates an inert stream handle for tests and future phase wiring.
    pub fn placeholder(pool: &TlsPool) -> Self {
        Self {
            stats: Arc::clone(&pool.stats),
        }
    }

    /// Returns a snapshot of the parent pool statistics.
    pub fn stats(&self) -> PoolStatsSnapshot {
        self.stats.snapshot()
    }
}

/// Placement chosen for a submitted operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationPlacement {
    /// Operation ran on the submitting thread.
    Immediate,
    /// Operation was routed to a background executor.
    Background { executor: usize },
}

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
        let stream = TlsStream::placeholder(&pool);
        assert_eq!(stream.stats(), pool.stats());
    }
}
