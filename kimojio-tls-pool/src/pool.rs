// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::{
    IdleBehavior, OperationError, OperationPlacement, PlacementMode, PoolConfig, PoolConfigError,
    PoolStats, PoolStatsSnapshot, TlsStream,
};

type Job = Box<dyn FnOnce() + Send + 'static>;

enum WorkerMessage {
    Run(Job),
    Shutdown,
}

/// User-facing pool that owns TLS operation placement policy and shared stats.
#[derive(Clone)]
pub struct TlsPool {
    pub(crate) inner: Arc<PoolInner>,
}

impl TlsPool {
    /// Creates a pool handle from a validated configuration.
    pub fn new(config: PoolConfig) -> Result<Self, PoolConfigError> {
        config.validate()?;
        Ok(Self {
            inner: PoolInner::new(config),
        })
    }

    /// Creates a stream handle associated with this pool.
    pub fn stream(&self) -> TlsStream {
        TlsStream::new(Arc::clone(&self.inner))
    }

    /// Returns the pool configuration.
    pub fn config(&self) -> &PoolConfig {
        &self.inner.config
    }

    /// Returns a snapshot of pool statistics.
    pub fn stats(&self) -> PoolStatsSnapshot {
        self.inner.stats.snapshot()
    }

    /// Starts an orderly pool shutdown.
    pub fn shutdown(&self) {
        self.inner.shutdown();
    }
}

impl Default for TlsPool {
    fn default() -> Self {
        Self::new(PoolConfig::default()).expect("default TLS pool config should be valid")
    }
}

pub(crate) struct PoolInner {
    config: PoolConfig,
    pub(crate) stats: Arc<PoolStats>,
    workers: Vec<WorkerHandle>,
    executor_loads: Vec<AtomicU64>,
    next_executor: AtomicUsize,
    shutdown: AtomicBool,
}

impl PoolInner {
    fn new(config: PoolConfig) -> Arc<Self> {
        let executor_count = config.executor_count();
        Arc::new_cyclic(|weak: &std::sync::Weak<Self>| {
            let stats = Arc::new(PoolStats::new(executor_count));
            let mut workers = Vec::with_capacity(executor_count);

            for id in 0..executor_count {
                let (sender, receiver) = mpsc::channel();
                let worker_stats = Arc::clone(&stats);
                let idle_behavior = config.idle_behavior();
                let weak_pool = weak.clone();
                let handle = thread::Builder::new()
                    .name(format!("kimojio-tls-pool-{id}"))
                    .spawn(move || {
                        worker_loop(id, receiver, worker_stats, idle_behavior, weak_pool)
                    })
                    .expect("failed to spawn TLS pool executor");
                workers.push(WorkerHandle {
                    sender,
                    handle: Mutex::new(Some(handle)),
                });
            }

            Self {
                config,
                stats,
                workers,
                executor_loads: (0..executor_count).map(|_| AtomicU64::new(0)).collect(),
                next_executor: AtomicUsize::new(0),
                shutdown: AtomicBool::new(false),
            }
        })
    }

    pub(crate) fn choose_placement(&self, operation_size: usize) -> OperationPlacement {
        match self.config.placement_mode() {
            PlacementMode::ImmediateOnly => OperationPlacement::Immediate,
            PlacementMode::BackgroundOnly => OperationPlacement::Background {
                executor: self.choose_executor(),
            },
            PlacementMode::Adaptive => {
                let thresholds = self.config.thresholds();
                if operation_size <= thresholds.small_max() {
                    OperationPlacement::Immediate
                } else if operation_size >= thresholds.background_min() {
                    OperationPlacement::Background {
                        executor: self.choose_executor(),
                    }
                } else {
                    OperationPlacement::Immediate
                }
            }
        }
    }

    pub(crate) fn send_to_executor(
        self: &Arc<Self>,
        executor: usize,
        operation_size: usize,
        job: Job,
    ) -> Result<(), OperationError> {
        if self.shutdown.load(Ordering::Acquire) {
            return Err(OperationError::Shutdown);
        }

        let cost = estimated_cost(operation_size);
        let Some(load) = self.executor_loads.get(executor) else {
            return Err(OperationError::Shutdown);
        };
        let Some(worker) = self.workers.get(executor) else {
            return Err(OperationError::Shutdown);
        };

        load.fetch_add(cost, Ordering::AcqRel);
        let inner = Arc::clone(self);
        let wrapped = Box::new(move || {
            job();
            inner.executor_loads[executor].fetch_sub(cost, Ordering::AcqRel);
        });

        worker
            .sender
            .send(WorkerMessage::Run(wrapped))
            .map_err(|_| {
                self.executor_loads[executor].fetch_sub(cost, Ordering::AcqRel);
                OperationError::Shutdown
            })
    }

    pub(crate) fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    pub(crate) fn shutdown(&self) {
        if self.shutdown.swap(true, Ordering::AcqRel) {
            return;
        }

        for worker in &self.workers {
            let _ = worker.sender.send(WorkerMessage::Shutdown);
        }
    }

    fn choose_executor(&self) -> usize {
        let start = self.next_executor.fetch_add(1, Ordering::Relaxed);
        let len = self.executor_loads.len();
        let mut best = start % len;
        let mut best_load = self.executor_loads[best].load(Ordering::Acquire);

        for offset in 1..len {
            let index = (start + offset) % len;
            let load = self.executor_loads[index].load(Ordering::Acquire);
            if load < best_load {
                best = index;
                best_load = load;
            }
        }

        best
    }

    #[cfg(test)]
    fn add_executor_load_for_test(&self, executor: usize, load: u64) {
        self.executor_loads[executor].fetch_add(load, Ordering::AcqRel);
    }
}

impl Drop for PoolInner {
    fn drop(&mut self) {
        self.shutdown();
        for worker in &self.workers {
            if let Some(handle) = worker.handle.lock().expect("worker handle poisoned").take() {
                let _ = handle.join();
            }
        }
    }
}

struct WorkerHandle {
    sender: mpsc::Sender<WorkerMessage>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

fn estimated_cost(operation_size: usize) -> u64 {
    u64::try_from(operation_size.max(1)).unwrap_or(u64::MAX)
}

fn worker_loop(
    id: usize,
    receiver: mpsc::Receiver<WorkerMessage>,
    stats: Arc<PoolStats>,
    idle_behavior: IdleBehavior,
    pool: std::sync::Weak<PoolInner>,
) {
    loop {
        match receiver.try_recv() {
            Ok(message) => {
                if handle_worker_message(message) {
                    return;
                }
            }
            Err(mpsc::TryRecvError::Disconnected) => return,
            Err(mpsc::TryRecvError::Empty) => {
                if id < idle_behavior.ready_executors() {
                    match spin_for_work(&receiver, &stats, id, idle_behavior.spin_for()) {
                        SpinResult::RanJob => continue,
                        SpinResult::Shutdown => return,
                        SpinResult::NoWork => {}
                    }
                }

                stats.record_idle_transition(id);
                match receiver.recv() {
                    Ok(message) => {
                        if handle_worker_message(message) {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        }

        if pool.upgrade().is_none() {
            return;
        }
    }
}

enum SpinResult {
    RanJob,
    Shutdown,
    NoWork,
}

fn spin_for_work(
    receiver: &mpsc::Receiver<WorkerMessage>,
    stats: &PoolStats,
    id: usize,
    duration: Duration,
) -> SpinResult {
    stats.record_ready_spin(id);
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        match receiver.try_recv() {
            Ok(message) => {
                return if handle_worker_message(message) {
                    SpinResult::Shutdown
                } else {
                    SpinResult::RanJob
                };
            }
            Err(mpsc::TryRecvError::Disconnected) => return SpinResult::Shutdown,
            Err(mpsc::TryRecvError::Empty) => thread::yield_now(),
        }
    }
    SpinResult::NoWork
}

fn handle_worker_message(message: WorkerMessage) -> bool {
    match message {
        WorkerMessage::Run(job) => {
            job();
            false
        }
        WorkerMessage::Shutdown => true,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;
    use crate::SizeThresholds;

    #[test]
    fn immediate_mode_callback_runs_on_submitter_thread() {
        let pool =
            TlsPool::new(PoolConfig::new(1).with_placement_mode(PlacementMode::ImmediateOnly))
                .unwrap();
        let stream = pool.stream();
        let submitter = thread::current().id();
        let (sender, receiver) = mpsc::channel();

        stream
            .submit_operation(
                1,
                || Ok(thread::current().id()),
                Box::new(move |result| {
                    sender.send(result.unwrap()).unwrap();
                }),
            )
            .unwrap();

        assert_eq!(receiver.recv().unwrap(), submitter);
        let stats = pool.stats();
        assert_eq!(stats.submitted, 1);
        assert_eq!(stats.immediate, 1);
        assert_eq!(stats.completed, 1);
    }

    #[test]
    fn background_mode_callback_runs_on_executor_thread() {
        let pool =
            TlsPool::new(PoolConfig::new(1).with_placement_mode(PlacementMode::BackgroundOnly))
                .unwrap();
        let stream = pool.stream();
        let submitter = thread::current().id();
        let (sender, receiver) = mpsc::channel();

        stream
            .submit_operation(
                1,
                || Ok(thread::current().id()),
                Box::new(move |result| {
                    sender.send(result.unwrap()).unwrap();
                }),
            )
            .unwrap();

        assert_ne!(receiver.recv().unwrap(), submitter);
        let stats = pool.stats();
        assert_eq!(stats.submitted, 1);
        assert_eq!(stats.background_routed, 1);
        assert_eq!(stats.completed, 1);
        assert_eq!(stats.executors[0].completed, 1);
    }

    #[test]
    fn load_based_selection_prefers_lower_estimated_load() {
        let pool =
            TlsPool::new(PoolConfig::new(2).with_placement_mode(PlacementMode::BackgroundOnly))
                .unwrap();
        pool.inner.add_executor_load_for_test(0, 100);

        assert_eq!(
            pool.inner.choose_placement(1),
            OperationPlacement::Background { executor: 1 }
        );
    }

    #[test]
    fn ready_executor_records_idle_transition() {
        let pool = TlsPool::new(
            PoolConfig::new(1)
                .with_placement_mode(PlacementMode::BackgroundOnly)
                .with_idle_behavior(IdleBehavior::new(1, Duration::from_micros(1))),
        )
        .unwrap();
        let stream = pool.stream();
        let (sender, receiver) = mpsc::channel();

        stream
            .submit_operation(
                1,
                || Ok(()),
                Box::new(move |result| {
                    result.unwrap();
                    sender.send(()).unwrap();
                }),
            )
            .unwrap();
        receiver.recv().unwrap();

        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            let stats = pool.stats();
            if stats.executors[0].ready_spins != 0 && stats.executors[0].idle_transitions != 0 {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("executor did not record ready/idle behavior");
    }

    #[test]
    fn shutdown_rejects_new_background_operations() {
        let pool =
            TlsPool::new(PoolConfig::new(1).with_placement_mode(PlacementMode::BackgroundOnly))
                .unwrap();
        let stream = pool.stream();
        pool.shutdown();

        let error = stream
            .submit_operation(1, || Ok(()), Box::new(|_| {}))
            .unwrap_err();
        assert!(matches!(error, OperationError::Shutdown));
    }

    #[test]
    fn adaptive_uses_immediate_for_small_and_background_for_large() {
        let pool = TlsPool::new(
            PoolConfig::new(1)
                .with_placement_mode(PlacementMode::Adaptive)
                .with_thresholds(SizeThresholds::new(4, 8)),
        )
        .unwrap();

        assert_eq!(
            pool.inner.choose_placement(4),
            OperationPlacement::Immediate
        );
        assert_eq!(
            pool.inner.choose_placement(8),
            OperationPlacement::Background { executor: 0 }
        );
    }
}
