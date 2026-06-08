// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::atomic::{AtomicU64, Ordering};

/// Thread-safe pool statistics accumulator.
///
/// Most users read statistics through [`crate::TlsPool::stats`] or
/// [`crate::TlsStream::stats`], which return [`PoolStatsSnapshot`]. This type is
/// public so advanced users can construct and inspect standalone counters in
/// tests or diagnostics.
#[derive(Debug)]
pub struct PoolStats {
    submitted: AtomicU64,
    immediate: AtomicU64,
    background_routed: AtomicU64,
    queued: AtomicU64,
    stream_queued: AtomicU64,
    executor_queued: AtomicU64,
    readiness_waits: AtomicU64,
    readiness_resumed: AtomicU64,
    completed: AtomicU64,
    failed: AtomicU64,
    ready_spins: AtomicU64,
    idle_transitions: AtomicU64,
    executors: Vec<ExecutorStats>,
}

impl PoolStats {
    /// Creates pool statistics for `executor_count` background executors.
    ///
    /// ```
    /// use kimojio_tls_pool::PoolStats;
    ///
    /// let stats = PoolStats::new(2);
    /// let snapshot = stats.snapshot();
    /// assert_eq!(snapshot.executors.len(), 2);
    /// ```
    pub fn new(executor_count: usize) -> Self {
        Self {
            submitted: AtomicU64::new(0),
            immediate: AtomicU64::new(0),
            background_routed: AtomicU64::new(0),
            queued: AtomicU64::new(0),
            stream_queued: AtomicU64::new(0),
            executor_queued: AtomicU64::new(0),
            readiness_waits: AtomicU64::new(0),
            readiness_resumed: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            ready_spins: AtomicU64::new(0),
            idle_transitions: AtomicU64::new(0),
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

    /// Records queueing behind another same-stream operation.
    pub fn record_stream_queued(&self) {
        self.queued.fetch_add(1, Ordering::Relaxed);
        self.stream_queued.fetch_add(1, Ordering::Relaxed);
    }

    /// Records queueing for `executor`.
    pub fn record_executor_queued(&self, executor: usize) {
        self.queued.fetch_add(1, Ordering::Relaxed);
        self.executor_queued.fetch_add(1, Ordering::Relaxed);
        if let Some(stats) = self.executors.get(executor) {
            stats.queued.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Records an operation parked on socket readiness instead of occupying an executor.
    pub fn record_readiness_wait(&self) {
        self.readiness_waits.fetch_add(1, Ordering::Relaxed);
    }

    /// Records readiness-resumed progress on `executor`.
    pub fn record_readiness_resumed(&self, executor: usize) {
        self.readiness_resumed.fetch_add(1, Ordering::Relaxed);
        if let Some(stats) = self.executors.get(executor) {
            stats.readiness_resumed.fetch_add(1, Ordering::Relaxed);
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

    /// Records that an executor stayed ready and checked for work.
    pub fn record_ready_spin(&self, executor: usize) {
        self.ready_spins.fetch_add(1, Ordering::Relaxed);
        if let Some(stats) = self.executors.get(executor) {
            stats.ready_spins.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Records that an executor transitioned to idle waiting.
    pub fn record_idle_transition(&self, executor: usize) {
        self.idle_transitions.fetch_add(1, Ordering::Relaxed);
        if let Some(stats) = self.executors.get(executor) {
            stats.idle_transitions.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Returns a point-in-time statistics snapshot.
    ///
    /// The snapshot is internally consistent enough for diagnostics, but it is
    /// not a synchronization primitive. Counters may change immediately after
    /// the snapshot is taken.
    pub fn snapshot(&self) -> PoolStatsSnapshot {
        PoolStatsSnapshot {
            submitted: self.submitted.load(Ordering::Relaxed),
            immediate: self.immediate.load(Ordering::Relaxed),
            background_routed: self.background_routed.load(Ordering::Relaxed),
            queued: self.queued.load(Ordering::Relaxed),
            stream_queued: self.stream_queued.load(Ordering::Relaxed),
            executor_queued: self.executor_queued.load(Ordering::Relaxed),
            readiness_waits: self.readiness_waits.load(Ordering::Relaxed),
            readiness_resumed: self.readiness_resumed.load(Ordering::Relaxed),
            completed: self.completed.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            ready_spins: self.ready_spins.load(Ordering::Relaxed),
            idle_transitions: self.idle_transitions.load(Ordering::Relaxed),
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
    readiness_resumed: AtomicU64,
    ready_spins: AtomicU64,
    idle_transitions: AtomicU64,
}

impl ExecutorStats {
    fn new(id: usize) -> Self {
        Self {
            id,
            routed: AtomicU64::new(0),
            queued: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            readiness_resumed: AtomicU64::new(0),
            ready_spins: AtomicU64::new(0),
            idle_transitions: AtomicU64::new(0),
        }
    }

    fn snapshot(&self) -> ExecutorStatsSnapshot {
        ExecutorStatsSnapshot {
            id: self.id,
            routed: self.routed.load(Ordering::Relaxed),
            queued: self.queued.load(Ordering::Relaxed),
            completed: self.completed.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            readiness_resumed: self.readiness_resumed.load(Ordering::Relaxed),
            ready_spins: self.ready_spins.load(Ordering::Relaxed),
            idle_transitions: self.idle_transitions.load(Ordering::Relaxed),
        }
    }
}

/// Point-in-time pool statistics.
///
/// Use this to answer questions such as:
///
/// - Are operations going immediate or background?
/// - Is work queueing behind same-stream serialization or executor capacity?
/// - Are socket-blocked operations parking on readiness instead of occupying
///   executors?
/// - Which executors are completing work?
///
/// ```
/// use kimojio_tls_pool::TlsPool;
///
/// let pool = TlsPool::default();
/// let stats = pool.stats();
/// assert_eq!(stats.submitted, stats.completed + stats.failed);
/// ```
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
    /// Operations queued behind same-stream work.
    pub stream_queued: u64,
    /// Operations queued to background executors.
    pub executor_queued: u64,
    /// Operations parked on socket readiness without occupying an executor.
    pub readiness_waits: u64,
    /// Readiness-resumed operation attempts executed by pool executors.
    pub readiness_resumed: u64,
    /// Operations completed successfully.
    pub completed: u64,
    /// Operations completed with failure.
    pub failed: u64,
    /// Ready-spin checks performed by executors.
    pub ready_spins: u64,
    /// Executor transitions into idle waiting.
    pub idle_transitions: u64,
    /// Per-executor statistics.
    pub executors: Vec<ExecutorStatsSnapshot>,
}

/// Point-in-time executor statistics.
///
/// Executor counters are indexed by `id`, which corresponds to the pool worker
/// identifier. They help identify whether routing is spreading work across the
/// pool or concentrating it on one executor.
///
/// ```
/// use kimojio_tls_pool::TlsPool;
///
/// let pool = TlsPool::default();
/// let executor = &pool.stats().executors[0];
/// assert_eq!(executor.id, 0);
/// ```
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
    /// Readiness-resumed operation attempts executed by this executor.
    pub readiness_resumed: u64,
    /// Ready-spin checks performed by this executor.
    pub ready_spins: u64,
    /// Transitions into idle waiting by this executor.
    pub idle_transitions: u64,
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
        stats.record_executor_queued(1);
        stats.record_readiness_wait();
        stats.record_readiness_resumed(1);
        stats.record_completed(Some(1));
        stats.record_failed(Some(1));

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.submitted, 1);
        assert_eq!(snapshot.immediate, 1);
        assert_eq!(snapshot.background_routed, 1);
        assert_eq!(snapshot.queued, 1);
        assert_eq!(snapshot.stream_queued, 0);
        assert_eq!(snapshot.executor_queued, 1);
        assert_eq!(snapshot.readiness_waits, 1);
        assert_eq!(snapshot.readiness_resumed, 1);
        assert_eq!(snapshot.completed, 1);
        assert_eq!(snapshot.failed, 1);
        assert_eq!(snapshot.executors[1].routed, 1);
        assert_eq!(snapshot.executors[1].queued, 1);
        assert_eq!(snapshot.executors[1].readiness_resumed, 1);
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
                    stats.record_executor_queued(0);
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
