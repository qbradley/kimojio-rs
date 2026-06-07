// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::os::fd::{BorrowedFd, RawFd};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use openssl::ssl::{SslAcceptor, SslConnector};
use rustix::event::{PollFd, PollFlags, Timespec, poll};

use crate::{
    IdleBehavior, OperationError, OperationPlacement, PoolConfig, PoolConfigError, PoolStats,
    PoolStatsSnapshot, TlsPoolError, TlsStream,
};

type Job = Box<dyn FnOnce() + Send + 'static>;

enum WorkerMessage {
    Run(Job),
    Shutdown,
}

enum ReadinessMessage {
    Wait(ReadinessWait),
    Shutdown,
}

pub(crate) enum ReadinessInterest {
    Read,
    Write,
}

struct ReadinessWait {
    fd: RawFd,
    interest: ReadinessInterest,
    ready: Box<dyn FnOnce() + Send + 'static>,
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
            inner: PoolInner::new(config)?,
        })
    }

    /// Creates a stream handle associated with this pool.
    #[cfg(test)]
    pub(crate) fn stream(&self) -> TlsStream {
        TlsStream::new(Arc::clone(&self.inner))
    }

    /// Creates a client TLS stream over a connected transport.
    pub fn client<S>(
        &self,
        connector: &SslConnector,
        domain: &str,
        stream: S,
    ) -> Result<TlsStream, TlsPoolError>
    where
        S: std::io::Read + std::io::Write + std::os::fd::AsFd + Send + 'static,
    {
        let tls = crate::tls::connect(connector, domain, stream)?;
        Ok(TlsStream::from_tls(Arc::clone(&self.inner), tls))
    }

    /// Creates a server TLS stream over a connected transport.
    pub fn server<S>(&self, acceptor: &SslAcceptor, stream: S) -> Result<TlsStream, TlsPoolError>
    where
        S: std::io::Read + std::io::Write + std::os::fd::AsFd + Send + 'static,
    {
        let tls = crate::tls::accept(acceptor, stream)?;
        Ok(TlsStream::from_tls(Arc::clone(&self.inner), tls))
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
    readiness_sender: mpsc::Sender<ReadinessMessage>,
    readiness_handle: Mutex<Option<JoinHandle<()>>>,
    readiness_thread_id: thread::ThreadId,
    executor_loads: Vec<AtomicU64>,
    next_executor: AtomicUsize,
    shutdown: AtomicBool,
    lifecycle: Mutex<()>,
}

impl PoolInner {
    fn new(config: PoolConfig) -> Result<Arc<Self>, PoolConfigError> {
        let executor_count = config.executor_count();
        let mut spawn_error = None;
        let inner = Arc::new_cyclic(|weak: &std::sync::Weak<Self>| {
            let stats = Arc::new(PoolStats::new(executor_count));
            let mut workers = Vec::with_capacity(executor_count);

            for id in 0..executor_count {
                let (sender, receiver) = mpsc::channel();
                let worker_stats = Arc::clone(&stats);
                let idle_behavior = config.idle_behavior();
                let weak_pool = weak.clone();
                let handle = match thread::Builder::new()
                    .name(format!("kimojio-tls-pool-{id}"))
                    .spawn(move || {
                        worker_loop(id, receiver, worker_stats, idle_behavior, weak_pool)
                    }) {
                    Ok(handle) => handle,
                    Err(error) => {
                        spawn_error = Some(error.to_string());
                        break;
                    }
                };
                workers.push(WorkerHandle {
                    sender,
                    thread_id: handle.thread().id(),
                    handle: Mutex::new(Some(handle)),
                });
            }

            let (readiness_sender, readiness_receiver) = mpsc::channel();
            let readiness_handle = thread::Builder::new()
                .name("kimojio-tls-pool-readiness".to_string())
                .spawn(move || readiness_loop(readiness_receiver))
                .expect("failed to spawn TLS pool readiness thread");
            let readiness_thread_id = readiness_handle.thread().id();

            Self {
                config,
                stats,
                workers,
                readiness_sender,
                readiness_handle: Mutex::new(Some(readiness_handle)),
                readiness_thread_id,
                executor_loads: (0..executor_count).map(|_| AtomicU64::new(0)).collect(),
                next_executor: AtomicUsize::new(0),
                shutdown: AtomicBool::new(false),
                lifecycle: Mutex::new(()),
            }
        });

        if let Some(error) = spawn_error {
            inner.shutdown();
            return Err(PoolConfigError::ExecutorSpawnFailed(error));
        }
        Ok(inner)
    }

    pub(crate) fn choose_placement(
        &self,
        kind: crate::operation::OperationKind,
        operation_size: usize,
    ) -> OperationPlacement {
        let start = self.next_executor.fetch_add(1, Ordering::Relaxed);
        crate::policy::choose_placement(
            &self.config,
            kind,
            operation_size,
            &self.executor_loads_snapshot(),
            start,
        )
    }

    pub(crate) fn send_to_executor(
        self: &Arc<Self>,
        executor: usize,
        operation_size: usize,
        job: Job,
    ) -> Result<(), OperationError> {
        let _lifecycle = self
            .lifecycle
            .lock()
            .expect("pool lifecycle mutex poisoned");
        if self.shutdown.load(Ordering::Acquire) {
            return Err(OperationError::Shutdown);
        }

        let cost = crate::policy::estimated_operation_cost(operation_size);
        let Some(load) = self.executor_loads.get(executor) else {
            return Err(OperationError::Shutdown);
        };
        let Some(worker) = self.workers.get(executor) else {
            return Err(OperationError::Shutdown);
        };

        load.fetch_add(cost, Ordering::AcqRel);
        let inner = Arc::clone(self);
        let wrapped = Box::new(move || {
            let _load_guard = LoadGuard {
                inner: Arc::clone(&inner),
                executor,
                cost,
            };
            job();
        });

        worker
            .sender
            .send(WorkerMessage::Run(wrapped))
            .map_err(|_| {
                self.executor_loads[executor].fetch_sub(cost, Ordering::AcqRel);
                OperationError::Shutdown
            })
    }

    pub(crate) fn run_when_ready(
        &self,
        fd: RawFd,
        interest: ReadinessInterest,
        job: Job,
    ) -> Result<(), OperationError> {
        if self.shutdown.load(Ordering::Acquire) {
            return Err(OperationError::Shutdown);
        }
        self.readiness_sender
            .send(ReadinessMessage::Wait(ReadinessWait {
                fd,
                interest,
                ready: job,
            }))
            .map_err(|_| OperationError::Shutdown)
    }

    pub(crate) fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    pub(crate) fn config(&self) -> &PoolConfig {
        &self.config
    }

    pub(crate) fn shutdown(&self) {
        let _lifecycle = self
            .lifecycle
            .lock()
            .expect("pool lifecycle mutex poisoned");
        if self.shutdown.swap(true, Ordering::AcqRel) {
            return;
        }

        for worker in &self.workers {
            let _ = worker.sender.send(WorkerMessage::Shutdown);
        }
        let _ = self.readiness_sender.send(ReadinessMessage::Shutdown);
    }

    #[cfg(test)]
    fn add_executor_load_for_test(&self, executor: usize, load: u64) {
        self.executor_loads[executor].fetch_add(load, Ordering::AcqRel);
    }

    fn executor_loads_snapshot(&self) -> Vec<u64> {
        self.executor_loads
            .iter()
            .map(|load| load.load(Ordering::Acquire))
            .collect()
    }
}

impl Drop for PoolInner {
    fn drop(&mut self) {
        self.shutdown();
        let current = thread::current().id();
        for worker in &self.workers {
            if let Some(handle) = worker.handle.lock().expect("worker handle poisoned").take()
                && worker.thread_id != current
            {
                let _ = handle.join();
            }
        }
        if self.readiness_thread_id != current
            && let Some(handle) = self
                .readiness_handle
                .lock()
                .expect("readiness handle poisoned")
                .take()
        {
            let _ = handle.join();
        }
    }
}

struct WorkerHandle {
    sender: mpsc::Sender<WorkerMessage>,
    thread_id: thread::ThreadId,
    handle: Mutex<Option<JoinHandle<()>>>,
}

struct LoadGuard {
    inner: Arc<PoolInner>,
    executor: usize,
    cost: u64,
}

impl Drop for LoadGuard {
    fn drop(&mut self) {
        self.inner.executor_loads[self.executor].fetch_sub(self.cost, Ordering::AcqRel);
    }
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
            let _ = catch_unwind(AssertUnwindSafe(job));
            false
        }
        WorkerMessage::Shutdown => true,
    }
}

fn readiness_loop(receiver: mpsc::Receiver<ReadinessMessage>) {
    let mut waits = Vec::new();
    loop {
        while let Ok(message) = receiver.try_recv() {
            match message {
                ReadinessMessage::Wait(wait) => waits.push(wait),
                ReadinessMessage::Shutdown => return,
            }
        }

        if waits.is_empty() {
            match receiver.recv() {
                Ok(ReadinessMessage::Wait(wait)) => waits.push(wait),
                Ok(ReadinessMessage::Shutdown) | Err(_) => return,
            }
            continue;
        }

        let mut fds = waits
            .iter()
            .map(|wait| {
                let flags = match wait.interest {
                    ReadinessInterest::Read => PollFlags::IN,
                    ReadinessInterest::Write => PollFlags::OUT,
                };
                // SAFETY: the fd is owned by the TLS stream held alive by the
                // readiness callback. The borrow only lives for this poll call.
                let fd = unsafe { BorrowedFd::borrow_raw(wait.fd) };
                PollFd::from_borrowed_fd(fd, flags)
            })
            .collect::<Vec<_>>();

        let timeout = Timespec {
            tv_sec: 0,
            tv_nsec: 1_000_000,
        };
        let _ = poll(&mut fds, Some(&timeout));

        let mut index = waits.len();
        while index != 0 {
            index -= 1;
            let ready = fds[index].revents();
            if ready.intersects(
                PollFlags::IN | PollFlags::OUT | PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL,
            ) {
                let wait = waits.swap_remove(index);
                (wait.ready)();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;
    use crate::operation::OperationKind;
    use crate::{PlacementMode, SizeThresholds};

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
            pool.inner
                .choose_placement(OperationKind::Write { fd: 0 }, 1),
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
            pool.inner
                .choose_placement(OperationKind::Write { fd: 0 }, 4),
            OperationPlacement::Immediate
        );
        assert_eq!(
            pool.inner
                .choose_placement(OperationKind::Write { fd: 0 }, 8),
            OperationPlacement::Background { executor: 0 }
        );
    }
}
