// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::{OperationPlacement, PlacementMode, PoolConfig};

pub(crate) fn choose_placement(
    config: &PoolConfig,
    operation_size: usize,
    executor_loads: &[u64],
    round_robin_start: usize,
) -> OperationPlacement {
    match config.placement_mode() {
        PlacementMode::ImmediateOnly => OperationPlacement::Immediate,
        PlacementMode::BackgroundOnly => OperationPlacement::Background {
            executor: select_executor(executor_loads, round_robin_start),
        },
        PlacementMode::Adaptive => {
            choose_adaptive(config, operation_size, executor_loads, round_robin_start)
        }
    }
}

pub(crate) fn estimated_operation_cost(operation_size: usize) -> u64 {
    u64::try_from(operation_size.max(1)).unwrap_or(u64::MAX)
}

pub(crate) fn select_executor(executor_loads: &[u64], round_robin_start: usize) -> usize {
    let len = executor_loads.len();
    let mut best = round_robin_start % len;
    let mut best_load = executor_loads[best];

    for offset in 1..len {
        let index = (round_robin_start + offset) % len;
        let load = executor_loads[index];
        if load < best_load {
            best = index;
            best_load = load;
        }
    }

    best
}

fn choose_adaptive(
    config: &PoolConfig,
    operation_size: usize,
    executor_loads: &[u64],
    round_robin_start: usize,
) -> OperationPlacement {
    let thresholds = config.thresholds();
    if operation_size <= thresholds.small_max() {
        return OperationPlacement::Immediate;
    }

    let executor = select_executor(executor_loads, round_robin_start);
    if operation_size >= thresholds.background_min()
        || executor_loads[executor] < estimated_operation_cost(operation_size)
    {
        OperationPlacement::Background { executor }
    } else {
        OperationPlacement::Immediate
    }
}

#[cfg(test)]
mod tests {
    use crate::{OperationPlacement, PlacementMode, PoolConfig, SizeThresholds};

    use super::*;

    #[test]
    fn small_adaptive_operations_choose_immediate() {
        let config = PoolConfig::default();
        assert_eq!(
            choose_placement(&config, config.thresholds().small_max(), &[0, 0], 0),
            OperationPlacement::Immediate
        );
    }

    #[test]
    fn near_maximum_adaptive_operations_are_background_eligible() {
        let config = PoolConfig::default();
        assert_eq!(
            choose_placement(&config, 32 * 1024, &[0, 0], 0),
            OperationPlacement::Background { executor: 0 }
        );
    }

    #[test]
    fn load_estimates_select_lower_completion_path() {
        assert_eq!(select_executor(&[100, 10, 50], 0), 1);
        assert_eq!(select_executor(&[100, 10, 0], 1), 2);
    }

    #[test]
    fn insufficient_load_uses_immediate_path() {
        let config = PoolConfig::new(2)
            .with_placement_mode(PlacementMode::Adaptive)
            .with_thresholds(SizeThresholds::new(4 * 1024, 24 * 1024));
        assert_eq!(
            choose_placement(&config, 1024, &[0, 0], 0),
            OperationPlacement::Immediate
        );
    }

    #[test]
    fn medium_operation_uses_background_when_executor_is_empty() {
        let config = PoolConfig::new(2)
            .with_placement_mode(PlacementMode::Adaptive)
            .with_thresholds(SizeThresholds::new(4 * 1024, 24 * 1024));
        assert_eq!(
            choose_placement(&config, 8 * 1024, &[0, 100_000], 0),
            OperationPlacement::Background { executor: 0 }
        );
    }

    #[test]
    fn medium_operation_stays_immediate_when_executors_are_loaded() {
        let config = PoolConfig::new(2)
            .with_placement_mode(PlacementMode::Adaptive)
            .with_thresholds(SizeThresholds::new(4 * 1024, 24 * 1024));
        assert_eq!(
            choose_placement(&config, 8 * 1024, &[100_000, 100_000], 0),
            OperationPlacement::Immediate
        );
    }
}
