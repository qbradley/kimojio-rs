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
//! The runtime does not expose implicit I/O methods directly on the context.
//! Instead, code creates [`Ring`] handles with [`RuntimeContext::create_ring`],
//! [`RuntimeContext::create_worker_ring`], or
//! [`RuntimeContext::create_shared_ring`]. This keeps worker-local versus shared
//! I/O costs visible to the application. Shared rings submit through runtime
//! workers instead of hiding a helper OS thread per ring; the current shared
//! operation surface includes no-op, timeout, and socket read/write lifecycle
//! operations while broader file/storage I/O remains outside this crate's first
//! downstream migration slice.
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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::{self, JoinHandle as ThreadJoinHandle};
use std::time::{Duration, Instant};

use crossbeam_deque::{Injector, Steal, Stealer, Worker as CrossbeamWorker};
pub use kimojio_stack::StackUsage;
use kimojio_stack::{
    Errno, IoReadBuffer, IoWriteBuffer, NoPayloadIo, RawIo, ReadOutput, RuntimeCapabilities,
    RuntimeCapability, RuntimeFamily, RuntimeWaitable, SchedulerWake, StackfulWaitContext,
    StackfulWaitRegistration, StackfulWaiter, UnsupportedCapability, WaitRegistration, Waitable,
    WriteOutput,
};
use rustix::net::Shutdown;

pub mod runtime_api;

const NO_WORKER: usize = usize::MAX;
const DEFAULT_QUEUE_CAPACITY: usize = 1024;
const DEFAULT_SHARED_RING_QUEUE_CAPACITY: usize = 1024;
static NEXT_RUNTIME_ID: AtomicUsize = AtomicUsize::new(1);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct RuntimeId(usize);

impl RuntimeId {
    fn next() -> Self {
        Self(NEXT_RUNTIME_ID.fetch_add(1, Ordering::Relaxed))
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
    /// Number of worker threads requested for the runtime.
    pub workers: NonZeroUsize,
    /// Policy controlling how queued work may be stolen.
    pub steal_policy: StealPolicy,
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
            workers: NonZeroUsize::new(1).expect("one is nonzero"),
            steal_policy: StealPolicy::Disabled,
            max_worker_queue_len: DEFAULT_QUEUE_CAPACITY,
            max_global_queue_len: DEFAULT_QUEUE_CAPACITY,
            max_shared_ring_queue_len: DEFAULT_SHARED_RING_QUEUE_CAPACITY,
        }
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

/// Scheduler metrics from the most recent [`Runtime::block_on`] invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeMetrics {
    /// Worker count used by the invocation.
    pub worker_count: usize,
    /// Stealing policy used by the invocation.
    pub steal_policy: StealPolicy,
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
    /// Most recent owner worker observed for a completed worker-pool job.
    pub last_owner_worker: Option<WorkerId>,
    /// Most recent executing worker observed for a completed worker-pool job.
    pub last_executing_worker: Option<WorkerId>,
    /// Most recent victim worker selected by the stealing path.
    pub last_steal_victim: Option<WorkerId>,
    /// Per-worker completed worker-pool job counts for utilization visibility.
    pub worker_completed_tasks: Vec<usize>,
}

impl Default for RuntimeMetrics {
    fn default() -> Self {
        Self {
            worker_count: 1,
            steal_policy: StealPolicy::Disabled,
            steal_attempts: 0,
            successful_steals: 0,
            failed_steals: 0,
            global_queue_polls: 0,
            completed_tasks: 0,
            rejected_tasks: 0,
            max_local_queue_depth: 0,
            max_global_queue_depth: 0,
            last_owner_worker: None,
            last_executing_worker: None,
            last_steal_victim: None,
            worker_completed_tasks: vec![0],
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
    pub fn with_config(config: RuntimeConfig) -> Self {
        Self {
            id: RuntimeId::next(),
            config,
            last_metrics: RuntimeMetrics {
                worker_count: config.workers.get(),
                steal_policy: config.steal_policy,
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
        let output = inner.block_on(|inner| {
            let cx = RuntimeContext {
                inner,
                runtime_id: self.id,
                worker: WorkerId(0),
                steal_runtime: None,
                active_steal_scope: None,
                local_shared_operations: Some(Rc::clone(&local_shared_operations)),
                shared_ring_queue_capacity: self.config.max_shared_ring_queue_len,
                socket_ring: RefCell::new(None),
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
        let mut inner = self.inner_runtime();
        let output = panic::catch_unwind(AssertUnwindSafe(|| {
            inner.block_on(|inner| {
                let cx = RuntimeContext {
                    inner,
                    runtime_id: self.id,
                    worker: WorkerId(NO_WORKER),
                    steal_runtime: Some(Arc::clone(&runtime)),
                    active_steal_scope: None,
                    local_shared_operations: None,
                    shared_ring_queue_capacity: self.config.max_shared_ring_queue_len,
                    socket_ring: RefCell::new(None),
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
    local_shared_operations: Option<Rc<LocalSharedOperations>>,
    shared_ring_queue_capacity: usize,
    socket_ring: RefCell<Option<Ring>>,
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
                let read = self.read_async(cx, fd, vec![0_u8; buf.len()])?.get(cx)?;
                let bytes = read.bytes;
                buf[..bytes].copy_from_slice(&read.buffer[..bytes]);
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
            RingInner::Shared(_) => self
                .write_async(cx, fd, buf.to_vec())?
                .get(cx)
                .map(|output| output.bytes),
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

    /// Requests cancellation without waiting for completion.
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

    fn add_stackful_waiter(&self, waiter: Box<dyn StackfulWaiter>) -> bool {
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

    fn add_stackful_waiter(&self, waiter: Box<dyn StackfulWaiter>) -> bool {
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

    fn add_stackful_waiter(&self, waiter: Box<dyn StackfulWaiter>) -> bool {
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

    fn add_stackful_waiter(&self, waiter: Box<dyn StackfulWaiter>) -> bool {
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
            Some(Box::new(StealWaitRegistration {
                inner: registration,
                scope: Arc::clone(scope),
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
    scope: Arc<StealScopeState>,
}

impl StackfulWaitRegistration for StealWaitRegistration<'_> {
    fn waiter(&self) -> Box<dyn StackfulWaiter> {
        let waiter = self.inner.waiter();
        if !self.scope.push_cancel_waiter(self.inner.waiter()) {
            let immediate_wake = self.inner.waiter();
            if immediate_wake.mark_ready() {
                immediate_wake.wake_ready();
            }
        }
        waiter
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
    local_shared_operations: Option<Rc<LocalSharedOperations>>,
    steal_scope: Option<NonNull<RefCell<Option<Arc<StealScopeState>>>>>,
    shared_ring_queue_capacity: usize,
}

impl<'scope, 'env: 'scope> Scope<'scope, 'env> {
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
        JoinHandle {
            inner: JoinInner::Local(self.inner.spawn({
                let runtime_id = self.runtime_id;
                let worker = self.worker;
                let steal_runtime = self.steal_runtime.clone();
                let active_steal_scope = self.active_steal_scope.clone();
                let local_shared_operations = self.local_shared_operations.clone();
                let shared_ring_queue_capacity = self.shared_ring_queue_capacity;
                move |inner| {
                    let cx = RuntimeContext {
                        inner,
                        runtime_id,
                        worker,
                        steal_runtime,
                        active_steal_scope,
                        local_shared_operations,
                        shared_ring_queue_capacity,
                        socket_ring: RefCell::new(None),
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
        let Some(runtime) = self.steal_runtime.clone() else {
            return self.spawn_local(f);
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
            Box::new(move |cx| {
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
            let message = match error {
                SubmitError::ShuttingDown => {
                    "stealable task rejected because runtime is shutting down"
                }
                SubmitError::QueueFull => "stealable task rejected because runtime queue is full",
            };
            join.complete(StealOutcome::Panicked(Box::new(message)));
            steal_scope.complete_one();
        }

        JoinHandle {
            inner: JoinInner::Stealable(join, std::marker::PhantomData),
        }
    }

    /// Spawns local stackful work with a custom usable stack size.
    pub fn spawn_with_stack_size<F, T>(&self, stack_size: usize, f: F) -> JoinHandle<'scope, T>
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
                let local_shared_operations = self.local_shared_operations.clone();
                let shared_ring_queue_capacity = self.shared_ring_queue_capacity;
                move |inner| {
                    let cx = RuntimeContext {
                        inner,
                        runtime_id,
                        worker,
                        steal_runtime,
                        active_steal_scope,
                        local_shared_operations,
                        shared_ring_queue_capacity,
                        socket_ring: RefCell::new(None),
                    };
                    f(&cx)
                }
            })),
        }
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

    fn add_waiter(&self, waiter: Box<dyn StackfulWaiter>) -> bool {
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
    first: Option<Box<dyn StackfulWaiter>>,
    rest: Vec<Box<dyn StackfulWaiter>>,
}

impl StackfulWaiters {
    fn push(&mut self, waiter: Box<dyn StackfulWaiter>) {
        if self.first.is_none() {
            self.first = Some(waiter);
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

struct QueuedJob {
    owner: WorkerId,
    job: Job,
    active_steal_scope: Option<Arc<StealScopeState>>,
}

impl QueuedJob {
    fn new(owner: WorkerId, job: Job, active_steal_scope: Option<Arc<StealScopeState>>) -> Self {
        Self {
            owner,
            job,
            active_steal_scope,
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
    stealers: Vec<Stealer<QueuedJob>>,
    global: Injector<QueuedJob>,
    shared_ring_queues: Vec<Mutex<VecDeque<Arc<SharedRingCore>>>>,
    scheduler_wakes: Vec<Mutex<Option<SchedulerWake>>>,
    next_shared_ring_worker: AtomicUsize,
    local_depths: Vec<AtomicUsize>,
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
            stealers,
            global: Injector::new(),
            shared_ring_queues: (0..worker_count)
                .map(|_| Mutex::new(VecDeque::with_capacity(config.max_shared_ring_queue_len)))
                .collect(),
            scheduler_wakes: (0..worker_count).map(|_| Mutex::new(None)).collect(),
            next_shared_ring_worker: AtomicUsize::new(0),
            local_depths: (0..worker_count).map(|_| AtomicUsize::new(0)).collect(),
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
        let submitted_from_owner_worker = self.config.steal_policy != StealPolicy::Disabled
            && CURRENT_WORKER.with(|current| current.get()) == Some(owner);
        let job = QueuedJob::new(WorkerId(owner), job, active_steal_scope);
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
        Self::reserve_depth(&self.local_depths[worker], self.config.max_worker_queue_len)
    }

    fn release_local(&self, worker: usize, count: usize) {
        Self::release_depth(&self.local_depths[worker], count);
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
    ) -> bool {
        let cores = {
            let mut queue = self.shared_ring_queues[worker.index()]
                .lock()
                .expect("shared ring command queue mutex poisoned");
            std::mem::take(&mut *queue)
        };
        let mut made_progress = false;
        for core in cores {
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
        made_progress
    }

    fn close_worker_shared_operations(
        &self,
        worker: WorkerId,
        active: &mut Vec<ActiveSharedOperation>,
    ) {
        let cores = {
            let mut queue = self.shared_ring_queues[worker.index()]
                .lock()
                .expect("shared ring command queue mutex poisoned");
            std::mem::take(&mut *queue)
        };
        for core in cores {
            core.clear_driver_queued();
            core.close();
        }
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
        let mut inner = kimojio_stack::Runtime::with_config(inner_config);
        inner.block_on(|root| {
            let _scheduler_wake =
                self.install_scheduler_wake(worker, root.internal_scheduler_wake());
            root.scope(|scope| {
                let mut active_shared = Vec::new();
                let mut idle_iterations = 0_usize;
                loop {
                    if self.shutdown.load(Ordering::Acquire) {
                        self.close_worker_shared_operations(worker, &mut active_shared);
                        break;
                    }

                    let observed_wake = self.wake_epoch();
                    let mut made_progress =
                        self.drain_shared_ring_commands(worker, root, &mut active_shared);
                    made_progress |= Self::poll_active_shared_operations(root, &mut active_shared);

                    let spawned =
                        if let Some(ready) = self.take_job(worker, &local_queue, idle_iterations) {
                            let QueuedJob {
                                owner,
                                job,
                                active_steal_scope,
                            } = ready.queued;
                            self.metrics
                                .record_execution(owner, worker, ready.stolen_from);
                            let runtime = Arc::clone(self);
                            scope.spawn(move |inner| {
                                let runtime_id = runtime.runtime_id;
                                let shared_ring_queue_capacity =
                                    runtime.config.max_shared_ring_queue_len;
                                let cx = RuntimeContext {
                                    inner,
                                    runtime_id,
                                    worker,
                                    steal_runtime: Some(runtime),
                                    active_steal_scope,
                                    local_shared_operations: None,
                                    shared_ring_queue_capacity,
                                    socket_ring: RefCell::new(None),
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
                        self.close_worker_shared_operations(worker, &mut active_shared);
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
        RuntimeMetrics {
            worker_count: self.config.workers.get(),
            steal_policy: self.config.steal_policy,
            steal_attempts: self.metrics.steal_attempts.load(Ordering::Relaxed),
            successful_steals: self.metrics.successful_steals.load(Ordering::Relaxed),
            failed_steals: self.metrics.failed_steals.load(Ordering::Relaxed),
            global_queue_polls: self.metrics.global_queue_polls.load(Ordering::Relaxed),
            completed_tasks: self.metrics.completed_tasks.load(Ordering::Relaxed),
            rejected_tasks: self.metrics.rejected_tasks.load(Ordering::Relaxed),
            max_local_queue_depth: self.metrics.max_local_queue_depth.load(Ordering::Relaxed),
            max_global_queue_depth: self.metrics.max_global_queue_depth.load(Ordering::Relaxed),
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
    last_owner_worker: AtomicUsize,
    last_executing_worker: AtomicUsize,
    last_steal_victim: AtomicUsize,
    worker_completed_tasks: Vec<AtomicUsize>,
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
            last_owner_worker: AtomicUsize::new(NO_WORKER),
            last_executing_worker: AtomicUsize::new(NO_WORKER),
            last_steal_victim: AtomicUsize::new(NO_WORKER),
            worker_completed_tasks: (0..worker_count).map(|_| AtomicUsize::new(0)).collect(),
        }
    }

    fn record_local_queue_depth(&self, depth: usize) {
        Self::record_queue_depth(&self.max_local_queue_depth, depth);
    }

    fn record_global_queue_depth(&self, depth: usize) {
        Self::record_queue_depth(&self.max_global_queue_depth, depth);
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
    ) {
        self.last_owner_worker
            .store(owner.index(), Ordering::Relaxed);
        self.last_executing_worker
            .store(executing.index(), Ordering::Relaxed);
        if let Some(victim) = stolen_from {
            self.last_steal_victim
                .store(victim.index(), Ordering::Relaxed);
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

    fn push_cancel_waiter(&self, waiter: Box<dyn StackfulWaiter>) -> bool {
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
    taken: AtomicBool,
}

impl<T> StealJoinState<T> {
    fn new() -> Self {
        Self {
            inner: Mutex::new(StealJoinInner {
                outcome: None,
                waiters: StackfulWaiters::default(),
            }),
            ready: Condvar::new(),
            taken: AtomicBool::new(false),
        }
    }

    fn complete(&self, outcome: StealOutcome<T>) {
        let waiters = {
            let mut inner = self.inner.lock().expect("steal join mutex poisoned");
            if inner.outcome.is_some() {
                return;
            }
            inner.outcome = Some(outcome);
            std::mem::take(&mut inner.waiters)
        };
        waiters.wake_all();
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

    fn add_waiter(&self, waiter: Box<dyn StackfulWaiter>) -> bool {
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
    outcome: Option<StealOutcome<T>>,
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

    pub use super::StealPolicy;
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::HashSet;
    use std::num::NonZeroUsize;
    use std::os::fd::AsRawFd;
    use std::panic;
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
        CrossbeamWorker, ExecutionPlace, MultiRuntime, NO_WORKER, Ring, RingError, RingFd,
        RingInner, RingMode, Runtime, RuntimeConfig, RuntimeId, SharedOpLifecycle, SharedOpRoute,
        SharedOpState, SharedOpTestResource, SharedOperation, SharedOperationKind, SharedRingCore,
        SharedRingWake, StealPolicy, WorkerId,
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
            assert_eq!(drops.load(Ordering::Acquire), 0);
            wait_until(cx, || drops.load(Ordering::Acquire) == 1);
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
            assert_eq!(drops.load(Ordering::Acquire), 0);
            wait_until(cx, || drops.load(Ordering::Acquire) == 1);
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
