// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Work-stealing stackful coroutine runtime.
//!
//! `kimojio-stack-steal` is a multi-worker companion to `kimojio-stack`. It keeps
//! the structured, stackful programming model while allowing eligible work to run
//! on a worker pool with explicit stealing policies.
//! Each worker thread owns one worker-local stackful scheduler; stealing moves
//! eligible queued jobs before they start running, not already-running
//! continuations.
//!
//! The runtime context exposes the same direct io_uring operation surface as
//! `kimojio-stack`; direct calls run on the embedded stackful scheduler for the
//! current root or worker context. Code that needs to keep worker-local versus
//! shared-ring costs visible can create [`Ring`] handles with
//! [`RuntimeContext::create_ring`], [`RuntimeContext::create_worker_ring`], or
//! [`RuntimeContext::create_shared_ring`]. Shared rings submit through runtime
//! workers instead of hiding a helper OS thread per ring; they currently provide
//! no-op, timeout, and socket read/write lifecycle operations as the explicit
//! synchronized-ring subset.
//! Runtime-neutral socket cancellation requests keep result handles drainable, so
//! downstream HTTP/TLS timeout paths can cancel pending shared operations, let
//! worker-owned state retire, and then close/reclaim sockets explicitly.
//! Shared rings are runtime-affine. In multi-worker runtimes, each shared ring
//! is assigned a real owner worker and routed through that worker's embedded
//! io_uring scheduler. In single-worker runtimes, shared rings are driven by the
//! owning stack runtime directly. Worker-local rings require an actual worker
//! context and cannot be created from the root `block_on` context of a
//! multi-worker runtime. Pending shared-ring waits require a spawned stackful
//! context that can register an external waiter; root `block_on` code may create
//! and submit shared operations, but waiting for pending results from root returns
//! a clear no-stackful-context error instead of sleeping an OS thread.
//!
//! # Example
//!
//! ```no_run
//! use kimojio_stack_steal::{Runtime, RuntimeConfig, StealPolicy};
//!
//! # fn main() {
//! let mut runtime = Runtime::with_config(RuntimeConfig {
//!     workers: std::num::NonZeroUsize::new(4).unwrap(),
//!     steal_policy: StealPolicy::steal_one(),
//!     ..RuntimeConfig::default()
//! });
//!
//! let answer = runtime.block_on(|cx| {
//!     cx.scope(|scope| {
//!         let handle = scope.spawn_stealable(|cx| {
//!             let ring = cx.create_worker_ring();
//!             ring.nop(cx).unwrap();
//!             42
//!         });
//!
//!         handle.join(cx)
//!     })
//! });
//!
//! assert_eq!(answer, 42);
//! # }
//! ```
//!
//! # Local and stealable work
//!
//! Use [`Scope::spawn_local`] or [`Scope::spawn_pinned`] for work that captures
//! non-`Send` state and must remain on the owner worker. Use
//! [`Scope::spawn_stealable`] for work that may execute on another worker; the
//! closure and return value must be `Send + 'static`.
//! Accept loops that move already-accepted sockets into stealable jobs should
//! prefer [`Scope::try_spawn_stealable`] so queue saturation is reported
//! synchronously. The returned [`JoinHandle`] can be queried with
//! [`JoinHandle::has_started`] or waited with [`JoinHandle::wait_started`] to
//! distinguish an admitted-but-not-yet-started connection task from protocol or
//! socket read bugs.
//!
//! # Cost model
//!
//! Local runnable work is preferred before global queue polling or stealing.
//! Worker queues use crossbeam work-stealing deques and a global injector, while
//! worker-local stackful schedulers reuse completed guarded stacks so short-lived
//! coroutine churn does not pay stack mapping syscalls after warmup. Steal
//! behavior is selected explicitly with [`StealPolicy`], and [`Runtime::metrics`]
//! reports queue, steal, completion, and utilization data so applications can
//! measure policy effects instead of relying on hidden scheduler heuristics.
//! Shared synchronous ring read/write uses a fixed internal per-context payload
//! cache for successful same-size operations: one read buffer and one write
//! buffer up to 64 KiB each. Shared async operation state and cross-worker
//! ownership remain explicit costs.
//! Worker shared-ring command queues retain their configured capacity across
//! drain cycles, bounded by [`RuntimeConfig::max_shared_ring_queue_len`].
//! [`RuntimeContext::pool_diagnostics`] exposes the current context's embedded
//! stack scheduler pools and shared synchronous payload cache as a read-only
//! snapshot. Context-local cache values are not aggregated into
//! [`Runtime::metrics`], which reports completed-run scheduler and stealing
//! counters. [`RuntimeMetrics::backpressure`] summarizes queue depths,
//! enqueue-to-start ready wait, rejected submissions, and worker completion
//! imbalance so connection-accept loops can distinguish worker-pool
//! backpressure from protocol-level unread-socket bugs.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::hint;
use std::num::NonZeroUsize;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::panic::{self, AssertUnwindSafe};
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::{self, JoinHandle as ThreadJoinHandle};
use std::time::{Duration, Instant};

use crossbeam_deque::{Injector, Steal, Stealer, Worker as CrossbeamWorker};
use crossbeam_queue::SegQueue;
pub use kimojio_stack::{
    AddressFamily, AtFlags, BusyPoll, EpollCtlOp, EpollEvent, EpollEventData, EpollEventFlags,
    FallocateFlags, FileAdvice, FutexWaitFlags, FutexWaitvFlags, IoCounters, IoFd, IoVec,
    MemoryAdvice, Mode, MsgHdr, OFlags, Protocol, RecvFlags, RegisteredBuffer, RegisteredFd,
    RenameFlags, ResolveFlags, SendFlags, SocketType, StackUsage, Statx, StatxFlags,
    UringSpliceFlags,
};
use kimojio_stack::{
    Errno, IoReadBuffer, IoWriteBuffer, NoPayloadIo, RawIo, ReadOutput, RuntimeCapabilities,
    RuntimeCapability, RuntimeFamily, RuntimeWaitable, SchedulerWake, StackfulWaitContext,
    StackfulWaitRegistration, StackfulWaiterHandle, UnsupportedCapability, WaitRegistration,
    Waitable, WriteOutput,
};
use rustix::io_uring::{IoringOp, io_uring_user_data};
use rustix::net::{Shutdown, addr::SocketAddrArg};
use rustix::path;
use rustix_uring::types::{CancelBuilder, FutexWaitV};

pub mod runtime_api;

const NO_WORKER: usize = usize::MAX;
const DEFAULT_QUEUE_CAPACITY: usize = 1024;
const DEFAULT_SHARED_RING_QUEUE_CAPACITY: usize = 1024;
const SHARED_SYNC_BUFFER_CACHE_ENTRIES: usize = 1;
const SHARED_SYNC_BUFFER_MAX_CAPACITY: usize = 64 * 1024;
static NEXT_RUNTIME_ID: AtomicUsize = AtomicUsize::new(1);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct RuntimeId(usize);

impl RuntimeId {
    fn next() -> Self {
        Self(NEXT_RUNTIME_ID.fetch_add(1, Ordering::Relaxed))
    }
}

/// Runtime-local fairness domain identifier.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TenantId(u64);

impl TenantId {
    /// Returns the numeric value of this runtime-local tenant identifier.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Identifier for a runtime worker.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct WorkerId(usize);

impl WorkerId {
    /// Returns this worker's zero-based index.
    ///
    /// Values returned by [`RuntimeContext::current_worker`] are always valid
    /// zero-based worker indices. [`RuntimeContext::worker_id`] is retained for
    /// compatibility and may return an internal root sentinel when called from a
    /// multi-worker root context; prefer [`RuntimeContext::current_worker`] or
    /// [`RuntimeContext::execution_place`] for new code.
    pub fn index(self) -> usize {
        self.0
    }
}

/// Identifies whether a runtime context is executing on the root thread or a worker.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ExecutionPlace {
    /// The root `block_on` context, not a worker-pool worker.
    Root,
    /// A worker-pool worker with a zero-based worker id.
    Worker(WorkerId),
}

thread_local! {
    static CURRENT_WORKER: Cell<Option<usize>> = const { Cell::new(None) };
}

/// Runs stackful coroutines with a work-stealing-capable API.
#[derive(Debug)]
pub struct Runtime {
    id: RuntimeId,
    config: RuntimeConfig,
    last_metrics: RuntimeMetrics,
}

/// Runtime configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeConfig {
    /// Usable stack bytes for each stackful coroutine.
    pub stack_size: usize,
    /// Maximum completed coroutine stacks retained per worker-local scheduler.
    pub max_cached_stacks_per_worker: usize,
    /// Number of fixed-file slots to register with each embedded io_uring runtime.
    pub registered_file_slots: u32,
    /// Number of fixed-buffer slots to register with each embedded io_uring runtime.
    pub registered_buffer_slots: u16,
    /// Policy controlling whether worker-local embedded runtimes busy-poll
    /// pending I/O completions.
    pub busy_poll: BusyPoll,
    /// Optional io_uring SQPOLL idle timeout in milliseconds for each embedded
    /// worker-local runtime.
    pub sqpoll_idle: Option<u32>,
    /// Number of worker threads requested for the runtime.
    pub workers: NonZeroUsize,
    /// Policy controlling how queued work may be stolen.
    pub steal_policy: StealPolicy,
    /// Ready-task scheduler configuration.
    pub scheduler: SchedulerConfig,
    /// Maximum queued stealable jobs per worker-local queue.
    pub max_worker_queue_len: usize,
    /// Maximum queued stealable jobs in the global injector queue.
    pub max_global_queue_len: usize,
    /// Maximum queued requests per shared ring and pending shared-ring driver
    /// commands per worker.
    pub max_shared_ring_queue_len: usize,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            stack_size: 64 * 1024,
            max_cached_stacks_per_worker: 1024,
            registered_file_slots: 0,
            registered_buffer_slots: 0,
            busy_poll: BusyPoll::Never,
            sqpoll_idle: None,
            workers: NonZeroUsize::new(1).expect("one is nonzero"),
            steal_policy: StealPolicy::Disabled,
            scheduler: SchedulerConfig::default(),
            max_worker_queue_len: DEFAULT_QUEUE_CAPACITY,
            max_global_queue_len: DEFAULT_QUEUE_CAPACITY,
            max_shared_ring_queue_len: DEFAULT_SHARED_RING_QUEUE_CAPACITY,
        }
    }
}

#[derive(Default)]
struct SharedSyncBufferCache {
    read: Vec<Vec<u8>>,
    write: Vec<Vec<u8>>,
    #[cfg(test)]
    read_reuses: usize,
    #[cfg(test)]
    write_reuses: usize,
}

impl SharedSyncBufferCache {
    fn acquire_read(&mut self, len: usize) -> Vec<u8> {
        if let Some(index) = self.read.iter().position(|buffer| buffer.len() == len) {
            #[cfg(test)]
            {
                self.read_reuses += 1;
            }
            self.read.swap_remove(index)
        } else {
            vec![0_u8; len]
        }
    }

    fn acquire_write(&mut self, buf: &[u8]) -> Vec<u8> {
        if let Some(index) = self
            .write
            .iter()
            .position(|buffer| buffer.len() == buf.len())
        {
            #[cfg(test)]
            {
                self.write_reuses += 1;
            }
            let mut buffer = self.write.swap_remove(index);
            buffer.copy_from_slice(buf);
            buffer
        } else {
            buf.to_vec()
        }
    }

    fn recycle_read(&mut self, buffer: Vec<u8>) {
        if buffer.capacity() <= SHARED_SYNC_BUFFER_MAX_CAPACITY
            && self.read.len() < SHARED_SYNC_BUFFER_CACHE_ENTRIES
        {
            self.read.push(buffer);
        }
    }

    fn recycle_write(&mut self, buffer: Vec<u8>) {
        if buffer.capacity() <= SHARED_SYNC_BUFFER_MAX_CAPACITY
            && self.write.len() < SHARED_SYNC_BUFFER_CACHE_ENTRIES
        {
            self.write.push(buffer);
        }
    }

    fn read_capacity(&self) -> usize {
        self.read.iter().map(Vec::capacity).sum()
    }

    fn write_capacity(&self) -> usize {
        self.write.iter().map(Vec::capacity).sum()
    }
}

/// Work-stealing policy configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StealPolicy {
    /// Do not steal work between workers.
    Disabled,
    /// An idle worker steals at most one eligible task at a time.
    StealOne {
        /// How often an idle worker checks global injected work.
        global_queue_interval: NonZeroUsize,
    },
    /// An idle worker steals half of a victim worker's eligible queue.
    StealHalf {
        /// How often an idle worker checks global injected work.
        global_queue_interval: NonZeroUsize,
    },
    /// An idle worker steals at most `batch` eligible tasks at a time.
    StealBatch {
        /// Maximum number of eligible tasks to steal.
        batch: NonZeroUsize,
        /// How often an idle worker checks global injected work.
        global_queue_interval: NonZeroUsize,
    },
}

impl StealPolicy {
    /// Returns a `StealOne` policy with a global queue check every idle loop.
    pub fn steal_one() -> Self {
        Self::StealOne {
            global_queue_interval: NonZeroUsize::new(1).expect("one is nonzero"),
        }
    }

    /// Returns a `StealHalf` policy with a global queue check every idle loop.
    pub fn steal_half() -> Self {
        Self::StealHalf {
            global_queue_interval: NonZeroUsize::new(1).expect("one is nonzero"),
        }
    }

    fn global_queue_interval(self) -> Option<NonZeroUsize> {
        match self {
            Self::Disabled => None,
            Self::StealOne {
                global_queue_interval,
            }
            | Self::StealHalf {
                global_queue_interval,
            }
            | Self::StealBatch {
                global_queue_interval,
                ..
            } => Some(global_queue_interval),
        }
    }
}

/// Ready-task scheduler family.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SchedulerMode {
    /// Current local/global queue plus stealing scheduler.
    #[default]
    Current,
    /// Tenant-aware stochastic fair queue scheduler.
    StochasticFair,
}

/// Worker-side ready partition selection policy for the SFQ scheduler.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum QueueSelectionPolicy {
    /// Use one tenant-selected partition.
    #[default]
    SingleChoice,
    /// Choose the shortest partition from a tenant-selected subset.
    RandomSubsetShortest {
        /// Number of candidate partitions sampled for each tenant.
        subset: NonZeroUsize,
    },
    /// Choose the globally shortest visible partition.
    Shortest,
    /// Choose a short partition while accounting for cross-worker movement cost.
    MovementCost {
        /// Number of candidate partitions sampled for each tenant.
        subset: NonZeroUsize,
        /// Queue-length penalty applied when work moves away from its home worker.
        movement_penalty: usize,
    },
}

/// Tenant-to-partition reassignment policy for the SFQ scheduler.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TenantReassignmentPolicy {
    /// Keep tenant-to-partition hashing stable for the runtime invocation.
    #[default]
    Stable,
    /// Advance the hash epoch after this many scheduling decisions.
    EverySchedulingDecisions(NonZeroUsize),
}

/// Scheduler configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchedulerConfig {
    /// Scheduler family.
    pub mode: SchedulerMode,
    /// Number of ready-work partitions used by the SFQ scheduler.
    pub ready_partitions: NonZeroUsize,
    /// Worker-side partition selection policy.
    pub selection: QueueSelectionPolicy,
    /// Tenant reassignment policy.
    pub reassignment: TenantReassignmentPolicy,
}

impl SchedulerConfig {
    /// Creates a configuration for the current scheduler.
    pub fn current() -> Self {
        Self {
            mode: SchedulerMode::Current,
            ..Self::default()
        }
    }

    /// Creates a baseline SFQ configuration with `ready_partitions` partitions.
    pub fn stochastic_fair(ready_partitions: NonZeroUsize) -> Self {
        Self {
            mode: SchedulerMode::StochasticFair,
            ready_partitions,
            ..Self::default()
        }
    }

    fn normalized(self, workers: NonZeroUsize) -> Self {
        let ready_partitions = if self.mode == SchedulerMode::StochasticFair {
            self.ready_partitions.max(workers)
        } else {
            self.ready_partitions
        };
        let selection = match self.selection {
            QueueSelectionPolicy::RandomSubsetShortest { subset } => {
                QueueSelectionPolicy::RandomSubsetShortest {
                    subset: subset.min(ready_partitions),
                }
            }
            QueueSelectionPolicy::MovementCost {
                subset,
                movement_penalty,
            } => QueueSelectionPolicy::MovementCost {
                subset: subset.min(ready_partitions),
                movement_penalty,
            },
            selection => selection,
        };
        Self {
            ready_partitions,
            selection,
            ..self
        }
    }
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            mode: SchedulerMode::Current,
            ready_partitions: NonZeroUsize::new(1).expect("one is nonzero"),
            selection: QueueSelectionPolicy::SingleChoice,
            reassignment: TenantReassignmentPolicy::Stable,
        }
    }
}

/// Scheduler metrics from the most recent [`Runtime::block_on`] invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeMetrics {
    /// Worker count used by the invocation.
    pub worker_count: usize,
    /// Stealing policy used by the invocation.
    pub steal_policy: StealPolicy,
    /// Ready-task scheduler configuration used by the invocation.
    pub scheduler: SchedulerConfig,
    /// Number of steal attempts.
    pub steal_attempts: usize,
    /// Number of successful steals.
    pub successful_steals: usize,
    /// Number of failed steals.
    pub failed_steals: usize,
    /// Number of global queue polls.
    pub global_queue_polls: usize,
    /// Number of completed stealable jobs.
    pub completed_tasks: usize,
    /// Number of rejected job submissions.
    pub rejected_tasks: usize,
    /// Maximum sampled local queue depth.
    pub max_local_queue_depth: usize,
    /// Maximum sampled global injector queue depth.
    pub max_global_queue_depth: usize,
    /// Maximum sampled SFQ ready partition depth.
    pub max_sfq_partition_depth: usize,
    /// Number of SFQ ready partition polls.
    pub sfq_queue_polls: usize,
    /// Final SFQ reassignment epoch observed by the runtime.
    pub sfq_epoch: u64,
    /// Number of worker-pool jobs that recorded enqueue-to-start wait time.
    pub ready_wait_samples: usize,
    /// Total enqueue-to-start wait time across sampled worker-pool jobs.
    ///
    /// This counter saturates at `usize::MAX` internally.
    pub total_ready_wait_ns: u128,
    /// Maximum enqueue-to-start wait time observed for one worker-pool job.
    pub max_ready_wait_ns: u128,
    /// Tenant associated with the maximum observed ready wait.
    pub max_ready_wait_tenant: Option<TenantId>,
    /// Most recent owner worker observed for a completed worker-pool job.
    pub last_owner_worker: Option<WorkerId>,
    /// Most recent executing worker observed for a completed worker-pool job.
    pub last_executing_worker: Option<WorkerId>,
    /// Most recent victim worker selected by the stealing path.
    pub last_steal_victim: Option<WorkerId>,
    /// Per-worker completed worker-pool job counts for utilization visibility.
    pub worker_completed_tasks: Vec<usize>,
    /// Number of allocated tenant identifiers observed by this runtime.
    pub allocated_tenants: u64,
    /// Last tenant identifier observed on a completed worker-pool job.
    pub last_tenant: Option<TenantId>,
}

impl RuntimeMetrics {
    /// Returns a compact scheduler-backpressure summary for this completed run.
    ///
    /// This is intended for service harnesses that accept sockets and then spawn
    /// stealable connection jobs. If accepted connections remain unread under
    /// load, this summary makes queueing, ready-wait, rejected submissions, and
    /// worker imbalance visible without requiring callers to inspect every raw
    /// metric field.
    pub fn backpressure(&self) -> RuntimeBackpressure {
        let max_worker_completed = self
            .worker_completed_tasks
            .iter()
            .copied()
            .max()
            .unwrap_or_default();
        let min_worker_completed = self
            .worker_completed_tasks
            .iter()
            .copied()
            .min()
            .unwrap_or_default();
        let max_queue_depth = self
            .max_global_queue_depth
            .max(self.max_local_queue_depth)
            .max(self.max_sfq_partition_depth);
        RuntimeBackpressure {
            queued_work_observed: max_queue_depth != 0,
            rejected_tasks: self.rejected_tasks,
            ready_wait_samples: self.ready_wait_samples,
            max_ready_wait_ns: self.max_ready_wait_ns,
            average_ready_wait_ns: (self.ready_wait_samples != 0)
                .then_some(self.total_ready_wait_ns / self.ready_wait_samples as u128),
            max_queue_depth,
            max_global_queue_depth: self.max_global_queue_depth,
            max_local_queue_depth: self.max_local_queue_depth,
            max_sfq_partition_depth: self.max_sfq_partition_depth,
            worker_completion_imbalance: max_worker_completed.saturating_sub(min_worker_completed),
        }
    }
}

/// Compact summary of worker-pool queueing and readiness delay.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RuntimeBackpressure {
    /// Whether any global, local, or SFQ ready queue depth was sampled above zero.
    pub queued_work_observed: bool,
    /// Number of rejected stealable submissions.
    pub rejected_tasks: usize,
    /// Number of jobs with enqueue-to-start wait samples.
    pub ready_wait_samples: usize,
    /// Maximum enqueue-to-start wait time in nanoseconds.
    pub max_ready_wait_ns: u128,
    /// Average enqueue-to-start wait time in nanoseconds, if any samples exist.
    pub average_ready_wait_ns: Option<u128>,
    /// Maximum depth across all sampled ready queues.
    pub max_queue_depth: usize,
    /// Maximum sampled global injector depth.
    pub max_global_queue_depth: usize,
    /// Maximum sampled local queue depth.
    pub max_local_queue_depth: usize,
    /// Maximum sampled SFQ partition depth.
    pub max_sfq_partition_depth: usize,
    /// Difference between the busiest and least busy worker completion counts.
    pub worker_completion_imbalance: usize,
}

/// Snapshot of runtime-context-owned pool state.
///
/// This diagnostic surface combines the embedded stack runtime's scheduler pool
/// snapshot with the current context's shared synchronous ring payload cache.
/// It is read-only observability, not allocator accounting. Fields may grow as
/// this alpha runtime adds more internal pools.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct PoolDiagnostics {
    /// Embedded stack runtime scheduler-pool diagnostics.
    pub stack: kimojio_stack::PoolDiagnostics,
    /// Number of cached shared synchronous read buffers in this context.
    pub shared_sync_read_buffers: usize,
    /// Total retained capacity of cached shared synchronous read buffers.
    pub shared_sync_read_capacity: usize,
    /// Number of cached shared synchronous write buffers in this context.
    pub shared_sync_write_buffers: usize,
    /// Total retained capacity of cached shared synchronous write buffers.
    pub shared_sync_write_capacity: usize,
    /// Maximum cached buffers retained per read/write direction in one context.
    pub max_shared_sync_buffers_per_direction: usize,
    /// Maximum retained capacity for each shared synchronous cached buffer.
    pub max_shared_sync_buffer_capacity: usize,
}

impl Default for RuntimeMetrics {
    fn default() -> Self {
        Self {
            worker_count: 1,
            steal_policy: StealPolicy::Disabled,
            scheduler: SchedulerConfig::default(),
            steal_attempts: 0,
            successful_steals: 0,
            failed_steals: 0,
            global_queue_polls: 0,
            completed_tasks: 0,
            rejected_tasks: 0,
            max_local_queue_depth: 0,
            max_global_queue_depth: 0,
            max_sfq_partition_depth: 0,
            sfq_queue_polls: 0,
            sfq_epoch: 0,
            ready_wait_samples: 0,
            total_ready_wait_ns: 0,
            max_ready_wait_ns: 0,
            max_ready_wait_tenant: None,
            last_owner_worker: None,
            last_executing_worker: None,
            last_steal_victim: None,
            worker_completed_tasks: vec![0],
            allocated_tenants: 0,
            last_tenant: None,
        }
    }
}

impl Runtime {
    /// Creates a single-worker runtime with stealing disabled.
    pub fn new() -> Self {
        Self {
            id: RuntimeId::next(),
            config: RuntimeConfig::default(),
            last_metrics: RuntimeMetrics::default(),
        }
    }

    /// Creates a runtime with a custom usable stack size for each coroutine.
    pub fn with_stack_size(stack_size: usize) -> Self {
        Self {
            id: RuntimeId::next(),
            config: RuntimeConfig {
                stack_size,
                ..RuntimeConfig::default()
            },
            last_metrics: RuntimeMetrics::default(),
        }
    }

    /// Creates a runtime with custom configuration.
    pub fn with_config(mut config: RuntimeConfig) -> Self {
        config.scheduler = config.scheduler.normalized(config.workers);
        Self {
            id: RuntimeId::next(),
            config,
            last_metrics: RuntimeMetrics {
                worker_count: config.workers.get(),
                steal_policy: config.steal_policy,
                scheduler: config.scheduler.normalized(config.workers),
                ..RuntimeMetrics::default()
            },
        }
    }

    /// Returns this runtime's configuration.
    pub fn config(&self) -> RuntimeConfig {
        self.config
    }

    /// Returns scheduler metrics from the most recent [`Runtime::block_on`].
    pub fn metrics(&self) -> RuntimeMetrics {
        self.last_metrics.clone()
    }

    /// Runs `main` to completion.
    pub fn block_on<F, T>(&mut self, main: F) -> T
    where
        F: FnOnce(&RuntimeContext<'_>) -> T,
    {
        if self.config.workers.get() == 1 {
            return self.block_on_single(main);
        }

        self.block_on_multi(main)
    }

    fn block_on_single<F, T>(&mut self, main: F) -> T
    where
        F: FnOnce(&RuntimeContext<'_>) -> T,
    {
        let mut inner = self.inner_runtime();
        let local_shared_operations = Rc::new(LocalSharedOperations::default());
        let tenant_allocator = Arc::new(AtomicU64::new(2));
        let output = inner.block_on(|inner| {
            let cx = RuntimeContext {
                inner,
                runtime_id: self.id,
                worker: WorkerId(0),
                steal_runtime: None,
                active_steal_scope: None,
                tenant: TenantId(1),
                tenant_allocator: Arc::clone(&tenant_allocator),
                local_shared_operations: Some(Rc::clone(&local_shared_operations)),
                shared_ring_queue_capacity: self.config.max_shared_ring_queue_len,
                socket_ring: RefCell::new(None),
                shared_sync_buffers: RefCell::new(SharedSyncBufferCache::default()),
            };
            let output = panic::catch_unwind(AssertUnwindSafe(|| main(&cx)));
            local_shared_operations.close_all();
            match output {
                Ok(output) => output,
                Err(payload) => panic::resume_unwind(payload),
            }
        });
        self.last_metrics = RuntimeMetrics {
            worker_count: 1,
            steal_policy: self.config.steal_policy,
            scheduler: self.config.scheduler,
            allocated_tenants: tenant_allocator.load(Ordering::Relaxed).saturating_sub(1),
            ..RuntimeMetrics::default()
        };
        output
    }

    fn block_on_multi<F, T>(&mut self, main: F) -> T
    where
        F: FnOnce(&RuntimeContext<'_>) -> T,
    {
        let worker_pool = WorkerPool::start(self.config, self.id);
        let runtime = Arc::clone(&worker_pool.runtime);
        let tenant_allocator = Arc::clone(&runtime.tenant_allocator);
        let mut inner = self.inner_runtime();
        let output = panic::catch_unwind(AssertUnwindSafe(|| {
            inner.block_on(|inner| {
                let cx = RuntimeContext {
                    inner,
                    runtime_id: self.id,
                    worker: WorkerId(NO_WORKER),
                    steal_runtime: Some(Arc::clone(&runtime)),
                    active_steal_scope: None,
                    tenant: TenantId(1),
                    tenant_allocator: Arc::clone(&tenant_allocator),
                    local_shared_operations: None,
                    shared_ring_queue_capacity: self.config.max_shared_ring_queue_len,
                    socket_ring: RefCell::new(None),
                    shared_sync_buffers: RefCell::new(SharedSyncBufferCache::default()),
                };
                main(&cx)
            })
        }));
        self.last_metrics = worker_pool.shutdown();

        match output {
            Ok(output) => output,
            Err(payload) => panic::resume_unwind(payload),
        }
    }

    fn inner_runtime(&self) -> kimojio_stack::Runtime {
        let mut config = kimojio_stack::RuntimeConfig::default();
        config.stack_size = self.config.stack_size;
        config.max_cached_stacks = self.config.max_cached_stacks_per_worker;
        config.registered_file_slots = self.config.registered_file_slots;
        config.registered_buffer_slots = self.config.registered_buffer_slots;
        config.busy_poll = self.config.busy_poll;
        config.sqpoll_idle = self.config.sqpoll_idle;
        kimojio_stack::Runtime::with_config(config)
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

/// Methods available to code running inside a [`Runtime`].
pub struct RuntimeContext<'cx> {
    inner: &'cx kimojio_stack::RuntimeContext<'cx>,
    runtime_id: RuntimeId,
    worker: WorkerId,
    steal_runtime: Option<Arc<MultiRuntime>>,
    active_steal_scope: Option<Arc<StealScopeState>>,
    tenant: TenantId,
    tenant_allocator: Arc<AtomicU64>,
    local_shared_operations: Option<Rc<LocalSharedOperations>>,
    shared_ring_queue_capacity: usize,
    socket_ring: RefCell<Option<Ring>>,
    shared_sync_buffers: RefCell<SharedSyncBufferCache>,
}

impl RuntimeContext<'_> {
    /// Creates a structured concurrency scope.
    pub fn scope<'env, F, T>(&'env self, f: F) -> T
    where
        F: for<'scope> FnOnce(&Scope<'scope, 'env>) -> T,
    {
        let steal_scope_cell = self.steal_runtime.as_ref().map(|_| RefCell::new(None));
        let steal_scope = steal_scope_cell.as_ref().map(NonNull::from);
        let output = panic::catch_unwind(AssertUnwindSafe(|| {
            self.inner.scope(|inner| {
                let scope = Scope {
                    inner,
                    runtime_id: self.runtime_id,
                    worker: self.worker,
                    steal_runtime: self.steal_runtime.clone(),
                    active_steal_scope: self.active_steal_scope.clone(),
                    tenant: self.tenant,
                    tenant_allocator: Arc::clone(&self.tenant_allocator),
                    local_shared_operations: self.local_shared_operations.clone(),
                    steal_scope,
                    shared_ring_queue_capacity: self.shared_ring_queue_capacity,
                };
                panic::catch_unwind(AssertUnwindSafe(|| f(&scope)))
            })
        }));
        if let Some(steal_scope) = steal_scope.and_then(|steal_scope| {
            // SAFETY: `steal_scope` points at the stack-local RefCell created at
            // the start of this method. `kimojio_stack::RuntimeContext::scope`
            // prevents `Scope` from escaping, so no user code can retain this
            // pointer after the inner scope returns.
            unsafe { steal_scope.as_ref().borrow_mut().take() }
        }) {
            let cancel_immediately = !matches!(output, Ok(Ok(_)));
            steal_scope.wait_with(self, cancel_immediately);
            if output.as_ref().is_ok_and(|output| output.is_ok()) {
                steal_scope.resume_unobserved_panic();
            }
        }

        match output {
            Ok(Ok(output)) => output,
            Ok(Err(payload)) | Err(payload) => panic::resume_unwind(payload),
        }
    }

    /// Cooperatively yields the current stackful coroutine.
    pub fn yield_now(&self) {
        self.inner.yield_now();
    }

    /// Parks the current stackful coroutine or advances root runtime work.
    pub fn park(&self) {
        self.inner.park_stackful();
    }

    /// Returns where this context is executing.
    pub fn execution_place(&self) -> ExecutionPlace {
        self.current_worker()
            .map_or(ExecutionPlace::Root, ExecutionPlace::Worker)
    }

    /// Returns the tenant identifier associated with the current stackful context.
    pub fn tenant_id(&self) -> TenantId {
        self.tenant
    }

    /// Allocates a fresh runtime-local tenant identifier.
    pub fn new_tenant_id(&self) -> TenantId {
        TenantId(self.tenant_allocator.fetch_add(1, Ordering::Relaxed))
    }

    /// Returns the worker currently executing this context.
    ///
    /// Returns `None` for the root `block_on` context in multi-worker mode.
    pub fn current_worker(&self) -> Option<WorkerId> {
        (self.worker.index() != NO_WORKER).then_some(self.worker)
    }

    /// Returns the worker currently executing this context.
    ///
    /// Prefer [`RuntimeContext::current_worker`] or
    /// [`RuntimeContext::execution_place`] for new code. In multi-worker mode,
    /// the root `block_on` context is not a worker and this compatibility method
    /// returns an internal sentinel value that must not be used as an array index.
    pub fn worker_id(&self) -> WorkerId {
        self.worker
    }

    /// Returns the default usable stack size inherited by new scopes.
    pub fn stack_size(&self) -> usize {
        self.inner.stack_size()
    }

    /// Returns stack usage for the current stackful coroutine.
    pub fn stack_usage(&self) -> Option<StackUsage> {
        self.inner.stack_usage()
    }

    /// Creates an explicit ring handle for I/O operations.
    pub fn create_ring(&self, mode: RingMode) -> Result<Ring, RingError> {
        Ok(match mode {
            RingMode::WorkerLocal => Ring {
                inner: RingInner::WorkerLocal {
                    runtime_id: self.runtime_id,
                    owner: self.current_worker().ok_or(RingError::NoCurrentWorker)?,
                },
            },
            RingMode::Shared => Ring {
                inner: RingInner::Shared(SharedRing::new(
                    self.shared_ring_queue_capacity,
                    self.shared_ring_driver(),
                )?),
            },
        })
    }

    fn shared_ring_driver(&self) -> SharedRingDriver {
        if let Some(runtime) = &self.steal_runtime {
            let owner = self
                .current_worker()
                .unwrap_or_else(|| runtime.next_shared_ring_owner());
            SharedRingDriver::Multi {
                runtime: Arc::clone(runtime),
                owner,
            }
        } else {
            SharedRingDriver::Local {
                runtime_id: self.runtime_id,
            }
        }
    }

    /// Creates a ring handle pinned to the current worker.
    pub fn create_worker_ring(&self) -> Ring {
        self.create_ring(RingMode::WorkerLocal)
            .expect("worker-local ring creation requires a worker context")
    }

    /// Creates a synchronized ring handle that may be cloned across workers.
    ///
    /// Shared rings close when the final handle is dropped and reject
    /// submissions once their bounded queue is full.
    ///
    /// # Panics
    ///
    /// Panics if shared ring creation fails. Use [`RuntimeContext::create_ring`]
    /// with [`RingMode::Shared`] to handle creation errors as a [`RingError`].
    pub fn create_shared_ring(&self) -> Ring {
        self.create_ring(RingMode::Shared)
            .expect("shared ring creation failed")
    }

    fn acquire_shared_sync_read_buffer(&self, len: usize) -> Vec<u8> {
        self.shared_sync_buffers.borrow_mut().acquire_read(len)
    }

    fn recycle_shared_sync_read_buffer(&self, buffer: Vec<u8>) {
        self.shared_sync_buffers.borrow_mut().recycle_read(buffer);
    }

    fn acquire_shared_sync_write_buffer(&self, buf: &[u8]) -> Vec<u8> {
        self.shared_sync_buffers.borrow_mut().acquire_write(buf)
    }

    fn recycle_shared_sync_write_buffer(&self, buffer: Vec<u8>) {
        self.shared_sync_buffers.borrow_mut().recycle_write(buffer);
    }

    /// Submits an io_uring no-op and waits for completion.
    pub fn nop(&self) -> Result<(), Errno> {
        self.inner.nop()
    }

    /// Submits an io_uring no-op linked to a non-firing timeout and waits for
    /// the no-op completion.
    #[doc(hidden)]
    pub fn nop_with_link_timeout(&self, timeout: Duration) -> Result<(), Errno> {
        self.inner
            .submit_no_payload_nop_with_timeout(timeout)
            .wait(self.inner)
    }

    /// Submits an io_uring no-op and races it with a separate timeout.
    #[doc(hidden)]
    pub fn nop_with_select_timeout(&self, timeout: Duration) -> Result<(), Errno> {
        let timer = self.inner.timeout(timeout);
        let mut pending = self.inner.submit_no_payload_nop();
        loop {
            if let Some(result) = pending.try_wait() {
                return result;
            }
            let waitables: [&dyn Waitable; 2] = [&pending, &timer];
            let ready = self
                .inner
                .wait_any(&waitables, None)
                .expect("no-payload timeout race wait failed");
            if ready == 1 {
                pending.cancel_and_wait(self.inner)?;
                return Err(Errno::TIME);
            }
        }
    }

    /// Starts an internal no-payload no-op.
    #[doc(hidden)]
    pub fn submit_no_payload_nop(&self) -> NoPayloadIo {
        self.inner.submit_no_payload_nop()
    }

    /// Starts an internal no-payload no-op with a linked kernel timeout.
    #[doc(hidden)]
    pub fn submit_no_payload_nop_with_timeout(&self, timeout: Duration) -> NoPayloadIo {
        self.inner.submit_no_payload_nop_with_timeout(timeout)
    }

    /// Starts an internal no-payload timeout.
    #[doc(hidden)]
    pub fn submit_no_payload_timeout(&self, duration: Duration) -> NoPayloadIo {
        self.inner.submit_no_payload_timeout(duration)
    }

    /// Starts an internal raw read.
    ///
    /// # Safety
    ///
    /// `buffer` must be valid for writes of `len` bytes until the operation
    /// completes or is detached with equivalent ownership.
    #[doc(hidden)]
    pub unsafe fn submit_raw_read(&self, fd: &impl AsFd, buffer: *mut u8, len: usize) -> RawIo {
        unsafe { self.inner.submit_raw_read(fd, buffer, len) }
    }

    /// Starts an internal raw read with a linked kernel timeout.
    ///
    /// # Safety
    ///
    /// `buffer` must be valid for writes of `len` bytes until the operation
    /// completes or is detached with equivalent ownership.
    #[doc(hidden)]
    pub unsafe fn submit_raw_read_with_timeout(
        &self,
        fd: &impl AsFd,
        buffer: *mut u8,
        len: usize,
        timeout: Duration,
    ) -> RawIo {
        unsafe {
            self.inner
                .submit_raw_read_with_timeout(fd, buffer, len, timeout)
        }
    }

    /// Starts an internal raw write.
    ///
    /// # Safety
    ///
    /// `buffer` must be valid for reads of `len` bytes until the operation
    /// completes or is detached with equivalent ownership.
    #[doc(hidden)]
    pub unsafe fn submit_raw_write(&self, fd: &impl AsFd, buffer: *const u8, len: usize) -> RawIo {
        unsafe { self.inner.submit_raw_write(fd, buffer, len) }
    }

    /// Starts an internal raw write with a linked kernel timeout.
    ///
    /// # Safety
    ///
    /// `buffer` must be valid for reads of `len` bytes until the operation
    /// completes or is detached with equivalent ownership.
    #[doc(hidden)]
    pub unsafe fn submit_raw_write_with_timeout(
        &self,
        fd: &impl AsFd,
        buffer: *const u8,
        len: usize,
        timeout: Duration,
    ) -> RawIo {
        unsafe {
            self.inner
                .submit_raw_write_with_timeout(fd, buffer, len, timeout)
        }
    }

    /// Starts an internal raw socket shutdown.
    #[doc(hidden)]
    pub fn submit_raw_shutdown(&self, fd: &impl AsFd, how: Shutdown) -> RawIo {
        self.inner.submit_raw_shutdown(fd, how)
    }

    /// Starts an internal raw close.
    #[doc(hidden)]
    pub fn submit_raw_close(&self, fd: OwnedFd) -> RawIo {
        self.inner.submit_raw_close(fd)
    }

    /// Waits until `duration` has elapsed.
    pub fn sleep(&self, duration: Duration) -> Result<(), Errno> {
        self.inner.sleep(duration)
    }

    /// Registers `fd` in the embedded runtime's fixed-file table.
    pub fn register_fd(&self, fd: OwnedFd) -> Result<RegisteredFd, Errno> {
        self.inner.register_fd(fd)
    }

    /// Registers an owned buffer in the embedded runtime's fixed-buffer table.
    pub fn register_buffer<B>(&self, buffer: B) -> Result<RegisteredBuffer<B>, Errno>
    where
        B: IoReadBuffer + IoWriteBuffer + 'static,
    {
        self.inner.register_buffer(buffer)
    }

    /// Returns whether the current kernel reports support for an io_uring opcode.
    pub fn supports_io_uring_opcode(&self, opcode: IoringOp) -> bool {
        self.inner.supports_io_uring_opcode(opcode)
    }

    /// Opens `path` relative to the process current working directory.
    pub fn open<P: path::Arg>(&self, path: P, flags: OFlags, mode: Mode) -> Result<OwnedFd, Errno> {
        self.inner.open(path, flags, mode)
    }

    /// Opens `path` relative to `dirfd`.
    pub fn openat<P: path::Arg>(
        &self,
        dirfd: &impl AsFd,
        path: P,
        flags: OFlags,
        mode: Mode,
    ) -> Result<OwnedFd, Errno> {
        self.inner.openat(dirfd, path, flags, mode)
    }

    /// Opens `path` relative to the process current working directory using openat2.
    pub fn open2<P: path::Arg>(
        &self,
        path: P,
        flags: OFlags,
        mode: Mode,
        resolve: ResolveFlags,
    ) -> Result<OwnedFd, Errno> {
        self.inner.open2(path, flags, mode, resolve)
    }

    /// Opens `path` relative to `dirfd` using openat2.
    pub fn openat2<P: path::Arg>(
        &self,
        dirfd: &impl AsFd,
        path: P,
        flags: OFlags,
        mode: Mode,
        resolve: ResolveFlags,
    ) -> Result<OwnedFd, Errno> {
        self.inner.openat2(dirfd, path, flags, mode, resolve)
    }

    /// Gets extended file status for `path` in the process current working directory.
    pub fn statx<P: path::Arg>(
        &self,
        path: P,
        flags: AtFlags,
        mask: StatxFlags,
    ) -> Result<Statx, Errno> {
        self.inner.statx(path, flags, mask)
    }

    /// Gets extended file status relative to `dirfd`.
    pub fn statxat<P: path::Arg>(
        &self,
        dirfd: &impl AsFd,
        path: P,
        flags: AtFlags,
        mask: StatxFlags,
    ) -> Result<Statx, Errno> {
        self.inner.statxat(dirfd, path, flags, mask)
    }

    /// Creates a hard link using paths relative to the process current working directory.
    pub fn link<P: path::Arg, Q: path::Arg>(
        &self,
        oldpath: P,
        newpath: Q,
        flags: AtFlags,
    ) -> Result<(), Errno> {
        self.inner.link(oldpath, newpath, flags)
    }

    /// Creates a hard link using paths relative to directory fds.
    pub fn linkat<P: path::Arg, Q: path::Arg>(
        &self,
        olddirfd: &impl AsFd,
        oldpath: P,
        newdirfd: &impl AsFd,
        newpath: Q,
        flags: AtFlags,
    ) -> Result<(), Errno> {
        self.inner
            .linkat(olddirfd, oldpath, newdirfd, newpath, flags)
    }

    /// Creates a symbolic link in the process current working directory.
    pub fn symlink<P: path::Arg, Q: path::Arg>(&self, target: P, linkpath: Q) -> Result<(), Errno> {
        self.inner.symlink(target, linkpath)
    }

    /// Creates a symbolic link relative to `newdirfd`.
    pub fn symlinkat<P: path::Arg, Q: path::Arg>(
        &self,
        target: P,
        newdirfd: &impl AsFd,
        linkpath: Q,
    ) -> Result<(), Errno> {
        self.inner.symlinkat(target, newdirfd, linkpath)
    }

    /// Creates a directory relative to the process current working directory.
    pub fn mkdir<P: path::Arg>(&self, path: P, mode: Mode) -> Result<(), Errno> {
        self.inner.mkdir(path, mode)
    }

    /// Creates a directory relative to `dirfd`.
    pub fn mkdirat<P: path::Arg>(
        &self,
        dirfd: &impl AsFd,
        path: P,
        mode: Mode,
    ) -> Result<(), Errno> {
        self.inner.mkdirat(dirfd, path, mode)
    }

    /// Removes a directory.
    pub fn rmdir<P: path::Arg>(&self, path: P) -> Result<(), Errno> {
        self.inner.rmdir(path)
    }

    /// Removes a filesystem entry.
    pub fn unlink<P: path::Arg>(&self, path: P) -> Result<(), Errno> {
        self.inner.unlink(path)
    }

    /// Removes a filesystem entry relative to `dirfd`.
    pub fn unlinkat<P: path::Arg>(
        &self,
        dirfd: &impl AsFd,
        path: P,
        flags: AtFlags,
    ) -> Result<(), Errno> {
        self.inner.unlinkat(dirfd, path, flags)
    }

    /// Renames a filesystem entry in the process current working directory.
    pub fn rename<P: path::Arg, Q: path::Arg>(
        &self,
        oldpath: P,
        newpath: Q,
        flags: RenameFlags,
    ) -> Result<(), Errno> {
        self.inner.rename(oldpath, newpath, flags)
    }

    /// Renames a filesystem entry relative to directory fds.
    pub fn renameat<P: path::Arg, Q: path::Arg>(
        &self,
        olddirfd: &impl AsFd,
        oldpath: P,
        newdirfd: &impl AsFd,
        newpath: Q,
        flags: RenameFlags,
    ) -> Result<(), Errno> {
        self.inner
            .renameat(olddirfd, oldpath, newdirfd, newpath, flags)
    }

    /// Declares an expected file access pattern.
    pub fn fadvise(
        &self,
        fd: &impl AsFd,
        offset: u64,
        len: u32,
        advice: FileAdvice,
    ) -> Result<(), Errno> {
        self.inner.fadvise(fd, offset, len, advice)
    }

    /// Gives advice about a memory range.
    ///
    /// # Safety
    ///
    /// `addr..addr + len` must satisfy the safety requirements of `madvise(2)`.
    pub unsafe fn madvise(
        &self,
        addr: *mut std::ffi::c_void,
        len: usize,
        advice: MemoryAdvice,
    ) -> Result<(), Errno> {
        unsafe { self.inner.madvise(addr, len, advice) }
    }

    /// Preallocates or deallocates file space.
    pub fn fallocate(
        &self,
        fd: &impl AsFd,
        mode: FallocateFlags,
        offset: u64,
        len: u64,
    ) -> Result<(), Errno> {
        self.inner.fallocate(fd, mode, offset, len)
    }

    /// Sets the length of a file.
    pub fn ftruncate(&self, fd: &impl AsFd, len: u64) -> Result<(), Errno> {
        self.inner.ftruncate(fd, len)
    }

    /// Synchronizes file data and metadata.
    pub fn fsync(&self, fd: &impl AsFd) -> Result<(), Errno> {
        self.inner.fsync(fd)
    }

    /// Synchronizes a file range.
    pub fn sync_file_range(
        &self,
        fd: &impl AsFd,
        offset: u64,
        len: u32,
        flags: u32,
    ) -> Result<(), Errno> {
        self.inner.sync_file_range(fd, offset, len, flags)
    }

    /// Gets extended status for an open file descriptor.
    pub fn fstat(&self, fd: &impl AsFd, mask: StatxFlags) -> Result<Statx, Errno> {
        self.inner.fstat(fd, mask)
    }

    /// Splices data between file descriptors.
    pub fn splice(
        &self,
        fd_in: &impl AsFd,
        off_in: i64,
        fd_out: &impl AsFd,
        off_out: i64,
        len: u32,
        flags: UringSpliceFlags,
    ) -> Result<usize, Errno> {
        self.inner
            .splice(fd_in, off_in, fd_out, off_out, len, flags)
    }

    /// Duplicates pipe data between file descriptors.
    pub fn tee(
        &self,
        fd_in: &impl AsFd,
        fd_out: &impl AsFd,
        len: u32,
        flags: UringSpliceFlags,
    ) -> Result<usize, Errno> {
        self.inner.tee(fd_in, fd_out, len, flags)
    }

    /// Waits for one poll event set on `fd`.
    pub fn poll(&self, fd: &impl AsFd, flags: u32) -> Result<u32, Errno> {
        self.inner.poll(fd, flags)
    }

    /// Removes a previously submitted poll request by user data.
    pub fn poll_remove(&self, user_data: io_uring_user_data) -> Result<(), Errno> {
        self.inner.poll_remove(user_data)
    }

    /// Modifies an epoll instance.
    pub fn epoll_ctl(
        &self,
        epfd: &impl AsFd,
        fd: &impl AsFd,
        op: EpollCtlOp,
        event: &EpollEvent,
    ) -> Result<(), Errno> {
        self.inner.epoll_ctl(epfd, fd, op, event)
    }

    /// Creates a socket.
    pub fn socket(
        &self,
        domain: AddressFamily,
        socket_type: SocketType,
        protocol: Option<Protocol>,
    ) -> Result<OwnedFd, Errno> {
        self.inner.socket(domain, socket_type, protocol)
    }

    /// Binds a socket to `address`.
    pub fn bind(&self, fd: &impl AsFd, address: &impl SocketAddrArg) -> Result<(), Errno> {
        self.inner.bind(fd, address)
    }

    /// Marks a socket as accepting incoming connections.
    pub fn listen(&self, fd: &impl AsFd, backlog: i32) -> Result<(), Errno> {
        self.inner.listen(fd, backlog)
    }

    /// Accepts one connection from a listening socket.
    pub fn accept(&self, fd: &impl AsFd) -> Result<OwnedFd, Errno> {
        self.inner.accept(fd)
    }

    /// Connects a socket to `address`.
    pub fn connect(&self, fd: &impl AsFd, address: &impl SocketAddrArg) -> Result<(), Errno> {
        self.inner.connect(fd, address)
    }

    /// Reads from `fd` into `buf`.
    pub fn read(&self, fd: &impl AsFd, buf: &mut [u8]) -> Result<usize, Errno> {
        self.inner.read(fd, buf)
    }

    /// Reads from `fd` into `buf` at an explicit file offset.
    pub fn pread(&self, fd: &impl AsFd, buf: &mut [u8], offset: u64) -> Result<usize, Errno> {
        self.inner.pread(fd, buf, offset)
    }

    /// Vectored read from `fd` into `iovecs`.
    pub fn readv(&self, fd: &impl AsFd, iovecs: &mut [IoVec]) -> Result<usize, Errno> {
        self.inner.readv(fd, iovecs)
    }

    /// Reads from a registered fixed fd into `buf`.
    pub fn read_registered_fd(&self, fd: &RegisteredFd, buf: &mut [u8]) -> Result<usize, Errno> {
        self.inner.read_registered_fd(fd, buf)
    }

    /// Reads from `fd` into a registered fixed buffer.
    pub fn read_registered_buffer<B>(
        &self,
        fd: &impl AsFd,
        buffer: &mut RegisteredBuffer<B>,
    ) -> Result<usize, Errno> {
        self.inner.read_registered_buffer(fd, buffer)
    }

    /// Reads from a registered fixed fd into a registered fixed buffer.
    pub fn read_registered_fixed<B>(
        &self,
        fd: &RegisteredFd,
        buffer: &mut RegisteredBuffer<B>,
    ) -> Result<usize, Errno> {
        self.inner.read_registered_fixed(fd, buffer)
    }

    /// Starts a read into an owned buffer.
    pub fn read_async<B>(&self, fd: &IoFd, buffer: B) -> IoResult<ReadOutput<B>, B>
    where
        B: IoReadBuffer + 'static,
    {
        IoResult::new(self.inner.read_async(fd, buffer))
    }

    /// Starts a read from a registered fixed fd into an owned buffer.
    pub fn read_registered_fd_async<B>(
        &self,
        fd: &RegisteredFd,
        buffer: B,
    ) -> IoResult<ReadOutput<B>, B>
    where
        B: IoReadBuffer + 'static,
    {
        IoResult::new(self.inner.read_registered_fd_async(fd, buffer))
    }

    /// Starts a read from `fd` into a registered fixed buffer.
    pub fn read_registered_buffer_async<B: 'static>(
        &self,
        fd: &IoFd,
        buffer: RegisteredBuffer<B>,
    ) -> IoResult<ReadOutput<RegisteredBuffer<B>>, RegisteredBuffer<B>> {
        IoResult::new(self.inner.read_registered_buffer_async(fd, buffer))
    }

    /// Starts a read from a registered fixed fd into a registered fixed buffer.
    pub fn read_registered_fixed_async<B: 'static>(
        &self,
        fd: &RegisteredFd,
        buffer: RegisteredBuffer<B>,
    ) -> IoResult<ReadOutput<RegisteredBuffer<B>>, RegisteredBuffer<B>> {
        IoResult::new(self.inner.read_registered_fixed_async(fd, buffer))
    }

    /// Writes `buf` to `fd`.
    pub fn write(&self, fd: &impl AsFd, buf: &[u8]) -> Result<usize, Errno> {
        self.inner.write(fd, buf)
    }

    /// Writes `buf` to `fd` at an explicit file offset.
    pub fn pwrite(&self, fd: &impl AsFd, buf: &[u8], offset: u64) -> Result<usize, Errno> {
        self.inner.pwrite(fd, buf, offset)
    }

    /// Vectored write from `iovecs` to `fd`.
    pub fn writev(&self, fd: &impl AsFd, iovecs: &[IoVec]) -> Result<usize, Errno> {
        self.inner.writev(fd, iovecs)
    }

    /// Writes `buf` to a registered fixed fd.
    pub fn write_registered_fd(&self, fd: &RegisteredFd, buf: &[u8]) -> Result<usize, Errno> {
        self.inner.write_registered_fd(fd, buf)
    }

    /// Writes from a registered fixed buffer to `fd`.
    pub fn write_registered_buffer<B>(
        &self,
        fd: &impl AsFd,
        buffer: &RegisteredBuffer<B>,
    ) -> Result<usize, Errno> {
        self.inner.write_registered_buffer(fd, buffer)
    }

    /// Writes from a registered fixed buffer to a registered fixed fd.
    pub fn write_registered_fixed<B>(
        &self,
        fd: &RegisteredFd,
        buffer: &RegisteredBuffer<B>,
    ) -> Result<usize, Errno> {
        self.inner.write_registered_fixed(fd, buffer)
    }

    /// Sends bytes on a connected socket.
    pub fn send(&self, fd: &impl AsFd, buf: &[u8]) -> Result<usize, Errno> {
        self.inner.send(fd, buf)
    }

    /// Sends a message on a socket.
    pub fn sendmsg(&self, fd: &impl AsFd, msg: &MsgHdr, flags: SendFlags) -> Result<usize, Errno> {
        self.inner.sendmsg(fd, msg, flags)
    }

    /// Receives bytes from a connected socket.
    pub fn recv(&self, fd: &impl AsFd, buf: &mut [u8]) -> Result<usize, Errno> {
        self.inner.recv(fd, buf)
    }

    /// Receives a message from a socket.
    pub fn recvmsg(
        &self,
        fd: &impl AsFd,
        msg: &mut MsgHdr,
        flags: RecvFlags,
    ) -> Result<usize, Errno> {
        self.inner.recvmsg(fd, msg, flags)
    }

    /// Starts a write from an owned buffer.
    pub fn write_async<B>(&self, fd: &IoFd, buffer: B) -> IoResult<WriteOutput<B>, B>
    where
        B: IoWriteBuffer + 'static,
    {
        IoResult::new(self.inner.write_async(fd, buffer))
    }

    /// Starts a write from an owned buffer to a registered fixed fd.
    pub fn write_registered_fd_async<B>(
        &self,
        fd: &RegisteredFd,
        buffer: B,
    ) -> IoResult<WriteOutput<B>, B>
    where
        B: IoWriteBuffer + 'static,
    {
        IoResult::new(self.inner.write_registered_fd_async(fd, buffer))
    }

    /// Starts a write from a registered fixed buffer to `fd`.
    pub fn write_registered_buffer_async<B: 'static>(
        &self,
        fd: &IoFd,
        buffer: RegisteredBuffer<B>,
    ) -> IoResult<WriteOutput<RegisteredBuffer<B>>, RegisteredBuffer<B>> {
        IoResult::new(self.inner.write_registered_buffer_async(fd, buffer))
    }

    /// Starts a write from a registered fixed buffer to a registered fixed fd.
    pub fn write_registered_fixed_async<B: 'static>(
        &self,
        fd: &RegisteredFd,
        buffer: RegisteredBuffer<B>,
    ) -> IoResult<WriteOutput<RegisteredBuffer<B>>, RegisteredBuffer<B>> {
        IoResult::new(self.inner.write_registered_fixed_async(fd, buffer))
    }

    /// Closes `fd`.
    pub fn close(&self, fd: OwnedFd) -> Result<(), Errno> {
        self.inner.close(fd)
    }

    /// Shuts down part or all of a connected socket.
    pub fn shutdown(&self, fd: &impl AsFd, how: Shutdown) -> Result<(), Errno> {
        self.inner.shutdown(fd, how)
    }

    /// Starts an io_uring timeout and returns a waitable handle.
    pub fn timeout(&self, duration: Duration) -> Timeout {
        Timeout::new(self.inner.timeout(duration))
    }

    /// Returns a snapshot of runtime I/O lifecycle counters.
    pub fn io_counters(&self) -> IoCounters {
        self.inner.io_counters()
    }

    /// Returns a read-only snapshot of stack and context-local pool state.
    pub fn pool_diagnostics(&self) -> PoolDiagnostics {
        let stack = self.inner.pool_diagnostics();
        let shared_sync_buffers = self.shared_sync_buffers.borrow();
        PoolDiagnostics {
            stack,
            shared_sync_read_buffers: shared_sync_buffers.read.len(),
            shared_sync_read_capacity: shared_sync_buffers.read_capacity(),
            shared_sync_write_buffers: shared_sync_buffers.write.len(),
            shared_sync_write_capacity: shared_sync_buffers.write_capacity(),
            max_shared_sync_buffers_per_direction: SHARED_SYNC_BUFFER_CACHE_ENTRIES,
            max_shared_sync_buffer_capacity: SHARED_SYNC_BUFFER_MAX_CAPACITY,
        }
    }

    /// Attempts to cancel a pending [`IoResult`] without consuming its handle.
    pub fn cancel_io<T, B>(&self, io: &IoResult<T, B>) -> Result<(), Errno> {
        if let Some(inner) = io.as_inner() {
            self.inner.cancel_io(inner)
        } else {
            Ok(())
        }
    }

    /// Attempts to cancel a pending internal [`RawIo`] without consuming its handle.
    #[doc(hidden)]
    pub fn cancel_raw_io(&self, io: &RawIo) -> Result<(), Errno> {
        self.inner.cancel_raw_io(io)
    }

    /// Attempts to cancel requests matched by `builder`.
    pub fn cancel_matching(&self, builder: CancelBuilder) -> Result<(), Errno> {
        self.inner.cancel_matching(builder)
    }

    /// Waits on one futex.
    pub fn futex_wait(
        &self,
        futex: &AtomicU32,
        val: u64,
        mask: u64,
        futex_flags: FutexWaitFlags,
    ) -> Result<(), Errno> {
        self.inner.futex_wait(futex, val, mask, futex_flags)
    }

    /// Wakes waiters on one futex.
    pub fn futex_wake(
        &self,
        futex: &AtomicU32,
        count: u64,
        mask: u64,
        futex_flags: FutexWaitFlags,
    ) -> Result<usize, Errno> {
        self.inner.futex_wake(futex, count, mask, futex_flags)
    }

    /// Waits on any futex in `futexes`.
    pub fn futex_waitv(&self, futexes: &[FutexWaitV]) -> Result<usize, Errno> {
        self.inner.futex_waitv(futexes)
    }

    /// Issues a device-specific 80-byte io_uring command.
    ///
    /// # Safety
    ///
    /// The caller must satisfy the target device driver's command requirements.
    pub unsafe fn uring_cmd80(
        &self,
        fd: &impl AsFd,
        cmd_op: u32,
        cmd: [u8; 80],
        buf_index: Option<u16>,
    ) -> Result<u32, Errno> {
        unsafe { self.inner.uring_cmd80(fd, cmd_op, cmd, buf_index) }
    }

    /// Issues a device-specific 80-byte io_uring command to a registered fd.
    ///
    /// # Safety
    ///
    /// The caller must satisfy the target device driver's command requirements.
    pub unsafe fn uring_cmd80_registered_fd(
        &self,
        fd: &RegisteredFd,
        cmd_op: u32,
        cmd: [u8; 80],
        buf_index: Option<u16>,
    ) -> Result<u32, Errno> {
        unsafe {
            self.inner
                .uring_cmd80_registered_fd(fd, cmd_op, cmd, buf_index)
        }
    }
}

/// Explicit ring ownership/sharing mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RingMode {
    /// Operations may only be submitted from the worker that created the ring.
    WorkerLocal,
    /// Operations may be submitted through cloned handles from any worker.
    Shared,
}

/// Error returned by explicit ring-handle operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RingError {
    /// A worker-local ring was requested from a context that is not running on a
    /// worker.
    NoCurrentWorker,
    /// A worker-local ring was used from a different worker.
    WrongWorker {
        /// Worker that owns the ring.
        owner: WorkerId,
        /// Worker that attempted the operation.
        current: WorkerId,
    },
    /// A ring was used from a different runtime instance.
    WrongRuntime,
    /// A pending shared-ring operation was waited from a context that cannot
    /// register and park a stackful coroutine.
    NoStackfulContext,
    /// The runtime or ring queue is at its configured capacity.
    QueueFull,
    /// A runtime-level shared ring resource limit has been reached.
    ResourceLimit,
    /// The requested duration cannot be represented by this platform's clock.
    DurationOutOfRange,
    /// A descriptor close was requested while other handles still reference it.
    FdInUse,
    /// The shared ring was closed before the operation completed.
    Closed,
    /// The operation was canceled before completion.
    Canceled,
    /// The underlying io_uring operation failed.
    Io(Errno),
}

impl From<Errno> for RingError {
    fn from(error: Errno) -> Self {
        Self::Io(error)
    }
}

impl fmt::Display for RingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCurrentWorker => {
                f.write_str("worker-local ring requested outside a worker context")
            }
            Self::WrongWorker { owner, current } => write!(
                f,
                "ring is owned by worker {} but was used from worker {}",
                owner.index(),
                current.index()
            ),
            Self::WrongRuntime => f.write_str("ring belongs to a different runtime"),
            Self::NoStackfulContext => {
                f.write_str("pending shared-ring operation requires a stackful wait context")
            }
            Self::QueueFull => f.write_str("ring queue is full"),
            Self::ResourceLimit => f.write_str("shared ring resource limit reached"),
            Self::DurationOutOfRange => f.write_str("duration is outside the supported range"),
            Self::FdInUse => f.write_str("descriptor still has cloned or pending handles"),
            Self::Closed => f.write_str("ring is closed"),
            Self::Canceled => f.write_str("ring operation was canceled"),
            Self::Io(error) => write!(f, "io_uring operation failed: {error}"),
        }
    }
}

impl Error for RingError {}

/// Explicit handle for submitting I/O through the stealing runtime.
#[derive(Clone)]
pub struct Ring {
    inner: RingInner,
}

/// Socket/file descriptor handle used by [`Ring`] socket operations.
///
/// The handle clones by incrementing a userspace reference count, not by calling
/// `dup(2)`. Pending shared-ring operations clone it so the descriptor remains
/// open until the worker-owned io_uring completion is reaped.
#[derive(Clone)]
pub struct RingFd {
    fd: Arc<OwnedFd>,
}

impl RingFd {
    /// Takes ownership of `fd` for ring socket I/O.
    pub fn from_owned(fd: OwnedFd) -> Self {
        Self { fd: Arc::new(fd) }
    }

    /// Converts back into the owned fd when no clones or pending operations exist.
    pub fn into_owned(self) -> Result<OwnedFd, Self> {
        match Arc::try_unwrap(self.fd) {
            Ok(fd) => Ok(fd),
            Err(fd) => Err(Self { fd }),
        }
    }
}

impl AsFd for RingFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl fmt::Debug for RingFd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RingFd").finish_non_exhaustive()
    }
}

/// A pending direct io_uring operation submitted through [`RuntimeContext`].
///
/// This is a thin stack-steal wrapper around the embedded stack runtime's
/// [`kimojio_stack::IoResult`]. It preserves the same buffer ownership,
/// readiness, and cancellation behavior while accepting stack-steal
/// [`RuntimeContext`] values in [`IoResult::get`].
#[must_use = "pending IO should be completed with get, try_get, cancel, or detach"]
pub struct IoResult<T, B = Vec<u8>> {
    inner: Option<kimojio_stack::IoResult<T, B>>,
}

impl<T, B> IoResult<T, B> {
    fn new(inner: kimojio_stack::IoResult<T, B>) -> Self {
        Self { inner: Some(inner) }
    }

    /// Returns the completed result if the operation is ready.
    pub fn try_get(&mut self) -> Option<Result<T, Errno>> {
        let result = self.inner.as_mut()?.try_get()?;
        self.inner = None;
        Some(result)
    }

    /// Waits for the operation to complete.
    pub fn get(mut self, cx: &RuntimeContext<'_>) -> Result<T, Errno> {
        self.inner
            .take()
            .expect("IoResult value already taken")
            .get(cx.inner)
    }

    /// Requests cancellation of this operation and detaches this result handle.
    pub fn cancel(&mut self) {
        if let Some(mut inner) = self.inner.take() {
            inner.cancel();
        }
    }

    /// Detaches this operation without requesting cancellation.
    pub fn detach(mut self) {
        if let Some(inner) = self.inner.take() {
            inner.detach();
        }
    }

    fn as_inner(&self) -> Option<&kimojio_stack::IoResult<T, B>> {
        self.inner.as_ref()
    }
}

impl<T, B> Waitable for IoResult<T, B> {
    fn is_ready(&self) -> bool {
        self.inner
            .as_ref()
            .is_none_or(kimojio_stack::Waitable::is_ready)
    }

    fn add_waiter(&self, cx: &kimojio_stack::RuntimeContext<'_>, registration: &WaitRegistration) {
        if let Some(inner) = &self.inner {
            inner.add_waiter(cx, registration);
        }
    }
}

impl<T, B> RuntimeWaitable for IoResult<T, B> {
    fn is_ready(&self) -> bool {
        Waitable::is_ready(self)
    }

    fn add_stackful_waiter(&self, waiter: StackfulWaiterHandle) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|inner| inner.add_stackful_waiter(waiter))
    }
}

impl<T, B> fmt::Debug for IoResult<T, B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IoResult")
            .field("ready", &Waitable::is_ready(self))
            .finish()
    }
}

/// A waitable direct io_uring timeout submitted through [`RuntimeContext`].
#[must_use = "timeouts should be waited on, canceled, detached, or allowed to complete"]
pub struct Timeout {
    inner: Option<kimojio_stack::Timeout>,
}

impl Timeout {
    fn new(inner: kimojio_stack::Timeout) -> Self {
        Self { inner: Some(inner) }
    }

    /// Returns the completed timeout result if it is already ready.
    pub fn try_wait(&mut self) -> Option<Result<(), Errno>> {
        let result = self.inner.as_mut()?.try_wait()?;
        self.inner = None;
        Some(result)
    }

    /// Waits until the timeout completes.
    pub fn wait(mut self, cx: &RuntimeContext<'_>) -> Result<(), Errno> {
        self.inner
            .take()
            .expect("Timeout value already taken")
            .wait(cx.inner)
    }

    /// Updates the timeout duration.
    pub fn update(&self, cx: &RuntimeContext<'_>, duration: Duration) -> Result<(), Errno> {
        if let Some(inner) = &self.inner {
            inner.update(cx.inner, duration)
        } else {
            Ok(())
        }
    }

    /// Requests cancellation of this timeout without waiting for completion.
    pub fn cancel(&mut self) {
        if let Some(mut inner) = self.inner.take() {
            inner.cancel();
        }
    }

    /// Detaches this timeout without requesting cancellation.
    pub fn detach(mut self) {
        if let Some(inner) = self.inner.take() {
            inner.detach();
        }
    }

    /// Requests cancellation and waits for the cancellation CQE.
    pub fn cancel_and_wait(mut self, cx: &RuntimeContext<'_>) -> Result<(), Errno> {
        if let Some(inner) = self.inner.take() {
            inner.cancel_and_wait(cx.inner)
        } else {
            Ok(())
        }
    }
}

impl Waitable for Timeout {
    fn is_ready(&self) -> bool {
        self.inner
            .as_ref()
            .is_none_or(kimojio_stack::Waitable::is_ready)
    }

    fn add_waiter(&self, cx: &kimojio_stack::RuntimeContext<'_>, registration: &WaitRegistration) {
        if let Some(inner) = &self.inner {
            inner.add_waiter(cx, registration);
        }
    }
}

impl RuntimeWaitable for Timeout {
    fn is_ready(&self) -> bool {
        Waitable::is_ready(self)
    }

    fn add_stackful_waiter(&self, waiter: StackfulWaiterHandle) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|inner| inner.add_stackful_waiter(waiter))
    }
}

impl fmt::Debug for Timeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Timeout")
            .field("ready", &Waitable::is_ready(self))
            .finish()
    }
}

#[derive(Clone)]
enum RingInner {
    WorkerLocal {
        runtime_id: RuntimeId,
        owner: WorkerId,
    },
    Shared(SharedRing),
}

impl Ring {
    /// Returns this ring's ownership/sharing mode.
    pub fn mode(&self) -> RingMode {
        match &self.inner {
            RingInner::WorkerLocal { .. } => RingMode::WorkerLocal,
            RingInner::Shared(_) => RingMode::Shared,
        }
    }

    /// Returns the owning worker for worker-local rings.
    pub fn owner(&self) -> Option<WorkerId> {
        match &self.inner {
            RingInner::WorkerLocal { owner, .. } => Some(*owner),
            RingInner::Shared(_) => None,
        }
    }

    /// Submits an io_uring no-op and waits for completion.
    pub fn nop(&self, cx: &RuntimeContext<'_>) -> Result<(), RingError> {
        match &self.inner {
            RingInner::WorkerLocal { runtime_id, owner } => {
                ensure_owner(*runtime_id, *owner, cx)?;
                cx.inner.nop().map_err(RingError::from)
            }
            RingInner::Shared(shared) => shared.submit_nop(cx)?.wait(cx),
        }
    }

    /// Starts a timeout operation and returns a waitable handle.
    pub fn timeout(
        &self,
        cx: &RuntimeContext<'_>,
        duration: Duration,
    ) -> Result<RingTimeout, RingError> {
        Ok(match &self.inner {
            RingInner::WorkerLocal { runtime_id, owner } => {
                ensure_owner(*runtime_id, *owner, cx)?;
                RingTimeout::local(cx.inner.timeout(duration))
            }
            RingInner::Shared(shared) => shared.submit_timeout(cx, duration)?,
        })
    }

    /// Waits until `duration` has elapsed.
    pub fn sleep(&self, cx: &RuntimeContext<'_>, duration: Duration) -> Result<(), RingError> {
        self.timeout(cx, duration)?.wait(cx)
    }

    /// Reads from `fd` into `buf`.
    pub fn read(
        &self,
        cx: &RuntimeContext<'_>,
        fd: &RingFd,
        buf: &mut [u8],
    ) -> Result<usize, RingError> {
        match &self.inner {
            RingInner::WorkerLocal { runtime_id, owner } => {
                ensure_owner(*runtime_id, *owner, cx)?;
                cx.inner.read(fd, buf).map_err(RingError::from)
            }
            RingInner::Shared(_) => {
                let buffer = cx.acquire_shared_sync_read_buffer(buf.len());
                let read = self.read_async(cx, fd, buffer)?.get(cx)?;
                let bytes = read.bytes;
                buf[..bytes].copy_from_slice(&read.buffer[..bytes]);
                cx.recycle_shared_sync_read_buffer(read.buffer);
                Ok(bytes)
            }
        }
    }

    /// Writes `buf` to `fd`.
    pub fn write(
        &self,
        cx: &RuntimeContext<'_>,
        fd: &RingFd,
        buf: &[u8],
    ) -> Result<usize, RingError> {
        match &self.inner {
            RingInner::WorkerLocal { runtime_id, owner } => {
                ensure_owner(*runtime_id, *owner, cx)?;
                cx.inner.write(fd, buf).map_err(RingError::from)
            }
            RingInner::Shared(_) => {
                let buffer = cx.acquire_shared_sync_write_buffer(buf);
                let output = self.write_async(cx, fd, buffer)?.get(cx)?;
                let bytes = output.bytes;
                cx.recycle_shared_sync_write_buffer(output.buffer);
                Ok(bytes)
            }
        }
    }

    /// Starts a read into an owned buffer.
    pub fn read_async<B>(
        &self,
        cx: &RuntimeContext<'_>,
        fd: &RingFd,
        mut buffer: B,
    ) -> Result<RingReadResult<B>, RingError>
    where
        B: IoReadBuffer + Send + 'static,
    {
        Ok(match &self.inner {
            RingInner::WorkerLocal { runtime_id, owner } => {
                ensure_owner(*runtime_id, *owner, cx)?;
                let len = buffer.io_buffer_len();
                let ptr = buffer.io_buffer_mut_ptr();
                let io = unsafe { cx.inner.submit_raw_read(fd, ptr, len) };
                RingIoResult::local(io, fd.clone(), buffer, read_output::<B>)
            }
            RingInner::Shared(shared) => shared.submit_read(cx, fd.clone(), buffer)?,
        })
    }

    /// Starts a write from an owned buffer.
    pub fn write_async<B>(
        &self,
        cx: &RuntimeContext<'_>,
        fd: &RingFd,
        buffer: B,
    ) -> Result<RingWriteResult<B>, RingError>
    where
        B: IoWriteBuffer + Send + 'static,
    {
        Ok(match &self.inner {
            RingInner::WorkerLocal { runtime_id, owner } => {
                ensure_owner(*runtime_id, *owner, cx)?;
                let len = buffer.io_buffer_len();
                let ptr = buffer.io_buffer_ptr();
                let io = unsafe { cx.inner.submit_raw_write(fd, ptr, len) };
                RingIoResult::local(io, fd.clone(), buffer, write_output::<B>)
            }
            RingInner::Shared(shared) => shared.submit_write(cx, fd.clone(), buffer)?,
        })
    }

    /// Shuts down part or all of a connected socket.
    pub fn shutdown(
        &self,
        cx: &RuntimeContext<'_>,
        fd: &RingFd,
        how: Shutdown,
    ) -> Result<(), RingError> {
        match &self.inner {
            RingInner::WorkerLocal { runtime_id, owner } => {
                ensure_owner(*runtime_id, *owner, cx)?;
                cx.inner.shutdown(fd, how).map_err(RingError::from)
            }
            RingInner::Shared(shared) => shared.submit_shutdown(cx, fd.clone(), how)?.wait(cx),
        }
    }

    /// Closes `fd` through this ring.
    pub fn close(&self, cx: &RuntimeContext<'_>, fd: RingFd) -> Result<(), RingError> {
        match &self.inner {
            RingInner::WorkerLocal { runtime_id, owner } => {
                ensure_owner(*runtime_id, *owner, cx)?;
                let fd = fd.into_owned().map_err(|_| RingError::FdInUse)?;
                cx.inner.close(fd).map_err(RingError::from)
            }
            RingInner::Shared(shared) => shared.submit_close(cx, fd)?.wait(cx),
        }
    }

    #[cfg(test)]
    fn shared_submitted_operation_count(&self) -> Option<usize> {
        match &self.inner {
            RingInner::Shared(shared) => Some(shared.core.submitted_operation_count()),
            RingInner::WorkerLocal { .. } => None,
        }
    }

    #[cfg(test)]
    fn shared_driver_owner(&self) -> Option<WorkerId> {
        match &self.inner {
            RingInner::Shared(shared) => match &shared.driver {
                SharedRingDriver::Local { .. } => None,
                SharedRingDriver::Multi { owner, .. } => Some(*owner),
            },
            RingInner::WorkerLocal { .. } => None,
        }
    }

    #[cfg(test)]
    fn shared_test_counters(&self) -> Option<SharedRingTestCounterSnapshot> {
        match &self.inner {
            RingInner::Shared(shared) => Some(shared.core.test_counters()),
            RingInner::WorkerLocal { .. } => None,
        }
    }

    #[cfg(test)]
    fn shared_core_ptr(&self) -> Option<*const SharedRingCore> {
        match &self.inner {
            RingInner::Shared(shared) => Some(Arc::as_ptr(&shared.core)),
            RingInner::WorkerLocal { .. } => None,
        }
    }
}

/// Waitable result for an owned-buffer ring read.
pub type RingReadResult<B> = RingIoResult<ReadOutput<B>, B>;

/// Waitable result for an owned-buffer ring write.
pub type RingWriteResult<B> = RingIoResult<WriteOutput<B>, B>;

/// Waitable result for an owned-buffer ring I/O operation.
#[must_use = "ring I/O results should be completed with get, try_get, or cancel"]
pub struct RingIoResult<T, B: 'static> {
    inner: Option<RingIoResultInner<B>>,
    output: fn(B, u32) -> T,
}

enum RingIoResultInner<B: 'static> {
    Local(LocalBufferOperation<B>),
    Shared(SharedBufferOperation<B>),
}

impl<T, B> RingIoResult<T, B>
where
    B: 'static,
{
    fn local(io: RawIo, fd: RingFd, buffer: B, output: fn(B, u32) -> T) -> Self {
        Self {
            inner: Some(RingIoResultInner::Local(LocalBufferOperation {
                io,
                fd: Some(fd),
                buffer: Some(buffer),
            })),
            output,
        }
    }

    fn shared(
        state: Arc<SharedOpState>,
        buffer: Arc<Mutex<Option<B>>>,
        fd: RingFd,
        ring: SharedRing,
        output: fn(B, u32) -> T,
    ) -> Self {
        Self {
            inner: Some(RingIoResultInner::Shared(SharedBufferOperation {
                state,
                buffer,
                fd: Some(fd),
                _ring: ring,
            })),
            output,
        }
    }

    /// Returns the completed result if the operation is ready.
    pub fn try_get(&mut self) -> Option<Result<T, RingError>> {
        let inner = self.inner.as_mut()?;
        let result = match inner {
            RingIoResultInner::Local(operation) => operation.try_get(),
            RingIoResultInner::Shared(operation) => operation.try_get(),
        }?;
        self.inner = None;
        Some(result.map(|(bytes, buffer)| (self.output)(buffer, bytes)))
    }

    /// Waits for the operation to complete.
    pub fn get(mut self, cx: &RuntimeContext<'_>) -> Result<T, RingError> {
        loop {
            if let Some(result) = self.try_get() {
                return result;
            }

            match self.inner.as_ref().expect("ring I/O result missing") {
                RingIoResultInner::Local(_) => {
                    cx.inner
                        .wait_all(&[&self as &dyn Waitable], None)
                        .expect("wait_all with non-empty waitable list cannot fail");
                }
                RingIoResultInner::Shared(operation) => {
                    wait_until_shared_state_ready(&operation.state, cx)?;
                }
            }
        }
    }

    /// Requests cancellation and detaches this result handle.
    ///
    /// After direct cancellation, this handle is detached and should not be used
    /// to drain a completion result. Runtime-neutral code that needs a drainable
    /// canceled read/write result should call `SocketIoRuntime::cancel_read` or
    /// `SocketIoRuntime::cancel_write` on the runtime context instead.
    pub fn cancel(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        match inner {
            RingIoResultInner::Local(mut operation) => operation.cancel(),
            RingIoResultInner::Shared(operation) => operation.cancel(),
        }
    }

    fn request_cancel(&mut self, cx: &RuntimeContext<'_>) -> Result<(), RingError> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(());
        };
        match inner {
            RingIoResultInner::Local(operation) => operation.request_cancel(cx),
            RingIoResultInner::Shared(operation) => operation.request_cancel(cx),
        }
    }
}

impl<T, B> Waitable for RingIoResult<T, B>
where
    B: 'static,
{
    fn is_ready(&self) -> bool {
        match self.inner.as_ref() {
            None => true,
            Some(RingIoResultInner::Local(operation)) => Waitable::is_ready(&operation.io),
            Some(RingIoResultInner::Shared(operation)) => operation.state.is_terminal(),
        }
    }

    fn add_waiter(&self, cx: &kimojio_stack::RuntimeContext<'_>, registration: &WaitRegistration) {
        match self.inner.as_ref() {
            None => {}
            Some(RingIoResultInner::Local(operation)) => operation.io.add_waiter(cx, registration),
            Some(RingIoResultInner::Shared(operation)) => {
                Waitable::add_waiter(&*operation.state, cx, registration);
            }
        }
    }
}

impl<T, B> RuntimeWaitable for RingIoResult<T, B>
where
    B: 'static,
{
    fn is_ready(&self) -> bool {
        Waitable::is_ready(self)
    }

    fn add_stackful_waiter(&self, waiter: StackfulWaiterHandle) -> bool {
        match self.inner.as_ref() {
            None => false,
            Some(RingIoResultInner::Local(operation)) => operation.io.add_stackful_waiter(waiter),
            Some(RingIoResultInner::Shared(operation)) => operation.state.add_waiter(waiter),
        }
    }
}

impl<T, B> fmt::Debug for RingIoResult<T, B>
where
    B: 'static,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RingIoResult")
            .field("ready", &Waitable::is_ready(self))
            .finish()
    }
}

impl<T, B> Drop for RingIoResult<T, B>
where
    B: 'static,
{
    fn drop(&mut self) {
        self.cancel();
    }
}

struct LocalBufferOperation<B> {
    io: RawIo,
    fd: Option<RingFd>,
    buffer: Option<B>,
}

impl<B> LocalBufferOperation<B>
where
    B: 'static,
{
    fn try_get(&mut self) -> Option<Result<(u32, B), RingError>> {
        let result = self.io.try_wait()?.map_err(RingError::from);
        match result {
            Ok(bytes) => {
                let buffer = self
                    .buffer
                    .take()
                    .expect("local ring I/O completed without buffer");
                Some(Ok((bytes, buffer)))
            }
            Err(error) => {
                drop(self.buffer.take());
                Some(Err(error))
            }
        }
    }

    fn cancel(&mut self) {
        match (self.fd.take(), self.buffer.take()) {
            (Some(fd), Some(buffer)) => self.io.cancel_with_payload((fd, buffer)),
            (fd, buffer) => {
                drop(fd);
                drop(buffer);
                self.io.cancel();
            }
        }
    }

    fn request_cancel(&mut self, cx: &RuntimeContext<'_>) -> Result<(), RingError> {
        if !Waitable::is_ready(&self.io) {
            cx.inner.cancel_raw_io(&self.io).map_err(RingError::from)?;
        }
        Ok(())
    }
}

struct SharedBufferOperation<B> {
    state: Arc<SharedOpState>,
    buffer: Arc<Mutex<Option<B>>>,
    fd: Option<RingFd>,
    _ring: SharedRing,
}

impl<B> SharedBufferOperation<B> {
    fn try_get(&self) -> Option<Result<(u32, B), RingError>> {
        let result = self.state.try_take()?;
        match result {
            Ok(bytes) => {
                let buffer = self
                    .buffer
                    .lock()
                    .expect("shared ring I/O buffer mutex poisoned")
                    .take()
                    .expect("shared ring I/O completed without buffer");
                Some(Ok((bytes, buffer)))
            }
            Err(error) => {
                drop(
                    self.buffer
                        .lock()
                        .expect("shared ring I/O buffer mutex poisoned")
                        .take(),
                );
                Some(Err(error))
            }
        }
    }

    fn cancel(&self) {
        self.state.cancel();
        drop(
            self.buffer
                .lock()
                .expect("shared ring I/O buffer mutex poisoned")
                .take(),
        );
    }

    fn request_cancel(&mut self, _cx: &RuntimeContext<'_>) -> Result<(), RingError> {
        self.state.cancel();
        self.fd = None;
        Ok(())
    }
}

fn read_output<B>(buffer: B, bytes: u32) -> ReadOutput<B> {
    ReadOutput {
        bytes: bytes as usize,
        buffer,
    }
}

fn write_output<B>(buffer: B, bytes: u32) -> WriteOutput<B> {
    WriteOutput {
        bytes: bytes as usize,
        buffer,
    }
}

impl fmt::Debug for Ring {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ring")
            .field("mode", &self.mode())
            .field("owner", &self.owner())
            .finish_non_exhaustive()
    }
}

fn ensure_owner(
    runtime_id: RuntimeId,
    owner: WorkerId,
    cx: &RuntimeContext<'_>,
) -> Result<(), RingError> {
    if runtime_id != cx.runtime_id {
        return Err(RingError::WrongRuntime);
    }
    if owner != cx.worker_id() {
        return Err(RingError::WrongWorker {
            owner,
            current: cx.worker_id(),
        });
    }
    Ok(())
}

/// A waitable timeout submitted through an explicit [`Ring`].
#[must_use = "ring timeouts should be waited on or canceled"]
pub struct RingTimeout {
    inner: Option<RingTimeoutInner>,
}

enum RingTimeoutInner {
    Local(kimojio_stack::Timeout),
    SharedLocalDriver(LocalSharedOperationHandle),
    Shared(Arc<SharedOpState>),
}

impl RingTimeout {
    fn local(timeout: kimojio_stack::Timeout) -> Self {
        Self {
            inner: Some(RingTimeoutInner::Local(timeout)),
        }
    }

    fn shared(state: Arc<SharedOpState>) -> Self {
        Self {
            inner: Some(RingTimeoutInner::Shared(state)),
        }
    }

    fn shared_local_driver(operation: LocalSharedOperationHandle) -> Self {
        Self {
            inner: Some(RingTimeoutInner::SharedLocalDriver(operation)),
        }
    }

    /// Returns the completed timeout result if it is already ready.
    pub fn try_wait(&mut self) -> Option<Result<(), RingError>> {
        match self.inner.as_mut()? {
            RingTimeoutInner::Local(timeout) => {
                let result = timeout.try_wait()?.map_err(RingError::from);
                self.inner = None;
                Some(result)
            }
            RingTimeoutInner::SharedLocalDriver(operation) => {
                if let Some(result) = operation.try_wait() {
                    self.inner = None;
                    return Some(result);
                }
                None
            }
            RingTimeoutInner::Shared(state) => {
                let result = state.try_take()?;
                self.inner = None;
                Some(result.map(|_| ()))
            }
        }
    }

    /// Waits until the timeout completes.
    pub fn wait(mut self, cx: &RuntimeContext<'_>) -> Result<(), RingError> {
        let Some(inner) = self.inner.take() else {
            return Ok(());
        };
        match inner {
            RingTimeoutInner::Local(timeout) => timeout.wait(cx.inner).map_err(RingError::from),
            RingTimeoutInner::SharedLocalDriver(operation) => operation.wait(cx),
            RingTimeoutInner::Shared(state) => {
                let mut cancel_on_unwind = SharedWaitCancelGuard::new(&state);
                let result = wait_shared_operation(state, cx);
                cancel_on_unwind.disarm();
                result
            }
        }
    }

    /// Requests cancellation of this timeout without waiting for completion.
    pub fn cancel(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        match inner {
            RingTimeoutInner::Local(mut timeout) => timeout.cancel(),
            RingTimeoutInner::SharedLocalDriver(operation) => operation.cancel(),
            RingTimeoutInner::Shared(state) => {
                state.cancel();
            }
        }
    }
}

impl RuntimeWaitable for RingTimeout {
    fn is_ready(&self) -> bool {
        match self.inner.as_ref() {
            None => true,
            Some(RingTimeoutInner::Local(timeout)) => RuntimeWaitable::is_ready(timeout),
            Some(RingTimeoutInner::SharedLocalDriver(operation)) => operation.is_wait_ready(),
            Some(RingTimeoutInner::Shared(state)) => state.is_terminal(),
        }
    }

    fn add_stackful_waiter(&self, waiter: StackfulWaiterHandle) -> bool {
        match self.inner.as_ref() {
            None => false,
            Some(RingTimeoutInner::Local(timeout)) => timeout.add_stackful_waiter(waiter),
            Some(RingTimeoutInner::SharedLocalDriver(operation)) => {
                operation.add_stackful_waiter(waiter)
            }
            Some(RingTimeoutInner::Shared(state)) => state.add_waiter(waiter),
        }
    }
}

#[derive(Default)]
struct LocalSharedOperations {
    operations: RefCell<Vec<LocalSharedOperationHandle>>,
}

impl LocalSharedOperations {
    fn register(&self, operation: LocalSharedOperationHandle) {
        let mut operations = self.operations.borrow_mut();
        operations.retain(|operation| !operation.is_retired());
        operations.push(operation);
    }

    fn close_all(&self) {
        for operation in self.operations.borrow_mut().drain(..) {
            operation.close();
        }
    }
}

#[derive(Clone)]
struct LocalSharedOperationHandle {
    state: Arc<SharedOpState>,
    io: Rc<RefCell<Option<ActiveUnitIo>>>,
    fd: Rc<RefCell<Option<RingFd>>>,
}

impl LocalSharedOperationHandle {
    fn new(state: Arc<SharedOpState>, io: ActiveUnitIo, fd: Option<RingFd>) -> Self {
        Self {
            state,
            io: Rc::new(RefCell::new(Some(io))),
            fd: Rc::new(RefCell::new(fd)),
        }
    }

    fn try_wait(&self) -> Option<Result<(), RingError>> {
        if let Some(result) = self.state.try_take() {
            self.cancel_io();
            return Some(result.map(|_| ()));
        }

        let mut io_slot = self.io.borrow_mut();
        let io = io_slot.as_mut()?;
        if let Some(result) = io.try_wait() {
            self.state
                .complete(result.map(|_| 0).map_err(RingError::from));
            *io_slot = None;
            *self.fd.borrow_mut() = None;
            return Some(
                self.state
                    .try_take()
                    .expect("local shared operation completed without result")
                    .map(|_| ()),
            );
        }
        None
    }

    fn wait(&self, cx: &RuntimeContext<'_>) -> Result<(), RingError> {
        let mut cancel_on_unwind = LocalSharedWaitCancelGuard::new(self.clone());
        loop {
            if let Some(result) = self.try_wait() {
                cancel_on_unwind.disarm();
                return result;
            }

            let io = self.io.borrow();
            if let Some(io) = io.as_ref() {
                let waitables: [&dyn Waitable; 2] = [io, &*self.state];
                cx.inner
                    .wait_any(&waitables, None)
                    .expect("wait_any with non-empty waitable list cannot fail");
            } else {
                cx.inner.yield_now();
            }
        }
    }

    fn is_wait_ready(&self) -> bool {
        self.state.is_terminal() || self.io.borrow().as_ref().is_some_and(Waitable::is_ready)
    }

    fn add_stackful_waiter(&self, waiter: StackfulWaiterHandle) -> bool {
        if self.state.is_terminal() {
            return false;
        }

        if let Some(io) = self.io.borrow().as_ref() {
            RuntimeWaitable::add_stackful_waiter(io, waiter)
        } else {
            self.state.add_waiter(waiter)
        }
    }

    fn cancel(&self) {
        self.state.cancel();
        self.cancel_io();
    }

    fn close(&self) {
        self.state.close();
        self.cancel_io();
    }

    fn is_retired(&self) -> bool {
        self.state.is_terminal() && self.io.borrow().is_none() && self.fd.borrow().is_none()
    }

    fn cancel_io(&self) {
        let fd = self.fd.borrow_mut().take();
        if let Some(mut io) = self.io.borrow_mut().take() {
            if let Some(fd) = fd {
                io.cancel_with_payload(fd);
            } else {
                io.cancel();
            }
        } else {
            drop(fd);
        }
    }
}

enum ActiveUnitIo {
    NoPayload(NoPayloadIo),
    Raw(RawIo),
}

impl ActiveUnitIo {
    fn try_wait(&mut self) -> Option<Result<u32, Errno>> {
        match self {
            Self::NoPayload(io) => io.try_wait().map(|result| result.map(|_| 0)),
            Self::Raw(io) => io.try_wait(),
        }
    }

    fn cancel(&mut self) {
        match self {
            Self::NoPayload(io) => io.cancel(),
            Self::Raw(io) => io.cancel(),
        }
    }

    fn cancel_with_payload<B>(&mut self, payload: B)
    where
        B: 'static,
    {
        match self {
            Self::NoPayload(io) => {
                drop(payload);
                io.cancel();
            }
            Self::Raw(io) => io.cancel_with_payload(payload),
        }
    }
}

impl Waitable for ActiveUnitIo {
    fn is_ready(&self) -> bool {
        match self {
            Self::NoPayload(io) => Waitable::is_ready(io),
            Self::Raw(io) => Waitable::is_ready(io),
        }
    }

    fn add_waiter(&self, cx: &kimojio_stack::RuntimeContext<'_>, registration: &WaitRegistration) {
        match self {
            Self::NoPayload(io) => io.add_waiter(cx, registration),
            Self::Raw(io) => io.add_waiter(cx, registration),
        }
    }
}

impl RuntimeWaitable for ActiveUnitIo {
    fn is_ready(&self) -> bool {
        Waitable::is_ready(self)
    }

    fn add_stackful_waiter(&self, waiter: StackfulWaiterHandle) -> bool {
        match self {
            Self::NoPayload(io) => RuntimeWaitable::add_stackful_waiter(io, waiter),
            Self::Raw(io) => RuntimeWaitable::add_stackful_waiter(io, waiter),
        }
    }
}

struct LocalSharedWaitCancelGuard {
    operation: Option<LocalSharedOperationHandle>,
}

impl LocalSharedWaitCancelGuard {
    fn new(operation: LocalSharedOperationHandle) -> Self {
        Self {
            operation: Some(operation),
        }
    }

    fn disarm(&mut self) {
        self.operation = None;
    }
}

impl Drop for LocalSharedWaitCancelGuard {
    fn drop(&mut self) {
        if let Some(operation) = self.operation.take() {
            operation.cancel();
        }
    }
}

struct SharedWaitCancelGuard {
    state: Option<Arc<SharedOpState>>,
}

impl SharedWaitCancelGuard {
    fn new(state: &Arc<SharedOpState>) -> Self {
        Self {
            state: Some(Arc::clone(state)),
        }
    }

    fn disarm(&mut self) {
        self.state = None;
    }
}

impl Drop for SharedWaitCancelGuard {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            state.cancel();
        }
    }
}

impl fmt::Debug for RingTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RingTimeout")
            .field("pending", &self.inner.is_some())
            .finish()
    }
}

impl Drop for RingTimeout {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn wait_shared_operation(
    state: Arc<SharedOpState>,
    cx: &RuntimeContext<'_>,
) -> Result<(), RingError> {
    wait_shared_operation_value(state, cx).map(|_| ())
}

fn wait_shared_operation_value(
    state: Arc<SharedOpState>,
    cx: &RuntimeContext<'_>,
) -> Result<u32, RingError> {
    loop {
        if let Some(result) = state.try_take() {
            return result;
        }

        wait_until_shared_state_ready(&state, cx)?;
    }
}

fn wait_until_shared_state_ready(
    state: &Arc<SharedOpState>,
    cx: &RuntimeContext<'_>,
) -> Result<(), RingError> {
    if let Some(registration) = cx.stackful_wait_registration() {
        if state.add_waiter(registration.waiter()) {
            cx.park_stackful();
        }
        Ok(())
    } else {
        Err(RingError::NoStackfulContext)
    }
}

impl RuntimeCapabilities for RuntimeContext<'_> {
    fn runtime_family(&self) -> RuntimeFamily {
        RuntimeFamily::Other("kimojio-stack-steal")
    }

    fn supports(&self, capability: RuntimeCapability) -> bool {
        matches!(
            capability,
            RuntimeCapability::StackfulWait
                | RuntimeCapability::ExplicitRingIo
                | RuntimeCapability::SocketIo
        )
    }

    fn require_capability(
        &self,
        capability: RuntimeCapability,
    ) -> Result<(), UnsupportedCapability> {
        if self.supports(capability) {
            Ok(())
        } else {
            Err(UnsupportedCapability::new(
                match capability {
                    RuntimeCapability::StackfulWait => "stackful-wait",
                    RuntimeCapability::ExplicitRingIo => "explicit-ring-io",
                    RuntimeCapability::SocketIo => "socket-io",
                },
                self.runtime_family(),
            ))
        }
    }
}

impl StackfulWaitContext for RuntimeContext<'_> {
    fn stackful_wait_registration(&self) -> Option<Box<dyn StackfulWaitRegistration + '_>> {
        let registration = self.inner.stackful_wait_registration()?;
        if let Some(scope) = &self.active_steal_scope {
            if !scope.push_cancel_waiter(registration.waiter()) {
                let immediate_wake = registration.waiter();
                if immediate_wake.mark_ready() {
                    immediate_wake.wake_ready();
                }
            }
            Some(Box::new(StealWaitRegistration {
                inner: registration,
            }))
        } else {
            Some(registration)
        }
    }

    fn park_stackful(&self) {
        self.inner.park_stackful();
        if self
            .active_steal_scope
            .as_ref()
            .is_some_and(|scope| scope.is_cancel_requested())
        {
            panic::panic_any(StealTaskCanceled);
        }
    }
}

struct StealWaitRegistration<'registration> {
    inner: Box<dyn StackfulWaitRegistration + 'registration>,
}

impl StackfulWaitRegistration for StealWaitRegistration<'_> {
    fn waiter(&self) -> StackfulWaiterHandle {
        self.inner.waiter()
    }
}

impl fmt::Debug for RuntimeContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuntimeContext")
            .field("worker", &self.worker)
            .finish_non_exhaustive()
    }
}

/// A structured concurrency scope for stackful coroutines.
pub struct Scope<'scope, 'env: 'scope> {
    inner: &'scope kimojio_stack::Scope<'scope, 'env>,
    runtime_id: RuntimeId,
    worker: WorkerId,
    steal_runtime: Option<Arc<MultiRuntime>>,
    active_steal_scope: Option<Arc<StealScopeState>>,
    tenant: TenantId,
    tenant_allocator: Arc<AtomicU64>,
    local_shared_operations: Option<Rc<LocalSharedOperations>>,
    steal_scope: Option<NonNull<RefCell<Option<Arc<StealScopeState>>>>>,
    shared_ring_queue_capacity: usize,
}

impl<'scope, 'env: 'scope> Scope<'scope, 'env> {
    /// Returns the tenant identifier associated with the context that created this scope.
    pub fn tenant_id(&self) -> TenantId {
        self.tenant
    }

    /// Spawns local stackful work in this scope.
    ///
    /// Local work is not eligible for cross-worker stealing and may capture
    /// non-`Send` values.
    pub fn spawn<F, T>(&self, f: F) -> JoinHandle<'scope, T>
    where
        F: FnOnce(&RuntimeContext<'_>) -> T + 'scope,
        T: 'scope,
    {
        self.spawn_local(f)
    }

    /// Spawns local stackful work in this scope.
    pub fn spawn_local<F, T>(&self, f: F) -> JoinHandle<'scope, T>
    where
        F: FnOnce(&RuntimeContext<'_>) -> T + 'scope,
        T: 'scope,
    {
        self.spawn_local_with_tenant(self.new_tenant_id(), f)
    }

    /// Spawns local stackful work associated with `tenant`.
    ///
    /// Tenant identifiers on local work are attribution and propagation metadata;
    /// local work still runs on the embedded single-threaded stack scheduler.
    pub fn spawn_local_with_tenant<F, T>(&self, tenant: TenantId, f: F) -> JoinHandle<'scope, T>
    where
        F: FnOnce(&RuntimeContext<'_>) -> T + 'scope,
        T: 'scope,
    {
        JoinHandle {
            inner: JoinInner::Local(self.inner.spawn({
                let runtime_id = self.runtime_id;
                let worker = self.worker;
                let steal_runtime = self.steal_runtime.clone();
                let active_steal_scope = self.active_steal_scope.clone();
                let tenant_allocator = Arc::clone(&self.tenant_allocator);
                let local_shared_operations = self.local_shared_operations.clone();
                let shared_ring_queue_capacity = self.shared_ring_queue_capacity;
                move |inner| {
                    let cx = RuntimeContext {
                        inner,
                        runtime_id,
                        worker,
                        steal_runtime,
                        active_steal_scope,
                        tenant,
                        tenant_allocator,
                        local_shared_operations,
                        shared_ring_queue_capacity,
                        socket_ring: RefCell::new(None),
                        shared_sync_buffers: RefCell::new(SharedSyncBufferCache::default()),
                    };
                    f(&cx)
                }
            })),
        }
    }

    /// Spawns local stackful work pinned to the current worker.
    pub fn spawn_pinned<F, T>(&self, f: F) -> JoinHandle<'scope, T>
    where
        F: FnOnce(&RuntimeContext<'_>) -> T + 'scope,
        T: 'scope,
    {
        self.spawn_local(f)
    }

    /// Spawns local work pinned to the current worker and associated with `tenant`.
    pub fn spawn_pinned_with_tenant<F, T>(&self, tenant: TenantId, f: F) -> JoinHandle<'scope, T>
    where
        F: FnOnce(&RuntimeContext<'_>) -> T + 'scope,
        T: 'scope,
    {
        self.spawn_local_with_tenant(tenant, f)
    }

    /// Spawns work that satisfies the stealing eligibility boundary.
    ///
    /// Stealable work may execute on another worker and must therefore be
    /// `Send + 'static`. Work that captures borrowed or non-`Send` state should
    /// use [`Scope::spawn_local`] or [`Scope::spawn_pinned`].
    ///
    /// # Queue saturation
    ///
    /// This first API reports worker-pool shutdown or queue saturation through
    /// the returned [`JoinHandle`]: joining the handle resumes a panic payload
    /// describing the rejection. Use `Runtime::metrics().rejected_tasks` after
    /// `block_on` to inspect rejected submissions. A future fallible spawn API
    /// should expose this as recoverable backpressure instead of panic payloads.
    pub fn spawn_stealable<F, T>(&self, f: F) -> JoinHandle<'scope, T>
    where
        F: FnOnce(&RuntimeContext<'_>) -> T + Send + 'static,
        T: Send + 'static,
    {
        self.spawn_stealable_with_tenant(self.new_tenant_id(), f)
    }

    /// Spawns stealable work associated with `tenant`.
    pub fn spawn_stealable_with_tenant<F, T>(&self, tenant: TenantId, f: F) -> JoinHandle<'scope, T>
    where
        F: FnOnce(&RuntimeContext<'_>) -> T + Send + 'static,
        T: Send + 'static,
    {
        match self.try_spawn_stealable_with_tenant(tenant, f) {
            Ok(handle) => handle,
            Err(error) => {
                let join = Arc::new(StealJoinState::new());
                let message = match error {
                    SpawnError::ShuttingDown => {
                        "stealable task rejected because runtime is shutting down"
                    }
                    SpawnError::QueueFull => {
                        "stealable task rejected because runtime queue is full"
                    }
                };
                join.complete(StealOutcome::Panicked(Box::new(message)));
                JoinHandle {
                    inner: JoinInner::Stealable(join, std::marker::PhantomData),
                }
            }
        }
    }

    /// Attempts to spawn stealable work and reports queue saturation synchronously.
    pub fn try_spawn_stealable<F, T>(&self, f: F) -> Result<JoinHandle<'scope, T>, SpawnError>
    where
        F: FnOnce(&RuntimeContext<'_>) -> T + Send + 'static,
        T: Send + 'static,
    {
        self.try_spawn_stealable_with_tenant(self.new_tenant_id(), f)
    }

    /// Attempts to spawn stealable work associated with `tenant`.
    pub fn try_spawn_stealable_with_tenant<F, T>(
        &self,
        tenant: TenantId,
        f: F,
    ) -> Result<JoinHandle<'scope, T>, SpawnError>
    where
        F: FnOnce(&RuntimeContext<'_>) -> T + Send + 'static,
        T: Send + 'static,
    {
        let Some(runtime) = self.steal_runtime.clone() else {
            return Ok(self.spawn_local_with_tenant(tenant, f));
        };
        let steal_scope = {
            let steal_scope = self.steal_scope.expect("stealable scope state missing");
            // SAFETY: `steal_scope` is installed by `RuntimeContext::scope` and
            // remains valid until that call returns; `Scope` cannot escape.
            let mut steal_scope = unsafe { steal_scope.as_ref() }.borrow_mut();
            Arc::clone(steal_scope.get_or_insert_with(|| Arc::new(StealScopeState::default())))
        };
        let join = Arc::new(StealJoinState::new());
        let panic_source: Arc<dyn StealPanicSource> = join.clone();
        steal_scope.register(panic_source);

        let join_for_job = Arc::clone(&join);
        let scope_for_job = Arc::clone(&steal_scope);
        let active_scope_for_job = Arc::clone(&steal_scope);
        let owner = self.worker;
        let submitted = runtime.submit(
            owner,
            tenant,
            Box::new(move |cx| {
                join_for_job.mark_started();
                let result = panic::catch_unwind(AssertUnwindSafe(|| f(cx)));
                match result {
                    Ok(value) => join_for_job.complete(StealOutcome::Value(value)),
                    Err(payload) if payload.is::<StealTaskCanceled>() => {
                        join_for_job.complete(StealOutcome::Canceled);
                    }
                    Err(payload) => {
                        join_for_job.complete(StealOutcome::Panicked(payload));
                    }
                }
                scope_for_job.complete_one();
            }),
            Some(active_scope_for_job),
        );

        if let Err(error) = submitted {
            steal_scope.complete_one();
            return Err(error.into());
        }

        Ok(JoinHandle {
            inner: JoinInner::Stealable(join, std::marker::PhantomData),
        })
    }

    /// Spawns local stackful work with a custom usable stack size.
    pub fn spawn_with_stack_size<F, T>(&self, stack_size: usize, f: F) -> JoinHandle<'scope, T>
    where
        F: FnOnce(&RuntimeContext<'_>) -> T + 'scope,
        T: 'scope,
    {
        self.spawn_with_stack_size_and_tenant(stack_size, self.new_tenant_id(), f)
    }

    /// Spawns local stackful work with custom stack size and `tenant` metadata.
    pub fn spawn_with_stack_size_and_tenant<F, T>(
        &self,
        stack_size: usize,
        tenant: TenantId,
        f: F,
    ) -> JoinHandle<'scope, T>
    where
        F: FnOnce(&RuntimeContext<'_>) -> T + 'scope,
        T: 'scope,
    {
        JoinHandle {
            inner: JoinInner::Local(self.inner.spawn_with_stack_size(stack_size, {
                let runtime_id = self.runtime_id;
                let worker = self.worker;
                let steal_runtime = self.steal_runtime.clone();
                let active_steal_scope = self.active_steal_scope.clone();
                let tenant_allocator = Arc::clone(&self.tenant_allocator);
                let local_shared_operations = self.local_shared_operations.clone();
                let shared_ring_queue_capacity = self.shared_ring_queue_capacity;
                move |inner| {
                    let cx = RuntimeContext {
                        inner,
                        runtime_id,
                        worker,
                        steal_runtime,
                        active_steal_scope,
                        tenant,
                        tenant_allocator,
                        local_shared_operations,
                        shared_ring_queue_capacity,
                        socket_ring: RefCell::new(None),
                        shared_sync_buffers: RefCell::new(SharedSyncBufferCache::default()),
                    };
                    f(&cx)
                }
            })),
        }
    }

    /// Allocates a fresh runtime-local tenant identifier.
    pub fn new_tenant_id(&self) -> TenantId {
        TenantId(self.tenant_allocator.fetch_add(1, Ordering::Relaxed))
    }
}

impl fmt::Debug for Scope<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scope")
            .field("worker", &self.worker)
            .finish_non_exhaustive()
    }
}

/// A handle returned by [`Scope::spawn`].
pub struct JoinHandle<'scope, T> {
    inner: JoinInner<'scope, T>,
}

enum JoinInner<'scope, T> {
    Local(kimojio_stack::JoinHandle<'scope, T>),
    Stealable(
        Arc<StealJoinState<T>>,
        std::marker::PhantomData<&'scope mut T>,
    ),
}

impl<T> JoinHandle<'_, T> {
    /// Returns whether this handle's work has started executing.
    ///
    /// Local work is admitted to the current stack scheduler synchronously and is
    /// reported as started. Stealable work reports true only after a worker begins
    /// running the submitted job. Accept loops can use this to distinguish
    /// accepted-but-not-yet-serviced connections from protocol/socket bugs.
    pub fn has_started(&self) -> bool {
        match self {
            Self {
                inner: JoinInner::Local(_),
            } => true,
            Self {
                inner: JoinInner::Stealable(inner, _),
            } => inner.has_started(),
        }
    }

    /// Waits until this handle's work starts executing.
    pub fn wait_started(&self, cx: &RuntimeContext<'_>) {
        match self {
            Self {
                inner: JoinInner::Local(_),
            } => {}
            Self {
                inner: JoinInner::Stealable(inner, _),
            } => loop {
                if inner.has_started() {
                    return;
                }
                if let Some(registration) = cx.stackful_wait_registration() {
                    if inner.add_start_waiter(registration.waiter()) {
                        cx.park_stackful();
                    }
                } else {
                    inner.wait_started_blocking_for(Duration::from_millis(1));
                    cx.yield_now();
                }
            },
        }
    }

    /// Returns the completed stackful coroutine result if it is ready.
    pub fn try_join(&self) -> Option<T> {
        match self {
            Self {
                inner: JoinInner::Local(inner),
            } => inner.try_join(),
            Self {
                inner: JoinInner::Stealable(inner, _),
            } => inner.try_join(),
        }
    }

    /// Returns final stack usage if the coroutine has completed.
    pub fn stack_usage(&self) -> Option<StackUsage> {
        match self {
            Self {
                inner: JoinInner::Local(inner),
            } => inner.stack_usage(),
            Self {
                inner: JoinInner::Stealable(_, _),
            } => None,
        }
    }

    /// Returns the completed coroutine result and final stack usage if ready.
    pub fn try_join_with_stack_usage(&self) -> Option<(T, StackUsage)> {
        match self {
            Self {
                inner: JoinInner::Local(inner),
            } => inner.try_join_with_stack_usage(),
            Self {
                inner: JoinInner::Stealable(_, _),
            } => None,
        }
    }

    /// Waits for the stackful coroutine to finish and returns its result.
    ///
    /// # Panics
    ///
    /// Resumes any panic payload from the spawned coroutine. For handles returned
    /// by [`Scope::spawn_stealable`], this also includes scheduler rejection
    /// payloads for worker-pool shutdown or queue saturation in the current API.
    pub fn join(self, cx: &RuntimeContext<'_>) -> T {
        match self {
            Self {
                inner: JoinInner::Local(inner),
            } => inner.join(cx.inner),
            Self {
                inner: JoinInner::Stealable(inner, _),
            } => loop {
                if let Some(output) = inner.try_join() {
                    return output;
                }
                if let Some(registration) = cx.stackful_wait_registration() {
                    if inner.add_waiter(registration.waiter()) {
                        cx.park_stackful();
                    }
                } else {
                    inner.wait_blocking_for(Duration::from_millis(1));
                    cx.yield_now();
                }
            },
        }
    }

    /// Waits for the coroutine to finish and returns its result plus final stack usage.
    ///
    /// # Panics
    ///
    /// Panics for handles returned by [`Scope::spawn_stealable`] because
    /// stealable worker-pool jobs do not currently report final stack usage.
    pub fn join_with_stack_usage(self, cx: &RuntimeContext<'_>) -> (T, StackUsage) {
        match self {
            Self {
                inner: JoinInner::Local(inner),
            } => inner.join_with_stack_usage(cx.inner),
            Self {
                inner: JoinInner::Stealable(_, _),
            } => {
                panic!("stack usage for stealable worker-thread jobs is not available yet")
            }
        }
    }
}

impl<T> fmt::Debug for JoinHandle<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JoinHandle").finish_non_exhaustive()
    }
}

struct SharedRing {
    core: Arc<SharedRingCore>,
    driver: SharedRingDriver,
}

impl SharedRing {
    fn new(queue_capacity: usize, driver: SharedRingDriver) -> Result<Self, RingError> {
        let core = Arc::new(SharedRingCore::new(queue_capacity, driver.wake_handle()));
        Ok(Self { core, driver })
    }

    fn submit_nop(&self, cx: &RuntimeContext<'_>) -> Result<RingTimeout, RingError> {
        let state = Arc::new(SharedOpState::new(&self.core));
        self.submit_operation(cx, SharedOperationKind::Nop, state, None)
    }

    fn submit_timeout(
        &self,
        cx: &RuntimeContext<'_>,
        duration: Duration,
    ) -> Result<RingTimeout, RingError> {
        let state = Arc::new(SharedOpState::new(&self.core));
        let deadline = Instant::now()
            .checked_add(duration)
            .ok_or(RingError::DurationOutOfRange)?;
        self.submit_operation(
            cx,
            SharedOperationKind::Timeout { deadline },
            state,
            Some(duration),
        )
    }

    fn submit_read<B>(
        &self,
        cx: &RuntimeContext<'_>,
        fd: RingFd,
        mut buffer: B,
    ) -> Result<RingReadResult<B>, RingError>
    where
        B: IoReadBuffer + Send + 'static,
    {
        match &self.driver {
            SharedRingDriver::Local { runtime_id } => {
                if *runtime_id != cx.runtime_id {
                    return Err(RingError::WrongRuntime);
                }
                let len = buffer.io_buffer_len();
                let ptr = buffer.io_buffer_mut_ptr();
                let io = unsafe { cx.inner.submit_raw_read(&fd, ptr, len) };
                Ok(RingIoResult::local(io, fd, buffer, read_output::<B>))
            }
            SharedRingDriver::Multi { runtime, owner } => {
                let state = Arc::new(SharedOpState::new(&self.core));
                let buffer = Arc::new(Mutex::new(Some(buffer)));
                let result_fd = fd.clone();
                self.core.submit_queued(
                    SharedOperation::buffer(Box::new(QueuedReadOperation {
                        state: Arc::clone(&state),
                        fd,
                        buffer: Arc::clone(&buffer),
                    })),
                    SharedOpRoute::worker(*owner),
                )?;
                if let Err(error) = runtime.submit_shared_ring(*owner, &self.core) {
                    if matches!(error, RingError::QueueFull) {
                        self.core.reject_queued(RingError::QueueFull);
                    }
                    return Err(error);
                }
                Ok(RingIoResult::shared(
                    state,
                    buffer,
                    result_fd,
                    self.clone(),
                    read_output::<B>,
                ))
            }
        }
    }

    fn submit_write<B>(
        &self,
        cx: &RuntimeContext<'_>,
        fd: RingFd,
        buffer: B,
    ) -> Result<RingWriteResult<B>, RingError>
    where
        B: IoWriteBuffer + Send + 'static,
    {
        match &self.driver {
            SharedRingDriver::Local { runtime_id } => {
                if *runtime_id != cx.runtime_id {
                    return Err(RingError::WrongRuntime);
                }
                let len = buffer.io_buffer_len();
                let ptr = buffer.io_buffer_ptr();
                let io = unsafe { cx.inner.submit_raw_write(&fd, ptr, len) };
                Ok(RingIoResult::local(io, fd, buffer, write_output::<B>))
            }
            SharedRingDriver::Multi { runtime, owner } => {
                let state = Arc::new(SharedOpState::new(&self.core));
                let buffer = Arc::new(Mutex::new(Some(buffer)));
                let result_fd = fd.clone();
                self.core.submit_queued(
                    SharedOperation::buffer(Box::new(QueuedWriteOperation {
                        state: Arc::clone(&state),
                        fd,
                        buffer: Arc::clone(&buffer),
                    })),
                    SharedOpRoute::worker(*owner),
                )?;
                if let Err(error) = runtime.submit_shared_ring(*owner, &self.core) {
                    if matches!(error, RingError::QueueFull) {
                        self.core.reject_queued(RingError::QueueFull);
                    }
                    return Err(error);
                }
                Ok(RingIoResult::shared(
                    state,
                    buffer,
                    result_fd,
                    self.clone(),
                    write_output::<B>,
                ))
            }
        }
    }

    fn submit_shutdown(
        &self,
        cx: &RuntimeContext<'_>,
        fd: RingFd,
        how: Shutdown,
    ) -> Result<RingTimeout, RingError> {
        let state = Arc::new(SharedOpState::new(&self.core));
        self.submit_operation(cx, SharedOperationKind::Shutdown { fd, how }, state, None)
    }

    fn submit_close(&self, cx: &RuntimeContext<'_>, fd: RingFd) -> Result<RingTimeout, RingError> {
        match &self.driver {
            SharedRingDriver::Local { .. } => {
                let fd = fd.into_owned().map_err(|_| RingError::FdInUse)?;
                let state = Arc::new(SharedOpState::new(&self.core));
                self.submit_operation(cx, SharedOperationKind::Close { fd: Some(fd) }, state, None)
            }
            SharedRingDriver::Multi { runtime, owner } => {
                let state = Arc::new(SharedOpState::new(&self.core));
                self.core.submit_queued(
                    SharedOperation::deferred_close(Arc::clone(&state), fd),
                    SharedOpRoute::worker(*owner),
                )?;
                if let Err(error) = runtime.submit_shared_ring(*owner, &self.core) {
                    if matches!(error, RingError::QueueFull) {
                        self.core.reject_queued(RingError::QueueFull);
                    }
                    return Err(error);
                }
                Ok(RingTimeout::shared(state))
            }
        }
    }

    fn submit_operation(
        &self,
        cx: &RuntimeContext<'_>,
        kind: SharedOperationKind,
        state: Arc<SharedOpState>,
        local_timeout_duration: Option<Duration>,
    ) -> Result<RingTimeout, RingError> {
        match &self.driver {
            SharedRingDriver::Local { runtime_id } => {
                if *runtime_id != cx.runtime_id {
                    state.complete(Err(RingError::WrongRuntime));
                    return Err(RingError::WrongRuntime);
                }
                self.core
                    .submit_local(Arc::clone(&state), SharedOpRoute::local())?;
                let (io, fd) = match kind {
                    SharedOperationKind::Nop => (
                        ActiveUnitIo::NoPayload(cx.inner.submit_no_payload_nop()),
                        None,
                    ),
                    SharedOperationKind::Timeout { .. } => (
                        ActiveUnitIo::NoPayload(
                            cx.inner
                                .submit_no_payload_timeout(local_timeout_duration.expect(
                                    "local shared timeout operation missing original duration",
                                )),
                        ),
                        None,
                    ),
                    SharedOperationKind::Shutdown { fd, how } => (
                        ActiveUnitIo::Raw(cx.inner.submit_raw_shutdown(&fd, how)),
                        Some(fd),
                    ),
                    SharedOperationKind::Close { fd } => (
                        ActiveUnitIo::Raw(
                            cx.inner
                                .submit_raw_close(fd.expect("shared close operation missing fd")),
                        ),
                        None,
                    ),
                };
                let operation = LocalSharedOperationHandle::new(state, io, fd);
                if let Some(local_shared_operations) = &cx.local_shared_operations {
                    local_shared_operations.register(operation.clone());
                }
                Ok(RingTimeout::shared_local_driver(operation))
            }
            SharedRingDriver::Multi { runtime, owner } => {
                self.core.submit_queued(
                    SharedOperation::unit(kind, Arc::clone(&state)),
                    SharedOpRoute::worker(*owner),
                )?;
                if let Err(error) = runtime.submit_shared_ring(*owner, &self.core) {
                    if matches!(error, RingError::QueueFull) {
                        self.core.reject_queued(RingError::QueueFull);
                    }
                    return Err(error);
                }
                Ok(RingTimeout::shared(state))
            }
        }
    }
}

impl Clone for SharedRing {
    fn clone(&self) -> Self {
        self.core.handle_count.fetch_add(1, Ordering::AcqRel);
        Self {
            core: Arc::clone(&self.core),
            driver: self.driver.clone(),
        }
    }
}

impl Drop for SharedRing {
    fn drop(&mut self) {
        let previous = self.core.handle_count.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous != 0, "shared ring handle count underflow");
        if previous == 1 {
            self.core.close();
        }
    }
}

#[derive(Clone)]
enum SharedRingDriver {
    Local {
        runtime_id: RuntimeId,
    },
    Multi {
        runtime: Arc<MultiRuntime>,
        owner: WorkerId,
    },
}

impl SharedRingDriver {
    fn wake_handle(&self) -> SharedRingWake {
        match self {
            Self::Local { .. } => SharedRingWake::Local,
            Self::Multi { runtime, owner } => SharedRingWake::Multi {
                runtime: Arc::downgrade(runtime),
                owner: *owner,
            },
        }
    }
}

#[derive(Clone)]
enum SharedRingWake {
    Local,
    Multi {
        runtime: Weak<MultiRuntime>,
        owner: WorkerId,
    },
}

impl SharedRingWake {
    fn wake(&self) {
        match self {
            Self::Local => {}
            Self::Multi { runtime, owner } => {
                if let Some(runtime) = runtime.upgrade() {
                    runtime.wake_worker(*owner);
                }
            }
        }
    }
}

struct SharedRingCore {
    queue: Mutex<VecDeque<SharedOperation>>,
    submitted_operations: Mutex<Vec<Arc<SharedOpState>>>,
    closed: AtomicBool,
    driver_queued: AtomicBool,
    handle_count: AtomicUsize,
    queue_capacity: usize,
    wake: SharedRingWake,
    #[cfg(test)]
    test_counters: SharedRingTestCounters,
}

impl SharedRingCore {
    fn new(queue_capacity: usize, wake: SharedRingWake) -> Self {
        Self {
            queue: Mutex::new(VecDeque::with_capacity(queue_capacity)),
            submitted_operations: Mutex::new(Vec::with_capacity(queue_capacity)),
            closed: AtomicBool::new(false),
            driver_queued: AtomicBool::new(false),
            handle_count: AtomicUsize::new(1),
            queue_capacity,
            wake,
            #[cfg(test)]
            test_counters: SharedRingTestCounters::new(),
        }
    }

    fn submit_local(
        &self,
        state: Arc<SharedOpState>,
        route: SharedOpRoute,
    ) -> Result<(), RingError> {
        if self.closed.load(Ordering::Acquire) {
            state.close();
            return Err(RingError::Closed);
        }
        if self.queue_capacity == 0 {
            state.complete(Err(RingError::QueueFull));
            return Err(RingError::QueueFull);
        }
        if !state.mark_queued(route) {
            return Err(state.terminal_error().unwrap_or(RingError::Canceled));
        }
        if !state.mark_submitted(route) {
            return Err(state.terminal_error().unwrap_or(RingError::Canceled));
        }
        if !self.register_submitted(&state) {
            return Err(state.terminal_error().unwrap_or(RingError::Closed));
        }
        #[cfg(test)]
        self.test_counters.record_accepted();
        #[cfg(test)]
        self.test_counters.record_driver_submission();
        Ok(())
    }

    fn submit_queued(
        &self,
        operation: SharedOperation,
        route: SharedOpRoute,
    ) -> Result<(), RingError> {
        if self.closed.load(Ordering::Acquire) {
            operation.close();
            return Err(RingError::Closed);
        }

        let mut queue = self.queue.lock().expect("shared ring queue mutex poisoned");
        if self.closed.load(Ordering::Acquire) {
            drop(queue);
            operation.close();
            return Err(RingError::Closed);
        }
        if queue.len() >= self.queue_capacity {
            drop(queue);
            operation.complete(Err(RingError::QueueFull));
            return Err(RingError::QueueFull);
        }
        if !operation.mark_queued(route) {
            return Err(operation.terminal_error().unwrap_or(RingError::Canceled));
        }
        queue.push_back(operation);
        #[cfg(test)]
        self.test_counters.record_accepted();
        Ok(())
    }

    fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let queued = {
            let mut queue = self.queue.lock().expect("shared ring queue mutex poisoned");
            std::mem::take(&mut *queue)
        };
        for operation in queued {
            operation.close();
        }
        let submitted = {
            let mut submitted = self
                .submitted_operations
                .lock()
                .expect("shared ring submitted operation mutex poisoned");
            std::mem::take(&mut *submitted)
        };
        for state in submitted {
            state.close();
        }
        self.wake.wake();
    }

    fn wake_driver(&self) {
        self.wake.wake();
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn register_submitted(&self, state: &Arc<SharedOpState>) -> bool {
        if self.closed.load(Ordering::Acquire) {
            state.close();
            return false;
        }

        let mut submitted = self
            .submitted_operations
            .lock()
            .expect("shared ring submitted operation mutex poisoned");
        if self.closed.load(Ordering::Acquire) {
            drop(submitted);
            state.close();
            return false;
        }
        if submitted.len() >= self.queue_capacity {
            drop(submitted);
            state.complete(Err(RingError::QueueFull));
            return false;
        }
        let state_inner = state.inner.lock().expect("shared operation mutex poisoned");
        if state_inner.lifecycle.is_terminal() {
            return false;
        }
        submitted.push(Arc::clone(state));
        drop(state_inner);
        true
    }

    fn mark_driver_queued(&self) -> bool {
        !self.driver_queued.swap(true, Ordering::AcqRel)
    }

    fn clear_driver_queued(&self) {
        self.driver_queued.store(false, Ordering::Release);
    }

    fn unregister_submitted_ptr(&self, state: &SharedOpState) {
        let state = state as *const SharedOpState;
        let mut submitted = self
            .submitted_operations
            .lock()
            .expect("shared ring submitted operation mutex poisoned");
        if let Some(index) = submitted
            .iter()
            .position(|active| Arc::as_ptr(active) == state)
        {
            submitted.swap_remove(index);
        }
    }

    fn reject_queued(&self, error: RingError) {
        let queued = {
            let mut queue = self.queue.lock().expect("shared ring queue mutex poisoned");
            std::mem::take(&mut *queue)
        };
        for operation in queued {
            operation.complete(Err(error));
        }
    }

    fn pop_queued(&self) -> Option<SharedOperation> {
        self.queue
            .lock()
            .expect("shared ring queue mutex poisoned")
            .pop_front()
    }

    #[cfg(test)]
    fn submitted_operation_count(&self) -> usize {
        self.submitted_operations
            .lock()
            .expect("shared ring submitted operation mutex poisoned")
            .len()
    }

    #[cfg(test)]
    fn test_counters(&self) -> SharedRingTestCounterSnapshot {
        self.test_counters.snapshot()
    }

    #[cfg(test)]
    fn pop_queued_for_test(&self) -> Option<SharedOperation> {
        self.pop_queued()
    }

    #[cfg(test)]
    fn record_terminal_for_test(&self, lifecycle: SharedOpLifecycle) {
        self.test_counters.record_terminal(lifecycle);
    }

    #[cfg(test)]
    fn record_driver_submission_for_test(&self) {
        self.test_counters.record_driver_submission();
    }

    #[cfg(test)]
    fn record_driver_retired_for_test(&self) {
        self.test_counters.record_driver_retired();
    }
}

#[cfg(test)]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct SharedRingTestCounterSnapshot {
    accepted: usize,
    driver_submitted: usize,
    driver_retired: usize,
    completed: usize,
    canceled: usize,
    closed: usize,
    rejected: usize,
    cleaned_up: usize,
    driver_threads: Vec<thread::ThreadId>,
}

#[cfg(test)]
struct SharedRingTestCounters {
    accepted: AtomicUsize,
    driver_submitted: AtomicUsize,
    driver_retired: AtomicUsize,
    completed: AtomicUsize,
    canceled: AtomicUsize,
    closed: AtomicUsize,
    rejected: AtomicUsize,
    cleaned_up: AtomicUsize,
    driver_threads: Mutex<Vec<thread::ThreadId>>,
}

#[cfg(test)]
impl SharedRingTestCounters {
    fn new() -> Self {
        Self {
            accepted: AtomicUsize::new(0),
            driver_submitted: AtomicUsize::new(0),
            driver_retired: AtomicUsize::new(0),
            completed: AtomicUsize::new(0),
            canceled: AtomicUsize::new(0),
            closed: AtomicUsize::new(0),
            rejected: AtomicUsize::new(0),
            cleaned_up: AtomicUsize::new(0),
            driver_threads: Mutex::new(Vec::new()),
        }
    }

    fn record_accepted(&self) {
        self.accepted.fetch_add(1, Ordering::Relaxed);
    }

    fn record_driver_submission(&self) {
        self.driver_submitted.fetch_add(1, Ordering::Relaxed);
        self.driver_threads
            .lock()
            .expect("shared ring test counter mutex poisoned")
            .push(thread::current().id());
    }

    fn record_driver_retired(&self) {
        self.driver_retired.fetch_add(1, Ordering::Relaxed);
    }

    fn record_terminal(&self, lifecycle: SharedOpLifecycle) {
        match lifecycle {
            SharedOpLifecycle::Completed => {
                self.completed.fetch_add(1, Ordering::Relaxed);
            }
            SharedOpLifecycle::Canceled => {
                self.canceled.fetch_add(1, Ordering::Relaxed);
            }
            SharedOpLifecycle::Closed => {
                self.closed.fetch_add(1, Ordering::Relaxed);
            }
            SharedOpLifecycle::Rejected => {
                self.rejected.fetch_add(1, Ordering::Relaxed);
            }
            SharedOpLifecycle::Created
            | SharedOpLifecycle::Queued(_)
            | SharedOpLifecycle::Submitted(_) => {}
        }
        self.cleaned_up.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> SharedRingTestCounterSnapshot {
        SharedRingTestCounterSnapshot {
            accepted: self.accepted.load(Ordering::Relaxed),
            driver_submitted: self.driver_submitted.load(Ordering::Relaxed),
            driver_retired: self.driver_retired.load(Ordering::Relaxed),
            completed: self.completed.load(Ordering::Relaxed),
            canceled: self.canceled.load(Ordering::Relaxed),
            closed: self.closed.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
            cleaned_up: self.cleaned_up.load(Ordering::Relaxed),
            driver_threads: self
                .driver_threads
                .lock()
                .expect("shared ring test counter mutex poisoned")
                .clone(),
        }
    }
}

enum SharedOperation {
    Unit {
        kind: SharedOperationKind,
        state: Arc<SharedOpState>,
    },
    Buffer(Box<dyn QueuedSharedBufferOperation>),
    DeferredClose {
        state: Arc<SharedOpState>,
        fd: RingFd,
    },
}

impl SharedOperation {
    fn unit(kind: SharedOperationKind, state: Arc<SharedOpState>) -> Self {
        Self::Unit { kind, state }
    }

    fn buffer(operation: Box<dyn QueuedSharedBufferOperation>) -> Self {
        Self::Buffer(operation)
    }

    fn deferred_close(state: Arc<SharedOpState>, fd: RingFd) -> Self {
        Self::DeferredClose { state, fd }
    }

    fn state(&self) -> &Arc<SharedOpState> {
        match self {
            Self::Unit { state, .. } => state,
            Self::Buffer(operation) => operation.state(),
            Self::DeferredClose { state, .. } => state,
        }
    }

    fn mark_queued(&self, route: SharedOpRoute) -> bool {
        self.state().mark_queued(route)
    }

    #[cfg(test)]
    fn mark_submitted(&self, route: SharedOpRoute) -> bool {
        self.state().mark_submitted(route)
    }

    fn complete(&self, result: Result<u32, RingError>) -> bool {
        self.state().complete(result)
    }

    fn close(&self) -> bool {
        self.state().close()
    }

    fn terminal_error(&self) -> Option<RingError> {
        self.state().terminal_error()
    }
}

enum SharedOperationKind {
    Nop,
    Timeout { deadline: Instant },
    Shutdown { fd: RingFd, how: Shutdown },
    Close { fd: Option<OwnedFd> },
}

trait QueuedSharedBufferOperation: Send {
    fn state(&self) -> &Arc<SharedOpState>;
    fn start(self: Box<Self>, root: &kimojio_stack::RuntimeContext<'_>) -> ActiveSharedOperation;
}

struct QueuedReadOperation<B> {
    state: Arc<SharedOpState>,
    fd: RingFd,
    buffer: Arc<Mutex<Option<B>>>,
}

struct QueuedWriteOperation<B> {
    state: Arc<SharedOpState>,
    fd: RingFd,
    buffer: Arc<Mutex<Option<B>>>,
}

impl<B> QueuedSharedBufferOperation for QueuedReadOperation<B>
where
    B: IoReadBuffer + Send + 'static,
{
    fn state(&self) -> &Arc<SharedOpState> {
        &self.state
    }

    fn start(self: Box<Self>, root: &kimojio_stack::RuntimeContext<'_>) -> ActiveSharedOperation {
        let mut buffer = self
            .buffer
            .lock()
            .expect("shared ring read buffer mutex poisoned")
            .take()
            .expect("shared ring read submitted without buffer");
        let len = buffer.io_buffer_len();
        let ptr = buffer.io_buffer_mut_ptr();
        let io = unsafe { root.submit_raw_read(&self.fd, ptr, len) };
        ActiveSharedOperation::Buffer(Box::new(ActiveBufferOperation {
            state: self.state,
            io,
            fd: Some(self.fd),
            buffer: Some(buffer),
            buffer_slot: self.buffer,
        }))
    }
}

impl<B> QueuedSharedBufferOperation for QueuedWriteOperation<B>
where
    B: IoWriteBuffer + Send + 'static,
{
    fn state(&self) -> &Arc<SharedOpState> {
        &self.state
    }

    fn start(self: Box<Self>, root: &kimojio_stack::RuntimeContext<'_>) -> ActiveSharedOperation {
        let buffer = self
            .buffer
            .lock()
            .expect("shared ring write buffer mutex poisoned")
            .take()
            .expect("shared ring write submitted without buffer");
        let len = buffer.io_buffer_len();
        let ptr = buffer.io_buffer_ptr();
        let io = unsafe { root.submit_raw_write(&self.fd, ptr, len) };
        ActiveSharedOperation::Buffer(Box::new(ActiveBufferOperation {
            state: self.state,
            io,
            fd: Some(self.fd),
            buffer: Some(buffer),
            buffer_slot: self.buffer,
        }))
    }
}

trait ActiveSharedBufferOperation {
    fn state(&self) -> &Arc<SharedOpState>;
    fn cancel(&mut self);
    fn poll(&mut self) -> bool;
}

struct ActiveBufferOperation<B> {
    state: Arc<SharedOpState>,
    io: RawIo,
    fd: Option<RingFd>,
    buffer: Option<B>,
    buffer_slot: Arc<Mutex<Option<B>>>,
}

impl<B> ActiveSharedBufferOperation for ActiveBufferOperation<B>
where
    B: Send + 'static,
{
    fn state(&self) -> &Arc<SharedOpState> {
        &self.state
    }

    fn cancel(&mut self) {
        match (self.fd.take(), self.buffer.take()) {
            (Some(fd), Some(buffer)) => self.io.cancel_with_payload((fd, buffer)),
            (fd, buffer) => {
                drop(fd);
                drop(buffer);
                self.io.cancel();
            }
        }
    }

    fn poll(&mut self) -> bool {
        if self.state.is_terminal() {
            self.cancel();
            return true;
        }

        if let Some(result) = self.io.try_wait() {
            let buffer = self
                .buffer
                .take()
                .expect("active shared ring I/O completed without buffer");
            *self
                .buffer_slot
                .lock()
                .expect("shared ring active buffer mutex poisoned") = Some(buffer);
            self.state
                .complete_from_driver(result.map_err(RingError::from));
            return true;
        }

        false
    }
}

struct ActiveCloseOperation {
    state: Arc<SharedOpState>,
    fd: Option<RingFd>,
    io: Option<ActiveUnitIo>,
}

impl ActiveCloseOperation {
    fn new(state: Arc<SharedOpState>, fd: RingFd) -> Self {
        Self {
            state,
            fd: Some(fd),
            io: None,
        }
    }

    fn cancel(&mut self) {
        if let Some(io) = self.io.as_mut() {
            io.cancel();
        }
        self.io = None;
        self.fd = None;
    }

    fn poll(&mut self, root: &kimojio_stack::RuntimeContext<'_>) -> bool {
        if self.state.is_terminal() {
            self.cancel();
            return true;
        }

        if let Some(io) = self.io.as_mut() {
            if let Some(result) = io.try_wait() {
                self.state
                    .complete_from_driver(result.map_err(RingError::from));
                self.io = None;
                return true;
            }
            return false;
        }

        let Some(fd) = self.fd.take() else {
            self.state.complete_from_driver(Err(RingError::Closed));
            return true;
        };

        match fd.into_owned() {
            Ok(fd) => {
                self.io = Some(ActiveUnitIo::Raw(root.submit_raw_close(fd)));
            }
            Err(fd) => {
                self.fd = Some(fd);
            }
        }
        false
    }
}

struct SharedOpState {
    inner: Mutex<SharedOpInner>,
    core: Weak<SharedRingCore>,
}

impl SharedOpState {
    fn new(core: &Arc<SharedRingCore>) -> Self {
        Self {
            inner: Mutex::new(SharedOpInner {
                result: None,
                lifecycle: SharedOpLifecycle::Created,
                waiters: StackfulWaiters::default(),
                #[cfg(test)]
                resource: None,
            }),
            core: Arc::downgrade(core),
        }
    }

    fn try_take(&self) -> Option<Result<u32, RingError>> {
        let mut inner = self.inner.lock().expect("shared operation mutex poisoned");
        if !inner.lifecycle.is_terminal() {
            return None;
        }
        inner.result.take()
    }

    fn add_waiter(&self, waiter: StackfulWaiterHandle) -> bool {
        let mut inner = self.inner.lock().expect("shared operation mutex poisoned");
        if inner.lifecycle.is_terminal() {
            false
        } else {
            inner.waiters.push(waiter);
            true
        }
    }

    fn mark_queued(&self, route: SharedOpRoute) -> bool {
        let mut inner = self.inner.lock().expect("shared operation mutex poisoned");
        if inner.lifecycle.is_terminal() {
            false
        } else {
            inner.lifecycle = SharedOpLifecycle::Queued(route);
            true
        }
    }

    fn mark_submitted(&self, route: SharedOpRoute) -> bool {
        let mut inner = self.inner.lock().expect("shared operation mutex poisoned");
        if inner.lifecycle.is_terminal() {
            false
        } else {
            inner.lifecycle = SharedOpLifecycle::Submitted(route);
            true
        }
    }

    fn cancel(&self) -> bool {
        self.finish(Err(RingError::Canceled), SharedOpLifecycle::Canceled, true)
    }

    fn close(&self) -> bool {
        self.finish(Err(RingError::Closed), SharedOpLifecycle::Closed, true)
    }

    fn complete(&self, result: Result<u32, RingError>) -> bool {
        self.finish_completion(result, true)
    }

    fn complete_from_driver(&self, result: Result<u32, RingError>) -> bool {
        self.finish_completion(result, false)
    }

    fn finish_completion(&self, result: Result<u32, RingError>, wake_driver: bool) -> bool {
        let lifecycle = match result {
            Err(RingError::Canceled) => SharedOpLifecycle::Canceled,
            Err(RingError::Closed) => SharedOpLifecycle::Closed,
            Err(RingError::QueueFull) => SharedOpLifecycle::Rejected,
            Ok(_) | Err(_) => SharedOpLifecycle::Completed,
        };
        self.finish(result, lifecycle, wake_driver)
    }

    fn finish(
        &self,
        result: Result<u32, RingError>,
        lifecycle: SharedOpLifecycle,
        wake_driver: bool,
    ) -> bool {
        #[cfg(test)]
        let resource;
        let waiters = {
            let mut inner = self.inner.lock().expect("shared operation mutex poisoned");
            if inner.lifecycle.is_terminal() {
                return false;
            }
            inner.lifecycle = lifecycle;
            inner.result = Some(result);
            #[cfg(test)]
            {
                resource = inner.resource.take();
            }
            std::mem::take(&mut inner.waiters)
        };
        #[cfg(test)]
        drop(resource);
        if let Some(core) = self.core.upgrade() {
            #[cfg(test)]
            core.record_terminal_for_test(lifecycle);
            core.unregister_submitted_ptr(self);
            if wake_driver {
                core.wake_driver();
            }
        }
        waiters.wake_all();
        true
    }

    #[cfg(test)]
    fn attach_test_resource(&self, resource: SharedOpTestResource) {
        let mut inner = self.inner.lock().expect("shared operation mutex poisoned");
        assert!(
            !inner.lifecycle.is_terminal(),
            "shared test resource must be attached before terminal completion"
        );
        assert!(
            inner.resource.replace(resource).is_none(),
            "shared test resource already attached"
        );
    }

    fn is_terminal(&self) -> bool {
        let inner = self.inner.lock().expect("shared operation mutex poisoned");
        inner.lifecycle.is_terminal()
    }

    fn terminal_error(&self) -> Option<RingError> {
        let inner = self.inner.lock().expect("shared operation mutex poisoned");
        match inner.result {
            Some(Err(error)) if inner.lifecycle.is_terminal() => Some(error),
            _ => None,
        }
    }

    #[cfg(test)]
    fn lifecycle_for_test(&self) -> SharedOpLifecycle {
        self.inner
            .lock()
            .expect("shared operation mutex poisoned")
            .lifecycle
    }

    #[cfg(test)]
    fn record_driver_retired_for_test(&self) {
        if let Some(core) = self.core.upgrade() {
            core.record_driver_retired_for_test();
        }
    }
}

impl Waitable for SharedOpState {
    fn is_ready(&self) -> bool {
        self.is_terminal()
    }

    fn add_waiter(&self, cx: &kimojio_stack::RuntimeContext<'_>, registration: &WaitRegistration) {
        if let Some(waiter) = registration.external_waiter(cx) {
            self.add_waiter(waiter);
        }
    }
}

struct SharedOpInner {
    result: Option<Result<u32, RingError>>,
    lifecycle: SharedOpLifecycle,
    waiters: StackfulWaiters,
    #[cfg(test)]
    resource: Option<SharedOpTestResource>,
}

#[cfg(test)]
struct SharedOpTestResource {
    drops: Arc<AtomicUsize>,
}

#[cfg(test)]
impl SharedOpTestResource {
    fn new(drops: Arc<AtomicUsize>) -> Self {
        Self { drops }
    }
}

#[cfg(test)]
impl Drop for SharedOpTestResource {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
enum SharedOpLifecycle {
    Created,
    Queued(SharedOpRoute),
    Submitted(SharedOpRoute),
    Completed,
    Closed,
    Canceled,
    Rejected,
}

impl SharedOpLifecycle {
    fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Closed | Self::Canceled | Self::Rejected
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
struct SharedOpRoute {
    owner_worker: Option<WorkerId>,
    driver: SharedOpDriver,
}

impl SharedOpRoute {
    fn local() -> Self {
        Self {
            owner_worker: None,
            driver: SharedOpDriver::Local,
        }
    }

    fn worker(owner: WorkerId) -> Self {
        Self {
            owner_worker: Some(owner),
            driver: SharedOpDriver::Worker,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SharedOpDriver {
    Local,
    Worker,
}

type Job = Box<dyn for<'cx> FnOnce(&RuntimeContext<'cx>) + Send + 'static>;
type PanicPayload = Box<dyn Any + Send + 'static>;

struct StealTaskCanceled;

trait StealPanicSource: Send + Sync {
    fn take_unobserved_panic(&self) -> Option<PanicPayload>;
}

#[derive(Default)]
struct StackfulWaiters {
    first: Option<StackfulWaiterHandle>,
    second: Option<StackfulWaiterHandle>,
    rest: Vec<StackfulWaiterHandle>,
}

impl StackfulWaiters {
    fn push(&mut self, waiter: StackfulWaiterHandle) {
        if self.first.is_none() {
            self.first = Some(waiter);
        } else if self.second.is_none() {
            self.second = Some(waiter);
        } else {
            self.rest.push(waiter);
        }
    }

    fn wake_all(self) {
        if let Some(waiter) = self.first
            && waiter.mark_ready()
        {
            waiter.wake_ready();
        }
        if let Some(waiter) = self.second
            && waiter.mark_ready()
        {
            waiter.wake_ready();
        }
        for waiter in self.rest {
            if waiter.mark_ready() {
                waiter.wake_ready();
            }
        }
    }
}

#[derive(Default)]
struct PanicSources {
    first: Option<Arc<dyn StealPanicSource>>,
    rest: Vec<Arc<dyn StealPanicSource>>,
}

impl PanicSources {
    fn push(&mut self, source: Arc<dyn StealPanicSource>) {
        if self.first.is_none() {
            self.first = Some(source);
        } else {
            self.rest.push(source);
        }
    }

    fn resume_first_unobserved(&self) {
        if let Some(source) = &self.first
            && let Some(payload) = source.take_unobserved_panic()
        {
            panic::resume_unwind(payload);
        }
        for source in &self.rest {
            if let Some(payload) = source.take_unobserved_panic() {
                panic::resume_unwind(payload);
            }
        }
    }
}

enum SubmitError {
    ShuttingDown,
    QueueFull,
}

/// Recoverable error returned by fallible stealable spawn APIs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpawnError {
    /// The worker pool is shutting down.
    ShuttingDown,
    /// The configured worker/global queue capacity is exhausted.
    QueueFull,
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ShuttingDown => f.write_str("stealable task rejected: runtime is shutting down"),
            Self::QueueFull => f.write_str("stealable task rejected: runtime queue is full"),
        }
    }
}

impl Error for SpawnError {}

impl From<SubmitError> for SpawnError {
    fn from(value: SubmitError) -> Self {
        match value {
            SubmitError::ShuttingDown => Self::ShuttingDown,
            SubmitError::QueueFull => Self::QueueFull,
        }
    }
}

struct QueuedJob {
    owner: WorkerId,
    tenant: TenantId,
    job: Job,
    active_steal_scope: Option<Arc<StealScopeState>>,
    queued_at: Instant,
    #[cfg(test)]
    sequence: usize,
}

impl QueuedJob {
    fn new(
        owner: WorkerId,
        tenant: TenantId,
        job: Job,
        active_steal_scope: Option<Arc<StealScopeState>>,
    ) -> Self {
        Self {
            owner,
            tenant,
            job,
            active_steal_scope,
            queued_at: Instant::now(),
            #[cfg(test)]
            sequence: 0,
        }
    }

    #[cfg(test)]
    fn new_for_test(owner: WorkerId, tenant: TenantId, sequence: usize) -> Self {
        Self {
            owner,
            tenant,
            job: Box::new(|_| {}),
            active_steal_scope: None,
            queued_at: Instant::now(),
            sequence,
        }
    }
}

thread_local! {
    static CURRENT_WORKER_QUEUE: Cell<*const CrossbeamWorker<QueuedJob>> = const {
        Cell::new(std::ptr::null())
    };
}

struct ReadyJob {
    queued: QueuedJob,
    stolen_from: Option<WorkerId>,
}

struct WorkerPool {
    runtime: Arc<MultiRuntime>,
    handles: Vec<ThreadJoinHandle<()>>,
}

impl WorkerPool {
    fn start(config: RuntimeConfig, runtime_id: RuntimeId) -> Self {
        let workers = (0..config.workers.get())
            .map(|_| CrossbeamWorker::new_lifo())
            .collect::<Vec<_>>();
        let stealers = workers
            .iter()
            .map(CrossbeamWorker::stealer)
            .collect::<Vec<_>>();
        let runtime = Arc::new(MultiRuntime::with_runtime_id(config, runtime_id, stealers));
        let mut handles = Vec::with_capacity(config.workers.get());
        for (worker, local_queue) in workers.into_iter().enumerate() {
            let runtime = Arc::clone(&runtime);
            handles.push(thread::spawn(move || {
                runtime.worker_loop(WorkerId(worker), local_queue);
            }));
        }

        Self { runtime, handles }
    }

    fn shutdown(self) -> RuntimeMetrics {
        self.runtime.shutdown.store(true, Ordering::Release);
        self.runtime.wake_all_workers();
        for handle in self.handles {
            handle.join().expect("worker thread panicked");
        }
        self.runtime.metrics()
    }
}

struct CurrentWorkerGuard {
    previous: Option<usize>,
    previous_queue: *const CrossbeamWorker<QueuedJob>,
}

impl CurrentWorkerGuard {
    fn enter(worker: WorkerId, local_queue: &CrossbeamWorker<QueuedJob>) -> Self {
        let previous = CURRENT_WORKER.with(|current| {
            let previous = current.get();
            current.set(Some(worker.index()));
            previous
        });
        let previous_queue = CURRENT_WORKER_QUEUE.with(|current| {
            let previous = current.get();
            current.set(local_queue);
            previous
        });
        Self {
            previous,
            previous_queue,
        }
    }
}

impl Drop for CurrentWorkerGuard {
    fn drop(&mut self) {
        CURRENT_WORKER.with(|current| current.set(self.previous));
        CURRENT_WORKER_QUEUE.with(|current| current.set(self.previous_queue));
    }
}

struct ActiveJobGuard<'runtime> {
    runtime: &'runtime MultiRuntime,
}

impl Drop for ActiveJobGuard<'_> {
    fn drop(&mut self) {
        self.runtime.release_active();
    }
}

struct SchedulerWakeGuard<'runtime> {
    runtime: &'runtime MultiRuntime,
    worker: WorkerId,
}

impl Drop for SchedulerWakeGuard<'_> {
    fn drop(&mut self) {
        *self.runtime.scheduler_wakes[self.worker.index()]
            .lock()
            .expect("scheduler wake mutex poisoned") = None;
    }
}

enum ActiveSharedOperation {
    Unit {
        state: Arc<SharedOpState>,
        io: ActiveUnitIo,
        fd: Option<RingFd>,
    },
    Buffer(Box<dyn ActiveSharedBufferOperation>),
    Close(ActiveCloseOperation),
}

impl ActiveSharedOperation {
    fn state(&self) -> &Arc<SharedOpState> {
        match self {
            Self::Unit { state, .. } => state,
            Self::Buffer(operation) => operation.state(),
            Self::Close(operation) => &operation.state,
        }
    }

    fn cancel(&mut self) {
        match self {
            Self::Unit { io, fd, .. } => {
                if let Some(fd) = fd.take() {
                    io.cancel_with_payload(fd);
                } else {
                    io.cancel();
                }
            }
            Self::Buffer(operation) => operation.cancel(),
            Self::Close(operation) => operation.cancel(),
        }
    }

    fn poll(&mut self, root: &kimojio_stack::RuntimeContext<'_>) -> bool {
        match self {
            Self::Unit { state, io, fd } => {
                if state.is_terminal() {
                    if let Some(fd) = fd.take() {
                        io.cancel_with_payload(fd);
                    } else {
                        io.cancel();
                    }
                    return true;
                }

                if let Some(result) = io.try_wait() {
                    state.complete_from_driver(result.map_err(RingError::from));
                    *fd = None;
                    return true;
                }

                false
            }
            Self::Buffer(operation) => operation.poll(),
            Self::Close(operation) => operation.poll(root),
        }
    }
}

struct MultiRuntime {
    runtime_id: RuntimeId,
    config: RuntimeConfig,
    tenant_allocator: Arc<AtomicU64>,
    stealers: Vec<Stealer<QueuedJob>>,
    global: Injector<QueuedJob>,
    sfq_queues: Vec<SegQueue<QueuedJob>>,
    sfq_depths: Vec<AtomicUsize>,
    total_sfq_depth: AtomicUsize,
    sfq_decisions: AtomicU64,
    sfq_worker_cursors: Vec<AtomicUsize>,
    shared_ring_queues: Vec<Mutex<VecDeque<Arc<SharedRingCore>>>>,
    scheduler_wakes: Vec<Mutex<Option<SchedulerWake>>>,
    next_shared_ring_worker: AtomicUsize,
    local_depths: Vec<AtomicUsize>,
    total_local_depth: AtomicUsize,
    global_depth: AtomicUsize,
    active_jobs: AtomicUsize,
    wake_state: Mutex<WakeState>,
    wake: Condvar,
    shutdown: AtomicBool,
    metrics: MultiMetrics,
}

#[derive(Default)]
struct WakeState {
    epoch: u64,
}

struct VictimIter {
    worker: usize,
    worker_count: usize,
    start_offset: usize,
    yielded: usize,
    victims: usize,
}

fn mix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

impl Iterator for VictimIter {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        if self.yielded >= self.victims {
            return None;
        }

        let offset = 1 + ((self.start_offset + self.yielded) % self.victims);
        self.yielded += 1;
        Some((self.worker + offset) % self.worker_count)
    }
}

impl MultiRuntime {
    fn with_runtime_id(
        config: RuntimeConfig,
        runtime_id: RuntimeId,
        stealers: Vec<Stealer<QueuedJob>>,
    ) -> Self {
        let worker_count = config.workers.get();
        Self {
            runtime_id,
            config,
            tenant_allocator: Arc::new(AtomicU64::new(2)),
            stealers,
            global: Injector::new(),
            sfq_queues: (0..config.scheduler.ready_partitions.get())
                .map(|_| SegQueue::new())
                .collect(),
            sfq_depths: (0..config.scheduler.ready_partitions.get())
                .map(|_| AtomicUsize::new(0))
                .collect(),
            total_sfq_depth: AtomicUsize::new(0),
            sfq_decisions: AtomicU64::new(0),
            sfq_worker_cursors: (0..worker_count).map(|_| AtomicUsize::new(0)).collect(),
            shared_ring_queues: (0..worker_count)
                .map(|_| Mutex::new(VecDeque::with_capacity(config.max_shared_ring_queue_len)))
                .collect(),
            scheduler_wakes: (0..worker_count).map(|_| Mutex::new(None)).collect(),
            next_shared_ring_worker: AtomicUsize::new(0),
            local_depths: (0..worker_count).map(|_| AtomicUsize::new(0)).collect(),
            total_local_depth: AtomicUsize::new(0),
            global_depth: AtomicUsize::new(0),
            active_jobs: AtomicUsize::new(0),
            wake_state: Mutex::new(WakeState::default()),
            wake: Condvar::new(),
            shutdown: AtomicBool::new(false),
            metrics: MultiMetrics::new(worker_count),
        }
    }

    fn next_shared_ring_owner(&self) -> WorkerId {
        let worker = self.next_shared_ring_worker.fetch_add(1, Ordering::Relaxed);
        WorkerId(worker % self.stealers.len())
    }

    fn submit_shared_ring(
        &self,
        owner: WorkerId,
        core: &Arc<SharedRingCore>,
    ) -> Result<(), RingError> {
        if self.shutdown.load(Ordering::Acquire) {
            core.close();
            return Err(RingError::Closed);
        }

        let owner = owner.index();
        debug_assert!(
            owner < self.shared_ring_queues.len(),
            "shared ring owner worker out of range"
        );
        if !core.mark_driver_queued() {
            self.wake_worker(WorkerId(owner));
            return Ok(());
        }

        let mut queue = self.shared_ring_queues[owner]
            .lock()
            .expect("shared ring command queue mutex poisoned");
        if queue.len() >= self.config.max_shared_ring_queue_len {
            core.clear_driver_queued();
            return Err(RingError::QueueFull);
        }
        queue.push_back(Arc::clone(core));
        drop(queue);
        self.wake_worker(WorkerId(owner));
        Ok(())
    }

    fn wake_worker(&self, worker: WorkerId) {
        if let Some(wake) = self.scheduler_wakes[worker.index()]
            .lock()
            .expect("scheduler wake mutex poisoned")
            .as_ref()
        {
            wake.wake();
        }
        self.wake_all();
    }

    fn wake_all_workers(&self) {
        for wake in &self.scheduler_wakes {
            if let Some(wake) = wake.lock().expect("scheduler wake mutex poisoned").as_ref() {
                wake.wake();
            }
        }
        self.wake_all();
    }

    fn submit(
        &self,
        owner: WorkerId,
        tenant: TenantId,
        job: Job,
        active_steal_scope: Option<Arc<StealScopeState>>,
    ) -> Result<(), SubmitError> {
        if self.shutdown.load(Ordering::Acquire) {
            self.metrics.rejected_tasks.fetch_add(1, Ordering::Relaxed);
            drop(job);
            return Err(SubmitError::ShuttingDown);
        }

        if self.reserve_active().is_none() {
            self.metrics.rejected_tasks.fetch_add(1, Ordering::Relaxed);
            drop(job);
            return Err(SubmitError::QueueFull);
        }

        let owner = if owner.index() == NO_WORKER {
            0
        } else {
            owner.index() % self.stealers.len()
        };
        if self.config.scheduler.mode == SchedulerMode::StochasticFair {
            let job = QueuedJob::new(WorkerId(owner), tenant, job, active_steal_scope);
            if let Err(job) = self.submit_sfq(job) {
                self.release_active();
                self.metrics.rejected_tasks.fetch_add(1, Ordering::Relaxed);
                drop(job);
                return Err(SubmitError::QueueFull);
            }
            self.wake_one();
            return Ok(());
        }

        let submitted_from_owner_worker = self.config.steal_policy != StealPolicy::Disabled
            && CURRENT_WORKER.with(|current| current.get()) == Some(owner);
        let job = QueuedJob::new(WorkerId(owner), tenant, job, active_steal_scope);
        let result = if submitted_from_owner_worker {
            self.submit_local_current(owner, job)
        } else {
            self.submit_global(job)
        };
        if let Err(job) = result {
            self.release_active();
            self.metrics.rejected_tasks.fetch_add(1, Ordering::Relaxed);
            drop(job);
            return Err(SubmitError::QueueFull);
        }
        self.wake_one();
        Ok(())
    }

    fn submit_sfq(&self, job: QueuedJob) -> Result<(), QueuedJob> {
        let partition = self.select_sfq_enqueue_partition(job.tenant, job.owner);
        let Some(depth) = self.reserve_sfq_partition(partition) else {
            return Err(job);
        };
        self.metrics.record_sfq_partition_depth(depth);
        self.sfq_queues[partition].push(job);
        Ok(())
    }

    fn submit_local_current(&self, worker: usize, job: QueuedJob) -> Result<(), QueuedJob> {
        let Some(depth) = self.reserve_local(worker) else {
            return Err(job);
        };

        let mut job = Some(job);
        let submitted = CURRENT_WORKER_QUEUE.with(|current| {
            let queue = current.get();
            if queue.is_null() {
                false
            } else {
                // SAFETY: CURRENT_WORKER_QUEUE is installed by the owning worker
                // thread while its worker-loop stack frame owns `local_queue`.
                // `submit_local_current` is only used on that same thread.
                unsafe { &*queue }.push(job.take().expect("job already submitted"));
                true
            }
        });
        if !submitted {
            self.release_local(worker, 1);
            return Err(job.expect("missing unsubmitted job"));
        }

        self.metrics.record_local_queue_depth(depth);
        Ok(())
    }

    fn submit_global(&self, job: QueuedJob) -> Result<(), QueuedJob> {
        let Some(depth) = self.reserve_global() else {
            return Err(job);
        };
        self.global.push(job);
        self.metrics.record_global_queue_depth(depth);
        Ok(())
    }

    fn reserve_local(&self, worker: usize) -> Option<usize> {
        let depth =
            Self::reserve_depth(&self.local_depths[worker], self.config.max_worker_queue_len)?;
        self.total_local_depth.fetch_add(1, Ordering::Relaxed);
        Some(depth)
    }

    fn release_local(&self, worker: usize, count: usize) {
        Self::release_depth(&self.local_depths[worker], count);
        Self::release_depth(&self.total_local_depth, count);
    }

    fn reserve_global(&self) -> Option<usize> {
        Self::reserve_depth(&self.global_depth, self.config.max_global_queue_len)
    }

    fn release_global(&self) {
        Self::release_depth(&self.global_depth, 1);
    }

    fn reserve_active(&self) -> Option<usize> {
        Self::reserve_depth(&self.active_jobs, self.active_job_limit())
    }

    fn release_active(&self) {
        Self::release_depth(&self.active_jobs, 1);
    }

    fn active_job_limit(&self) -> usize {
        self.config
            .max_worker_queue_len
            .saturating_mul(self.stealers.len())
            .saturating_add(self.config.max_global_queue_len)
    }

    fn reserve_depth(counter: &AtomicUsize, limit: usize) -> Option<usize> {
        let mut current = counter.load(Ordering::Relaxed);
        loop {
            if current >= limit {
                return None;
            }
            let depth = current.checked_add(1).expect("queue depth overflow");
            match counter.compare_exchange_weak(
                current,
                depth,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(depth),
                Err(actual) => current = actual,
            }
        }
    }

    fn release_depth(counter: &AtomicUsize, count: usize) {
        let mut current = counter.load(Ordering::Relaxed);
        loop {
            let next = current
                .checked_sub(count)
                .expect("queue depth accounting underflow");
            match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    fn saturating_fetch_add(counter: &AtomicUsize, value: usize) {
        let mut current = counter.load(Ordering::Relaxed);
        loop {
            let next = current.saturating_add(value);
            match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    fn install_scheduler_wake(
        &self,
        worker: WorkerId,
        wake: SchedulerWake,
    ) -> SchedulerWakeGuard<'_> {
        *self.scheduler_wakes[worker.index()]
            .lock()
            .expect("scheduler wake mutex poisoned") = Some(wake);
        SchedulerWakeGuard {
            runtime: self,
            worker,
        }
    }

    fn drain_shared_ring_commands(
        &self,
        worker: WorkerId,
        root: &kimojio_stack::RuntimeContext<'_>,
        active: &mut Vec<ActiveSharedOperation>,
        shared_command_scratch: &mut VecDeque<Arc<SharedRingCore>>,
    ) -> bool {
        // The scratch queue is swapped with the mutex-protected worker queue so
        // the preallocated worker capacity stays behind for future commands.
        // It must be empty on entry and is drained before returning.
        debug_assert!(shared_command_scratch.is_empty());
        {
            let mut queue = self.shared_ring_queues[worker.index()]
                .lock()
                .expect("shared ring command queue mutex poisoned");
            std::mem::swap(&mut *queue, shared_command_scratch);
        }
        let mut made_progress = false;
        while let Some(core) = shared_command_scratch.pop_front() {
            core.clear_driver_queued();
            while let Some(operation) = core.pop_queued() {
                made_progress = true;
                if self.shutdown.load(Ordering::Acquire) || core.is_closed() {
                    operation.close();
                    continue;
                }

                if !operation
                    .state()
                    .mark_submitted(SharedOpRoute::worker(worker))
                {
                    continue;
                }
                if !core.register_submitted(operation.state()) {
                    continue;
                }

                let active_operation = match operation {
                    SharedOperation::Unit { kind, state } => {
                        let (io, fd) = match kind {
                            SharedOperationKind::Nop => {
                                (ActiveUnitIo::NoPayload(root.submit_no_payload_nop()), None)
                            }
                            SharedOperationKind::Timeout { deadline } => (
                                ActiveUnitIo::NoPayload(root.submit_no_payload_timeout(
                                    deadline.saturating_duration_since(Instant::now()),
                                )),
                                None,
                            ),
                            SharedOperationKind::Shutdown { fd, how } => (
                                ActiveUnitIo::Raw(root.submit_raw_shutdown(&fd, how)),
                                Some(fd),
                            ),
                            SharedOperationKind::Close { fd } => (
                                ActiveUnitIo::Raw(root.submit_raw_close(
                                    fd.expect("shared close operation missing fd"),
                                )),
                                None,
                            ),
                        };
                        ActiveSharedOperation::Unit { state, io, fd }
                    }
                    SharedOperation::Buffer(operation) => operation.start(root),
                    SharedOperation::DeferredClose { state, fd } => {
                        ActiveSharedOperation::Close(ActiveCloseOperation::new(state, fd))
                    }
                };
                #[cfg(test)]
                core.record_driver_submission_for_test();
                active.push(active_operation);
            }
        }
        debug_assert!(shared_command_scratch.is_empty());
        made_progress
    }

    fn close_worker_shared_operations(
        &self,
        worker: WorkerId,
        active: &mut Vec<ActiveSharedOperation>,
        shared_command_scratch: &mut VecDeque<Arc<SharedRingCore>>,
    ) {
        // See `drain_shared_ring_commands`: this scratch queue is empty on
        // entry, receives queued commands by swap, then is drained before exit.
        debug_assert!(shared_command_scratch.is_empty());
        {
            let mut queue = self.shared_ring_queues[worker.index()]
                .lock()
                .expect("shared ring command queue mutex poisoned");
            std::mem::swap(&mut *queue, shared_command_scratch);
        }
        while let Some(core) = shared_command_scratch.pop_front() {
            core.clear_driver_queued();
            core.close();
        }
        debug_assert!(shared_command_scratch.is_empty());
        for mut operation in active.drain(..) {
            operation.state().close();
            operation.cancel();
        }
    }

    fn poll_active_shared_operations(
        root: &kimojio_stack::RuntimeContext<'_>,
        active: &mut Vec<ActiveSharedOperation>,
    ) -> bool {
        let mut made_progress = false;
        let mut index = 0;
        while index < active.len() {
            if active[index].poll(root) {
                #[cfg(test)]
                active[index].state().record_driver_retired_for_test();
                active.swap_remove(index);
                made_progress = true;
            } else {
                index += 1;
            }
        }
        made_progress
    }

    fn worker_loop(self: &Arc<Self>, worker: WorkerId, local_queue: CrossbeamWorker<QueuedJob>) {
        let _current_worker = CurrentWorkerGuard::enter(worker, &local_queue);
        let mut inner_config = kimojio_stack::RuntimeConfig::default();
        inner_config.stack_size = self.config.stack_size;
        inner_config.max_cached_stacks = self.config.max_cached_stacks_per_worker;
        inner_config.registered_file_slots = self.config.registered_file_slots;
        inner_config.registered_buffer_slots = self.config.registered_buffer_slots;
        let mut inner = kimojio_stack::Runtime::with_config(inner_config);
        inner.block_on(|root| {
            let _scheduler_wake =
                self.install_scheduler_wake(worker, root.internal_scheduler_wake());
            root.scope(|scope| {
                let mut active_shared = Vec::new();
                let mut shared_command_scratch =
                    VecDeque::with_capacity(self.config.max_shared_ring_queue_len);
                let mut idle_iterations = 0_usize;
                loop {
                    if self.shutdown.load(Ordering::Acquire) {
                        self.close_worker_shared_operations(
                            worker,
                            &mut active_shared,
                            &mut shared_command_scratch,
                        );
                        break;
                    }

                    let observed_wake = self.wake_epoch();
                    let mut made_progress = self.drain_shared_ring_commands(
                        worker,
                        root,
                        &mut active_shared,
                        &mut shared_command_scratch,
                    );
                    made_progress |= Self::poll_active_shared_operations(root, &mut active_shared);

                    let spawned = if let Some(ready) =
                        self.take_job(worker, &local_queue, idle_iterations)
                    {
                        let QueuedJob {
                            owner,
                            tenant,
                            job,
                            active_steal_scope,
                            queued_at,
                            ..
                        } = ready.queued;
                        self.metrics.record_execution(
                            owner,
                            worker,
                            ready.stolen_from,
                            tenant,
                            queued_at.elapsed(),
                        );
                        let runtime = Arc::clone(self);
                        scope.spawn(move |inner| {
                            let runtime_id = runtime.runtime_id;
                            let shared_ring_queue_capacity =
                                runtime.config.max_shared_ring_queue_len;
                            let cx = RuntimeContext {
                                inner,
                                runtime_id,
                                worker,
                                steal_runtime: Some(Arc::clone(&runtime)),
                                active_steal_scope,
                                tenant,
                                tenant_allocator: Arc::clone(&runtime.tenant_allocator),
                                local_shared_operations: None,
                                shared_ring_queue_capacity,
                                socket_ring: RefCell::new(None),
                                shared_sync_buffers: RefCell::new(SharedSyncBufferCache::default()),
                            };
                            let runtime = cx
                                .steal_runtime
                                .as_ref()
                                .expect("worker context missing runtime");
                            let active_job = ActiveJobGuard { runtime };
                            job(&cx);
                            drop(active_job);
                            runtime
                                .metrics
                                .completed_tasks
                                .fetch_add(1, Ordering::Relaxed);
                            runtime.metrics.record_completion(worker);
                        });
                        true
                    } else {
                        false
                    };
                    made_progress |= spawned;

                    made_progress |= root.internal_scheduler_tick();
                    made_progress |= Self::poll_active_shared_operations(root, &mut active_shared);

                    if self.shutdown.load(Ordering::Acquire) {
                        self.close_worker_shared_operations(
                            worker,
                            &mut active_shared,
                            &mut shared_command_scratch,
                        );
                        break;
                    }

                    if made_progress {
                        idle_iterations = 0;
                    } else if active_shared.is_empty() {
                        idle_iterations = idle_iterations.wrapping_add(1);
                        self.wait_for_wake_after(observed_wake);
                    }
                }
            });
        });
    }

    fn take_job(
        &self,
        worker: WorkerId,
        local_queue: &CrossbeamWorker<QueuedJob>,
        idle_iterations: usize,
    ) -> Option<ReadyJob> {
        if self.config.scheduler.mode == SchedulerMode::StochasticFair {
            return self.take_sfq_job(worker, idle_iterations);
        }

        if let Some(job) = self.pop_local(worker, local_queue) {
            return Some(ReadyJob {
                queued: job,
                stolen_from: None,
            });
        }

        if self.should_poll_global(idle_iterations) {
            self.metrics
                .global_queue_polls
                .fetch_add(1, Ordering::Relaxed);
            if let Some(job) = self.pop_global() {
                return Some(ReadyJob {
                    queued: job,
                    stolen_from: None,
                });
            }
        }

        self.steal(worker, local_queue, idle_iterations)
    }

    fn take_sfq_job(&self, worker: WorkerId, idle_iterations: usize) -> Option<ReadyJob> {
        if self.total_sfq_depth.load(Ordering::Acquire) == 0 {
            return None;
        }

        self.record_sfq_scheduling_decision();
        let partitions = self.sfq_queues.len();
        let start = self.select_sfq_dequeue_start(worker, idle_iterations);
        for offset in 0..partitions {
            self.metrics.sfq_queue_polls.fetch_add(1, Ordering::Relaxed);
            let partition = (start + offset) % partitions;
            if self.sfq_depths[partition].load(Ordering::Acquire) == 0 {
                continue;
            }
            if let Some(job) = self.sfq_queues[partition].pop() {
                self.release_sfq_partition(partition);
                return Some(ReadyJob {
                    queued: job,
                    stolen_from: None,
                });
            }
        }

        None
    }

    fn select_sfq_dequeue_start(&self, worker: WorkerId, idle_iterations: usize) -> usize {
        let partitions = self.sfq_queues.len();
        let cursor = self.sfq_worker_cursors[worker.index()].fetch_add(1, Ordering::Relaxed);
        match self.config.scheduler.selection {
            QueueSelectionPolicy::SingleChoice => (worker.index() + cursor) % partitions,
            QueueSelectionPolicy::RandomSubsetShortest { subset } => {
                self.sfq_shortest_dequeue_subset_with_cost(worker, cursor, subset.get(), |_| 0)
            }
            QueueSelectionPolicy::Shortest => self
                .sfq_shortest_non_empty()
                .unwrap_or((worker.index() + cursor + idle_iterations) % partitions),
            QueueSelectionPolicy::MovementCost {
                subset,
                movement_penalty,
            } => {
                let home = worker.index() % partitions;
                self.sfq_shortest_dequeue_subset_with_cost(
                    worker,
                    cursor,
                    subset.get(),
                    |partition| usize::from(partition != home) * movement_penalty,
                )
            }
        }
    }

    fn sfq_shortest_dequeue_subset_with_cost(
        &self,
        worker: WorkerId,
        cursor: usize,
        subset: usize,
        cost: impl Fn(usize) -> usize,
    ) -> usize {
        let partitions = self.sfq_queues.len();
        let count = subset.max(1).min(partitions);
        (0..count)
            .map(|offset| (worker.index() + cursor + offset) % partitions)
            .min_by_key(|&partition| {
                self.sfq_depths[partition].load(Ordering::Relaxed) + cost(partition)
            })
            .expect("SFQ dequeue subset is empty")
    }

    fn sfq_shortest_non_empty(&self) -> Option<usize> {
        self.sfq_depths
            .iter()
            .enumerate()
            .filter_map(|(partition, depth)| {
                let depth = depth.load(Ordering::Relaxed);
                (depth != 0).then_some((partition, depth))
            })
            .min_by_key(|(_, depth)| *depth)
            .map(|(partition, _)| partition)
    }

    fn select_sfq_enqueue_partition(&self, tenant: TenantId, owner: WorkerId) -> usize {
        let scheduler = self.config.scheduler;
        match scheduler.selection {
            QueueSelectionPolicy::SingleChoice => self.sfq_partition(tenant, 0),
            QueueSelectionPolicy::RandomSubsetShortest { subset } => {
                self.sfq_shortest_subset(tenant, subset.get(), |_| 0)
            }
            QueueSelectionPolicy::Shortest => self.sfq_global_shortest(),
            QueueSelectionPolicy::MovementCost {
                subset,
                movement_penalty,
            } => {
                let home = owner.index() % self.sfq_queues.len();
                self.sfq_shortest_subset(tenant, subset.get(), |partition| {
                    usize::from(partition != home) * movement_penalty
                })
            }
        }
    }

    fn sfq_shortest_subset(
        &self,
        tenant: TenantId,
        subset: usize,
        cost: impl Fn(usize) -> usize,
    ) -> usize {
        let subset = subset.max(1).min(self.sfq_queues.len());
        (0..subset)
            .map(|offset| self.sfq_partition(tenant, offset as u64))
            .min_by_key(|&partition| {
                self.sfq_depths[partition].load(Ordering::Relaxed) + cost(partition)
            })
            .expect("SFQ subset is empty")
    }

    fn sfq_global_shortest(&self) -> usize {
        self.sfq_depths
            .iter()
            .enumerate()
            .min_by_key(|(_, depth)| depth.load(Ordering::Relaxed))
            .map(|(partition, _)| partition)
            .expect("SFQ has no partitions")
    }

    fn sfq_partition(&self, tenant: TenantId, offset: u64) -> usize {
        let epoch = self.sfq_epoch();
        (mix64(tenant.get() ^ epoch ^ offset) as usize) % self.sfq_queues.len()
    }

    fn sfq_epoch(&self) -> u64 {
        match self.config.scheduler.reassignment {
            TenantReassignmentPolicy::Stable => 0,
            TenantReassignmentPolicy::EverySchedulingDecisions(interval) => {
                self.sfq_decisions.load(Ordering::Relaxed) / interval.get() as u64
            }
        }
    }

    fn record_sfq_scheduling_decision(&self) {
        if matches!(
            self.config.scheduler.reassignment,
            TenantReassignmentPolicy::EverySchedulingDecisions(_)
        ) {
            self.sfq_decisions.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn reserve_sfq_partition(&self, partition: usize) -> Option<usize> {
        let depth =
            Self::reserve_depth(&self.sfq_depths[partition], self.sfq_partition_capacity())?;
        self.total_sfq_depth.fetch_add(1, Ordering::Relaxed);
        Some(depth)
    }

    fn sfq_partition_capacity(&self) -> usize {
        self.active_job_limit().max(1)
    }

    fn release_sfq_partition(&self, partition: usize) {
        Self::release_depth(&self.sfq_depths[partition], 1);
        Self::release_depth(&self.total_sfq_depth, 1);
    }

    fn pop_local(
        &self,
        worker: WorkerId,
        local_queue: &CrossbeamWorker<QueuedJob>,
    ) -> Option<QueuedJob> {
        let job = local_queue.pop()?;
        self.release_local(worker.index(), 1);
        Some(job)
    }

    fn pop_global(&self) -> Option<QueuedJob> {
        let job = Self::steal_retry(|| self.global.steal())?;
        self.release_global();
        Some(job)
    }

    fn should_poll_global(&self, idle_iterations: usize) -> bool {
        if self.global_depth.load(Ordering::Relaxed) != 0 {
            return true;
        }

        match self.config.steal_policy.global_queue_interval() {
            Some(interval) => idle_iterations.is_multiple_of(interval.get()),
            None => true,
        }
    }

    fn steal(
        &self,
        worker: WorkerId,
        local_queue: &CrossbeamWorker<QueuedJob>,
        idle_iterations: usize,
    ) -> Option<ReadyJob> {
        if self.config.steal_policy == StealPolicy::Disabled {
            return None;
        }
        if self.stealable_depth_excluding(worker.index()) == 0 {
            return None;
        }

        for victim in self.victims(worker, idle_iterations) {
            self.metrics.steal_attempts.fetch_add(1, Ordering::Relaxed);
            if let Some(job) = self.steal_from(worker.index(), victim, local_queue) {
                self.metrics
                    .successful_steals
                    .fetch_add(1, Ordering::Relaxed);
                return Some(ReadyJob {
                    queued: job,
                    stolen_from: Some(WorkerId(victim)),
                });
            }
            self.metrics.failed_steals.fetch_add(1, Ordering::Relaxed);
        }

        None
    }

    fn stealable_depth_excluding(&self, worker: usize) -> usize {
        self.total_local_depth
            .load(Ordering::Acquire)
            .saturating_sub(self.local_depths[worker].load(Ordering::Acquire))
    }

    fn victims(&self, worker: WorkerId, idle_iterations: usize) -> VictimIter {
        let victims = self.stealers.len().saturating_sub(1);
        let start_offset = if victims == 0 {
            0
        } else {
            idle_iterations % victims
        };
        VictimIter {
            worker: worker.index(),
            worker_count: self.stealers.len(),
            start_offset,
            yielded: 0,
            victims,
        }
    }

    fn steal_from(
        &self,
        worker: usize,
        victim: usize,
        local_queue: &CrossbeamWorker<QueuedJob>,
    ) -> Option<QueuedJob> {
        match self.config.steal_policy {
            StealPolicy::Disabled => None,
            StealPolicy::StealOne { .. } => self.steal_one_from(victim),
            StealPolicy::StealHalf { .. } => {
                self.steal_batch(worker, victim, local_queue, usize::MAX)
            }
            StealPolicy::StealBatch { batch, .. } => {
                self.steal_batch(worker, victim, local_queue, batch.get())
            }
        }
    }

    fn steal_one_from(&self, victim: usize) -> Option<QueuedJob> {
        let job = Self::steal_retry(|| self.stealers[victim].steal())?;
        self.release_local(victim, 1);
        Some(job)
    }

    fn steal_batch(
        &self,
        worker: usize,
        victim: usize,
        local_queue: &CrossbeamWorker<QueuedJob>,
        max_batch: usize,
    ) -> Option<QueuedJob> {
        let victim_depth = self.local_depths[victim].load(Ordering::Acquire);
        let batch = (victim_depth / 2).max(1).min(max_batch);
        let mut first = None;
        for _ in 0..batch {
            let reserved_extra_depth = if first.is_some() {
                self.reserve_local(worker)
            } else {
                None
            };
            if first.is_some() && reserved_extra_depth.is_none() {
                break;
            }
            let Some(job) = self.steal_one_from(victim) else {
                if reserved_extra_depth.is_some() {
                    self.release_local(worker, 1);
                }
                break;
            };
            if first.is_none() {
                first = Some(job);
            } else {
                local_queue.push(job);
                self.metrics
                    .record_local_queue_depth(reserved_extra_depth.expect("missing reservation"));
            }
        }
        first
    }

    fn steal_retry<T>(mut steal: impl FnMut() -> Steal<T>) -> Option<T> {
        loop {
            match steal() {
                Steal::Success(job) => return Some(job),
                Steal::Empty => return None,
                Steal::Retry => hint::spin_loop(),
            }
        }
    }

    fn wake_epoch(&self) -> u64 {
        self.wake_state
            .lock()
            .expect("worker wake mutex poisoned")
            .epoch
    }

    fn wake_one(&self) {
        self.advance_wake_epoch();
        self.wake.notify_one();
    }

    fn wake_all(&self) {
        self.advance_wake_epoch();
        self.wake.notify_all();
    }

    fn advance_wake_epoch(&self) {
        let mut state = self.wake_state.lock().expect("worker wake mutex poisoned");
        state.epoch = state.epoch.wrapping_add(1);
    }

    fn wait_for_wake_after(&self, observed_epoch: u64) {
        let state = self.wake_state.lock().expect("worker wake mutex poisoned");
        if state.epoch == observed_epoch {
            let _guard = self.wake.wait(state).expect("worker wake mutex poisoned");
        }
    }

    fn metrics(&self) -> RuntimeMetrics {
        let max_ready_wait = *self
            .metrics
            .max_ready_wait
            .lock()
            .expect("max ready-wait metric mutex poisoned");
        RuntimeMetrics {
            worker_count: self.config.workers.get(),
            steal_policy: self.config.steal_policy,
            scheduler: self.config.scheduler,
            steal_attempts: self.metrics.steal_attempts.load(Ordering::Relaxed),
            successful_steals: self.metrics.successful_steals.load(Ordering::Relaxed),
            failed_steals: self.metrics.failed_steals.load(Ordering::Relaxed),
            global_queue_polls: self.metrics.global_queue_polls.load(Ordering::Relaxed),
            completed_tasks: self.metrics.completed_tasks.load(Ordering::Relaxed),
            rejected_tasks: self.metrics.rejected_tasks.load(Ordering::Relaxed),
            max_local_queue_depth: self.metrics.max_local_queue_depth.load(Ordering::Relaxed),
            max_global_queue_depth: self.metrics.max_global_queue_depth.load(Ordering::Relaxed),
            max_sfq_partition_depth: self.metrics.max_sfq_partition_depth.load(Ordering::Relaxed),
            sfq_queue_polls: self.metrics.sfq_queue_polls.load(Ordering::Relaxed),
            sfq_epoch: match self.config.scheduler.reassignment {
                TenantReassignmentPolicy::Stable => 0,
                TenantReassignmentPolicy::EverySchedulingDecisions(interval) => {
                    self.sfq_decisions.load(Ordering::Relaxed) / interval.get() as u64
                }
            },
            ready_wait_samples: self.metrics.ready_wait_samples.load(Ordering::Relaxed),
            total_ready_wait_ns: self.metrics.total_ready_wait_ns.load(Ordering::Relaxed) as u128,
            max_ready_wait_ns: max_ready_wait.ns as u128,
            max_ready_wait_tenant: (max_ready_wait.tenant != 0)
                .then_some(TenantId(max_ready_wait.tenant)),
            last_owner_worker: self.metrics.load_worker(&self.metrics.last_owner_worker),
            last_executing_worker: self
                .metrics
                .load_worker(&self.metrics.last_executing_worker),
            last_steal_victim: self.metrics.load_worker(&self.metrics.last_steal_victim),
            worker_completed_tasks: self
                .metrics
                .worker_completed_tasks
                .iter()
                .map(|count| count.load(Ordering::Relaxed))
                .collect(),
            allocated_tenants: self
                .tenant_allocator
                .load(Ordering::Relaxed)
                .saturating_sub(1),
            last_tenant: self.metrics.load_tenant(&self.metrics.last_tenant),
        }
    }
}

struct MultiMetrics {
    steal_attempts: AtomicUsize,
    successful_steals: AtomicUsize,
    failed_steals: AtomicUsize,
    global_queue_polls: AtomicUsize,
    completed_tasks: AtomicUsize,
    rejected_tasks: AtomicUsize,
    max_local_queue_depth: AtomicUsize,
    max_global_queue_depth: AtomicUsize,
    max_sfq_partition_depth: AtomicUsize,
    sfq_queue_polls: AtomicUsize,
    ready_wait_samples: AtomicUsize,
    total_ready_wait_ns: AtomicUsize,
    max_ready_wait: Mutex<MaxReadyWait>,
    last_owner_worker: AtomicUsize,
    last_executing_worker: AtomicUsize,
    last_steal_victim: AtomicUsize,
    last_tenant: AtomicU64,
    worker_completed_tasks: Vec<AtomicUsize>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct MaxReadyWait {
    ns: usize,
    tenant: u64,
}

impl MultiMetrics {
    fn new(worker_count: usize) -> Self {
        Self {
            steal_attempts: AtomicUsize::new(0),
            successful_steals: AtomicUsize::new(0),
            failed_steals: AtomicUsize::new(0),
            global_queue_polls: AtomicUsize::new(0),
            completed_tasks: AtomicUsize::new(0),
            rejected_tasks: AtomicUsize::new(0),
            max_local_queue_depth: AtomicUsize::new(0),
            max_global_queue_depth: AtomicUsize::new(0),
            max_sfq_partition_depth: AtomicUsize::new(0),
            sfq_queue_polls: AtomicUsize::new(0),
            ready_wait_samples: AtomicUsize::new(0),
            total_ready_wait_ns: AtomicUsize::new(0),
            max_ready_wait: Mutex::new(MaxReadyWait::default()),
            last_owner_worker: AtomicUsize::new(NO_WORKER),
            last_executing_worker: AtomicUsize::new(NO_WORKER),
            last_steal_victim: AtomicUsize::new(NO_WORKER),
            last_tenant: AtomicU64::new(0),
            worker_completed_tasks: (0..worker_count).map(|_| AtomicUsize::new(0)).collect(),
        }
    }

    fn record_local_queue_depth(&self, depth: usize) {
        Self::record_queue_depth(&self.max_local_queue_depth, depth);
    }

    fn record_global_queue_depth(&self, depth: usize) {
        Self::record_queue_depth(&self.max_global_queue_depth, depth);
    }

    fn record_sfq_partition_depth(&self, depth: usize) {
        Self::record_queue_depth(&self.max_sfq_partition_depth, depth);
    }

    fn record_queue_depth(counter: &AtomicUsize, depth: usize) {
        let mut current = counter.load(Ordering::Relaxed);
        while depth > current {
            match counter.compare_exchange(current, depth, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(previous) => current = previous,
            }
        }
    }

    fn record_execution(
        &self,
        owner: WorkerId,
        executing: WorkerId,
        stolen_from: Option<WorkerId>,
        tenant: TenantId,
        ready_wait: Duration,
    ) {
        self.last_owner_worker
            .store(owner.index(), Ordering::Relaxed);
        self.last_executing_worker
            .store(executing.index(), Ordering::Relaxed);
        if let Some(victim) = stolen_from {
            self.last_steal_victim
                .store(victim.index(), Ordering::Relaxed);
        }
        self.last_tenant.store(tenant.get(), Ordering::Relaxed);
        let wait_ns = ready_wait.as_nanos().min(usize::MAX as u128) as usize;
        self.ready_wait_samples.fetch_add(1, Ordering::Relaxed);
        MultiRuntime::saturating_fetch_add(&self.total_ready_wait_ns, wait_ns);
        let mut max_ready_wait = self
            .max_ready_wait
            .lock()
            .expect("max ready-wait metric mutex poisoned");
        if wait_ns > max_ready_wait.ns {
            *max_ready_wait = MaxReadyWait {
                ns: wait_ns,
                tenant: tenant.get(),
            };
        }
    }

    fn record_completion(&self, worker: WorkerId) {
        self.worker_completed_tasks[worker.index()].fetch_add(1, Ordering::Relaxed);
    }

    fn load_worker(&self, counter: &AtomicUsize) -> Option<WorkerId> {
        match counter.load(Ordering::Relaxed) {
            NO_WORKER => None,
            worker => Some(WorkerId(worker)),
        }
    }

    fn load_tenant(&self, counter: &AtomicU64) -> Option<TenantId> {
        match counter.load(Ordering::Relaxed) {
            0 => None,
            tenant => Some(TenantId(tenant)),
        }
    }
}

#[derive(Default)]
struct StealScopeState {
    inner: Mutex<StealScopeInner>,
    pending_ready: Condvar,
}

impl StealScopeState {
    fn register(&self, panic_source: Arc<dyn StealPanicSource>) {
        let mut inner = self.inner.lock().expect("scope pending mutex poisoned");
        inner.pending += 1;
        inner.panic_sources.push(panic_source);
    }

    fn push_cancel_waiter(&self, waiter: StackfulWaiterHandle) -> bool {
        let mut inner = self.inner.lock().expect("scope pending mutex poisoned");
        if inner.cancel_requested {
            false
        } else {
            inner.cancel_waiters.push(waiter);
            true
        }
    }

    fn is_cancel_requested(&self) -> bool {
        self.inner
            .lock()
            .expect("scope pending mutex poisoned")
            .cancel_requested
    }

    fn request_cancel(&self) {
        let waiters = {
            let mut inner = self.inner.lock().expect("scope pending mutex poisoned");
            if inner.cancel_requested {
                StackfulWaiters::default()
            } else {
                inner.cancel_requested = true;
                std::mem::take(&mut inner.cancel_waiters)
            }
        };
        waiters.wake_all();
    }

    fn complete_one(&self) {
        let (waiters, notify) = {
            let mut inner = self.inner.lock().expect("scope pending mutex poisoned");
            inner.pending = inner
                .pending
                .checked_sub(1)
                .expect("stealable scope pending count underflow");
            if inner.pending == 0 {
                (std::mem::take(&mut inner.waiters), true)
            } else {
                (StackfulWaiters::default(), false)
            }
        };
        if notify {
            self.pending_ready.notify_all();
            waiters.wake_all();
        }
    }

    fn wait_with(&self, cx: &RuntimeContext<'_>, cancel_immediately: bool) {
        let mut cancel_requested = cancel_immediately;
        if cancel_requested {
            self.request_cancel();
        }
        loop {
            let mut inner = self.inner.lock().expect("scope pending mutex poisoned");
            if inner.pending == 0 {
                return;
            }
            if !cancel_requested {
                let (guard, _) = self
                    .pending_ready
                    .wait_timeout(inner, Duration::from_millis(1))
                    .expect("scope pending mutex poisoned");
                drop(guard);
                cancel_requested = true;
                self.request_cancel();
                continue;
            }
            if let Some(registration) = cx.stackful_wait_registration() {
                inner.waiters.push(registration.waiter());
                drop(inner);
                cx.park_stackful();
            } else {
                let _guard = self
                    .pending_ready
                    .wait(inner)
                    .expect("scope pending mutex poisoned");
            }
        }
    }

    fn resume_unobserved_panic(&self) {
        let inner = self.inner.lock().expect("scope pending mutex poisoned");
        inner.panic_sources.resume_first_unobserved();
    }
}

#[derive(Default)]
struct StealScopeInner {
    pending: usize,
    waiters: StackfulWaiters,
    cancel_waiters: StackfulWaiters,
    cancel_requested: bool,
    panic_sources: PanicSources,
}

struct StealJoinState<T> {
    inner: Mutex<StealJoinInner<T>>,
    ready: Condvar,
    started_ready: Condvar,
    taken: AtomicBool,
}

impl<T> StealJoinState<T> {
    fn new() -> Self {
        Self {
            inner: Mutex::new(StealJoinInner {
                started: false,
                outcome: None,
                start_waiters: StackfulWaiters::default(),
                waiters: StackfulWaiters::default(),
            }),
            ready: Condvar::new(),
            started_ready: Condvar::new(),
            taken: AtomicBool::new(false),
        }
    }

    fn mark_started(&self) {
        let waiters = {
            let mut inner = self.inner.lock().expect("steal join mutex poisoned");
            if inner.started {
                return;
            }
            inner.started = true;
            std::mem::take(&mut inner.start_waiters)
        };
        waiters.wake_all();
        self.started_ready.notify_all();
    }

    fn has_started(&self) -> bool {
        self.inner
            .lock()
            .expect("steal join mutex poisoned")
            .started
    }

    fn add_start_waiter(&self, waiter: StackfulWaiterHandle) -> bool {
        let mut inner = self.inner.lock().expect("steal join mutex poisoned");
        if inner.started {
            false
        } else {
            inner.start_waiters.push(waiter);
            true
        }
    }

    fn wait_started_blocking_for(&self, duration: Duration) {
        let inner = self.inner.lock().expect("steal join mutex poisoned");
        if !inner.started {
            let _guard = self
                .started_ready
                .wait_timeout(inner, duration)
                .expect("steal join mutex poisoned");
        }
    }

    fn complete(&self, outcome: StealOutcome<T>) {
        let (start_waiters, completion_waiters) = {
            let mut inner = self.inner.lock().expect("steal join mutex poisoned");
            if inner.outcome.is_some() {
                return;
            }
            let start_waiters = if inner.started {
                StackfulWaiters::default()
            } else {
                inner.started = true;
                std::mem::take(&mut inner.start_waiters)
            };
            inner.outcome = Some(outcome);
            (start_waiters, std::mem::take(&mut inner.waiters))
        };
        start_waiters.wake_all();
        completion_waiters.wake_all();
        self.started_ready.notify_all();
        self.ready.notify_all();
    }

    fn try_join(&self) -> Option<T> {
        if self.taken.load(Ordering::Acquire) {
            panic!("JoinHandle value already taken");
        }
        let outcome = {
            let mut inner = self.inner.lock().expect("steal join mutex poisoned");
            inner.outcome.take()?
        };

        if self.taken.swap(true, Ordering::AcqRel) {
            panic!("JoinHandle value already taken");
        }

        match outcome {
            StealOutcome::Value(value) => Some(value),
            StealOutcome::Canceled => {
                panic!("stealable coroutine canceled before JoinHandle joined")
            }
            StealOutcome::Panicked(payload) => panic::resume_unwind(payload),
        }
    }

    fn add_waiter(&self, waiter: StackfulWaiterHandle) -> bool {
        if self.taken.load(Ordering::Acquire) {
            panic!("JoinHandle value already taken");
        }
        let mut inner = self.inner.lock().expect("steal join mutex poisoned");
        if inner.outcome.is_some() {
            false
        } else {
            inner.waiters.push(waiter);
            true
        }
    }

    fn wait_blocking_for(&self, duration: Duration) {
        if self.taken.load(Ordering::Acquire) {
            panic!("JoinHandle value already taken");
        }
        let inner = self.inner.lock().expect("steal join mutex poisoned");
        if inner.outcome.is_none() {
            let _guard = self
                .ready
                .wait_timeout(inner, duration)
                .expect("steal join mutex poisoned");
        }
    }
}

impl<T: Send + 'static> StealPanicSource for StealJoinState<T> {
    fn take_unobserved_panic(&self) -> Option<PanicPayload> {
        let mut inner = self.inner.lock().expect("steal join mutex poisoned");
        if matches!(inner.outcome, Some(StealOutcome::Panicked(_)))
            && let Some(StealOutcome::Panicked(payload)) = inner.outcome.take()
        {
            return Some(payload);
        }
        None
    }
}

struct StealJoinInner<T> {
    started: bool,
    outcome: Option<StealOutcome<T>>,
    start_waiters: StackfulWaiters,
    waiters: StackfulWaiters,
}

enum StealOutcome<T> {
    Value(T),
    Canceled,
    Panicked(PanicPayload),
}

/// Internal helpers used by criterion benchmarks.
#[doc(hidden)]
pub mod bench_support {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use crossbeam_queue::SegQueue;

    use super::{CrossbeamWorker, Injector, MultiRuntime};

    /// Minimal fixture for measuring only scheduler queue mechanics.
    pub struct RawSchedulerQueues {
        global: Injector<usize>,
        local: CrossbeamWorker<usize>,
        victim: CrossbeamWorker<usize>,
        victim_stealer: super::Stealer<usize>,
    }

    impl RawSchedulerQueues {
        /// Creates prewarmed crossbeam queues so the measured path does not grow
        /// queue buffers.
        pub fn new() -> Self {
            let local = CrossbeamWorker::new_lifo();
            let victim = CrossbeamWorker::new_lifo();
            let victim_stealer = victim.stealer();
            let fixture = Self {
                global: Injector::new(),
                local,
                victim,
                victim_stealer,
            };
            fixture.prewarm();
            fixture
        }

        fn prewarm(&self) {
            for value in 0..64 {
                self.global.push(value);
                assert!(MultiRuntime::steal_retry(|| self.global.steal()).is_some());
                self.victim.push(value);
                assert!(MultiRuntime::steal_retry(|| self.victim_stealer.steal()).is_some());
            }
            for value in 0..64 {
                self.victim.push(value);
            }
            assert!(
                MultiRuntime::steal_retry(|| self
                    .victim_stealer
                    .steal_batch_with_limit_and_pop(&self.local, usize::MAX))
                .is_some()
            );
            while self.local.pop().is_some() {}
        }

        /// Pushes one task into the global injector and immediately polls it.
        pub fn global_push_poll(&self, value: usize) -> usize {
            self.global.push(value);
            MultiRuntime::steal_retry(|| self.global.steal()).expect("global task missing")
        }

        /// Pushes one task into a victim deque and steals it.
        pub fn steal_one(&self, value: usize) -> usize {
            self.victim.push(value);
            MultiRuntime::steal_retry(|| self.victim_stealer.steal()).expect("stolen task missing")
        }

        /// Steals a batch from the victim deque, returns the first task, and
        /// drains the transferred local tasks.
        pub fn steal_batch_transfer_and_drain(&self, base: usize, count: usize) -> usize {
            for value in base..base + count {
                self.victim.push(value);
            }
            let mut sum = MultiRuntime::steal_retry(|| {
                self.victim_stealer
                    .steal_batch_with_limit_and_pop(&self.local, count)
            })
            .expect("stolen batch missing");
            while let Some(value) = self.local.pop() {
                sum += value;
            }
            sum
        }
    }

    impl Default for RawSchedulerQueues {
        fn default() -> Self {
            Self::new()
        }
    }

    /// Minimal fixture for measuring tenant-partition selection mechanics
    /// without coroutine, join-handle, worker-thread, or OS wakeup costs.
    pub struct RawSfqQueues {
        queues: Vec<VecDeque<usize>>,
    }

    impl RawSfqQueues {
        /// Creates prewarmed candidate SFQ queues.
        pub fn new(partitions: usize) -> Self {
            assert!(
                partitions != 0,
                "SFQ fixture requires at least one partition"
            );
            let mut queues = (0..partitions)
                .map(|_| VecDeque::with_capacity(64))
                .collect::<Vec<_>>();
            for (partition, queue) in queues.iter_mut().enumerate() {
                for value in 0..partition.min(8) {
                    queue.push_back(value);
                }
            }
            Self { queues }
        }

        /// Enqueues and dequeues using one tenant-hash-selected partition.
        pub fn single_choice_roundtrip(&mut self, tenant: u64, value: usize) -> usize {
            let partition = self.partition(tenant, 0);
            self.roundtrip(partition, value)
        }

        /// Enqueues into the shortest partition from a tenant-hash-selected subset.
        pub fn random_subset_shortest_roundtrip(
            &mut self,
            tenant: u64,
            subset: usize,
            value: usize,
        ) -> usize {
            let partition = self.shortest_subset_partition(tenant, subset.max(1), |_| 0);
            self.roundtrip(partition, value)
        }

        /// Enqueues into the globally shortest partition.
        pub fn global_shortest_roundtrip(&mut self, value: usize) -> usize {
            let partition = self
                .queues
                .iter()
                .enumerate()
                .min_by_key(|(_, queue)| queue.len())
                .map(|(partition, _)| partition)
                .expect("SFQ fixture has no partitions");
            self.roundtrip(partition, value)
        }

        /// Enqueues into the shortest tenant subset partition adjusted by a
        /// movement penalty when the candidate is not `home_partition`.
        pub fn movement_cost_roundtrip(
            &mut self,
            tenant: u64,
            subset: usize,
            home_partition: usize,
            movement_penalty: usize,
            value: usize,
        ) -> usize {
            let home = home_partition % self.queues.len();
            let partition = self.shortest_subset_partition(tenant, subset.max(1), |partition| {
                usize::from(partition != home) * movement_penalty
            });
            self.roundtrip(partition, value)
        }

        /// Reassigns a tenant by changing the hash epoch and returns its partition.
        pub fn reassigned_partition(&self, tenant: u64, epoch: u64) -> usize {
            self.partition(tenant, epoch)
        }

        fn shortest_subset_partition(
            &self,
            tenant: u64,
            subset: usize,
            cost: impl Fn(usize) -> usize,
        ) -> usize {
            let count = subset.min(self.queues.len());
            (0..count)
                .map(|offset| self.partition(tenant, offset as u64))
                .min_by_key(|&partition| self.queues[partition].len() + cost(partition))
                .expect("SFQ subset is empty")
        }

        fn roundtrip(&mut self, partition: usize, value: usize) -> usize {
            let queue = &mut self.queues[partition];
            queue.push_back(value);
            queue.pop_front().expect("roundtrip task missing")
        }

        fn partition(&self, tenant: u64, epoch: u64) -> usize {
            (mix64(tenant ^ epoch) as usize) % self.queues.len()
        }
    }

    impl Default for RawSfqQueues {
        fn default() -> Self {
            Self::new(16)
        }
    }

    /// Work item used by raw ready-queue candidate benchmarks.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct ReadyQueueTask {
        /// Scheduler tenant/fairness domain.
        pub tenant: u64,
        /// Opaque benchmark payload.
        pub value: usize,
    }

    impl ReadyQueueTask {
        /// Creates a benchmark task with tenant metadata.
        pub const fn new(tenant: u64, value: usize) -> Self {
            Self { tenant, value }
        }
    }

    /// Raw candidate configuration for scheduler/queue-boundary benchmarks.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct ReadyQueueConfig {
        /// Number of logical workers consuming from the candidate.
        pub workers: usize,
        /// Number of scheduler-visible subqueues/partitions.
        pub partitions: usize,
        /// Per-queue admission capacity used by correctness and stress tests.
        pub queue_capacity: usize,
    }

    impl ReadyQueueConfig {
        /// Creates a queue candidate configuration.
        pub fn new(workers: usize, partitions: usize, queue_capacity: usize) -> Self {
            assert!(workers != 0, "ready queue candidate requires workers");
            assert!(partitions != 0, "ready queue candidate requires partitions");
            assert!(
                queue_capacity != 0,
                "ready queue candidate requires non-zero capacity"
            );
            Self {
                workers,
                partitions,
                queue_capacity,
            }
        }
    }

    /// Candidate families for the scheduler/queue interface experiments.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum ReadyQueueCandidateKind {
        /// Current global injector plus worker-local crossbeam deques.
        CurrentGlobalLocal,
        /// Tenant-hashed MPMC crossbeam subqueues.
        CrossbeamQueueSubqueues,
        /// Tenant-hashed crossbeam work-stealing deque subqueues.
        CrossbeamStealSubqueues,
        /// Preallocated bounded subqueues with one mutex per ring.
        BoundedRingSubqueues,
        /// Bounded subqueues plus a non-empty bitmap to reduce empty scans.
        BitmapShardedSubqueues,
    }

    impl ReadyQueueCandidateKind {
        /// Stable benchmark/report label.
        pub const fn label(self) -> &'static str {
            match self {
                Self::CurrentGlobalLocal => "current_global_local",
                Self::CrossbeamQueueSubqueues => "crossbeam_queue_subqueues",
                Self::CrossbeamStealSubqueues => "crossbeam_steal_subqueues",
                Self::BoundedRingSubqueues => "bounded_ring_subqueues",
                Self::BitmapShardedSubqueues => "bitmap_sharded_subqueues",
            }
        }

        /// All raw candidates used by the benchmark matrix.
        pub const fn all() -> [Self; 5] {
            [
                Self::CurrentGlobalLocal,
                Self::CrossbeamQueueSubqueues,
                Self::CrossbeamStealSubqueues,
                Self::BoundedRingSubqueues,
                Self::BitmapShardedSubqueues,
            ]
        }
    }

    /// Queue mechanics boundary used by raw scheduler candidate benchmarks.
    ///
    /// The policy layer provides worker id, tenant id, and payload. Candidate
    /// queues own concurrent storage, admission, dequeue, and empty-state hints.
    pub trait ReadyQueueCandidate: Send + Sync {
        /// Candidate label used by reports and benchmark names.
        fn label(&self) -> &'static str;
        /// Number of logical consumers.
        fn workers(&self) -> usize;
        /// Number of scheduler-visible partitions.
        fn partitions(&self) -> usize;
        /// Returns the partition a tenant maps to for this candidate.
        fn partition_for(&self, tenant: u64) -> usize;
        /// Submits from worker-affine code.
        fn submit_worker(&self, worker: usize, task: ReadyQueueTask) -> bool;
        /// Submits from non-worker/root code.
        fn submit_external(&self, task: ReadyQueueTask) -> bool;
        /// Dequeues one task for `worker`.
        fn pop_worker(&self, worker: usize) -> Option<ReadyQueueTask>;
        /// Best-effort total queued task count.
        fn len_hint(&self) -> usize;
    }

    /// Builds a raw ready-queue candidate.
    pub fn build_ready_queue_candidate(
        kind: ReadyQueueCandidateKind,
        config: ReadyQueueConfig,
    ) -> Box<dyn ReadyQueueCandidate> {
        match kind {
            ReadyQueueCandidateKind::CurrentGlobalLocal => {
                Box::new(CurrentGlobalLocalQueue::new(config))
            }
            ReadyQueueCandidateKind::CrossbeamQueueSubqueues => {
                Box::new(CrossbeamQueueSubqueues::new(config))
            }
            ReadyQueueCandidateKind::CrossbeamStealSubqueues => {
                Box::new(CrossbeamStealSubqueues::new(config))
            }
            ReadyQueueCandidateKind::BoundedRingSubqueues => {
                Box::new(BoundedRingSubqueues::new(config))
            }
            ReadyQueueCandidateKind::BitmapShardedSubqueues => {
                Box::new(BitmapShardedSubqueues::new(config))
            }
        }
    }

    struct CurrentGlobalLocalQueue {
        config: ReadyQueueConfig,
        global: Injector<ReadyQueueTask>,
        workers: Vec<Mutex<CrossbeamWorker<ReadyQueueTask>>>,
        stealers: Vec<super::Stealer<ReadyQueueTask>>,
        local_depths: Vec<AtomicUsize>,
        global_depth: AtomicUsize,
        total_depth: AtomicUsize,
    }

    impl CurrentGlobalLocalQueue {
        fn new(config: ReadyQueueConfig) -> Self {
            let mut workers = Vec::with_capacity(config.workers);
            let mut stealers = Vec::with_capacity(config.workers);
            for _ in 0..config.workers {
                let worker = CrossbeamWorker::new_lifo();
                stealers.push(worker.stealer());
                workers.push(Mutex::new(worker));
            }
            Self {
                config,
                global: Injector::new(),
                workers,
                stealers,
                local_depths: (0..config.workers).map(|_| AtomicUsize::new(0)).collect(),
                global_depth: AtomicUsize::new(0),
                total_depth: AtomicUsize::new(0),
            }
        }

        fn release_local(&self, worker: usize) {
            release_counter(&self.local_depths[worker], 1);
            release_counter(&self.total_depth, 1);
        }

        fn release_global(&self) {
            release_counter(&self.global_depth, 1);
            release_counter(&self.total_depth, 1);
        }
    }

    impl ReadyQueueCandidate for CurrentGlobalLocalQueue {
        fn label(&self) -> &'static str {
            ReadyQueueCandidateKind::CurrentGlobalLocal.label()
        }

        fn workers(&self) -> usize {
            self.config.workers
        }

        fn partitions(&self) -> usize {
            self.config.workers + 1
        }

        fn partition_for(&self, tenant: u64) -> usize {
            (mix64(tenant) as usize) % self.partitions()
        }

        fn submit_worker(&self, worker: usize, task: ReadyQueueTask) -> bool {
            let worker = worker % self.config.workers;
            if !reserve_counter(&self.local_depths[worker], self.config.queue_capacity) {
                return false;
            }
            reserve_total(&self.total_depth);
            self.workers[worker]
                .lock()
                .expect("current local queue mutex poisoned")
                .push(task);
            true
        }

        fn submit_external(&self, task: ReadyQueueTask) -> bool {
            if !reserve_counter(&self.global_depth, self.config.queue_capacity) {
                return false;
            }
            reserve_total(&self.total_depth);
            self.global.push(task);
            true
        }

        fn pop_worker(&self, worker: usize) -> Option<ReadyQueueTask> {
            let worker = worker % self.config.workers;
            if let Some(task) = self.workers[worker]
                .lock()
                .expect("current local queue mutex poisoned")
                .pop()
            {
                self.release_local(worker);
                return Some(task);
            }

            if let Some(task) = MultiRuntime::steal_retry(|| self.global.steal()) {
                self.release_global();
                return Some(task);
            }

            for offset in 1..self.config.workers {
                let victim = (worker + offset) % self.config.workers;
                if let Some(task) = MultiRuntime::steal_retry(|| self.stealers[victim].steal()) {
                    self.release_local(victim);
                    return Some(task);
                }
            }
            None
        }

        fn len_hint(&self) -> usize {
            self.total_depth.load(Ordering::Relaxed)
        }
    }

    struct CrossbeamQueueSubqueues {
        config: ReadyQueueConfig,
        queues: Vec<SegQueue<ReadyQueueTask>>,
        depths: Vec<AtomicUsize>,
        cursors: Vec<AtomicUsize>,
        total_depth: AtomicUsize,
    }

    impl CrossbeamQueueSubqueues {
        fn new(config: ReadyQueueConfig) -> Self {
            Self {
                config,
                queues: (0..config.partitions).map(|_| SegQueue::new()).collect(),
                depths: (0..config.partitions)
                    .map(|_| AtomicUsize::new(0))
                    .collect(),
                cursors: (0..config.workers).map(|_| AtomicUsize::new(0)).collect(),
                total_depth: AtomicUsize::new(0),
            }
        }

        fn submit_partition(&self, partition: usize, task: ReadyQueueTask) -> bool {
            if !reserve_counter(&self.depths[partition], self.config.queue_capacity) {
                return false;
            }
            reserve_total(&self.total_depth);
            self.queues[partition].push(task);
            true
        }

        fn release_partition(&self, partition: usize) {
            release_counter(&self.depths[partition], 1);
            release_counter(&self.total_depth, 1);
        }
    }

    impl ReadyQueueCandidate for CrossbeamQueueSubqueues {
        fn label(&self) -> &'static str {
            ReadyQueueCandidateKind::CrossbeamQueueSubqueues.label()
        }

        fn workers(&self) -> usize {
            self.config.workers
        }

        fn partitions(&self) -> usize {
            self.config.partitions
        }

        fn partition_for(&self, tenant: u64) -> usize {
            hash_partition(tenant, self.config.partitions)
        }

        fn submit_worker(&self, _worker: usize, task: ReadyQueueTask) -> bool {
            self.submit_external(task)
        }

        fn submit_external(&self, task: ReadyQueueTask) -> bool {
            self.submit_partition(self.partition_for(task.tenant), task)
        }

        fn pop_worker(&self, worker: usize) -> Option<ReadyQueueTask> {
            scan_partitions(self.config.partitions, worker, &self.cursors, |partition| {
                if self.depths[partition].load(Ordering::Acquire) == 0 {
                    return None;
                }
                let task = self.queues[partition].pop()?;
                self.release_partition(partition);
                Some(task)
            })
        }

        fn len_hint(&self) -> usize {
            self.total_depth.load(Ordering::Relaxed)
        }
    }

    struct CrossbeamStealSubqueues {
        config: ReadyQueueConfig,
        queues: Vec<Mutex<CrossbeamWorker<ReadyQueueTask>>>,
        stealers: Vec<super::Stealer<ReadyQueueTask>>,
        depths: Vec<AtomicUsize>,
        cursors: Vec<AtomicUsize>,
        total_depth: AtomicUsize,
    }

    impl CrossbeamStealSubqueues {
        fn new(config: ReadyQueueConfig) -> Self {
            let mut queues = Vec::with_capacity(config.partitions);
            let mut stealers = Vec::with_capacity(config.partitions);
            for _ in 0..config.partitions {
                let queue = CrossbeamWorker::new_fifo();
                stealers.push(queue.stealer());
                queues.push(Mutex::new(queue));
            }
            Self {
                config,
                queues,
                stealers,
                depths: (0..config.partitions)
                    .map(|_| AtomicUsize::new(0))
                    .collect(),
                cursors: (0..config.workers).map(|_| AtomicUsize::new(0)).collect(),
                total_depth: AtomicUsize::new(0),
            }
        }

        fn submit_partition(&self, partition: usize, task: ReadyQueueTask) -> bool {
            if !reserve_counter(&self.depths[partition], self.config.queue_capacity) {
                return false;
            }
            reserve_total(&self.total_depth);
            self.queues[partition]
                .lock()
                .expect("crossbeam steal subqueue mutex poisoned")
                .push(task);
            true
        }

        fn release_partition(&self, partition: usize) {
            release_counter(&self.depths[partition], 1);
            release_counter(&self.total_depth, 1);
        }
    }

    impl ReadyQueueCandidate for CrossbeamStealSubqueues {
        fn label(&self) -> &'static str {
            ReadyQueueCandidateKind::CrossbeamStealSubqueues.label()
        }

        fn workers(&self) -> usize {
            self.config.workers
        }

        fn partitions(&self) -> usize {
            self.config.partitions
        }

        fn partition_for(&self, tenant: u64) -> usize {
            hash_partition(tenant, self.config.partitions)
        }

        fn submit_worker(&self, _worker: usize, task: ReadyQueueTask) -> bool {
            self.submit_external(task)
        }

        fn submit_external(&self, task: ReadyQueueTask) -> bool {
            self.submit_partition(self.partition_for(task.tenant), task)
        }

        fn pop_worker(&self, worker: usize) -> Option<ReadyQueueTask> {
            scan_partitions(self.config.partitions, worker, &self.cursors, |partition| {
                if self.depths[partition].load(Ordering::Acquire) == 0 {
                    return None;
                }
                let task = MultiRuntime::steal_retry(|| self.stealers[partition].steal())?;
                self.release_partition(partition);
                Some(task)
            })
        }

        fn len_hint(&self) -> usize {
            self.total_depth.load(Ordering::Relaxed)
        }
    }

    struct BoundedRingSubqueues {
        config: ReadyQueueConfig,
        queues: Vec<Mutex<VecDeque<ReadyQueueTask>>>,
        depths: Vec<AtomicUsize>,
        cursors: Vec<AtomicUsize>,
        total_depth: AtomicUsize,
    }

    impl BoundedRingSubqueues {
        fn new(config: ReadyQueueConfig) -> Self {
            Self {
                config,
                queues: (0..config.partitions)
                    .map(|_| Mutex::new(VecDeque::with_capacity(config.queue_capacity)))
                    .collect(),
                depths: (0..config.partitions)
                    .map(|_| AtomicUsize::new(0))
                    .collect(),
                cursors: (0..config.workers).map(|_| AtomicUsize::new(0)).collect(),
                total_depth: AtomicUsize::new(0),
            }
        }

        fn submit_partition(&self, partition: usize, task: ReadyQueueTask) -> bool {
            if !reserve_counter(&self.depths[partition], self.config.queue_capacity) {
                return false;
            }
            reserve_total(&self.total_depth);
            self.queues[partition]
                .lock()
                .expect("bounded ring subqueue mutex poisoned")
                .push_back(task);
            true
        }

        fn release_partition(&self, partition: usize) {
            release_counter(&self.depths[partition], 1);
            release_counter(&self.total_depth, 1);
        }
    }

    impl ReadyQueueCandidate for BoundedRingSubqueues {
        fn label(&self) -> &'static str {
            ReadyQueueCandidateKind::BoundedRingSubqueues.label()
        }

        fn workers(&self) -> usize {
            self.config.workers
        }

        fn partitions(&self) -> usize {
            self.config.partitions
        }

        fn partition_for(&self, tenant: u64) -> usize {
            hash_partition(tenant, self.config.partitions)
        }

        fn submit_worker(&self, _worker: usize, task: ReadyQueueTask) -> bool {
            self.submit_external(task)
        }

        fn submit_external(&self, task: ReadyQueueTask) -> bool {
            self.submit_partition(self.partition_for(task.tenant), task)
        }

        fn pop_worker(&self, worker: usize) -> Option<ReadyQueueTask> {
            scan_partitions(self.config.partitions, worker, &self.cursors, |partition| {
                if self.depths[partition].load(Ordering::Acquire) == 0 {
                    return None;
                }
                let task = self.queues[partition]
                    .lock()
                    .expect("bounded ring subqueue mutex poisoned")
                    .pop_front()?;
                self.release_partition(partition);
                Some(task)
            })
        }

        fn len_hint(&self) -> usize {
            self.total_depth.load(Ordering::Relaxed)
        }
    }

    struct BitmapShardedSubqueues {
        config: ReadyQueueConfig,
        queues: Vec<Mutex<VecDeque<ReadyQueueTask>>>,
        depths: Vec<AtomicUsize>,
        cursors: Vec<AtomicUsize>,
        non_empty: AtomicU64,
        total_depth: AtomicUsize,
    }

    impl BitmapShardedSubqueues {
        fn new(config: ReadyQueueConfig) -> Self {
            assert!(
                config.partitions <= u64::BITS as usize,
                "bitmap candidate supports at most 64 partitions"
            );
            Self {
                config,
                queues: (0..config.partitions)
                    .map(|_| Mutex::new(VecDeque::with_capacity(config.queue_capacity)))
                    .collect(),
                depths: (0..config.partitions)
                    .map(|_| AtomicUsize::new(0))
                    .collect(),
                cursors: (0..config.workers).map(|_| AtomicUsize::new(0)).collect(),
                non_empty: AtomicU64::new(0),
                total_depth: AtomicUsize::new(0),
            }
        }

        fn submit_partition(&self, partition: usize, task: ReadyQueueTask) -> bool {
            if !reserve_counter(&self.depths[partition], self.config.queue_capacity) {
                return false;
            }
            reserve_total(&self.total_depth);
            self.queues[partition]
                .lock()
                .expect("bitmap subqueue mutex poisoned")
                .push_back(task);
            self.non_empty
                .fetch_or(1_u64 << partition, Ordering::Release);
            true
        }

        fn release_partition(&self, partition: usize) {
            release_counter(&self.depths[partition], 1);
            release_counter(&self.total_depth, 1);
        }
    }

    impl ReadyQueueCandidate for BitmapShardedSubqueues {
        fn label(&self) -> &'static str {
            ReadyQueueCandidateKind::BitmapShardedSubqueues.label()
        }

        fn workers(&self) -> usize {
            self.config.workers
        }

        fn partitions(&self) -> usize {
            self.config.partitions
        }

        fn partition_for(&self, tenant: u64) -> usize {
            hash_partition(tenant, self.config.partitions)
        }

        fn submit_worker(&self, _worker: usize, task: ReadyQueueTask) -> bool {
            self.submit_external(task)
        }

        fn submit_external(&self, task: ReadyQueueTask) -> bool {
            self.submit_partition(self.partition_for(task.tenant), task)
        }

        fn pop_worker(&self, worker: usize) -> Option<ReadyQueueTask> {
            let cursor = self.cursors[worker % self.config.workers].fetch_add(1, Ordering::Relaxed);
            let start = (worker + cursor) % self.config.partitions;
            for _ in 0..self.config.partitions {
                let mask = self.non_empty.load(Ordering::Acquire);
                let partition = select_bitmap_partition(mask, start, self.config.partitions)?;
                let mut queue = self.queues[partition]
                    .lock()
                    .expect("bitmap subqueue mutex poisoned");
                if let Some(task) = queue.pop_front() {
                    if queue.is_empty() {
                        self.non_empty
                            .fetch_and(!(1_u64 << partition), Ordering::Release);
                    }
                    drop(queue);
                    self.release_partition(partition);
                    return Some(task);
                }
                self.non_empty
                    .fetch_and(!(1_u64 << partition), Ordering::Release);
            }
            None
        }

        fn len_hint(&self) -> usize {
            self.total_depth.load(Ordering::Relaxed)
        }
    }

    fn scan_partitions(
        partitions: usize,
        worker: usize,
        cursors: &[AtomicUsize],
        mut pop_partition: impl FnMut(usize) -> Option<ReadyQueueTask>,
    ) -> Option<ReadyQueueTask> {
        let cursor = cursors[worker % cursors.len()].fetch_add(1, Ordering::Relaxed);
        let start = (worker + cursor) % partitions;
        for offset in 0..partitions {
            if let Some(task) = pop_partition((start + offset) % partitions) {
                return Some(task);
            }
        }
        None
    }

    fn select_bitmap_partition(mask: u64, start: usize, partitions: usize) -> Option<usize> {
        if mask == 0 {
            return None;
        }
        for offset in 0..partitions {
            let partition = (start + offset) % partitions;
            if mask & (1_u64 << partition) != 0 {
                return Some(partition);
            }
        }
        None
    }

    fn hash_partition(tenant: u64, partitions: usize) -> usize {
        (mix64(tenant) as usize) % partitions
    }

    fn reserve_counter(counter: &AtomicUsize, capacity: usize) -> bool {
        let mut current = counter.load(Ordering::Relaxed);
        loop {
            if current >= capacity {
                return false;
            }
            let next = current.checked_add(1).expect("ready queue depth overflow");
            match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    fn reserve_total(counter: &AtomicUsize) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn release_counter(counter: &AtomicUsize, count: usize) {
        let mut current = counter.load(Ordering::Relaxed);
        loop {
            let next = current
                .checked_sub(count)
                .expect("ready queue depth accounting underflow");
            match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    fn mix64(mut value: u64) -> u64 {
        value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    pub use super::StealPolicy;
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::{HashSet, VecDeque};
    use std::ffi::c_void;
    use std::num::NonZeroUsize;
    use std::os::fd::AsRawFd;
    use std::panic;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use kimojio_stack::Runtime as StackRuntime;
    use kimojio_stack::channel::cross_thread;
    use kimojio_stack::channel::{RecvError, SendError};
    use kimojio_stack::{
        IoReadBuffer, IoWriteBuffer, RuntimeCapabilities, RuntimeCapability, RuntimeFamily,
        StackfulWaitContext, StackfulWaitRegistration,
    };
    use rustix::net::{AddressFamily, Shutdown, SocketFlags, SocketType, socketpair};
    use rustix::pipe::pipe;

    use super::{
        AtFlags, CrossbeamWorker, ExecutionPlace, IoFd, IoVec, Mode, MultiRuntime, NO_WORKER,
        OFlags, QueuedJob, RenameFlags, Ring, RingError, RingFd, RingInner, RingMode, Runtime,
        RuntimeConfig, RuntimeId, SchedulerConfig, SchedulerMode, SharedOpLifecycle, SharedOpRoute,
        SharedOpState, SharedOpTestResource, SharedOperation, SharedOperationKind, SharedRingCore,
        SharedRingWake, SpawnError, StatxFlags, StealPolicy, SubmitError, TenantId, WorkerId,
    };

    struct WaitProbe<'cx, 'a> {
        cx: &'a super::RuntimeContext<'cx>,
        parked: &'a AtomicBool,
    }

    fn wait_until(cx: &super::RuntimeContext<'_>, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "condition was not met before timeout"
            );
            cx.yield_now();
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn sfq_private_runtime(partitions: usize) -> (MultiRuntime, CrossbeamWorker<QueuedJob>) {
        let local = CrossbeamWorker::new_lifo();
        let stealer = local.stealer();
        let config = RuntimeConfig {
            workers: NonZeroUsize::new(1).unwrap(),
            scheduler: SchedulerConfig {
                mode: SchedulerMode::StochasticFair,
                ready_partitions: NonZeroUsize::new(partitions).unwrap(),
                ..SchedulerConfig::default()
            },
            ..RuntimeConfig::default()
        };
        (
            MultiRuntime::with_runtime_id(config, RuntimeId::next(), vec![stealer]),
            local,
        )
    }

    #[test]
    fn sfq_scheduler_dequeues_same_tenant_fifo_within_partition() {
        let (runtime, _local) = sfq_private_runtime(1);
        let tenant = TenantId(9);
        for sequence in 0..4 {
            assert!(
                runtime
                    .submit_sfq(QueuedJob::new_for_test(WorkerId(0), tenant, sequence))
                    .is_ok()
            );
        }

        let observed = (0..4)
            .map(|_| {
                runtime
                    .take_sfq_job(WorkerId(0), 0)
                    .expect("missing SFQ job")
                    .queued
                    .sequence
            })
            .collect::<Vec<_>>();
        assert_eq!(observed, vec![0, 1, 2, 3]);
    }

    #[test]
    fn sfq_scheduler_submit_rejects_after_shutdown() {
        let (runtime, _local) = sfq_private_runtime(2);
        runtime.shutdown.store(true, Ordering::Release);

        match runtime.submit(WorkerId(0), TenantId(3), Box::new(|_| {}), None) {
            Err(SubmitError::ShuttingDown) => {}
            Err(SubmitError::QueueFull) => panic!("expected shutdown rejection, got queue full"),
            Ok(()) => panic!("shutdown submission unexpectedly succeeded"),
        }
        assert_eq!(runtime.metrics.rejected_tasks.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn ready_queue_candidate_basic_semantics_match_interface() {
        for kind in super::bench_support::ReadyQueueCandidateKind::all() {
            let queue = super::bench_support::build_ready_queue_candidate(
                kind,
                super::bench_support::ReadyQueueConfig::new(4, 8, 16),
            );
            assert_eq!(queue.label(), kind.label());
            assert_eq!(queue.workers(), 4);
            assert!(queue.partitions() >= 1);
            assert_eq!(queue.partition_for(42), queue.partition_for(42));
            assert!(queue.partition_for(42) < queue.partitions());

            assert!(queue.submit_worker(0, super::bench_support::ReadyQueueTask::new(1, 10)));
            assert!(queue.submit_external(super::bench_support::ReadyQueueTask::new(2, 20)));
            assert_eq!(queue.len_hint(), 2);

            let mut values = Vec::new();
            for worker in 0..queue.workers() {
                while let Some(task) = queue.pop_worker(worker) {
                    values.push(task.value);
                }
            }
            values.sort_unstable();
            assert_eq!(values, vec![10, 20], "candidate {kind:?}");
            assert_eq!(queue.len_hint(), 0, "candidate {kind:?}");
        }
    }

    #[test]
    fn ready_queue_candidate_capacity_rejection_and_reuse() {
        for kind in super::bench_support::ReadyQueueCandidateKind::all() {
            let queue = super::bench_support::build_ready_queue_candidate(
                kind,
                super::bench_support::ReadyQueueConfig::new(2, 4, 1),
            );
            assert!(
                queue.submit_external(super::bench_support::ReadyQueueTask::new(7, 1)),
                "candidate {kind:?} rejected first task"
            );
            assert!(
                !queue.submit_external(super::bench_support::ReadyQueueTask::new(7, 2)),
                "candidate {kind:?} accepted beyond capacity"
            );
            assert_eq!(
                queue.pop_worker(0).map(|task| task.value),
                Some(1),
                "candidate {kind:?} lost queued task"
            );
            assert!(
                queue.submit_external(super::bench_support::ReadyQueueTask::new(7, 3)),
                "candidate {kind:?} failed to reuse released capacity"
            );
            assert_eq!(queue.pop_worker(1).map(|task| task.value), Some(3));
            assert_eq!(queue.len_hint(), 0, "candidate {kind:?}");
        }
    }

    #[test]
    fn ready_queue_candidate_no_lost_tasks_under_mixed_submission() {
        for kind in super::bench_support::ReadyQueueCandidateKind::all() {
            let queue = super::bench_support::build_ready_queue_candidate(
                kind,
                super::bench_support::ReadyQueueConfig::new(4, 8, 128),
            );
            for value in 0..64 {
                let task = super::bench_support::ReadyQueueTask::new((value % 9 + 1) as u64, value);
                let accepted = if value % 2 == 0 {
                    queue.submit_worker(value % queue.workers(), task)
                } else {
                    queue.submit_external(task)
                };
                assert!(accepted, "candidate {kind:?} rejected value {value}");
            }

            let mut observed = Vec::new();
            let mut idle_rounds = 0;
            while observed.len() < 64 && idle_rounds < queue.workers() * 4 {
                let before = observed.len();
                for worker in 0..queue.workers() {
                    while let Some(task) = queue.pop_worker(worker) {
                        observed.push(task.value);
                    }
                }
                if observed.len() == before {
                    idle_rounds += 1;
                }
            }
            observed.sort_unstable();
            assert_eq!(observed, (0..64).collect::<Vec<_>>(), "candidate {kind:?}");
            assert_eq!(queue.len_hint(), 0, "candidate {kind:?}");
        }
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

        let mut path = std::env::temp_dir();
        path.push(format!(
            "kimojio-stack-steal-{prefix}-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        path
    }

    struct DropCountingReadBuffer {
        bytes: Vec<u8>,
        drops: Arc<AtomicUsize>,
    }

    impl DropCountingReadBuffer {
        fn new(len: usize, drops: Arc<AtomicUsize>) -> Self {
            Self {
                bytes: vec![0; len],
                drops,
            }
        }
    }

    impl Drop for DropCountingReadBuffer {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::AcqRel);
        }
    }

    unsafe impl IoReadBuffer for DropCountingReadBuffer {
        fn io_buffer_mut_ptr(&mut self) -> *mut u8 {
            self.bytes.as_mut_ptr()
        }

        fn io_buffer_len(&self) -> usize {
            self.bytes.len()
        }
    }

    struct DropCountingWriteBuffer {
        bytes: Vec<u8>,
        drops: Arc<AtomicUsize>,
    }

    impl DropCountingWriteBuffer {
        fn new(len: usize, drops: Arc<AtomicUsize>) -> Self {
            Self {
                bytes: vec![b'x'; len],
                drops,
            }
        }
    }

    impl Drop for DropCountingWriteBuffer {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::AcqRel);
        }
    }

    unsafe impl IoWriteBuffer for DropCountingWriteBuffer {
        fn io_buffer_ptr(&self) -> *const u8 {
            self.bytes.as_ptr()
        }

        fn io_buffer_len(&self) -> usize {
            self.bytes.len()
        }
    }

    fn fill_pipe(write_fd: &impl AsRawFd) {
        let raw_fd = write_fd.as_raw_fd();
        let flags = unsafe { libc::fcntl(raw_fd, libc::F_GETFL) };
        assert!(flags >= 0, "F_GETFL failed");
        let set_nonblocking =
            unsafe { libc::fcntl(raw_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        assert!(set_nonblocking >= 0, "F_SETFL O_NONBLOCK failed");

        let chunk = [0_u8; 4096];
        loop {
            let written = unsafe { libc::write(raw_fd, chunk.as_ptr().cast(), chunk.len()) };
            if written >= 0 {
                continue;
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EAGAIN) {
                break;
            }
            panic!("pipe fill write failed: {error}");
        }

        let restore = unsafe { libc::fcntl(raw_fd, libc::F_SETFL, flags) };
        assert!(restore >= 0, "F_SETFL restore failed");
    }

    impl RuntimeCapabilities for WaitProbe<'_, '_> {
        fn runtime_family(&self) -> RuntimeFamily {
            self.cx.runtime_family()
        }

        fn supports(&self, capability: RuntimeCapability) -> bool {
            self.cx.supports(capability)
        }
    }

    impl StackfulWaitContext for WaitProbe<'_, '_> {
        fn stackful_wait_registration(&self) -> Option<Box<dyn StackfulWaitRegistration + '_>> {
            self.cx.stackful_wait_registration()
        }

        fn park_stackful(&self) {
            self.parked.store(true, Ordering::Release);
            self.cx.park_stackful();
        }
    }

    #[test]
    fn direct_context_positioned_filesystem_and_vectored_io_parity() {
        let root = unique_temp_dir("direct-fs");
        let file = root.join("file");
        let renamed = root.join("renamed");
        let _ = std::fs::remove_dir_all(&root);

        let mut runtime = Runtime::new();
        runtime.block_on(|cx| {
            cx.mkdir(&root, Mode::RWXU).unwrap();
            let fd = cx
                .open(
                    &file,
                    OFlags::CREATE | OFlags::RDWR | OFlags::TRUNC,
                    Mode::RUSR | Mode::WUSR,
                )
                .unwrap();
            assert_eq!(cx.pwrite(&fd, b"world", 5).unwrap(), 5);
            assert_eq!(cx.pwrite(&fd, b"hello", 0).unwrap(), 5);
            let stat = cx.fstat(&fd, StatxFlags::BASIC_STATS).unwrap();
            assert_eq!(stat.stx_size, 10);
            let mut prefix = [0_u8; 5];
            assert_eq!(cx.read(&fd, &mut prefix).unwrap(), 5);
            assert_eq!(&prefix, b"hello");
            let mut suffix = [0_u8; 5];
            assert_eq!(cx.pread(&fd, &mut suffix, 5).unwrap(), 5);
            assert_eq!(&suffix, b"world");
            cx.ftruncate(&fd, 3).unwrap();
            cx.fsync(&fd).unwrap();
            let stat = cx
                .statx(&file, AtFlags::empty(), StatxFlags::BASIC_STATS)
                .unwrap();
            assert_eq!(stat.stx_size, 3);
            cx.close(fd).unwrap();
            cx.rename(&file, &renamed, RenameFlags::empty()).unwrap();
            cx.unlink(&renamed).unwrap();

            let (read_fd, write_fd) = pipe().unwrap();
            let left = b"vec";
            let right = b"tor";
            let write_iov = [
                IoVec {
                    iov_base: left.as_ptr().cast::<c_void>().cast_mut(),
                    iov_len: left.len(),
                },
                IoVec {
                    iov_base: right.as_ptr().cast::<c_void>().cast_mut(),
                    iov_len: right.len(),
                },
            ];
            assert_eq!(cx.writev(&write_fd, &write_iov).unwrap(), 6);

            let mut first = [0_u8; 3];
            let mut second = [0_u8; 3];
            let mut read_iov = [
                IoVec {
                    iov_base: first.as_mut_ptr().cast::<c_void>(),
                    iov_len: first.len(),
                },
                IoVec {
                    iov_base: second.as_mut_ptr().cast::<c_void>(),
                    iov_len: second.len(),
                },
            ];
            assert_eq!(cx.readv(&read_fd, &mut read_iov).unwrap(), 6);
            assert_eq!([first.as_slice(), second.as_slice()].concat(), b"vector");
            cx.close(read_fd).unwrap();
            cx.close(write_fd).unwrap();

            cx.rmdir(&root).unwrap();
        });

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn direct_context_async_registered_and_socket_io_parity() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            registered_file_slots: 1,
            registered_buffer_slots: 1,
            ..RuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            let (read_fd, write_fd) = pipe().unwrap();
            let read_io = IoFd::from_owned(read_fd);
            let write_io = IoFd::from_owned(write_fd);
            let read = cx.read_async(&read_io, vec![0_u8; 4]);
            let write = cx.write_async(&write_io, b"ping".to_vec());

            let written = write.get(cx).unwrap();
            assert_eq!(written.bytes, 4);
            let read = read.get(cx).unwrap();
            assert_eq!(read.bytes, 4);
            assert_eq!(&read.buffer[..read.bytes], b"ping");
            cx.close(read_io.into_owned().unwrap()).unwrap();
            cx.close(write_io.into_owned().unwrap()).unwrap();

            let (read_fd, write_fd) = pipe().unwrap();
            let registered_fd = cx.register_fd(write_fd).unwrap();
            let mut registered_buffer = cx.register_buffer(vec![0_u8; 5]).unwrap();
            assert_eq!(cx.write_registered_fd(&registered_fd, b"hello").unwrap(), 5);
            assert_eq!(
                cx.read_registered_buffer(&read_fd, &mut registered_buffer)
                    .unwrap(),
                5
            );
            assert_eq!(&registered_buffer.buffer()[..5], b"hello");
            cx.close(read_fd).unwrap();

            let (left, right) = socketpair(
                AddressFamily::UNIX,
                SocketType::STREAM,
                SocketFlags::CLOEXEC,
                None,
            )
            .unwrap();
            assert_eq!(cx.send(&left, b"ok").unwrap(), 2);
            let mut buf = [0_u8; 2];
            assert_eq!(cx.recv(&right, &mut buf).unwrap(), 2);
            assert_eq!(&buf, b"ok");
            cx.shutdown(&left, Shutdown::Write).unwrap();
            cx.close(left).unwrap();
            cx.close(right).unwrap();
        });
    }

    #[test]
    fn multi_worker_direct_registered_slots_are_forwarded_to_workers() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            registered_file_slots: 1,
            registered_buffer_slots: 1,
            ..RuntimeConfig::default()
        });

        let worker = runtime.block_on(|cx| {
            cx.scope(|scope| {
                let handle = scope.spawn_stealable(|cx| {
                    let (read_fd, write_fd) = pipe().unwrap();
                    let registered_fd = cx.register_fd(write_fd).unwrap();
                    let mut registered_buffer = cx.register_buffer(vec![0_u8; 4]).unwrap();

                    assert_eq!(cx.write_registered_fd(&registered_fd, b"pong").unwrap(), 4);
                    assert_eq!(
                        cx.read_registered_buffer(&read_fd, &mut registered_buffer)
                            .unwrap(),
                        4
                    );
                    assert_eq!(&registered_buffer.buffer()[..4], b"pong");
                    cx.close(read_fd).unwrap();
                    cx.worker_id()
                });
                handle.join(cx)
            })
        });

        assert!(worker.index() < 2);
    }

    #[test]
    fn block_on_returns_value() {
        let mut runtime = Runtime::new();

        let output = runtime.block_on(|cx| {
            assert_eq!(cx.worker_id(), WorkerId(0));
            42
        });

        assert_eq!(output, 42);
    }

    #[test]
    fn scoped_spawn_returns_values() {
        let mut runtime = Runtime::new();

        let output = runtime.block_on(|cx| {
            cx.scope(|scope| {
                let left = scope.spawn(|_| 20);
                let right = scope.spawn(|_| 22);
                left.join(cx) + right.join(cx)
            })
        });

        assert_eq!(output, 42);
    }

    #[test]
    fn nested_scopes_finish_before_parent_continues() {
        let mut runtime = Runtime::new();

        let output = runtime.block_on(|cx| {
            cx.scope(|outer| {
                let task = outer.spawn(|cx| cx.scope(|inner| inner.spawn(|_| 5).join(cx)));
                task.join(cx) + 1
            })
        });

        assert_eq!(output, 6);
    }

    #[test]
    fn yield_now_reschedules_task() {
        let mut runtime = Runtime::new();

        let output = runtime.block_on(|cx| {
            cx.scope(|scope| {
                let first = scope.spawn(|cx| {
                    cx.yield_now();
                    1
                });
                let second = scope.spawn(|_| 2);
                first.join(cx) + second.join(cx)
            })
        });

        assert_eq!(output, 3);
    }

    #[test]
    fn joined_panic_propagates_to_joiner() {
        let result = panic::catch_unwind(|| {
            let mut runtime = Runtime::new();
            runtime.block_on(|cx| {
                cx.scope(|scope| {
                    let handle = scope.spawn(|_| -> () { panic!("task failed") });
                    handle.join(cx);
                });
            });
        });

        let error = result.expect_err("joined task panic should propagate");
        assert!(
            error
                .downcast_ref::<&'static str>()
                .is_some_and(|message| *message == "task failed")
        );
    }

    #[test]
    fn local_non_send_work_stays_on_owner_worker() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(4).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });
        let value = Rc::new(Cell::new(0));

        let output = runtime.block_on(|cx| {
            assert_eq!(cx.current_worker(), None);
            assert_eq!(cx.execution_place(), ExecutionPlace::Root);
            cx.scope(|scope| {
                let value = Rc::clone(&value);
                let handle = scope.spawn_local(move |cx| {
                    value.set(7);
                    assert_eq!(cx.current_worker(), None);
                    cx.worker_id()
                });
                handle.join(cx)
            })
        });

        assert_eq!(output, WorkerId(NO_WORKER));
        assert_eq!(value.get(), 7);
    }

    #[test]
    fn worker_context_reports_explicit_execution_place() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            assert_eq!(cx.current_worker(), None);
            assert_eq!(cx.execution_place(), ExecutionPlace::Root);
            cx.scope(|scope| {
                let handle = scope.spawn_stealable(|cx| {
                    let worker = cx.current_worker().expect("stealable work runs on worker");
                    assert!(worker.index() < 2);
                    assert_eq!(cx.execution_place(), ExecutionPlace::Worker(worker));
                });
                handle.join(cx);
            });
        });
    }

    #[test]
    fn stealable_work_can_execute_on_non_owner_workers() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(4).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });

        let workers = runtime.block_on(|cx| {
            cx.scope(|scope| {
                let handles = (0..64)
                    .map(|_| {
                        scope.spawn_stealable(|cx| {
                            std::thread::sleep(Duration::from_millis(1));
                            cx.worker_id()
                        })
                    })
                    .collect::<Vec<_>>();
                handles
                    .into_iter()
                    .map(|handle| handle.join(cx))
                    .collect::<HashSet<_>>()
            })
        });

        assert!(
            workers.iter().any(|worker| worker.index() != 0),
            "expected at least one stolen task, got {workers:?}"
        );
        assert!(runtime.metrics().global_queue_polls != 0);
        assert!(runtime.metrics().max_global_queue_depth != 0);
        assert!(runtime.metrics().last_owner_worker.is_some());
        assert!(runtime.metrics().last_executing_worker.is_some());
    }

    #[test]
    fn runtime_metrics_backpressure_summarizes_queueing_and_worker_balance() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let handles = (0..16)
                    .map(|_| {
                        scope.spawn_stealable(|cx| {
                            cx.yield_now();
                            cx.worker_id()
                        })
                    })
                    .collect::<Vec<_>>();
                for handle in handles {
                    handle.join(cx);
                }
            });
        });

        let diagnostics = runtime.metrics().backpressure();
        assert!(diagnostics.queued_work_observed);
        assert_ne!(diagnostics.ready_wait_samples, 0);
        assert!(diagnostics.average_ready_wait_ns.is_some());
        assert_eq!(diagnostics.rejected_tasks, 0);
        assert_eq!(
            diagnostics.max_queue_depth,
            diagnostics
                .max_global_queue_depth
                .max(diagnostics.max_local_queue_depth)
                .max(diagnostics.max_sfq_partition_depth)
        );
    }

    #[test]
    fn try_spawn_stealable_reports_queue_saturation_synchronously() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            max_worker_queue_len: 0,
            max_global_queue_len: 1,
            ..RuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let first = scope
                    .try_spawn_stealable(|_| {
                        std::thread::sleep(Duration::from_millis(50));
                        1
                    })
                    .unwrap();
                let rejected = scope.try_spawn_stealable(|_| ());
                assert!(matches!(rejected, Err(SpawnError::QueueFull)));
                assert_eq!(first.join(cx), 1);
            });
        });

        assert_eq!(runtime.metrics().rejected_tasks, 1);
    }

    #[test]
    fn stealable_handle_reports_start_observation() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(1).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            max_global_queue_len: 8,
            ..RuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let handle = scope.spawn_stealable(|cx| {
                    cx.yield_now();
                    7
                });
                handle.wait_started(cx);
                assert!(handle.has_started());
                assert_eq!(handle.join(cx), 7);
            });
        });
    }

    #[test]
    fn disabled_policy_multi_worker_drains_global_injected_work() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::Disabled,
            ..RuntimeConfig::default()
        });

        let output = runtime.block_on(|cx| {
            cx.scope(|scope| {
                let handle = scope.spawn_stealable(|cx| cx.worker_id());
                handle.join(cx)
            })
        });

        assert!(output.index() < 2);
        assert_eq!(runtime.metrics().successful_steals, 0);
        assert_ne!(runtime.metrics().global_queue_polls, 0);
        assert_eq!(
            runtime
                .metrics()
                .worker_completed_tasks
                .iter()
                .sum::<usize>(),
            1
        );
    }

    #[test]
    fn disabled_policy_nested_stealable_work_completes() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::Disabled,
            ..RuntimeConfig::default()
        });

        let output = runtime.block_on(|cx| {
            cx.scope(|scope| {
                let outer = scope.spawn_stealable(|cx| {
                    cx.scope(|scope| {
                        let inner = scope.spawn_stealable(|_| 41);
                        inner.join(cx) + 1
                    })
                });
                outer.join(cx)
            })
        });

        assert_eq!(output, 42);
        assert_eq!(runtime.metrics().successful_steals, 0);
        assert_eq!(
            runtime
                .metrics()
                .worker_completed_tasks
                .iter()
                .sum::<usize>(),
            2
        );
    }

    #[test]
    fn nested_stealable_channel_dependencies_do_not_starve_worker_pool() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });

        let total = runtime.block_on(|cx| {
            cx.scope(|scope| {
                let handles = (0..2)
                    .map(|value| {
                        scope.spawn_stealable(move |cx| {
                            let (tx, rx) = cross_thread::bounded(1).stackful();
                            cx.scope(|scope| {
                                let receiver =
                                    scope.spawn_stealable(move |cx| rx.recv_with(cx).unwrap());
                                let sender = scope
                                    .spawn_stealable(move |cx| tx.send_with(cx, value).unwrap());
                                sender.join(cx);
                                receiver.join(cx)
                            })
                        })
                    })
                    .collect::<Vec<_>>();
                handles
                    .into_iter()
                    .map(|handle| handle.join(cx))
                    .sum::<i32>()
            })
        });

        assert_eq!(total, 1);
    }

    #[test]
    fn scope_exit_drains_local_children_before_waiting_for_stealable_children() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });

        let receiver_ran = Arc::new(AtomicBool::new(false));
        let ran = Arc::clone(&receiver_ran);

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let (tx, rx) = cross_thread::bounded(1).stackful();
                let _receiver = scope.spawn_stealable(move |cx| {
                    let value = rx.recv_with(cx).unwrap();
                    ran.store(true, Ordering::Release);
                    value
                });
                let _sender = scope.spawn(move |cx| tx.send_with(cx, 42).unwrap());
            });
        });

        assert!(receiver_ran.load(Ordering::Acquire));
    }

    #[test]
    fn worker_submitted_stealable_work_can_be_stolen_from_local_queue() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(4).unwrap(),
            steal_policy: StealPolicy::steal_half(),
            ..RuntimeConfig::default()
        });

        let non_owner_workers = runtime.block_on(|cx| {
            cx.scope(|scope| {
                let outer = scope.spawn_stealable(|cx| {
                    let owner = cx.worker_id();
                    cx.scope(|scope| {
                        let handles = (0..64)
                            .map(|_| {
                                scope.spawn_stealable(|cx| {
                                    std::thread::sleep(Duration::from_millis(1));
                                    cx.worker_id()
                                })
                            })
                            .collect::<Vec<_>>();
                        handles
                            .into_iter()
                            .map(|handle| handle.join(cx))
                            .filter(|worker| *worker != owner)
                            .count()
                    })
                });
                outer.join(cx)
            })
        });

        assert_ne!(non_owner_workers, 0);
        assert!(runtime.metrics().successful_steals != 0);
        assert!(runtime.metrics().last_steal_victim.is_some());
        assert!(runtime.metrics().max_local_queue_depth != 0);
    }

    #[test]
    fn idle_steal_scan_visits_every_victim_before_sleeping() {
        let workers = (0..4)
            .map(|_| CrossbeamWorker::new_lifo())
            .collect::<Vec<_>>();
        let stealers = workers
            .iter()
            .map(CrossbeamWorker::stealer)
            .collect::<Vec<_>>();
        let runtime = MultiRuntime::with_runtime_id(
            RuntimeConfig {
                workers: NonZeroUsize::new(4).unwrap(),
                steal_policy: StealPolicy::steal_one(),
                ..RuntimeConfig::default()
            },
            RuntimeId::next(),
            stealers,
        );

        assert_eq!(
            runtime.victims(WorkerId(0), 0).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(
            runtime.victims(WorkerId(0), 1).collect::<Vec<_>>(),
            vec![2, 3, 1]
        );
        assert_eq!(
            runtime.victims(WorkerId(2), 2).collect::<Vec<_>>(),
            vec![1, 3, 0]
        );
    }

    #[test]
    fn global_queue_depth_forces_poll_before_idle_sleep() {
        let workers = (0..2)
            .map(|_| CrossbeamWorker::new_lifo())
            .collect::<Vec<_>>();
        let stealers = workers
            .iter()
            .map(CrossbeamWorker::stealer)
            .collect::<Vec<_>>();
        let runtime = MultiRuntime::with_runtime_id(
            RuntimeConfig {
                workers: NonZeroUsize::new(2).unwrap(),
                steal_policy: StealPolicy::StealBatch {
                    batch: NonZeroUsize::new(1).unwrap(),
                    global_queue_interval: NonZeroUsize::new(usize::MAX).unwrap(),
                },
                ..RuntimeConfig::default()
            },
            RuntimeId::next(),
            stealers,
        );

        assert!(!runtime.should_poll_global(1));
        runtime.global_depth.store(1, Ordering::Relaxed);
        assert!(runtime.should_poll_global(1));
    }

    #[test]
    fn idle_steal_skips_crossbeam_scan_when_no_local_work_exists() {
        let workers = (0..4)
            .map(|_| CrossbeamWorker::new_lifo())
            .collect::<Vec<_>>();
        let stealers = workers
            .iter()
            .map(CrossbeamWorker::stealer)
            .collect::<Vec<_>>();
        let runtime = MultiRuntime::with_runtime_id(
            RuntimeConfig {
                workers: NonZeroUsize::new(4).unwrap(),
                steal_policy: StealPolicy::steal_one(),
                ..RuntimeConfig::default()
            },
            RuntimeId::next(),
            stealers,
        );

        assert!(runtime.take_job(WorkerId(0), &workers[0], 0).is_none());
        assert_eq!(runtime.metrics.steal_attempts.load(Ordering::Relaxed), 0);
        assert_eq!(runtime.metrics.failed_steals.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn steal_policy_global_queue_intervals_are_distinct() {
        assert_eq!(
            StealPolicy::steal_one()
                .global_queue_interval()
                .map(NonZeroUsize::get),
            Some(1)
        );
        assert_eq!(
            StealPolicy::steal_half()
                .global_queue_interval()
                .map(NonZeroUsize::get),
            Some(1)
        );
        let batch = StealPolicy::StealBatch {
            batch: NonZeroUsize::new(2).unwrap(),
            global_queue_interval: NonZeroUsize::new(3).unwrap(),
        };
        assert_eq!(
            batch.global_queue_interval().map(NonZeroUsize::get),
            Some(3)
        );
    }

    #[test]
    fn steal_policies_complete_work_and_report_config() {
        for policy in [
            StealPolicy::steal_one(),
            StealPolicy::steal_half(),
            StealPolicy::StealBatch {
                batch: NonZeroUsize::new(2).unwrap(),
                global_queue_interval: NonZeroUsize::new(2).unwrap(),
            },
        ] {
            let mut runtime = Runtime::with_config(RuntimeConfig {
                workers: NonZeroUsize::new(3).unwrap(),
                steal_policy: policy,
                ..RuntimeConfig::default()
            });
            let total = runtime.block_on(|cx| {
                cx.scope(|scope| {
                    let handles = (0..12)
                        .map(|value| scope.spawn_stealable(move |_| value))
                        .collect::<Vec<_>>();
                    handles
                        .into_iter()
                        .map(|handle| handle.join(cx))
                        .sum::<usize>()
                })
            });

            assert_eq!(total, 66);
            assert_eq!(runtime.metrics().steal_policy, policy);
            assert_eq!(runtime.metrics().completed_tasks, 12);
        }
    }

    #[test]
    fn stolen_task_panic_propagates_to_joiner() {
        let result = panic::catch_unwind(|| {
            let mut runtime = Runtime::with_config(RuntimeConfig {
                workers: NonZeroUsize::new(2).unwrap(),
                steal_policy: StealPolicy::steal_one(),
                ..RuntimeConfig::default()
            });
            runtime.block_on(|cx| {
                cx.scope(|scope| {
                    let handle = scope.spawn_stealable(|_| {
                        panic!("stolen task failed");
                    });
                    handle.join(cx);
                });
            });
        });

        let error = result.expect_err("joined stolen task panic should propagate");
        assert!(
            error
                .downcast_ref::<&'static str>()
                .is_some_and(|message| *message == "stolen task failed")
        );
    }

    #[test]
    fn scope_waits_for_unjoined_stealable_work() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });
        let finished = Arc::new(AtomicBool::new(false));

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let finished = Arc::clone(&finished);
                let _handle = scope.spawn_stealable(move |_| {
                    std::thread::sleep(Duration::from_millis(10));
                    finished.store(true, Ordering::Release);
                });
            });
        });

        assert!(finished.load(Ordering::Acquire));
    }

    #[test]
    fn scope_exit_cancels_blocked_stealable_child() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });
        let (_tx, rx) = cross_thread::bounded::<u8>(1).stackful();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let _blocked = scope.spawn_stealable(move |cx| {
                    let _ = rx.recv_with(cx);
                });
            });
        });
    }

    #[test]
    fn parent_panic_cancels_blocked_stealable_child_before_resuming() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });
        let (_tx, rx) = cross_thread::bounded::<u8>(1).stackful();

        let result = panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(|cx| {
                cx.scope(|scope| {
                    let _blocked = scope.spawn_stealable(move |cx| {
                        let _ = rx.recv_with(cx);
                    });
                    panic!("parent failed");
                });
            });
        }));

        let error = result.expect_err("parent panic should resume");
        assert!(
            error
                .downcast_ref::<&'static str>()
                .is_some_and(|message| *message == "parent failed")
        );
    }

    #[test]
    fn unjoined_stolen_task_panic_propagates_at_scope_exit() {
        let result = panic::catch_unwind(|| {
            let mut runtime = Runtime::with_config(RuntimeConfig {
                workers: NonZeroUsize::new(2).unwrap(),
                steal_policy: StealPolicy::steal_one(),
                ..RuntimeConfig::default()
            });
            runtime.block_on(|cx| {
                cx.scope(|scope| {
                    let _handle = scope.spawn_stealable(|_| -> () { panic!("unjoined stolen") });
                });
            });
        });

        let error = result.expect_err("unjoined stolen task panic should propagate");
        assert!(
            error
                .downcast_ref::<&'static str>()
                .is_some_and(|message| *message == "unjoined stolen")
        );
    }

    #[test]
    fn stealable_spawn_accepts_send_closure_and_return() {
        let mut runtime = Runtime::new();

        let output = runtime.block_on(|cx| {
            cx.scope(|scope| {
                let input = String::from("stealable");
                let handle = scope.spawn_stealable(move |cx| (cx.worker_id(), input.len()));
                handle.join(cx)
            })
        });

        assert_eq!(output, (WorkerId(0), 9));
    }

    #[test]
    fn one_worker_runtime_accepts_disabled_stealing_policy() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            stack_size: 64 * 1024,
            workers: NonZeroUsize::new(1).unwrap(),
            steal_policy: StealPolicy::Disabled,
            ..RuntimeConfig::default()
        });

        assert_eq!(runtime.block_on(|_| 11), 11);
    }

    #[test]
    fn runtime_context_reports_capabilities() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            assert!(cx.supports(RuntimeCapability::StackfulWait));
            assert!(cx.supports(RuntimeCapability::ExplicitRingIo));
            cx.require_capability(RuntimeCapability::ExplicitRingIo)
                .unwrap();
        });
    }

    #[test]
    fn worker_ring_nop_and_timeout_complete() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let ring = cx.create_ring(RingMode::WorkerLocal).unwrap();
            assert_eq!(ring.mode(), RingMode::WorkerLocal);
            assert_eq!(ring.owner(), Some(WorkerId(0)));
            ring.nop(cx).unwrap();
            ring.sleep(cx, Duration::from_millis(1)).unwrap();
        });
    }

    #[test]
    fn shared_ring_can_be_used_from_stealable_worker() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            let ring = cx.create_shared_ring();
            assert_eq!(ring.mode(), RingMode::Shared);
            cx.scope(|scope| {
                let handle = scope.spawn_stealable(move |cx| ring.nop(cx));
                handle.join(cx).unwrap();
            });
        });
    }

    #[test]
    fn worker_ring_socket_read_write_and_async_handles_complete() {
        let (read_fd, write_fd) = pipe().unwrap();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let ring = cx.create_worker_ring();
            let read_fd = RingFd::from_owned(read_fd);
            let write_fd = RingFd::from_owned(write_fd);

            assert_eq!(ring.write(cx, &write_fd, b"ab").unwrap(), 2);
            let mut buffer = [0_u8; 2];
            assert_eq!(ring.read(cx, &read_fd, &mut buffer).unwrap(), 2);
            assert_eq!(&buffer, b"ab");

            let read = ring.read_async(cx, &read_fd, vec![0_u8; 2]).unwrap();
            let write = ring.write_async(cx, &write_fd, b"cd".to_vec()).unwrap();

            let written = write.get(cx).unwrap();
            assert_eq!(written.bytes, 2);
            assert_eq!(written.buffer, b"cd");

            let read = read.get(cx).unwrap();
            assert_eq!(read.bytes, 2);
            assert_eq!(&read.buffer[..read.bytes], b"cd");

            ring.close(cx, read_fd).unwrap();
            ring.close(cx, write_fd).unwrap();
        });
    }

    #[test]
    fn multi_worker_worker_ring_completion_is_not_timer_driven() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });
        let mut slowest_completion = Duration::ZERO;

        for _ in 0..8 {
            let (read_fd, write_fd) = pipe().unwrap();
            let (ready_tx, ready_rx) = mpsc::channel();
            let (done_tx, done_rx) = mpsc::channel();

            let completion_latency = runtime.block_on(|cx| {
                cx.scope(|scope| {
                    let handle = scope.spawn_stealable(move |cx| {
                        let ring = cx.create_worker_ring();
                        let read_fd = RingFd::from_owned(read_fd);
                        ready_tx.send(()).unwrap();

                        let mut buffer = [0_u8; 1];
                        assert_eq!(ring.read(cx, &read_fd, &mut buffer).unwrap(), 1);
                        assert_eq!(buffer, [7]);
                        done_tx.send(()).unwrap();
                        ring.close(cx, read_fd).unwrap();
                    });

                    ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
                    thread::sleep(Duration::from_millis(20));

                    let byte = [7_u8];
                    let start = Instant::now();
                    let written = unsafe {
                        libc::write(write_fd.as_raw_fd(), byte.as_ptr().cast(), byte.len())
                    };
                    assert_eq!(written, 1);
                    done_rx.recv_timeout(Duration::from_millis(25)).unwrap();
                    let completion_latency = start.elapsed();

                    handle.join(cx);
                    completion_latency
                })
            });
            slowest_completion = slowest_completion.max(completion_latency);
        }

        assert!(
            slowest_completion < Duration::from_millis(2),
            "worker-local I/O completion was stranded behind scheduler idle timing: {slowest_completion:?}"
        );
    }

    #[test]
    fn worker_ring_dropped_pending_read_retains_buffer_until_reap() {
        let (read_fd, _write_fd) = pipe().unwrap();
        let drops = Arc::new(AtomicUsize::new(0));
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let ring = cx.create_worker_ring();
            let read_fd = RingFd::from_owned(read_fd);
            let read = ring
                .read_async(
                    cx,
                    &read_fd,
                    DropCountingReadBuffer::new(1, Arc::clone(&drops)),
                )
                .unwrap();

            drop(read);
            assert_eq!(drops.load(Ordering::Acquire), 0);
            wait_until(cx, || drops.load(Ordering::Acquire) == 1);
        });
    }

    #[test]
    fn worker_ring_canceled_pending_write_retains_buffer_until_reap() {
        let (read_fd, write_fd) = pipe().unwrap();
        fill_pipe(&write_fd);
        let drops = Arc::new(AtomicUsize::new(0));
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let ring = cx.create_worker_ring();
            let _read_fd = RingFd::from_owned(read_fd);
            let write_fd = RingFd::from_owned(write_fd);
            let mut write = ring
                .write_async(
                    cx,
                    &write_fd,
                    DropCountingWriteBuffer::new(1, Arc::clone(&drops)),
                )
                .unwrap();

            write.cancel();
            assert_eq!(drops.load(Ordering::Acquire), 0);
            wait_until(cx, || drops.load(Ordering::Acquire) == 1);
        });
    }

    #[test]
    fn multi_root_shared_ring_socket_read_write_routes_through_worker_driver() {
        let (read_fd, write_fd) = pipe().unwrap();
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let shared_io = scope.spawn(move |cx| {
                    let ring = cx.create_shared_ring();
                    let read_fd = RingFd::from_owned(read_fd);
                    let write_fd = RingFd::from_owned(write_fd);

                    let read = ring.read_async(cx, &read_fd, vec![0_u8; 2]).unwrap();
                    let write = ring.write_async(cx, &write_fd, b"xy".to_vec()).unwrap();

                    let written = write.get(cx).unwrap();
                    assert_eq!(written.bytes, 2);
                    assert_eq!(written.buffer, b"xy");

                    let read = read.get(cx).unwrap();
                    assert_eq!(read.bytes, 2);
                    assert_eq!(&read.buffer[..read.bytes], b"xy");

                    let counters = ring.shared_test_counters().unwrap();
                    assert_eq!(counters.accepted, 2);
                    assert_eq!(counters.completed, 2);
                    assert!(counters.driver_submitted >= 2);

                    ring.close(cx, read_fd).unwrap();
                    ring.close(cx, write_fd).unwrap();
                });
                shared_io.join(cx);
            });
        });
    }

    #[test]
    fn shared_sync_buffers_reused_for_multi_worker_shared_ring() {
        let (read_fd, write_fd) = pipe().unwrap();
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let shared_io = scope.spawn(move |cx| {
                    let ring = cx.create_shared_ring();
                    let read_fd = RingFd::from_owned(read_fd);
                    let write_fd = RingFd::from_owned(write_fd);
                    let mut read_buffer = [0_u8; 4];

                    ring.write(cx, &write_fd, b"ping").unwrap();
                    assert_eq!(ring.read(cx, &read_fd, &mut read_buffer).unwrap(), 4);
                    assert_eq!(&read_buffer, b"ping");

                    {
                        let cache = cx.shared_sync_buffers.borrow();
                        assert_eq!(cache.write.len(), 1);
                        assert_eq!(cache.read.len(), 1);
                        assert_eq!(cache.write_reuses, 0);
                        assert_eq!(cache.read_reuses, 0);
                    }

                    ring.write(cx, &write_fd, b"pong").unwrap();
                    assert_eq!(ring.read(cx, &read_fd, &mut read_buffer).unwrap(), 4);
                    assert_eq!(&read_buffer, b"pong");

                    let cache = cx.shared_sync_buffers.borrow();
                    assert_eq!(cache.write.len(), 1);
                    assert_eq!(cache.read.len(), 1);
                    assert_eq!(cache.write_reuses, 1);
                    assert_eq!(cache.read_reuses, 1);

                    ring.close(cx, read_fd).unwrap();
                    ring.close(cx, write_fd).unwrap();
                });
                shared_io.join(cx);
            });
        });
    }

    #[test]
    fn shared_sync_buffers_reused_for_stealable_worker_shared_ring() {
        let (read_fd, write_fd) = pipe().unwrap();
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let shared_io = scope.spawn_stealable(move |cx| {
                    assert!(matches!(cx.execution_place(), ExecutionPlace::Worker(_)));
                    let ring = cx.create_shared_ring();
                    let read_fd = RingFd::from_owned(read_fd);
                    let write_fd = RingFd::from_owned(write_fd);
                    let mut read_buffer = [0_u8; 4];

                    ring.write(cx, &write_fd, b"ping").unwrap();
                    assert_eq!(ring.read(cx, &read_fd, &mut read_buffer).unwrap(), 4);
                    assert_eq!(&read_buffer, b"ping");

                    {
                        let cache = cx.shared_sync_buffers.borrow();
                        assert_eq!(cache.write.len(), 1);
                        assert_eq!(cache.read.len(), 1);
                        assert_eq!(cache.write_reuses, 0);
                        assert_eq!(cache.read_reuses, 0);
                    }

                    ring.write(cx, &write_fd, b"pong").unwrap();
                    assert_eq!(ring.read(cx, &read_fd, &mut read_buffer).unwrap(), 4);
                    assert_eq!(&read_buffer, b"pong");

                    let cache = cx.shared_sync_buffers.borrow();
                    assert_eq!(cache.write.len(), 1);
                    assert_eq!(cache.read.len(), 1);
                    assert_eq!(cache.write_reuses, 1);
                    assert_eq!(cache.read_reuses, 1);
                    drop(cache);

                    ring.close(cx, read_fd).unwrap();
                    ring.close(cx, write_fd).unwrap();
                });
                shared_io.join(cx);
            });
        });
    }

    #[test]
    fn shared_sync_buffers_reused_for_single_worker_shared_ring() {
        let (read_fd, write_fd) = pipe().unwrap();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let ring = cx.create_shared_ring();
            let read_fd = RingFd::from_owned(read_fd);
            let write_fd = RingFd::from_owned(write_fd);
            let mut read_buffer = [0_u8; 4];

            ring.write(cx, &write_fd, b"ping").unwrap();
            assert_eq!(ring.read(cx, &read_fd, &mut read_buffer).unwrap(), 4);
            ring.write(cx, &write_fd, b"pong").unwrap();
            assert_eq!(ring.read(cx, &read_fd, &mut read_buffer).unwrap(), 4);
            assert_eq!(&read_buffer, b"pong");

            let cache = cx.shared_sync_buffers.borrow();
            assert_eq!(cache.write.len(), 1);
            assert_eq!(cache.read.len(), 1);
            assert_eq!(cache.write_reuses, 1);
            assert_eq!(cache.read_reuses, 1);

            ring.close(cx, read_fd).unwrap();
            ring.close(cx, write_fd).unwrap();
        });
    }

    #[test]
    fn pool_diagnostics_reports_shared_sync_buffer_cache() {
        let (read_fd, write_fd) = pipe().unwrap();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let ring = cx.create_shared_ring();
            let read_fd = RingFd::from_owned(read_fd);
            let write_fd = RingFd::from_owned(write_fd);
            let mut read_buffer = [0_u8; 4];

            let initial = cx.pool_diagnostics();
            assert_eq!(initial.shared_sync_read_buffers, 0);
            assert_eq!(initial.shared_sync_write_buffers, 0);
            assert_eq!(
                initial.max_shared_sync_buffers_per_direction,
                super::SHARED_SYNC_BUFFER_CACHE_ENTRIES
            );
            assert_eq!(
                initial.max_shared_sync_buffer_capacity,
                super::SHARED_SYNC_BUFFER_MAX_CAPACITY
            );

            ring.write(cx, &write_fd, b"ping").unwrap();
            assert_eq!(ring.read(cx, &read_fd, &mut read_buffer).unwrap(), 4);

            let diagnostics = cx.pool_diagnostics();
            assert_eq!(diagnostics.shared_sync_write_buffers, 1);
            assert_eq!(diagnostics.shared_sync_read_buffers, 1);
            assert!(diagnostics.shared_sync_write_capacity >= 4);
            assert!(diagnostics.shared_sync_read_capacity >= 4);
            assert_eq!(diagnostics.stack.cached_scope_states, 0);

            ring.close(cx, read_fd).unwrap();
            ring.close(cx, write_fd).unwrap();
        });
    }

    #[test]
    fn pool_diagnostics_reports_oversized_shared_sync_buffers_absent() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let mut read = Vec::with_capacity(super::SHARED_SYNC_BUFFER_MAX_CAPACITY + 1);
            read.resize(1, 0);
            let mut write = Vec::with_capacity(super::SHARED_SYNC_BUFFER_MAX_CAPACITY + 1);
            write.resize(1, 0);

            {
                let mut cache = cx.shared_sync_buffers.borrow_mut();
                cache.recycle_read(read);
                cache.recycle_write(write);
            }

            let diagnostics = cx.pool_diagnostics();
            assert_eq!(diagnostics.shared_sync_read_buffers, 0);
            assert_eq!(diagnostics.shared_sync_read_capacity, 0);
            assert_eq!(diagnostics.shared_sync_write_buffers, 0);
            assert_eq!(diagnostics.shared_sync_write_capacity, 0);
        });
    }

    #[test]
    fn pool_diagnostics_preserves_runtime_metrics_semantics() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::Disabled,
            ..RuntimeConfig::default()
        });

        let output = runtime.block_on(|cx| {
            let _ = cx.pool_diagnostics();
            cx.scope(|scope| {
                let handle = scope.spawn_stealable(|_| 7);
                handle.join(cx)
            })
        });

        assert_eq!(output, 7);
        let metrics = runtime.metrics();
        assert_eq!(metrics.worker_count, 2);
        assert_eq!(metrics.steal_policy, StealPolicy::Disabled);
        assert_eq!(metrics.completed_tasks, 1);
        assert_eq!(metrics.worker_completed_tasks.iter().sum::<usize>(), 1);
    }

    #[test]
    fn shared_sync_buffer_cache_drops_oversized_buffers() {
        let mut cache = super::SharedSyncBufferCache::default();

        cache.recycle_read(vec![0_u8; super::SHARED_SYNC_BUFFER_MAX_CAPACITY + 1]);
        cache.recycle_write(vec![0_u8; super::SHARED_SYNC_BUFFER_MAX_CAPACITY + 1]);
        assert!(cache.read.is_empty());
        assert!(cache.write.is_empty());

        let mut read = Vec::with_capacity(super::SHARED_SYNC_BUFFER_MAX_CAPACITY + 1);
        read.resize(1, 0);
        let mut write = Vec::with_capacity(super::SHARED_SYNC_BUFFER_MAX_CAPACITY + 1);
        write.resize(1, 0);
        cache.recycle_read(read);
        cache.recycle_write(write);
        assert!(cache.read.is_empty());
        assert!(cache.write.is_empty());
    }

    #[test]
    fn shared_sync_error_does_not_return_buffer_to_cache() {
        let (read_fd, write_fd) = pipe().unwrap();
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let shared_io = scope.spawn(move |cx| {
                    let ring = cx.create_shared_ring();
                    let read_fd = RingFd::from_owned(read_fd);
                    let write_fd = RingFd::from_owned(write_fd);
                    let mut read_buffer = [0_u8; 4];

                    ring.write(cx, &write_fd, b"warm").unwrap();
                    assert_eq!(ring.read(cx, &read_fd, &mut read_buffer).unwrap(), 4);
                    {
                        let cache = cx.shared_sync_buffers.borrow();
                        assert_eq!(cache.write.len(), 1);
                        assert_eq!(cache.read.len(), 1);
                    }

                    match &ring.inner {
                        RingInner::Shared(shared) => shared.core.close(),
                        RingInner::WorkerLocal { .. } => unreachable!("expected shared ring"),
                    }

                    assert_eq!(ring.write(cx, &write_fd, b"fail"), Err(RingError::Closed));
                    assert_eq!(
                        ring.read(cx, &read_fd, &mut read_buffer),
                        Err(RingError::Closed)
                    );

                    let cache = cx.shared_sync_buffers.borrow();
                    assert!(cache.write.is_empty());
                    assert!(cache.read.is_empty());
                    assert_eq!(cache.write_reuses, 1);
                    assert_eq!(cache.read_reuses, 1);
                });
                shared_io.join(cx);
            });
        });
    }

    #[test]
    fn shared_ring_dropped_pending_read_retains_buffer_until_reap() {
        let (read_fd, _write_fd) = pipe().unwrap();
        let drops = Arc::new(AtomicUsize::new(0));
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            let ring = cx.create_shared_ring();
            let read_fd = RingFd::from_owned(read_fd);
            let read = ring
                .read_async(
                    cx,
                    &read_fd,
                    DropCountingReadBuffer::new(1, Arc::clone(&drops)),
                )
                .unwrap();

            wait_until(cx, || {
                ring.shared_test_counters().unwrap().driver_submitted == 1
            });
            drop(read);
            if drops.load(Ordering::Acquire) == 0 {
                assert_eq!(
                    ring.shared_test_counters().unwrap().driver_retired,
                    0,
                    "buffer should be retained until the driver retires the canceled read"
                );
            }
            wait_until(cx, || {
                let counters = ring.shared_test_counters().unwrap();
                drops.load(Ordering::Acquire) == 1 && counters.driver_retired == 1
            });
        });
    }

    #[test]
    fn shared_ring_canceled_pending_write_retains_buffer_until_reap() {
        let (read_fd, write_fd) = pipe().unwrap();
        fill_pipe(&write_fd);
        let drops = Arc::new(AtomicUsize::new(0));
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            let ring = cx.create_shared_ring();
            let _read_fd = RingFd::from_owned(read_fd);
            let write_fd = RingFd::from_owned(write_fd);
            let mut write = ring
                .write_async(
                    cx,
                    &write_fd,
                    DropCountingWriteBuffer::new(1, Arc::clone(&drops)),
                )
                .unwrap();

            wait_until(cx, || {
                ring.shared_test_counters().unwrap().driver_submitted == 1
            });
            write.cancel();
            if drops.load(Ordering::Acquire) == 0 {
                assert_eq!(
                    ring.shared_test_counters().unwrap().driver_retired,
                    0,
                    "buffer should be retained until the driver retires the canceled write"
                );
            }
            wait_until(cx, || {
                let counters = ring.shared_test_counters().unwrap();
                drops.load(Ordering::Acquire) == 1 && counters.driver_retired == 1
            });
        });
    }

    #[test]
    fn shared_ring_cancel_write_then_close_does_not_return_fd_in_use() {
        let (read_fd, write_fd) = pipe().unwrap();
        fill_pipe(&write_fd);
        let drops = Arc::new(AtomicUsize::new(0));
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let handle = scope.spawn(move |cx| {
                    let ring = cx.create_shared_ring();
                    let _read_fd = RingFd::from_owned(read_fd);
                    let write_fd = RingFd::from_owned(write_fd);
                    let mut write = ring
                        .write_async(
                            cx,
                            &write_fd,
                            DropCountingWriteBuffer::new(1, Arc::clone(&drops)),
                        )
                        .unwrap();

                    wait_until(cx, || {
                        ring.shared_test_counters().unwrap().driver_submitted == 1
                    });
                    let cancel_start = Instant::now();
                    write.request_cancel(cx).unwrap();
                    assert!(
                        cancel_start.elapsed() < Duration::from_millis(50),
                        "shared cancellation should not poll fd clone retirement"
                    );

                    ring.close(cx, write_fd).unwrap();
                    assert!(matches!(write.try_get(), Some(Err(RingError::Canceled))));
                    assert_eq!(drops.load(Ordering::Acquire), 1);
                });
                handle.join(cx);
            });
        });
    }

    #[test]
    fn multi_root_shared_ring_socket_shutdown_routes_through_worker_driver() {
        let (left_fd, right_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let shared_shutdown = scope.spawn(move |cx| {
                    let ring = cx.create_shared_ring();
                    let left_fd = RingFd::from_owned(left_fd);
                    let right_fd = RingFd::from_owned(right_fd);

                    ring.shutdown(cx, &left_fd, Shutdown::Write).unwrap();
                    let mut buffer = [0_u8; 1];
                    assert_eq!(ring.read(cx, &right_fd, &mut buffer).unwrap(), 0);

                    ring.close(cx, left_fd).unwrap();
                    ring.close(cx, right_fd).unwrap();
                });
                shared_shutdown.join(cx);
            });
        });
    }

    #[test]
    fn worker_ring_rejects_cross_runtime_use() {
        let ring = {
            let mut runtime = Runtime::new();
            runtime.block_on(|cx| cx.create_worker_ring())
        };
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            assert_eq!(ring.nop(cx), Err(RingError::WrongRuntime));
        });
    }

    #[test]
    fn single_worker_shared_ring_rejects_cross_runtime_use() {
        let ring = {
            let mut runtime = Runtime::new();
            runtime.block_on(|cx| cx.create_shared_ring())
        };
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            assert_eq!(ring.nop(cx), Err(RingError::WrongRuntime));
        });
    }

    #[test]
    fn worker_ring_creation_from_multi_root_is_rejected() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            assert_eq!(
                cx.create_ring(RingMode::WorkerLocal).err(),
                Some(RingError::NoCurrentWorker)
            );
        });
    }

    #[test]
    fn worker_ring_rejects_cross_worker_use() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(4).unwrap(),
            steal_policy: StealPolicy::steal_half(),
            ..RuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let handle = scope.spawn_stealable(|cx| {
                    let wrong_owner = WorkerId((cx.worker_id().index() + 1) % 4);
                    let ring = Ring {
                        inner: RingInner::WorkerLocal {
                            runtime_id: cx.runtime_id,
                            owner: wrong_owner,
                        },
                    };
                    assert_eq!(
                        ring.nop(cx),
                        Err(RingError::WrongWorker {
                            owner: wrong_owner,
                            current: cx.worker_id(),
                        })
                    );
                });
                handle.join(cx);
            });
        });
    }

    #[test]
    fn shared_operation_terminal_result_is_published_once() {
        let core = Arc::new(SharedRingCore::new(8, SharedRingWake::Local));
        let state = Arc::new(SharedOpState::new(&core));

        assert_eq!(state.lifecycle_for_test(), SharedOpLifecycle::Created);
        assert!(state.complete(Ok(0)));
        assert!(!state.cancel());
        assert!(!state.close());
        assert_eq!(state.try_take(), Some(Ok(0)));
        assert!(!state.complete(Err(RingError::Closed)));
    }

    #[test]
    fn shared_operation_close_before_submit_completes_queued_request() {
        let core = Arc::new(SharedRingCore::new(8, SharedRingWake::Local));
        let state = Arc::new(SharedOpState::new(&core));

        core.submit_queued(
            SharedOperation::unit(SharedOperationKind::Nop, Arc::clone(&state)),
            SharedOpRoute::worker(WorkerId(0)),
        )
        .unwrap();
        assert_eq!(
            state.lifecycle_for_test(),
            SharedOpLifecycle::Queued(SharedOpRoute::worker(WorkerId(0)))
        );

        core.close();
        assert_eq!(state.try_take(), Some(Err(RingError::Closed)));
        let counters = core.test_counters();
        assert_eq!(counters.accepted, 1);
        assert_eq!(counters.closed, 1);
        assert_eq!(counters.cleaned_up, 1);
    }

    #[test]
    fn shared_operation_cancel_before_submit_wins_late_driver_completion() {
        let core = Arc::new(SharedRingCore::new(8, SharedRingWake::Local));
        let state = Arc::new(SharedOpState::new(&core));

        core.submit_queued(
            SharedOperation::unit(SharedOperationKind::Nop, Arc::clone(&state)),
            SharedOpRoute::worker(WorkerId(0)),
        )
        .unwrap();
        assert!(state.cancel());

        let operation = core.pop_queued_for_test().expect("queued operation");
        assert!(!operation.mark_submitted(SharedOpRoute::worker(WorkerId(0))));
        assert!(!operation.complete(Ok(0)));
        assert_eq!(state.try_take(), Some(Err(RingError::Canceled)));
    }

    #[test]
    fn shared_operation_repeated_cancel_close_and_complete_are_idempotent() {
        let core = Arc::new(SharedRingCore::new(8, SharedRingWake::Local));
        let state = Arc::new(SharedOpState::new(&core));

        assert!(state.cancel());
        assert!(!state.cancel());
        assert!(!state.close());
        assert!(!state.complete(Ok(0)));
        assert_eq!(state.try_take(), Some(Err(RingError::Canceled)));
    }

    #[test]
    fn shared_ring_submitted_operations_are_capacity_bounded() {
        let core = Arc::new(SharedRingCore::new(1, SharedRingWake::Local));
        let first = Arc::new(SharedOpState::new(&core));
        let second = Arc::new(SharedOpState::new(&core));

        assert!(first.mark_submitted(SharedOpRoute::local()));
        assert!(core.register_submitted(&first));
        assert!(second.mark_submitted(SharedOpRoute::local()));
        assert!(!core.register_submitted(&second));
        assert_eq!(second.try_take(), Some(Err(RingError::QueueFull)));

        first.cancel();
    }

    #[test]
    fn shared_ring_late_submitted_registration_rejects_terminal_operation() {
        let core = Arc::new(SharedRingCore::new(1, SharedRingWake::Local));
        let state = Arc::new(SharedOpState::new(&core));

        assert!(state.mark_submitted(SharedOpRoute::worker(WorkerId(0))));
        assert!(state.cancel());
        assert!(!core.register_submitted(&state));
        assert_eq!(core.submitted_operation_count(), 0);
        assert_eq!(state.try_take(), Some(Err(RingError::Canceled)));
    }

    #[test]
    fn shared_ring_driver_commands_are_deduplicated_and_bounded() {
        let worker = CrossbeamWorker::new_fifo();
        let runtime = MultiRuntime::with_runtime_id(
            RuntimeConfig {
                workers: NonZeroUsize::new(1).unwrap(),
                max_shared_ring_queue_len: 1,
                ..RuntimeConfig::default()
            },
            RuntimeId(1),
            vec![worker.stealer()],
        );
        let first = Arc::new(SharedRingCore::new(1, SharedRingWake::Local));
        let second = Arc::new(SharedRingCore::new(1, SharedRingWake::Local));

        assert_eq!(runtime.submit_shared_ring(WorkerId(0), &first), Ok(()));
        assert_eq!(runtime.submit_shared_ring(WorkerId(0), &first), Ok(()));
        assert_eq!(
            runtime.shared_ring_queues[0]
                .lock()
                .expect("shared ring command queue mutex poisoned")
                .len(),
            1
        );
        assert_eq!(
            runtime.submit_shared_ring(WorkerId(0), &second),
            Err(RingError::QueueFull)
        );
    }

    #[test]
    fn shared_ring_driver_commands_retain_queue_capacity_after_drain() {
        const QUEUE_CAPACITY: usize = 4;

        let worker = CrossbeamWorker::new_fifo();
        let runtime = MultiRuntime::with_runtime_id(
            RuntimeConfig {
                workers: NonZeroUsize::new(1).unwrap(),
                max_shared_ring_queue_len: QUEUE_CAPACITY,
                ..RuntimeConfig::default()
            },
            RuntimeId(1),
            vec![worker.stealer()],
        );
        let first = Arc::new(SharedRingCore::new(QUEUE_CAPACITY, SharedRingWake::Local));
        let second = Arc::new(SharedRingCore::new(QUEUE_CAPACITY, SharedRingWake::Local));

        assert_eq!(runtime.submit_shared_ring(WorkerId(0), &first), Ok(()));

        let mut root = StackRuntime::new();
        root.block_on(|root| {
            let mut active = Vec::new();
            let mut queued = VecDeque::with_capacity(QUEUE_CAPACITY);
            assert!(!runtime.drain_shared_ring_commands(
                WorkerId(0),
                root,
                &mut active,
                &mut queued,
            ));
            assert!(active.is_empty());
            assert!(queued.is_empty());
            assert!(queued.capacity() >= QUEUE_CAPACITY);
        });

        let queue = runtime.shared_ring_queues[0]
            .lock()
            .expect("shared ring command queue mutex poisoned");
        assert!(queue.is_empty());
        assert!(queue.capacity() >= QUEUE_CAPACITY);
        drop(queue);

        assert_eq!(runtime.submit_shared_ring(WorkerId(0), &second), Ok(()));
        let queue = runtime.shared_ring_queues[0]
            .lock()
            .expect("shared ring command queue mutex poisoned");
        assert_eq!(queue.len(), 1);
        assert!(queue.capacity() >= QUEUE_CAPACITY);
    }

    #[test]
    fn shared_ring_driver_commands_retain_queue_capacity_after_close_cleanup() {
        const QUEUE_CAPACITY: usize = 4;

        let worker = CrossbeamWorker::new_fifo();
        let runtime = MultiRuntime::with_runtime_id(
            RuntimeConfig {
                workers: NonZeroUsize::new(1).unwrap(),
                max_shared_ring_queue_len: QUEUE_CAPACITY,
                ..RuntimeConfig::default()
            },
            RuntimeId(1),
            vec![worker.stealer()],
        );
        let first = Arc::new(SharedRingCore::new(QUEUE_CAPACITY, SharedRingWake::Local));
        let second = Arc::new(SharedRingCore::new(QUEUE_CAPACITY, SharedRingWake::Local));

        assert_eq!(runtime.submit_shared_ring(WorkerId(0), &first), Ok(()));

        let mut active = Vec::new();
        let mut queued = VecDeque::with_capacity(QUEUE_CAPACITY);
        runtime.close_worker_shared_operations(WorkerId(0), &mut active, &mut queued);
        assert!(queued.is_empty());
        assert!(queued.capacity() >= QUEUE_CAPACITY);
        assert!(first.is_closed());

        let queue = runtime.shared_ring_queues[0]
            .lock()
            .expect("shared ring command queue mutex poisoned");
        assert!(queue.is_empty());
        assert!(queue.capacity() >= QUEUE_CAPACITY);
        drop(queue);

        assert_eq!(runtime.submit_shared_ring(WorkerId(0), &second), Ok(()));
        let queue = runtime.shared_ring_queues[0]
            .lock()
            .expect("shared ring command queue mutex poisoned");
        assert_eq!(queue.len(), 1);
        assert!(queue.capacity() >= QUEUE_CAPACITY);
    }

    #[test]
    fn shared_operation_resource_owner_survives_until_terminal_cleanup() {
        let core = Arc::new(SharedRingCore::new(8, SharedRingWake::Local));
        let state = Arc::new(SharedOpState::new(&core));
        let drops = Arc::new(AtomicUsize::new(0));

        state.attach_test_resource(SharedOpTestResource::new(Arc::clone(&drops)));
        assert_eq!(drops.load(Ordering::Acquire), 0);
        assert!(state.cancel());
        assert_eq!(drops.load(Ordering::Acquire), 1);
        assert!(!state.close());
        assert_eq!(drops.load(Ordering::Acquire), 1);
    }

    #[test]
    fn shared_operation_dropped_handle_cleans_submitted_timeout() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let ring = cx.create_shared_ring();
            let timeout = ring.timeout(cx, Duration::from_secs(3600)).unwrap();
            wait_until(cx, || ring.shared_submitted_operation_count() == Some(1));
            drop(timeout);
            wait_until(cx, || ring.shared_submitted_operation_count() == Some(0));
        });
    }

    #[test]
    fn single_worker_shared_ring_uses_local_driver() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let ring = cx.create_shared_ring();
            assert_eq!(ring.shared_driver_owner(), None);
            ring.nop(cx).unwrap();
            ring.sleep(cx, Duration::from_millis(1)).unwrap();
        });
    }

    #[test]
    fn multi_root_shared_ring_selects_real_owner_worker() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            let ring = cx.create_shared_ring();
            let owner = ring
                .shared_driver_owner()
                .expect("multi-worker shared ring should have an owner");
            assert_ne!(owner, WorkerId(NO_WORKER));
            assert!(owner.index() < 2);
            cx.scope(|scope| {
                let nop = scope.spawn(move |cx| ring.nop(cx).unwrap());
                nop.join(cx);
            });
        });
    }

    #[test]
    fn multi_root_socket_adapter_reuses_shared_ring_core() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            assert_eq!(cx.current_worker(), None);
            let first = cx.socket_ring_for_test().unwrap();
            let second = cx.socket_ring_for_test().unwrap();
            assert_eq!(
                first.shared_core_ptr(),
                second.shared_core_ptr(),
                "root SocketIoRuntime adapter calls should reuse one shared ring core"
            );
        });
    }

    #[test]
    fn multi_root_shared_ring_long_timeout_does_not_block_later_nop() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            let ring = cx.create_shared_ring();
            let mut timeout = ring.timeout(cx, Duration::from_secs(3600)).unwrap();
            wait_until(cx, || ring.shared_submitted_operation_count() == Some(1));
            cx.scope(|scope| {
                let nop = scope.spawn({
                    let ring = ring.clone();
                    move |cx| ring.nop(cx).unwrap()
                });
                nop.join(cx);
            });
            timeout.cancel();
        });
    }

    #[test]
    fn multi_root_pending_shared_timeout_wait_requires_stackful_context() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            let ring = cx.create_shared_ring();
            let timeout = ring.timeout(cx, Duration::from_secs(60)).unwrap();

            assert_eq!(timeout.wait(cx), Err(RingError::NoStackfulContext));
        });
    }

    #[test]
    fn shared_rings_route_through_worker_threads_not_per_ring_helpers() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });

        let driver_thread_count = runtime.block_on(|cx| {
            let rings = (0..16).map(|_| cx.create_shared_ring()).collect::<Vec<_>>();
            cx.scope(|scope| {
                let nops = scope.spawn(move |cx| {
                    for ring in &rings {
                        ring.nop(cx).unwrap();
                    }
                    rings
                        .into_iter()
                        .flat_map(|ring| ring.shared_test_counters().unwrap().driver_threads)
                        .collect::<HashSet<_>>()
                        .len()
                });
                nops.join(cx)
            })
        });

        assert!(
            driver_thread_count <= 2,
            "shared rings should be driven by configured workers, not one helper thread per ring"
        );
    }

    #[test]
    fn shared_ring_constructor_does_not_spawn_helper_threads() {
        let source = include_str!("lib.rs");
        let start = source.find("impl SharedRing {").unwrap();
        let end = start
            + source[start..]
                .find("impl Clone for SharedRing")
                .expect("SharedRing impl should be followed by Clone impl");
        let shared_ring_impl = &source[start..end];

        assert!(
            !shared_ring_impl.contains("thread::spawn"),
            "SharedRing construction must not reintroduce a per-ring helper thread"
        );
    }

    #[test]
    fn shared_ring_operations_complete_from_multiple_worker_contexts() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(4).unwrap(),
            steal_policy: StealPolicy::steal_half(),
            ..RuntimeConfig::default()
        });

        let workers = runtime.block_on(|cx| {
            let ring = cx.create_shared_ring();
            cx.scope(|scope| {
                let handles = (0..64)
                    .map(|_| {
                        let ring = ring.clone();
                        scope.spawn_stealable(move |cx| {
                            std::thread::sleep(Duration::from_millis(1));
                            ring.nop(cx).unwrap();
                            ring.sleep(cx, Duration::ZERO).unwrap();
                            cx.current_worker()
                                .expect("stealable task should run on a worker")
                        })
                    })
                    .collect::<Vec<_>>();

                handles
                    .into_iter()
                    .map(|handle| handle.join(cx))
                    .collect::<HashSet<_>>()
            })
        });

        assert!(
            workers.len() >= 2,
            "expected shared ring submissions from at least two workers, got {workers:?}"
        );
    }

    #[test]
    fn shared_ring_test_counters_track_completion_cancellation_and_cleanup() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let ring = cx.create_shared_ring();
            ring.nop(cx).unwrap();
            let mut timeout = ring.timeout(cx, Duration::from_secs(3600)).unwrap();
            wait_until(cx, || ring.shared_submitted_operation_count() == Some(1));
            timeout.cancel();
            wait_until(cx, || ring.shared_submitted_operation_count() == Some(0));

            let counters = ring.shared_test_counters().unwrap();
            assert_eq!(counters.accepted, 2);
            assert_eq!(counters.driver_submitted, 2);
            assert_eq!(counters.driver_retired, 0);
            assert_eq!(counters.completed, 1);
            assert_eq!(counters.canceled, 1);
            assert_eq!(counters.closed, 0);
            assert_eq!(counters.rejected, 0);
            assert_eq!(counters.cleaned_up, 2);
        });
    }

    #[test]
    fn multi_worker_shared_timeout_cancel_retires_owner_driver_operation() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            let ring = cx.create_shared_ring();
            let mut timeout = ring.timeout(cx, Duration::from_secs(3600)).unwrap();
            wait_until(cx, || ring.shared_submitted_operation_count() == Some(1));
            timeout.cancel();
            wait_until(cx, || {
                ring.shared_test_counters()
                    .is_some_and(|counters| counters.driver_retired == 1)
            });

            let counters = ring.shared_test_counters().unwrap();
            assert_eq!(ring.shared_submitted_operation_count(), Some(0));
            assert_eq!(counters.canceled, 1);
            assert_eq!(counters.cleaned_up, 1);
        });
    }

    #[test]
    fn scope_exit_cancels_stealable_child_waiting_on_shared_ring_timeout() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            let ring = cx.create_shared_ring();
            let waiting = Arc::new(AtomicBool::new(false));
            cx.scope(|scope| {
                let child_ring = ring.clone();
                let child_waiting = Arc::clone(&waiting);
                scope.spawn_stealable(move |cx| {
                    let timeout = child_ring.timeout(cx, Duration::from_secs(3600)).unwrap();
                    child_waiting.store(true, Ordering::Release);
                    let _ = timeout.wait(cx);
                });
                wait_until(cx, || waiting.load(Ordering::Acquire));
            });
            wait_until(cx, || ring.shared_submitted_operation_count() == Some(0));
        });
    }

    #[test]
    fn shared_ring_parent_panic_cancels_stealable_child_waiting_on_timeout() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });

        let result = panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(|cx| {
                let ring = cx.create_shared_ring();
                let waiting = Arc::new(AtomicBool::new(false));
                cx.scope(|scope| {
                    let child_ring = ring.clone();
                    let child_waiting = Arc::clone(&waiting);
                    scope.spawn_stealable(move |cx| {
                        let timeout = child_ring.timeout(cx, Duration::from_secs(3600)).unwrap();
                        child_waiting.store(true, Ordering::Release);
                        let _ = timeout.wait(cx);
                    });
                    wait_until(cx, || waiting.load(Ordering::Acquire));
                    panic!("parent failed while child waited on shared ring");
                });
            });
        }));

        let payload = result.expect_err("parent panic should propagate");
        assert!(
            payload.downcast_ref::<&'static str>().is_some_and(
                |message| *message == "parent failed while child waited on shared ring"
            )
        );
    }

    #[test]
    fn shared_ring_runtime_shutdown_closes_pending_timeout() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });

        let (_ring, mut timeout) = runtime.block_on(|cx| {
            let ring = cx.create_shared_ring();
            let timeout = ring.timeout(cx, Duration::from_secs(3600)).unwrap();
            wait_until(cx, || ring.shared_submitted_operation_count() == Some(1));
            (ring, timeout)
        });

        assert_eq!(timeout.try_wait(), Some(Err(RingError::Closed)));
    }

    #[test]
    fn single_worker_shared_ring_runtime_shutdown_closes_pending_timeout_without_waiting() {
        let mut runtime = Runtime::new();
        let started = Instant::now();

        let (_ring, mut timeout) = runtime.block_on(|cx| {
            let ring = cx.create_shared_ring();
            let timeout = ring.timeout(cx, Duration::from_secs(3600)).unwrap();
            wait_until(cx, || ring.shared_submitted_operation_count() == Some(1));
            (ring, timeout)
        });

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "block_on should not wait for escaped local shared timeouts to expire"
        );
        assert_eq!(timeout.try_wait(), Some(Err(RingError::Closed)));
    }

    #[test]
    fn single_worker_final_shared_ring_drop_closes_pending_timeout_without_waiting() {
        let mut runtime = Runtime::new();
        let started = Instant::now();

        let mut timeout = runtime.block_on(|cx| {
            let ring = cx.create_shared_ring();
            let timeout = ring.timeout(cx, Duration::from_secs(3600)).unwrap();
            wait_until(cx, || ring.shared_submitted_operation_count() == Some(1));
            drop(ring);
            timeout
        });

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "dropping the final ring handle should not leave local driver I/O draining"
        );
        assert_eq!(timeout.try_wait(), Some(Err(RingError::Closed)));
    }

    #[test]
    fn dropping_pending_shared_timeout_cancels_without_blocking_ring() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let ring = cx.create_shared_ring();
            let timeout = ring.timeout(cx, Duration::from_secs(3600)).unwrap();
            drop(timeout);
            ring.nop(cx).unwrap();
        });
    }

    #[test]
    fn shared_ring_long_timeout_does_not_block_later_nop() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let ring = cx.create_shared_ring();
            let mut timeout = ring.timeout(cx, Duration::from_secs(3600)).unwrap();
            ring.nop(cx).unwrap();
            timeout.cancel();
        });
    }

    #[test]
    fn shared_ring_rejects_out_of_range_timeout_duration() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let ring = cx.create_shared_ring();
            assert_eq!(
                ring.timeout(cx, Duration::MAX).err(),
                Some(RingError::DurationOutOfRange)
            );
        });
    }

    #[test]
    fn shared_ring_can_complete_operations_from_multiple_tasks() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            let ring = cx.create_shared_ring();
            cx.scope(|scope| {
                let left_ring = ring.clone();
                let right_ring = ring.clone();
                let left = scope.spawn_stealable(move |cx| left_ring.nop(cx));
                let right = scope.spawn_stealable(move |cx| right_ring.nop(cx));

                left.join(cx).unwrap();
                right.join(cx).unwrap();
            });
        });
    }

    #[test]
    fn full_global_queue_rejects_stealable_spawn() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            max_global_queue_len: 0,
            ..RuntimeConfig::default()
        });
        let result = panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(|cx| {
                cx.scope(|scope| {
                    let handle = scope.spawn_stealable(|_| 1);
                    handle.join(cx);
                });
            });
        }));

        let error = result.expect_err("full queue should reject stealable spawn");
        assert!(error.downcast_ref::<&'static str>().is_some_and(
            |message| *message == "stealable task rejected because runtime queue is full"
        ));
        assert_eq!(runtime.metrics().rejected_tasks, 1);
        assert_eq!(runtime.metrics().completed_tasks, 0);
    }

    #[test]
    fn active_stealable_jobs_are_admission_limited() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            max_worker_queue_len: 0,
            max_global_queue_len: 1,
            ..RuntimeConfig::default()
        });

        let result = panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(|cx| {
                cx.scope(|scope| {
                    let first = scope.spawn_stealable(|_| {
                        std::thread::sleep(Duration::from_millis(10));
                    });
                    let rejected = scope.spawn_stealable(|_| ());
                    rejected.join(cx);
                    first.join(cx);
                });
            });
        }));

        let error = result.expect_err("second active job should be rejected");
        assert!(error.downcast_ref::<&'static str>().is_some_and(
            |message| *message == "stealable task rejected because runtime queue is full"
        ));
        assert_eq!(runtime.metrics().rejected_tasks, 1);
        assert_eq!(runtime.metrics().completed_tasks, 1);
    }

    #[test]
    fn full_shared_ring_queue_rejects_submission() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            max_shared_ring_queue_len: 0,
            ..RuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            let ring = cx.create_shared_ring();
            assert_eq!(ring.nop(cx), Err(RingError::QueueFull));
            let counters = ring.shared_test_counters().unwrap();
            assert_eq!(counters.accepted, 0);
            assert_eq!(counters.driver_submitted, 0);
            assert_eq!(counters.rejected, 1);
            assert_eq!(counters.cleaned_up, 1);
        });
    }

    #[test]
    fn canceled_shared_timeout_cleans_up_without_later_submission() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let ring = cx.create_shared_ring();
            let mut timeout = ring.timeout(cx, Duration::from_secs(3600)).unwrap();
            wait_until(cx, || ring.shared_submitted_operation_count() == Some(1));
            timeout.cancel();
            wait_until(cx, || ring.shared_submitted_operation_count() == Some(0));
        });
    }

    #[test]
    fn shared_ring_close_wakes_pending_stealable_timeout() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            let ring = cx.create_shared_ring();
            let worker_ring = ring.clone();
            cx.scope(|scope| {
                let handle = scope.spawn_stealable(move |cx| {
                    let timeout = worker_ring
                        .timeout(cx, Duration::from_secs(3600))
                        .expect("timeout should submit");
                    drop(worker_ring);
                    timeout.wait(cx)
                });
                wait_until(cx, || ring.shared_submitted_operation_count() == Some(1));
                drop(ring);
                assert_eq!(handle.join(cx), Err(RingError::Closed));
            });
        });
    }

    #[test]
    fn shared_ring_close_wakes_pending_timeout_with_error() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let ring = cx.create_shared_ring();
            let waiting = Arc::new(AtomicBool::new(false));
            cx.scope(|scope| {
                let child_ring = ring.clone();
                let child_waiting = Arc::clone(&waiting);
                let handle = scope.spawn(move |cx| {
                    let timeout = child_ring.timeout(cx, Duration::from_secs(3600)).unwrap();
                    drop(child_ring);
                    child_waiting.store(true, Ordering::Release);
                    timeout.wait(cx)
                });
                while !waiting.load(Ordering::Acquire) {
                    cx.yield_now();
                }
                drop(ring);
                assert_eq!(handle.join(cx), Err(RingError::Closed));
            });
        });
    }

    #[test]
    fn steal_sender_to_existing_stack_receiver() {
        let (tx, rx) = cross_thread::bounded(1).stackful();
        let (ready_tx, ready_rx) = mpsc::channel();
        let receiver = thread::spawn(move || {
            let mut runtime = StackRuntime::new();
            runtime.block_on(|cx| {
                cx.scope(|scope| {
                    let handle = scope.spawn(move |cx| {
                        ready_tx.send(()).unwrap();
                        rx.recv(cx)
                    });
                    handle.join(cx)
                })
            })
        });

        ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });
        runtime
            .block_on(|cx| {
                cx.scope(|scope| {
                    let handle = scope.spawn_stealable(move |cx| tx.send_with(cx, 42));
                    handle.join(cx)
                })
            })
            .unwrap();

        assert_eq!(receiver.join().unwrap(), Ok(42));
    }

    #[test]
    fn existing_stack_sender_to_steal_receiver() {
        let (tx, rx) = cross_thread::bounded(1).stackful();
        let (ready_tx, ready_rx) = mpsc::channel();
        let receiver = thread::spawn(move || {
            let mut runtime = Runtime::with_config(RuntimeConfig {
                workers: NonZeroUsize::new(2).unwrap(),
                steal_policy: StealPolicy::steal_one(),
                ..RuntimeConfig::default()
            });
            runtime.block_on(|cx| {
                cx.scope(|scope| {
                    let handle = scope.spawn_stealable(move |cx| {
                        ready_tx.send(()).unwrap();
                        rx.recv_with(cx)
                    });
                    handle.join(cx)
                })
            })
        });

        ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let mut runtime = StackRuntime::new();
        runtime
            .block_on(|cx| {
                cx.scope(|scope| {
                    let handle = scope.spawn(move |cx| tx.send(cx, 7));
                    handle.join(cx)
                })
            })
            .unwrap();

        assert_eq!(receiver.join().unwrap(), Ok(7));
    }

    #[test]
    fn thread_sender_to_steal_receiver() {
        let (tx, rx) = cross_thread::bounded(1).thread_to_stackful();
        let (ready_tx, ready_rx) = mpsc::channel();
        let receiver = thread::spawn(move || {
            let mut runtime = Runtime::with_config(RuntimeConfig {
                workers: NonZeroUsize::new(2).unwrap(),
                steal_policy: StealPolicy::steal_one(),
                ..RuntimeConfig::default()
            });
            runtime.block_on(|cx| {
                cx.scope(|scope| {
                    let handle = scope.spawn_stealable(move |cx| {
                        ready_tx.send(()).unwrap();
                        rx.recv_with(cx)
                    });
                    handle.join(cx)
                })
            })
        });

        ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        tx.send_blocking(11).unwrap();

        assert_eq!(receiver.join().unwrap(), Ok(11));
    }

    #[test]
    fn steal_sender_to_thread_receiver() {
        let (tx, rx) = cross_thread::bounded(1).stackful_to_thread();
        let receiver = thread::spawn(move || rx.recv_blocking());
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });

        runtime
            .block_on(|cx| {
                cx.scope(|scope| {
                    let handle = scope.spawn_stealable(move |cx| tx.send_with(cx, 13));
                    handle.join(cx)
                })
            })
            .unwrap();

        assert_eq!(receiver.join().unwrap(), Ok(13));
    }

    #[test]
    fn steal_sender_obeys_bounded_backpressure() {
        let (tx, rx) = cross_thread::bounded(1).stackful_to_thread();
        tx.try_send(1).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let sender = thread::spawn(move || {
            let mut runtime = Runtime::with_config(RuntimeConfig {
                workers: NonZeroUsize::new(2).unwrap(),
                steal_policy: StealPolicy::steal_one(),
                ..RuntimeConfig::default()
            });
            let sent = runtime
                .block_on(|cx| {
                    cx.scope(|scope| {
                        let handle = scope.spawn_stealable(move |cx| {
                            started_tx.send(()).unwrap();
                            tx.send_with(cx, 2)
                        });
                        handle.join(cx)
                    })
                })
                .is_ok();
            done_tx.send(sent).unwrap();
        });

        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(done_rx.recv_timeout(Duration::from_millis(25)).is_err());
        assert_eq!(rx.recv_blocking(), Ok(1));
        assert!(done_rx.recv_timeout(Duration::from_secs(1)).unwrap());
        assert_eq!(rx.recv_blocking(), Ok(2));
        sender.join().unwrap();
    }

    #[test]
    fn final_receiver_drop_wakes_steal_sender() {
        let (tx, rx) = cross_thread::bounded(1).stackful_to_thread();
        tx.try_send(1).unwrap();
        let parked = Arc::new(AtomicBool::new(false));
        let sender_parked = Arc::clone(&parked);
        let sender = thread::spawn(move || {
            let mut runtime = Runtime::with_config(RuntimeConfig {
                workers: NonZeroUsize::new(2).unwrap(),
                steal_policy: StealPolicy::steal_one(),
                ..RuntimeConfig::default()
            });
            runtime.block_on(|cx| {
                cx.scope(|scope| {
                    let handle = scope.spawn_stealable(move |cx| {
                        let probe = WaitProbe {
                            cx,
                            parked: &sender_parked,
                        };
                        tx.send_with(&probe, 2)
                    });
                    handle.join(cx)
                })
            })
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !parked.load(Ordering::Acquire) {
            assert!(
                std::time::Instant::now() < deadline,
                "steal sender did not register a stackful wait"
            );
            thread::yield_now();
        }
        drop(rx);

        match sender.join().unwrap() {
            Err(SendError(value)) => assert_eq!(value, 2),
            other => panic!("unexpected send result: {other:?}"),
        }
    }

    #[test]
    fn final_sender_drop_wakes_steal_receiver() {
        let (tx, rx) = cross_thread::bounded::<i32>(1).stackful();
        let parked = Arc::new(AtomicBool::new(false));
        let receiver_parked = Arc::clone(&parked);
        let receiver = thread::spawn(move || {
            let mut runtime = Runtime::with_config(RuntimeConfig {
                workers: NonZeroUsize::new(2).unwrap(),
                steal_policy: StealPolicy::steal_one(),
                ..RuntimeConfig::default()
            });
            runtime.block_on(|cx| {
                cx.scope(|scope| {
                    let handle = scope.spawn_stealable(move |cx| {
                        let probe = WaitProbe {
                            cx,
                            parked: &receiver_parked,
                        };
                        rx.recv_with(&probe)
                    });
                    handle.join(cx)
                })
            })
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !parked.load(Ordering::Acquire) {
            assert!(
                std::time::Instant::now() < deadline,
                "steal receiver did not register a stackful wait"
            );
            thread::yield_now();
        }

        drop(tx);

        assert_eq!(receiver.join().unwrap(), Err(RecvError));
    }

    #[test]
    fn stealable_join_with_stack_usage_panics() {
        let result = panic::catch_unwind(|| {
            let mut runtime = Runtime::with_config(RuntimeConfig {
                workers: NonZeroUsize::new(2).unwrap(),
                steal_policy: StealPolicy::steal_one(),
                ..RuntimeConfig::default()
            });
            runtime.block_on(|cx| {
                cx.scope(|scope| {
                    let handle = scope.spawn_stealable(|_| 1);
                    handle.join_with_stack_usage(cx);
                });
            });
        });

        let error = result.expect_err("stealable stack usage should be unavailable");
        assert!(
            error
                .downcast_ref::<&'static str>()
                .is_some_and(|message| *message
                    == "stack usage for stealable worker-thread jobs is not available yet")
        );
    }

    #[test]
    fn stealable_try_join_then_join_panics_instead_of_hanging() {
        let result = panic::catch_unwind(|| {
            let mut runtime = Runtime::with_config(RuntimeConfig {
                workers: NonZeroUsize::new(2).unwrap(),
                steal_policy: StealPolicy::steal_one(),
                ..RuntimeConfig::default()
            });
            runtime.block_on(|cx| {
                cx.scope(|scope| {
                    let handle = scope.spawn_stealable(|_| 1);
                    while handle.try_join().is_none() {
                        cx.yield_now();
                    }
                    handle.join(cx);
                });
            });
        });

        let error = result.expect_err("joining after try_join should fail deterministically");
        assert!(
            error
                .downcast_ref::<&'static str>()
                .is_some_and(|message| *message == "JoinHandle value already taken")
        );
    }

    #[cfg(feature = "tokio")]
    #[tokio::test(flavor = "current_thread")]
    async fn tokio_sender_to_steal_receiver() {
        let (tx, rx) = cross_thread::bounded(1).tokio_to_stackful();
        let receiver = thread::spawn(move || {
            let mut runtime = Runtime::with_config(RuntimeConfig {
                workers: NonZeroUsize::new(2).unwrap(),
                steal_policy: StealPolicy::steal_one(),
                ..RuntimeConfig::default()
            });
            runtime.block_on(|cx| {
                cx.scope(|scope| {
                    let handle = scope.spawn_stealable(move |cx| rx.recv_with(cx));
                    handle.join(cx)
                })
            })
        });

        tx.send(17).await.unwrap();

        assert_eq!(receiver.join().unwrap(), Ok(17));
    }

    #[cfg(feature = "tokio")]
    #[tokio::test(flavor = "current_thread")]
    async fn steal_sender_to_tokio_receiver() {
        let (tx, rx) = cross_thread::bounded(1).stackful_to_tokio();
        let sender = thread::spawn(move || {
            let mut runtime = Runtime::with_config(RuntimeConfig {
                workers: NonZeroUsize::new(2).unwrap(),
                steal_policy: StealPolicy::steal_one(),
                ..RuntimeConfig::default()
            });
            runtime
                .block_on(|cx| {
                    cx.scope(|scope| {
                        let handle = scope.spawn_stealable(move |cx| tx.send_with(cx, 19));
                        handle.join(cx)
                    })
                })
                .unwrap();
        });

        assert_eq!(rx.recv().await, Ok(19));
        sender.join().unwrap();
    }
}
