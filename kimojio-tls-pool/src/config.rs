// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::fmt;
use std::time::Duration;

/// Pool placement behavior.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlacementMode {
    /// Route every operation through immediate execution.
    ImmediateOnly,
    /// Route every operation through a background executor.
    BackgroundOnly,
    /// Choose immediate or background execution using configured thresholds and load.
    Adaptive,
}

/// Message size thresholds used by adaptive placement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SizeThresholds {
    small_max: usize,
    background_min: usize,
}

impl SizeThresholds {
    /// Creates thresholds for adaptive placement.
    pub fn new(small_max: usize, background_min: usize) -> Self {
        Self {
            small_max,
            background_min,
        }
    }

    /// Maximum size that should prefer immediate execution.
    pub fn small_max(&self) -> usize {
        self.small_max
    }

    /// Minimum size that is eligible for background execution.
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdleBehavior {
    ready_executors: usize,
    spin_for: Duration,
}

impl IdleBehavior {
    /// Creates idle behavior with a fixed ready executor count and spin duration.
    pub fn new(ready_executors: usize, spin_for: Duration) -> Self {
        Self {
            ready_executors,
            spin_for,
        }
    }

    /// Number of executors configured to remain ready.
    pub fn ready_executors(&self) -> usize {
        self.ready_executors
    }

    /// Duration an executor may stay ready before becoming idle.
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PoolConfig {
    executor_count: usize,
    placement_mode: PlacementMode,
    thresholds: SizeThresholds,
    idle_behavior: IdleBehavior,
}

impl PoolConfig {
    /// Creates a configuration with `executor_count` background executors.
    pub fn new(executor_count: usize) -> Self {
        Self {
            executor_count,
            ..Self::default()
        }
    }

    /// Sets the placement mode.
    pub fn with_placement_mode(mut self, placement_mode: PlacementMode) -> Self {
        self.placement_mode = placement_mode;
        self
    }

    /// Sets adaptive size thresholds.
    pub fn with_thresholds(mut self, thresholds: SizeThresholds) -> Self {
        self.thresholds = thresholds;
        self
    }

    /// Sets idle behavior.
    pub fn with_idle_behavior(mut self, idle_behavior: IdleBehavior) -> Self {
        self.idle_behavior = idle_behavior;
        self
    }

    /// Validates configuration invariants.
    pub fn validate(&self) -> Result<(), PoolConfigError> {
        if self.executor_count == 0 {
            return Err(PoolConfigError::NoExecutors);
        }
        if self.idle_behavior.ready_executors > self.executor_count {
            return Err(PoolConfigError::TooManyReadyExecutors {
                ready_executors: self.idle_behavior.ready_executors,
                executor_count: self.executor_count,
            });
        }
        self.thresholds.validate()
    }

    /// Number of configured executors.
    pub fn executor_count(&self) -> usize {
        self.executor_count
    }

    /// Placement mode.
    pub fn placement_mode(&self) -> PlacementMode {
        self.placement_mode
    }

    /// Adaptive thresholds.
    pub fn thresholds(&self) -> SizeThresholds {
        self.thresholds
    }

    /// Executor idle behavior.
    pub fn idle_behavior(&self) -> IdleBehavior {
        self.idle_behavior
    }
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            executor_count: 1,
            placement_mode: PlacementMode::Adaptive,
            thresholds: SizeThresholds::default(),
            idle_behavior: IdleBehavior::default(),
        }
    }
}

/// Invalid pool configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PoolConfigError {
    /// At least one background executor is required.
    NoExecutors,
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
}

impl fmt::Display for PoolConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoExecutors => f.write_str("TLS pool requires at least one executor"),
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
