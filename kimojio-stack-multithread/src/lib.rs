// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Multithreaded worker orchestration for `kimojio-stack`.
//!
//! This crate keeps the single-threaded [`kimojio_stack::Runtime`] hot path
//! intact by running one local runtime per worker thread. Pinned workers receive
//! work through a per-worker queue and never touch the stealable scheduler.
//! Stealable workers use a shared work-stealing queue and therefore pay the
//! synchronization cost there.
//!
//! Each worker calls [`kimojio_stack::Runtime::block_on`] once and keeps that
//! root context alive for the worker loop. As a result, every worker owns its own
//! local scheduler and io_uring instance for the lifetime of the worker thread.

use std::any::Any;
use std::collections::VecDeque;
use std::fmt;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::{self, JoinHandle as ThreadJoinHandle};

use kimojio_stack::{Runtime, RuntimeConfig, RuntimeContext};

type PanicPayload = Box<dyn Any + Send + 'static>;
type Job = Box<dyn Runnable + Send + 'static>;
type StealableJob = Box<dyn StealableRunnable + Send + 'static>;

/// Identifier for a pinned worker.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PinnedWorkerId(usize);

impl PinnedWorkerId {
    /// Returns the zero-based worker index.
    pub fn index(self) -> usize {
        self.0
    }
}

/// Identifier for a stealable worker.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StealableWorkerId(usize);

impl StealableWorkerId {
    /// Returns the zero-based worker index.
    pub fn index(self) -> usize {
        self.0
    }
}

/// Where a submitted job was placed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskPlacement {
    /// The job was sent to a specific pinned worker.
    Pinned(PinnedWorkerId),
    /// The job was sent to the stealable worker pool.
    Stealable,
}

/// Context passed to stealable jobs that opt into worker-awareness.
///
/// Failover is an explicit continuation hop: the closure passed to
/// [`StealableContext::failover_to`] runs on the target stealable worker and
/// returns its value to the caller. This does not migrate an in-flight
/// `kimojio_stack` coroutine stack or any worker-local io_uring operation.
pub struct StealableContext<'runtime, 'cx> {
    runtime: &'runtime RuntimeContext<'cx>,
    worker: StealableWorkerId,
    shared: Arc<StealShared>,
}

impl<'runtime, 'cx> StealableContext<'runtime, 'cx> {
    /// Returns the local stackful runtime context for the current worker.
    pub fn runtime(&self) -> &'runtime RuntimeContext<'cx> {
        self.runtime
    }

    /// Returns the stealable worker currently running this continuation.
    pub fn worker_id(&self) -> StealableWorkerId {
        self.worker
    }

    /// Returns the number of stealable workers in this runtime.
    pub fn worker_count(&self) -> usize {
        self.shared.worker_count()
    }

    /// Returns the stealable worker id for `index`.
    pub fn worker_id_for_index(&self, index: usize) -> Option<StealableWorkerId> {
        (index < self.worker_count()).then_some(StealableWorkerId(index))
    }

    /// Runs `f` on `target` and waits for the continuation result.
    ///
    /// This method is intentionally explicit: it forces the next continuation to
    /// run on a different stealable worker and returns an error if `target` is
    /// the current worker. Values captured by `f` must be `Send + 'static`.
    pub fn failover_to<F, T>(&self, target: StealableWorkerId, f: F) -> Result<T, FailoverError>
    where
        F: FnOnce(&StealableContext<'_, '_>) -> T + Send + 'static,
        T: Send + 'static,
    {
        if target == self.worker {
            return Err(FailoverError::SameWorker { worker: target });
        }

        let (job, handle) = new_stealable_context_job(f, TaskPlacement::Stealable);
        self.shared.submit_to(target.index(), job)?;
        handle.join().map_err(FailoverError::from)
    }

    /// Runs `f` on the next stealable worker by index.
    pub fn failover<F, T>(&self, f: F) -> Result<T, FailoverError>
    where
        F: FnOnce(&StealableContext<'_, '_>) -> T + Send + 'static,
        T: Send + 'static,
    {
        let count = self.worker_count();
        if count <= 1 {
            return Err(FailoverError::NoOtherWorker);
        }

        let next = StealableWorkerId((self.worker.index() + 1) % count);
        self.failover_to(next, f)
    }
}

/// Configuration for [`MultiRuntime`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MultiRuntimeConfig {
    /// Number of pinned workers.
    pub pinned_workers: usize,
    /// Number of stealable workers.
    pub stealable_workers: usize,
    /// Local `kimojio-stack` runtime configuration used by every worker.
    pub worker_runtime: RuntimeConfig,
}

impl MultiRuntimeConfig {
    /// Creates a configuration with explicit pinned and stealable worker counts.
    pub fn new(pinned_workers: usize, stealable_workers: usize) -> Self {
        Self {
            pinned_workers,
            stealable_workers,
            worker_runtime: RuntimeConfig::default(),
        }
    }
}

/// Builder for [`MultiRuntime`].
#[derive(Clone, Debug)]
pub struct MultiRuntimeBuilder {
    config: MultiRuntimeConfig,
}

impl MultiRuntimeBuilder {
    /// Sets the number of pinned workers.
    pub fn pinned_workers(mut self, workers: usize) -> Self {
        self.config.pinned_workers = workers;
        self
    }

    /// Sets the number of stealable workers.
    pub fn stealable_workers(mut self, workers: usize) -> Self {
        self.config.stealable_workers = workers;
        self
    }

    /// Sets the local `kimojio-stack` runtime config used by every worker.
    pub fn worker_runtime(mut self, config: RuntimeConfig) -> Self {
        self.config.worker_runtime = config;
        self
    }

    /// Builds the runtime and starts every worker thread.
    pub fn build(self) -> Result<MultiRuntime, BuildError> {
        MultiRuntime::with_config(self.config)
    }
}

impl Default for MultiRuntimeBuilder {
    fn default() -> Self {
        Self {
            config: MultiRuntimeConfig::new(1, 0),
        }
    }
}

/// Multithreaded worker runtime.
///
/// Pinned jobs are always executed by the requested pinned worker. Stealable
/// jobs are pushed into the stealable pool and may be picked up by any stealable
/// worker before they start running. Once a job starts running, all stackful
/// coroutines spawned inside it remain local to that worker's
/// `kimojio_stack::Runtime`.
pub struct MultiRuntime {
    pinned_workers: Vec<PinnedWorker>,
    stealable_pool: Option<StealPool>,
    metrics: Arc<MetricsInner>,
    shutting_down: AtomicBool,
}

impl fmt::Debug for MultiRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MultiRuntime")
            .field("pinned_workers", &self.pinned_worker_count())
            .field("stealable_workers", &self.stealable_worker_count())
            .field("metrics", &self.metrics())
            .finish()
    }
}

impl MultiRuntime {
    /// Returns a builder with one pinned worker and no stealable workers.
    pub fn builder() -> MultiRuntimeBuilder {
        MultiRuntimeBuilder::default()
    }

    /// Creates and starts a runtime with explicit worker counts.
    pub fn new(pinned_workers: usize, stealable_workers: usize) -> Result<Self, BuildError> {
        Self::with_config(MultiRuntimeConfig::new(pinned_workers, stealable_workers))
    }

    /// Creates and starts a runtime from a full config.
    pub fn with_config(config: MultiRuntimeConfig) -> Result<Self, BuildError> {
        if config.pinned_workers + config.stealable_workers == 0 {
            return Err(BuildError::NoWorkers);
        }

        let metrics = Arc::new(MetricsInner::default());
        let pinned_workers =
            start_pinned_workers(config.pinned_workers, config.worker_runtime, &metrics)?;
        let stealable_pool = if config.stealable_workers == 0 {
            None
        } else {
            Some(StealPool::start(
                config.stealable_workers,
                config.worker_runtime,
                Arc::clone(&metrics),
            )?)
        };

        Ok(Self {
            pinned_workers,
            stealable_pool,
            metrics,
            shutting_down: AtomicBool::new(false),
        })
    }

    /// Returns the number of pinned workers.
    pub fn pinned_worker_count(&self) -> usize {
        self.pinned_workers.len()
    }

    /// Returns the number of stealable workers.
    pub fn stealable_worker_count(&self) -> usize {
        self.stealable_pool
            .as_ref()
            .map_or(0, StealPool::worker_count)
    }

    /// Returns the pinned worker id for `index`.
    pub fn pinned_worker_id(&self, index: usize) -> Option<PinnedWorkerId> {
        (index < self.pinned_workers.len()).then_some(PinnedWorkerId(index))
    }

    /// Returns the stealable worker id for `index`.
    pub fn stealable_worker_id(&self, index: usize) -> Option<StealableWorkerId> {
        (index < self.stealable_worker_count()).then_some(StealableWorkerId(index))
    }

    /// Returns the current metrics snapshot.
    pub fn metrics(&self) -> RuntimeMetrics {
        self.metrics.snapshot()
    }

    /// Runs `f` on the selected pinned worker.
    ///
    /// Pinned jobs are not visible to the stealable queue, so they do not pay for
    /// work-stealing bookkeeping after submission.
    pub fn spawn_pinned<F, T>(
        &self,
        worker: PinnedWorkerId,
        f: F,
    ) -> Result<JoinHandle<T>, SpawnError>
    where
        F: FnOnce(&RuntimeContext<'_>) -> T + Send + 'static,
        T: Send + 'static,
    {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(SpawnError::ShuttingDown);
        }

        let Some(pinned) = self.pinned_workers.get(worker.index()) else {
            return Err(SpawnError::InvalidPinnedWorker {
                index: worker.index(),
                workers: self.pinned_workers.len(),
            });
        };

        let (job, handle) = new_job(f, TaskPlacement::Pinned(worker));
        pinned
            .sender
            .send(PinnedCommand::Run(job))
            .map_err(|_| SpawnError::WorkerStopped)?;
        self.metrics
            .pinned_submitted
            .fetch_add(1, Ordering::Relaxed);
        Ok(handle)
    }

    /// Runs `f` on the pinned worker at `index`.
    pub fn spawn_pinned_index<F, T>(&self, index: usize, f: F) -> Result<JoinHandle<T>, SpawnError>
    where
        F: FnOnce(&RuntimeContext<'_>) -> T + Send + 'static,
        T: Send + 'static,
    {
        self.spawn_pinned(PinnedWorkerId(index), f)
    }

    /// Runs `f` on the stealable worker pool.
    ///
    /// The job may be stolen before it starts running. Once it is running, its
    /// stackful coroutines stay on that worker's local runtime.
    pub fn spawn_stealable<F, T>(&self, f: F) -> Result<JoinHandle<T>, SpawnError>
    where
        F: FnOnce(&RuntimeContext<'_>) -> T + Send + 'static,
        T: Send + 'static,
    {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(SpawnError::ShuttingDown);
        }

        let Some(pool) = &self.stealable_pool else {
            return Err(SpawnError::NoStealableWorkers);
        };

        let (job, handle) = new_stealable_runtime_job(f, TaskPlacement::Stealable);
        pool.submit(job)?;
        self.metrics
            .stealable_submitted
            .fetch_add(1, Ordering::Relaxed);
        Ok(handle)
    }

    /// Runs `f` on the stealable worker pool with worker-aware context.
    pub fn spawn_stealable_with_context<F, T>(&self, f: F) -> Result<JoinHandle<T>, SpawnError>
    where
        F: FnOnce(&StealableContext<'_, '_>) -> T + Send + 'static,
        T: Send + 'static,
    {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(SpawnError::ShuttingDown);
        }

        let Some(pool) = &self.stealable_pool else {
            return Err(SpawnError::NoStealableWorkers);
        };

        let (job, handle) = new_stealable_context_job(f, TaskPlacement::Stealable);
        pool.submit(job)?;
        self.metrics
            .stealable_submitted
            .fetch_add(1, Ordering::Relaxed);
        Ok(handle)
    }

    /// Requests worker shutdown and joins all worker threads.
    pub fn shutdown(mut self) -> Result<(), ShutdownError> {
        self.shutdown_workers()
    }

    fn shutdown_workers(&mut self) -> Result<(), ShutdownError> {
        if self.shutting_down.swap(true, Ordering::AcqRel) {
            return Ok(());
        }

        for worker in &self.pinned_workers {
            let _ = worker.sender.send(PinnedCommand::Shutdown);
        }
        if let Some(pool) = &self.stealable_pool {
            pool.request_stop();
        }

        let mut panicked_workers = 0;
        for worker in &mut self.pinned_workers {
            if let Some(thread) = worker.thread.take()
                && thread.join().is_err()
            {
                panicked_workers += 1;
            }
        }
        if let Some(pool) = &mut self.stealable_pool {
            panicked_workers += pool.join();
        }

        if panicked_workers == 0 {
            Ok(())
        } else {
            Err(ShutdownError { panicked_workers })
        }
    }
}

impl Drop for MultiRuntime {
    fn drop(&mut self) {
        let _ = self.shutdown_workers();
    }
}

fn start_pinned_workers(
    count: usize,
    config: RuntimeConfig,
    metrics: &Arc<MetricsInner>,
) -> Result<Vec<PinnedWorker>, BuildError> {
    let mut workers = Vec::with_capacity(count);
    for index in 0..count {
        let (sender, receiver) = mpsc::channel();
        let metrics = Arc::clone(metrics);
        let thread = thread::Builder::new()
            .name(format!("kimojio-stack-pinned-{index}"))
            .spawn(move || pinned_worker_loop(PinnedWorkerId(index), config, receiver, metrics))
            .map_err(|source| {
                stop_pinned_workers(&mut workers);
                BuildError::ThreadSpawn {
                    worker: WorkerThread::Pinned(index),
                    source,
                }
            })?;

        workers.push(PinnedWorker {
            sender,
            thread: Some(thread),
        });
    }
    Ok(workers)
}

fn stop_pinned_workers(workers: &mut [PinnedWorker]) {
    for worker in workers.iter() {
        let _ = worker.sender.send(PinnedCommand::Shutdown);
    }
    for worker in workers.iter_mut() {
        if let Some(thread) = worker.thread.take() {
            let _ = thread.join();
        }
    }
}

fn pinned_worker_loop(
    _id: PinnedWorkerId,
    config: RuntimeConfig,
    receiver: mpsc::Receiver<PinnedCommand>,
    metrics: Arc<MetricsInner>,
) {
    let mut runtime = Runtime::with_config(config);
    runtime.block_on(|cx| {
        while let Ok(command) = receiver.recv() {
            match command {
                PinnedCommand::Run(job) => {
                    job.run(cx);
                    metrics.pinned_completed.fetch_add(1, Ordering::Relaxed);
                }
                PinnedCommand::Shutdown => break,
            }
        }
    });
}

struct PinnedWorker {
    sender: mpsc::Sender<PinnedCommand>,
    thread: Option<ThreadJoinHandle<()>>,
}

enum PinnedCommand {
    Run(Job),
    Shutdown,
}

struct StealPool {
    shared: Arc<StealShared>,
    threads: Vec<Option<ThreadJoinHandle<()>>>,
}

impl StealPool {
    fn start(
        count: usize,
        config: RuntimeConfig,
        metrics: Arc<MetricsInner>,
    ) -> Result<Self, BuildError> {
        let shared = Arc::new(StealShared {
            state: Mutex::new(StealState {
                queues: (0..count).map(|_| VecDeque::new()).collect(),
                pending: 0,
                stopping: false,
                next_queue: 0,
            }),
            available: Condvar::new(),
            metrics,
        });

        let mut threads = Vec::with_capacity(count);
        for index in 0..count {
            let worker_shared = Arc::clone(&shared);
            let thread = thread::Builder::new()
                .name(format!("kimojio-stack-stealable-{index}"))
                .spawn(move || {
                    stealable_worker_loop(StealableWorkerId(index), config, worker_shared);
                })
                .map_err(|source| {
                    request_steal_stop(&shared);
                    join_started_threads(&mut threads);
                    BuildError::ThreadSpawn {
                        worker: WorkerThread::Stealable(index),
                        source,
                    }
                })?;
            threads.push(Some(thread));
        }

        Ok(Self { shared, threads })
    }

    fn worker_count(&self) -> usize {
        self.threads.len()
    }

    fn submit(&self, job: StealableJob) -> Result<(), SpawnError> {
        self.shared.submit(job)
    }

    fn request_stop(&self) {
        request_steal_stop(&self.shared);
    }

    fn join(&mut self) -> usize {
        join_started_threads(&mut self.threads)
    }
}

fn request_steal_stop(shared: &StealShared) {
    let mut state = shared.state.lock().expect("steal pool poisoned");
    state.stopping = true;
    shared.available.notify_all();
}

fn join_started_threads(threads: &mut [Option<ThreadJoinHandle<()>>]) -> usize {
    let mut panicked = 0;
    for thread in threads {
        if let Some(thread) = thread.take()
            && thread.join().is_err()
        {
            panicked += 1;
        }
    }
    panicked
}

fn stealable_worker_loop(id: StealableWorkerId, config: RuntimeConfig, shared: Arc<StealShared>) {
    let mut runtime = Runtime::with_config(config);
    runtime.block_on(|cx| {
        while let Some(job) = take_stealable_job(id, &shared) {
            job.run(cx, id, Arc::clone(&shared));
            shared
                .metrics
                .stealable_completed
                .fetch_add(1, Ordering::Relaxed);
        }
    });
}

fn take_stealable_job(id: StealableWorkerId, shared: &StealShared) -> Option<StealableJob> {
    let mut state = shared.state.lock().expect("steal pool poisoned");
    loop {
        if let Some(job) = pop_stealable_job(id, &mut state, &shared.metrics) {
            return Some(job);
        }

        if state.stopping {
            return None;
        }

        state = shared.available.wait(state).expect("steal pool poisoned");
    }
}

fn pop_stealable_job(
    id: StealableWorkerId,
    state: &mut StealState,
    metrics: &MetricsInner,
) -> Option<StealableJob> {
    if state.pending == 0 {
        return None;
    }

    if let Some(job) = state.queues[id.index()].pop_front() {
        state.pending -= 1;
        return Some(job);
    }

    metrics.steal_attempts.fetch_add(1, Ordering::Relaxed);
    for offset in 1..state.queues.len() {
        let victim = (id.index() + offset) % state.queues.len();
        if let Some(job) = state.queues[victim].pop_back() {
            state.pending -= 1;
            metrics.steals.fetch_add(1, Ordering::Relaxed);
            return Some(job);
        }
    }

    None
}

struct StealShared {
    state: Mutex<StealState>,
    available: Condvar,
    metrics: Arc<MetricsInner>,
}

impl StealShared {
    fn worker_count(&self) -> usize {
        self.state.lock().expect("steal pool poisoned").queues.len()
    }

    fn submit(&self, job: StealableJob) -> Result<(), SpawnError> {
        let mut state = self.state.lock().expect("steal pool poisoned");
        if state.stopping {
            return Err(SpawnError::ShuttingDown);
        }

        let index = state.next_queue % state.queues.len();
        state.next_queue = state.next_queue.wrapping_add(1);
        push_stealable_job(&mut state, index, job);
        self.available.notify_one();
        Ok(())
    }

    fn submit_to(&self, index: usize, job: StealableJob) -> Result<(), FailoverError> {
        let mut state = self.state.lock().expect("steal pool poisoned");
        if state.stopping {
            return Err(FailoverError::ShuttingDown);
        }

        let workers = state.queues.len();
        if index >= workers {
            return Err(FailoverError::InvalidWorker { index, workers });
        }

        push_stealable_job(&mut state, index, job);
        self.available.notify_all();
        Ok(())
    }
}

fn push_stealable_job(state: &mut StealState, index: usize, job: StealableJob) {
    state.queues[index].push_back(job);
    state.pending += 1;
}

struct StealState {
    queues: Vec<VecDeque<StealableJob>>,
    pending: usize,
    stopping: bool,
    next_queue: usize,
}

trait Runnable {
    fn run(self: Box<Self>, cx: &RuntimeContext<'_>);
}

trait StealableRunnable {
    fn run(
        self: Box<Self>,
        cx: &RuntimeContext<'_>,
        worker: StealableWorkerId,
        shared: Arc<StealShared>,
    );
}

struct Task<F, T> {
    f: F,
    result: mpsc::SyncSender<Result<T, JoinError>>,
}

impl<F, T> Runnable for Task<F, T>
where
    F: FnOnce(&RuntimeContext<'_>) -> T + Send + 'static,
    T: Send + 'static,
{
    fn run(self: Box<Self>, cx: &RuntimeContext<'_>) {
        let Self { f, result } = *self;
        let output = panic::catch_unwind(AssertUnwindSafe(|| f(cx))).map_err(JoinError::panicked);
        let _ = result.send(output);
    }
}

struct StealableRuntimeTask<F, T> {
    f: F,
    result: mpsc::SyncSender<Result<T, JoinError>>,
}

impl<F, T> StealableRunnable for StealableRuntimeTask<F, T>
where
    F: FnOnce(&RuntimeContext<'_>) -> T + Send + 'static,
    T: Send + 'static,
{
    fn run(
        self: Box<Self>,
        cx: &RuntimeContext<'_>,
        _worker: StealableWorkerId,
        _shared: Arc<StealShared>,
    ) {
        let Self { f, result } = *self;
        let output = panic::catch_unwind(AssertUnwindSafe(|| f(cx))).map_err(JoinError::panicked);
        let _ = result.send(output);
    }
}

struct StealableContextTask<F, T> {
    f: F,
    result: mpsc::SyncSender<Result<T, JoinError>>,
}

impl<F, T> StealableRunnable for StealableContextTask<F, T>
where
    F: FnOnce(&StealableContext<'_, '_>) -> T + Send + 'static,
    T: Send + 'static,
{
    fn run(
        self: Box<Self>,
        cx: &RuntimeContext<'_>,
        worker: StealableWorkerId,
        shared: Arc<StealShared>,
    ) {
        let Self { f, result } = *self;
        let context = StealableContext {
            runtime: cx,
            worker,
            shared,
        };
        let output =
            panic::catch_unwind(AssertUnwindSafe(|| f(&context))).map_err(JoinError::panicked);
        let _ = result.send(output);
    }
}

fn new_job<F, T>(f: F, placement: TaskPlacement) -> (Job, JoinHandle<T>)
where
    F: FnOnce(&RuntimeContext<'_>) -> T + Send + 'static,
    T: Send + 'static,
{
    let (sender, receiver) = mpsc::sync_channel(1);
    let job = Box::new(Task { f, result: sender });
    (
        job,
        JoinHandle {
            receiver,
            placement,
        },
    )
}

fn new_stealable_runtime_job<F, T>(f: F, placement: TaskPlacement) -> (StealableJob, JoinHandle<T>)
where
    F: FnOnce(&RuntimeContext<'_>) -> T + Send + 'static,
    T: Send + 'static,
{
    let (sender, receiver) = mpsc::sync_channel(1);
    let job = Box::new(StealableRuntimeTask { f, result: sender });
    (
        job,
        JoinHandle {
            receiver,
            placement,
        },
    )
}

fn new_stealable_context_job<F, T>(f: F, placement: TaskPlacement) -> (StealableJob, JoinHandle<T>)
where
    F: FnOnce(&StealableContext<'_, '_>) -> T + Send + 'static,
    T: Send + 'static,
{
    let (sender, receiver) = mpsc::sync_channel(1);
    let job = Box::new(StealableContextTask { f, result: sender });
    (
        job,
        JoinHandle {
            receiver,
            placement,
        },
    )
}

/// Handle for a submitted multithread job.
pub struct JoinHandle<T> {
    receiver: mpsc::Receiver<Result<T, JoinError>>,
    placement: TaskPlacement,
}

impl<T> JoinHandle<T> {
    /// Returns where this job was submitted.
    pub fn placement(&self) -> TaskPlacement {
        self.placement
    }

    /// Blocks the current OS thread until the job completes.
    pub fn join(self) -> Result<T, JoinError> {
        self.receiver.recv().unwrap_or(Err(JoinError::Canceled))
    }
}

impl<T> fmt::Debug for JoinHandle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JoinHandle")
            .field("placement", &self.placement)
            .finish_non_exhaustive()
    }
}

/// Error returned when a submitted job cannot complete successfully.
pub enum JoinError {
    /// The job panicked.
    Panicked(PanicPayload),
    /// The worker stopped before reporting a result.
    Canceled,
}

impl JoinError {
    fn panicked(payload: PanicPayload) -> Self {
        Self::Panicked(payload)
    }

    /// Returns the panic payload if this is a panic error.
    pub fn into_panic_payload(self) -> Option<PanicPayload> {
        match self {
            Self::Panicked(payload) => Some(payload),
            Self::Canceled => None,
        }
    }
}

impl fmt::Debug for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Panicked(_) => f.write_str("JoinError::Panicked(..)"),
            Self::Canceled => f.write_str("JoinError::Canceled"),
        }
    }
}

impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Panicked(_) => f.write_str("multithread stackful job panicked"),
            Self::Canceled => f.write_str("multithread stackful job was canceled"),
        }
    }
}

impl std::error::Error for JoinError {}

/// Error returned when creating a [`MultiRuntime`] fails.
#[derive(Debug)]
pub enum BuildError {
    /// The runtime must have at least one worker.
    NoWorkers,
    /// A worker thread could not be spawned.
    ThreadSpawn {
        /// Worker that failed to start.
        worker: WorkerThread,
        /// OS thread spawn error.
        source: std::io::Error,
    },
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoWorkers => f.write_str("multithread runtime requires at least one worker"),
            Self::ThreadSpawn { worker, source } => {
                write!(f, "failed to spawn {worker}: {source}")
            }
        }
    }
}

impl std::error::Error for BuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NoWorkers => None,
            Self::ThreadSpawn { source, .. } => Some(source),
        }
    }
}

/// Worker thread kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerThread {
    /// Pinned worker thread at the given index.
    Pinned(usize),
    /// Stealable worker thread at the given index.
    Stealable(usize),
}

impl fmt::Display for WorkerThread {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pinned(index) => write!(f, "pinned worker {index}"),
            Self::Stealable(index) => write!(f, "stealable worker {index}"),
        }
    }
}

/// Error returned when a job cannot be submitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpawnError {
    /// No stealable workers were configured.
    NoStealableWorkers,
    /// The pinned worker id is out of range.
    InvalidPinnedWorker {
        /// Requested worker index.
        index: usize,
        /// Configured pinned worker count.
        workers: usize,
    },
    /// The selected worker stopped.
    WorkerStopped,
    /// The runtime is shutting down.
    ShuttingDown,
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoStealableWorkers => f.write_str("no stealable workers configured"),
            Self::InvalidPinnedWorker { index, workers } => {
                write!(
                    f,
                    "pinned worker {index} is out of range for {workers} workers"
                )
            }
            Self::WorkerStopped => f.write_str("selected worker stopped"),
            Self::ShuttingDown => f.write_str("runtime is shutting down"),
        }
    }
}

impl std::error::Error for SpawnError {}

/// Error returned by [`StealableContext::failover_to`].
pub enum FailoverError {
    /// No other stealable worker exists.
    NoOtherWorker,
    /// The requested worker id is invalid.
    InvalidWorker {
        /// Requested worker index.
        index: usize,
        /// Configured stealable worker count.
        workers: usize,
    },
    /// The requested worker is the current worker.
    SameWorker {
        /// Current worker.
        worker: StealableWorkerId,
    },
    /// The runtime is shutting down.
    ShuttingDown,
    /// The target worker stopped before the continuation could complete.
    Canceled,
    /// The continuation panicked.
    Panicked(PanicPayload),
}

impl From<JoinError> for FailoverError {
    fn from(error: JoinError) -> Self {
        match error {
            JoinError::Panicked(payload) => Self::Panicked(payload),
            JoinError::Canceled => Self::Canceled,
        }
    }
}

impl fmt::Debug for FailoverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoOtherWorker => f.write_str("FailoverError::NoOtherWorker"),
            Self::InvalidWorker { index, workers } => f
                .debug_struct("FailoverError::InvalidWorker")
                .field("index", index)
                .field("workers", workers)
                .finish(),
            Self::SameWorker { worker } => f
                .debug_struct("FailoverError::SameWorker")
                .field("worker", worker)
                .finish(),
            Self::ShuttingDown => f.write_str("FailoverError::ShuttingDown"),
            Self::Canceled => f.write_str("FailoverError::Canceled"),
            Self::Panicked(_) => f.write_str("FailoverError::Panicked(..)"),
        }
    }
}

impl fmt::Display for FailoverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoOtherWorker => f.write_str("no other stealable worker configured"),
            Self::InvalidWorker { index, workers } => {
                write!(
                    f,
                    "stealable worker {index} is out of range for {workers} workers"
                )
            }
            Self::SameWorker { worker } => {
                write!(f, "cannot fail over to current stealable worker {worker:?}")
            }
            Self::ShuttingDown => f.write_str("runtime is shutting down"),
            Self::Canceled => f.write_str("failover continuation was canceled"),
            Self::Panicked(_) => f.write_str("failover continuation panicked"),
        }
    }
}

impl std::error::Error for FailoverError {}

/// Error returned when worker shutdown observes panicked worker threads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShutdownError {
    panicked_workers: usize,
}

impl ShutdownError {
    /// Number of worker threads that panicked.
    pub fn panicked_workers(self) -> usize {
        self.panicked_workers
    }
}

impl fmt::Display for ShutdownError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} multithread stackful worker(s) panicked during shutdown",
            self.panicked_workers
        )
    }
}

impl std::error::Error for ShutdownError {}

/// Runtime metrics snapshot.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RuntimeMetrics {
    /// Jobs submitted to pinned workers.
    pub pinned_submitted: usize,
    /// Jobs completed by pinned workers.
    pub pinned_completed: usize,
    /// Jobs submitted to the stealable pool.
    pub stealable_submitted: usize,
    /// Jobs completed by stealable workers.
    pub stealable_completed: usize,
    /// Times a stealable worker looked for work on another worker queue.
    pub steal_attempts: usize,
    /// Jobs actually stolen from another worker queue.
    pub steals: usize,
}

#[derive(Default)]
struct MetricsInner {
    pinned_submitted: AtomicUsize,
    pinned_completed: AtomicUsize,
    stealable_submitted: AtomicUsize,
    stealable_completed: AtomicUsize,
    steal_attempts: AtomicUsize,
    steals: AtomicUsize,
}

impl MetricsInner {
    fn snapshot(&self) -> RuntimeMetrics {
        RuntimeMetrics {
            pinned_submitted: self.pinned_submitted.load(Ordering::Relaxed),
            pinned_completed: self.pinned_completed.load(Ordering::Relaxed),
            stealable_submitted: self.stealable_submitted.load(Ordering::Relaxed),
            stealable_completed: self.stealable_completed.load(Ordering::Relaxed),
            steal_attempts: self.steal_attempts.load(Ordering::Relaxed),
            steals: self.steals.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rustix::pipe::pipe;

    use super::{BuildError, JoinError, MultiRuntime, SpawnError, TaskPlacement};

    #[test]
    fn builder_rejects_zero_workers() {
        let error = MultiRuntime::new(0, 0).unwrap_err();
        assert!(matches!(error, BuildError::NoWorkers));
    }

    #[test]
    fn pinned_jobs_run_on_requested_workers() {
        let runtime = MultiRuntime::new(2, 0).unwrap();
        let first = runtime.spawn_pinned_index(0, worker_thread_name).unwrap();
        let second = runtime.spawn_pinned_index(1, worker_thread_name).unwrap();

        assert_eq!(
            first.placement(),
            TaskPlacement::Pinned(runtime.pinned_worker_id(0).unwrap())
        );
        assert_eq!(first.join().unwrap(), "kimojio-stack-pinned-0");
        assert_eq!(second.join().unwrap(), "kimojio-stack-pinned-1");

        let metrics = runtime.metrics();
        assert_eq!(metrics.pinned_submitted, 2);
        assert_eq!(metrics.pinned_completed, 2);
        assert_eq!(metrics.stealable_submitted, 0);
    }

    #[test]
    fn pinned_jobs_do_not_require_stealable_workers() {
        let runtime = MultiRuntime::new(1, 0).unwrap();
        let output = runtime
            .spawn_pinned_index(0, |cx| {
                cx.scope(|scope| {
                    let child = scope.spawn(|_| 41);
                    child.join(cx).unwrap() + 1
                })
            })
            .unwrap()
            .join()
            .unwrap();

        assert_eq!(output, 42);
        assert_eq!(
            runtime.spawn_stealable(|_| ()).unwrap_err(),
            SpawnError::NoStealableWorkers
        );
    }

    #[test]
    fn stealable_jobs_run_on_stealable_workers() {
        let runtime = MultiRuntime::new(0, 2).unwrap();
        let completed = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let completed = Arc::clone(&completed);
                runtime
                    .spawn_stealable(move |cx| {
                        cx.yield_now();
                        completed.fetch_add(1, Ordering::Relaxed);
                        worker_thread_name(cx)
                    })
                    .unwrap()
            })
            .collect();

        for handle in handles {
            let name = handle.join().unwrap();
            assert!(name.starts_with("kimojio-stack-stealable-"));
        }
        assert_eq!(completed.load(Ordering::Relaxed), 8);

        let metrics = runtime.metrics();
        assert_eq!(metrics.stealable_submitted, 8);
        assert_eq!(metrics.stealable_completed, 8);
    }

    #[test]
    fn pinned_and_stealable_workers_interoperate_with_pipe_io() {
        let runtime = MultiRuntime::new(1, 1).unwrap();
        let (read_fd, write_fd) = pipe().unwrap();

        let reader = runtime
            .spawn_stealable(move |cx| {
                let mut buffer = [0_u8; 5];
                let read = cx.read(&read_fd, &mut buffer).unwrap();
                assert_eq!(read, buffer.len());
                cx.close(read_fd).unwrap();
                (worker_thread_name(cx), buffer)
            })
            .unwrap();
        let writer = runtime
            .spawn_pinned_index(0, move |cx| {
                let written = cx.write(&write_fd, b"hello").unwrap();
                assert_eq!(written, 5);
                cx.close(write_fd).unwrap();
                worker_thread_name(cx)
            })
            .unwrap();

        assert_eq!(writer.join().unwrap(), "kimojio-stack-pinned-0");
        let (reader_name, bytes) = reader.join().unwrap();
        assert!(reader_name.starts_with("kimojio-stack-stealable-"));
        assert_eq!(&bytes, b"hello");

        let metrics = runtime.metrics();
        assert_eq!(metrics.pinned_submitted, 1);
        assert_eq!(metrics.pinned_completed, 1);
        assert_eq!(metrics.stealable_submitted, 1);
        assert_eq!(metrics.stealable_completed, 1);
    }

    #[test]
    fn stealable_failover_keeps_pipe_io_working_across_workers() {
        let runtime = MultiRuntime::new(1, 3).unwrap();
        let (stealable_read_fd, pinned_write_fd) = pipe().unwrap();
        let (pinned_read_fd, stealable_write_fd) = pipe().unwrap();

        let stealable = runtime
            .spawn_stealable_with_context(move |ctx| {
                let start_worker = ctx.worker_id();
                let read_worker = ctx
                    .worker_id_for_index((start_worker.index() + 1) % ctx.worker_count())
                    .unwrap();
                let write_worker = ctx
                    .worker_id_for_index((read_worker.index() + 1) % ctx.worker_count())
                    .unwrap();

                let (actual_read_worker, actual_write_worker, bytes) = ctx
                    .failover_to(read_worker, move |ctx| {
                        assert_eq!(ctx.worker_id(), read_worker);

                        let mut buffer = [0_u8; 5];
                        let read = ctx.runtime().read(&stealable_read_fd, &mut buffer).unwrap();
                        assert_eq!(read, buffer.len());
                        ctx.runtime().close(stealable_read_fd).unwrap();

                        let actual_write_worker = ctx
                            .failover_to(write_worker, move |ctx| {
                                assert_eq!(ctx.worker_id(), write_worker);
                                let written =
                                    ctx.runtime().write(&stealable_write_fd, b"world").unwrap();
                                assert_eq!(written, 5);
                                ctx.runtime().close(stealable_write_fd).unwrap();
                                ctx.worker_id()
                            })
                            .unwrap();

                        (ctx.worker_id(), actual_write_worker, buffer)
                    })
                    .unwrap();

                (start_worker, actual_read_worker, actual_write_worker, bytes)
            })
            .unwrap();
        let pinned = runtime
            .spawn_pinned_index(0, move |cx| {
                let written = cx.write(&pinned_write_fd, b"hello").unwrap();
                assert_eq!(written, 5);
                cx.close(pinned_write_fd).unwrap();

                let mut buffer = [0_u8; 5];
                let read = cx.read(&pinned_read_fd, &mut buffer).unwrap();
                assert_eq!(read, buffer.len());
                cx.close(pinned_read_fd).unwrap();

                (worker_thread_name(cx), buffer)
            })
            .unwrap();

        let (pinned_worker, pinned_bytes) = pinned.join().unwrap();
        assert_eq!(pinned_worker, "kimojio-stack-pinned-0");
        assert_eq!(&pinned_bytes, b"world");

        let (start_worker, read_worker, write_worker, stealable_bytes) = stealable.join().unwrap();
        assert_ne!(start_worker, read_worker);
        assert_ne!(start_worker, write_worker);
        assert_ne!(read_worker, write_worker);
        assert_eq!(&stealable_bytes, b"hello");

        let metrics = runtime.metrics();
        assert_eq!(metrics.pinned_submitted, 1);
        assert_eq!(metrics.pinned_completed, 1);
        assert_eq!(metrics.stealable_submitted, 1);
        assert_eq!(metrics.stealable_completed, 3);
    }

    #[test]
    fn invalid_pinned_worker_is_reported() {
        let runtime = MultiRuntime::new(1, 0).unwrap();
        let error = runtime.spawn_pinned_index(1, |_| ()).unwrap_err();
        assert_eq!(
            error,
            SpawnError::InvalidPinnedWorker {
                index: 1,
                workers: 1
            }
        );
    }

    #[test]
    fn job_panics_are_reported_to_join_handle() {
        let runtime = MultiRuntime::new(1, 0).unwrap();
        let handle = runtime
            .spawn_pinned_index(0, |_| -> () { panic!("expected panic") })
            .unwrap();

        assert!(matches!(handle.join(), Err(JoinError::Panicked(_))));
    }

    fn worker_thread_name(_: &kimojio_stack::RuntimeContext<'_>) -> String {
        std::thread::current().name().unwrap().to_owned()
    }
}
