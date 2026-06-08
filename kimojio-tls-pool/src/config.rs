// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::fmt;
use std::time::Duration;

/// Default maximum read buffer length accepted by [`crate::TlsStream::read`].
pub const DEFAULT_MAX_READ_LEN: usize = 32 * 1024;
/// Hard cap on executor count accepted by the configuration.
pub const MAX_EXECUTORS: usize = 256;

/// Pool placement behavior for submitted TLS operations.
///
/// Writes follow this policy directly. Reads always perform a small
/// nonblocking fast-path probe first so already-buffered plaintext can complete
/// without parking. If a read or write would block on the socket, it parks on
/// readiness and resumes on a pool executor.
///
/// ```
/// use kimojio_tls_pool::{PlacementMode, PoolConfig};
///
/// let config = PoolConfig::new(4)
///     .with_placement_mode(PlacementMode::Adaptive);
/// assert_eq!(config.placement_mode(), PlacementMode::Adaptive);
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlacementMode {
    /// Prefer caller-thread execution for work that can complete immediately.
    ImmediateOnly,
    /// Route normal operation work through a background executor.
    BackgroundOnly,
    /// Choose immediate or background execution using configured thresholds and load.
    Adaptive,
}

/// Message size thresholds used by adaptive placement.
///
/// Adaptive placement uses these thresholds as a latency-first heuristic:
/// small writes prefer immediate execution, near-maximum writes are eligible for
/// background execution, and medium writes use the current executor-load model.
///
/// ```
/// use kimojio_tls_pool::SizeThresholds;
///
/// let thresholds = SizeThresholds::new(2 * 1024, 16 * 1024);
/// assert_eq!(thresholds.small_max(), 2 * 1024);
/// assert_eq!(thresholds.background_min(), 16 * 1024);
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SizeThresholds {
    small_max: usize,
    background_min: usize,
}

impl SizeThresholds {
    /// Creates thresholds for adaptive placement.
    ///
    /// `small_max` must be less than or equal to `background_min`. The builder
    /// does not validate immediately so configuration can be assembled
    /// fluently; call [`PoolConfig::validate`] or construct a [`crate::TlsPool`]
    /// to surface invalid combinations.
    pub fn new(small_max: usize, background_min: usize) -> Self {
        Self {
            small_max,
            background_min,
        }
    }

    /// Maximum operation size that should prefer immediate execution.
    pub fn small_max(&self) -> usize {
        self.small_max
    }

    /// Minimum operation size that is always eligible for background execution.
    pub fn background_min(&self) -> usize {
        self.background_min
    }

    fn validate(&self) -> Result<(), PoolConfigError> {
        if self.small_max > self.background_min {
            return Err(PoolConfigError::InvalidThresholds {
                small_max: self.small_max,
                background_min: self.background_min,
            });
        }
        Ok(())
    }
}

impl Default for SizeThresholds {
    fn default() -> Self {
        Self {
            small_max: 4 * 1024,
            background_min: 24 * 1024,
        }
    }
}

/// Controls how long executors remain ready after work drains.
///
/// Ready executors spin briefly before blocking on the work queue. This reduces
/// wake-up latency for bursts at the cost of bounded CPU use. Executors beyond
/// `ready_executors` block immediately when they have no work.
///
/// ```
/// use std::time::Duration;
/// use kimojio_tls_pool::IdleBehavior;
///
/// let idle = IdleBehavior::new(2, Duration::from_micros(100));
/// assert_eq!(idle.ready_executors(), 2);
/// assert_eq!(idle.spin_for(), Duration::from_micros(100));
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdleBehavior {
    ready_executors: usize,
    spin_for: Duration,
}

impl IdleBehavior {
    /// Creates idle behavior with a fixed ready executor count and spin duration.
    ///
    /// `ready_executors` must not exceed [`PoolConfig::executor_count`].
    pub fn new(ready_executors: usize, spin_for: Duration) -> Self {
        Self {
            ready_executors,
            spin_for,
        }
    }

    /// Number of executors configured to remain ready after work drains.
    pub fn ready_executors(&self) -> usize {
        self.ready_executors
    }

    /// Duration a ready executor may spin before blocking for new work.
    pub fn spin_for(&self) -> Duration {
        self.spin_for
    }
}

impl Default for IdleBehavior {
    fn default() -> Self {
        Self {
            ready_executors: 1,
            spin_for: Duration::from_micros(50),
        }
    }
}

/// Configuration for a TLS pool.
///
/// `PoolConfig` is a builder-style value. Most setters do not fail
/// immediately; validation happens in [`PoolConfig::validate`] and
/// [`crate::TlsPool::new`].
///
/// ```
/// use std::time::Duration;
///
/// use kimojio_tls_pool::{
///     IdleBehavior, PlacementMode, PoolConfig, SizeThresholds,
/// };
///
/// let config = PoolConfig::new(4)
///     .with_placement_mode(PlacementMode::Adaptive)
///     .with_thresholds(SizeThresholds::new(4 * 1024, 24 * 1024))
///     .with_idle_behavior(IdleBehavior::new(1, Duration::from_micros(50)))
///     .with_max_read_len(32 * 1024);
///
/// config.validate().unwrap();
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PoolConfig {
    executor_count: usize,
    placement_mode: PlacementMode,
    thresholds: SizeThresholds,
    idle_behavior: IdleBehavior,
    max_read_len: usize,
}

impl PoolConfig {
    /// Creates a configuration with `executor_count` background executors.
    ///
    /// This does not spawn threads. Threads are created by [`crate::TlsPool::new`].
    pub fn new(executor_count: usize) -> Self {
        Self {
            executor_count,
            ..Self::default()
        }
    }

    /// Sets the placement mode.
    ///
    /// Use [`PlacementMode::Adaptive`] for the usual latency-first policy, and
    /// [`PlacementMode::BackgroundOnly`] when benchmarking offload overhead or
    /// forcing work through pool executors.
    pub fn with_placement_mode(mut self, placement_mode: PlacementMode) -> Self {
        self.placement_mode = placement_mode;
        self
    }

    /// Sets adaptive size thresholds.
    ///
    /// The thresholds only affect [`PlacementMode::Adaptive`].
    pub fn with_thresholds(mut self, thresholds: SizeThresholds) -> Self {
        self.thresholds = thresholds;
        self
    }

    /// Sets idle behavior.
    ///
    /// Use this to tune the latency/CPU tradeoff for short bursts.
    pub fn with_idle_behavior(mut self, idle_behavior: IdleBehavior) -> Self {
        self.idle_behavior = idle_behavior;
        self
    }

    /// Sets the maximum buffer length accepted by [`crate::TlsStream::read`].
    ///
    /// Oversized reads do not panic; they complete through the supplied callback
    /// with [`crate::OperationError::ReadTooLarge`].
    pub fn with_max_read_len(mut self, max_read_len: usize) -> Self {
        self.max_read_len = max_read_len;
        self
    }

    /// Validates configuration invariants.
    ///
    /// Call this if you want to reject invalid configuration before attempting
    /// to construct a pool.
    ///
    /// ```
    /// use kimojio_tls_pool::{PoolConfig, PoolConfigError};
    ///
    /// let config = PoolConfig::new(0);
    /// assert_eq!(config.validate(), Err(PoolConfigError::NoExecutors));
    /// ```
    pub fn validate(&self) -> Result<(), PoolConfigError> {
        if self.executor_count == 0 {
            return Err(PoolConfigError::NoExecutors);
        }
        if self.executor_count > MAX_EXECUTORS {
            return Err(PoolConfigError::TooManyExecutors {
                executor_count: self.executor_count,
                max_executors: MAX_EXECUTORS,
            });
        }
        if self.idle_behavior.ready_executors > self.executor_count {
            return Err(PoolConfigError::TooManyReadyExecutors {
                ready_executors: self.idle_behavior.ready_executors,
                executor_count: self.executor_count,
            });
        }
        self.thresholds.validate()
    }

    /// Number of background executor threads that [`crate::TlsPool::new`] will spawn.
    pub fn executor_count(&self) -> usize {
        self.executor_count
    }

    /// Placement mode used for submitted operations.
    pub fn placement_mode(&self) -> PlacementMode {
        self.placement_mode
    }

    /// Adaptive thresholds used when [`PoolConfig::placement_mode`] is [`PlacementMode::Adaptive`].
    pub fn thresholds(&self) -> SizeThresholds {
        self.thresholds
    }

    /// Executor idle behavior.
    pub fn idle_behavior(&self) -> IdleBehavior {
        self.idle_behavior
    }

    /// Maximum accepted read buffer length.
    pub fn max_read_len(&self) -> usize {
        self.max_read_len
    }
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            executor_count: 1,
            placement_mode: PlacementMode::Adaptive,
            thresholds: SizeThresholds::default(),
            idle_behavior: IdleBehavior::default(),
            max_read_len: DEFAULT_MAX_READ_LEN,
        }
    }
}

/// Invalid pool configuration.
///
/// ```
/// use std::time::Duration;
///
/// use kimojio_tls_pool::{IdleBehavior, PoolConfig, PoolConfigError};
///
/// let config = PoolConfig::new(1)
///     .with_idle_behavior(IdleBehavior::new(2, Duration::from_micros(10)));
/// assert!(matches!(
///     config.validate(),
///     Err(PoolConfigError::TooManyReadyExecutors { .. })
/// ));
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PoolConfigError {
    /// At least one background executor is required.
    NoExecutors,
    /// Executor count exceeds the configured hard cap.
    TooManyExecutors {
        /// Configured executor count.
        executor_count: usize,
        /// Maximum accepted executor count.
        max_executors: usize,
    },
    /// Ready executor count cannot exceed total executor count.
    TooManyReadyExecutors {
        /// Configured ready executors.
        ready_executors: usize,
        /// Configured executor count.
        executor_count: usize,
    },
    /// Small threshold cannot exceed background threshold.
    InvalidThresholds {
        /// Maximum immediate-preferred size.
        small_max: usize,
        /// Minimum background-eligible size.
        background_min: usize,
    },
    /// Failed to spawn a background executor.
    ExecutorSpawnFailed(String),
}

impl fmt::Display for PoolConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoExecutors => f.write_str("TLS pool requires at least one executor"),
            Self::TooManyExecutors {
                executor_count,
                max_executors,
            } => write!(
                f,
                "executor count {executor_count} exceeds maximum {max_executors}"
            ),
            Self::TooManyReadyExecutors {
                ready_executors,
                executor_count,
            } => write!(
                f,
                "ready executor count {ready_executors} exceeds executor count {executor_count}"
            ),
            Self::InvalidThresholds {
                small_max,
                background_min,
            } => write!(
                f,
                "small threshold {small_max} exceeds background threshold {background_min}"
            ),
            Self::ExecutorSpawnFailed(error) => {
                write!(f, "failed to spawn TLS pool executor: {error}")
            }
        }
    }
}

impl std::error::Error for PoolConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        let config = PoolConfig::default();
        config.validate().unwrap();
        assert_eq!(config.executor_count(), 1);
        assert_eq!(config.idle_behavior().ready_executors(), 1);
        assert_eq!(config.thresholds().small_max(), 4 * 1024);
        assert_eq!(config.thresholds().background_min(), 24 * 1024);
    }

    #[test]
    fn rejects_zero_executors() {
        let config = PoolConfig::new(0);
        assert_eq!(config.validate(), Err(PoolConfigError::NoExecutors));
    }

    #[test]
    fn rejects_too_many_executors() {
        let config = PoolConfig::new(MAX_EXECUTORS + 1);
        assert_eq!(
            config.validate(),
            Err(PoolConfigError::TooManyExecutors {
                executor_count: MAX_EXECUTORS + 1,
                max_executors: MAX_EXECUTORS,
            })
        );
    }

    #[test]
    fn rejects_too_many_ready_executors() {
        let config =
            PoolConfig::new(1).with_idle_behavior(IdleBehavior::new(2, Duration::from_micros(1)));
        assert_eq!(
            config.validate(),
            Err(PoolConfigError::TooManyReadyExecutors {
                ready_executors: 2,
                executor_count: 1,
            })
        );
    }

    #[test]
    fn rejects_inverted_thresholds() {
        let config = PoolConfig::new(1).with_thresholds(SizeThresholds::new(2, 1));
        assert_eq!(
            config.validate(),
            Err(PoolConfigError::InvalidThresholds {
                small_max: 2,
                background_min: 1,
            })
        );
    }
}
