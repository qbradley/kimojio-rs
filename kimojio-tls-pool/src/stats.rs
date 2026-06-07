// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::atomic::{AtomicU64, Ordering};

/// Thread-safe pool statistics.
#[derive(Debug)]
pub struct PoolStats {
    submitted: AtomicU64,
    immediate: AtomicU64,
    background_routed: AtomicU64,
    queued: AtomicU64,
    completed: AtomicU64,
    failed: AtomicU64,
    executors: Vec<ExecutorStats>,
}

impl PoolStats {
    /// Creates pool statistics for `executor_count` background executors.
    pub fn new(executor_count: usize) -> Self {
        Self {
            submitted: AtomicU64::new(0),
            immediate: AtomicU64::new(0),
            background_routed: AtomicU64::new(0),
            queued: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            executors: (0..executor_count).map(ExecutorStats::new).collect(),
        }
    }

    /// Records an operation submission.
    pub fn record_submitted(&self) {
        self.submitted.fetch_add(1, Ordering::Relaxed);
    }

    /// Records immediate execution.
    pub fn record_immediate(&self) {
        self.immediate.fetch_add(1, Ordering::Relaxed);
    }

    /// Records background routing for `executor`.
    pub fn record_background_routed(&self, executor: usize) {
        self.background_routed.fetch_add(1, Ordering::Relaxed);
        if let Some(stats) = self.executors.get(executor) {
            stats.routed.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Records queueing for `executor`.
    pub fn record_queued(&self, executor: usize) {
        self.queued.fetch_add(1, Ordering::Relaxed);
        if let Some(stats) = self.executors.get(executor) {
            stats.queued.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Records a completed operation.
    pub fn record_completed(&self, executor: Option<usize>) {
        self.completed.fetch_add(1, Ordering::Relaxed);
        if let Some(executor) = executor
            && let Some(stats) = self.executors.get(executor)
        {
            stats.completed.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Records a failed operation.
    pub fn record_failed(&self, executor: Option<usize>) {
        self.failed.fetch_add(1, Ordering::Relaxed);
        if let Some(executor) = executor
            && let Some(stats) = self.executors.get(executor)
        {
            stats.failed.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Returns a point-in-time statistics snapshot.
    pub fn snapshot(&self) -> PoolStatsSnapshot {
        PoolStatsSnapshot {
            submitted: self.submitted.load(Ordering::Relaxed),
            immediate: self.immediate.load(Ordering::Relaxed),
            background_routed: self.background_routed.load(Ordering::Relaxed),
            queued: self.queued.load(Ordering::Relaxed),
            completed: self.completed.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            executors: self.executors.iter().map(ExecutorStats::snapshot).collect(),
        }
    }
}

impl Default for PoolStats {
    fn default() -> Self {
        Self::new(0)
    }
}

#[derive(Debug)]
struct ExecutorStats {
    id: usize,
    routed: AtomicU64,
    queued: AtomicU64,
    completed: AtomicU64,
    failed: AtomicU64,
}

impl ExecutorStats {
    fn new(id: usize) -> Self {
        Self {
            id,
            routed: AtomicU64::new(0),
            queued: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            failed: AtomicU64::new(0),
        }
    }

    fn snapshot(&self) -> ExecutorStatsSnapshot {
        ExecutorStatsSnapshot {
            id: self.id,
            routed: self.routed.load(Ordering::Relaxed),
            queued: self.queued.load(Ordering::Relaxed),
            completed: self.completed.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
        }
    }
}

/// Point-in-time pool statistics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PoolStatsSnapshot {
    /// Operations submitted to the pool.
    pub submitted: u64,
    /// Operations executed immediately.
    pub immediate: u64,
    /// Operations routed to background executors.
    pub background_routed: u64,
    /// Operations queued for background executors.
    pub queued: u64,
    /// Operations completed successfully.
    pub completed: u64,
    /// Operations completed with failure.
    pub failed: u64,
    /// Per-executor statistics.
    pub executors: Vec<ExecutorStatsSnapshot>,
}

/// Point-in-time executor statistics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutorStatsSnapshot {
    /// Executor identifier.
    pub id: usize,
    /// Operations routed to this executor.
    pub routed: u64,
    /// Operations queued for this executor.
    pub queued: u64,
    /// Operations completed successfully on this executor.
    pub completed: u64,
    /// Operations completed with failure on this executor.
    pub failed: u64,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::*;

    #[test]
    fn empty_stats_snapshot_has_executor_entries() {
        let stats = PoolStats::new(2);
        let snapshot = stats.snapshot();
        assert_eq!(snapshot.submitted, 0);
        assert_eq!(snapshot.executors.len(), 2);
        assert_eq!(snapshot.executors[0].id, 0);
        assert_eq!(snapshot.executors[1].id, 1);
    }

    #[test]
    fn records_pool_and_executor_counts() {
        let stats = PoolStats::new(2);
        stats.record_submitted();
        stats.record_immediate();
        stats.record_background_routed(1);
        stats.record_queued(1);
        stats.record_completed(Some(1));
        stats.record_failed(Some(1));

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.submitted, 1);
        assert_eq!(snapshot.immediate, 1);
        assert_eq!(snapshot.background_routed, 1);
        assert_eq!(snapshot.queued, 1);
        assert_eq!(snapshot.completed, 1);
        assert_eq!(snapshot.failed, 1);
        assert_eq!(snapshot.executors[1].routed, 1);
        assert_eq!(snapshot.executors[1].queued, 1);
        assert_eq!(snapshot.executors[1].completed, 1);
        assert_eq!(snapshot.executors[1].failed, 1);
    }

    #[test]
    fn snapshots_are_usable_during_concurrent_updates() {
        let stats = Arc::new(PoolStats::new(1));
        let mut workers = Vec::new();

        for _ in 0..4 {
            let stats = Arc::clone(&stats);
            workers.push(thread::spawn(move || {
                for _ in 0..100 {
                    stats.record_submitted();
                    stats.record_background_routed(0);
                    stats.record_queued(0);
                    stats.record_completed(Some(0));
                }
            }));
        }

        for _ in 0..10 {
            let snapshot = stats.snapshot();
            assert!(snapshot.submitted >= snapshot.completed);
        }

        for worker in workers {
            worker.join().unwrap();
        }

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.submitted, 400);
        assert_eq!(snapshot.background_routed, 400);
        assert_eq!(snapshot.queued, 400);
        assert_eq!(snapshot.completed, 400);
        assert_eq!(snapshot.executors[0].completed, 400);
    }
}
