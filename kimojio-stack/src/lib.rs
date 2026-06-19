// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Stackful coroutines with structured concurrency.
//!
//! This crate is intentionally not an async runtime. It runs stackful coroutines
//! cooperatively on the current OS thread and exposes scoped spawning so
//! coroutines cannot outlive the scope that created them.
//!
//! # Getting started
//!
//! ```
//! use kimojio_stack::Runtime;
//!
//! let mut runtime = Runtime::new();
//! let answer = runtime.block_on(|cx| {
//!     cx.scope(|scope| {
//!         let worker = scope.spawn(|_| 40 + 2);
//!
//!         worker.join(cx)
//!     })
//! });
//!
//! assert_eq!(answer, 42);
//! ```
//!
//! # Design
//!
//! [`Runtime`] owns a single-threaded cooperative scheduler. [`Runtime::block_on`]
//! creates the scheduler, builds a root [`RuntimeContext`], runs the supplied
//! closure, and returns that closure's value. The closure receives the
//! [`RuntimeContext`] as the capability object for all runtime services: scoped
//! spawning, cooperative yielding, stack dumping, io_uring operations, and I/O
//! result joining.
//!
//! Stackful work is created with [`RuntimeContext::scope`] and [`Scope::spawn`].
//! This mirrors `std::thread::scope`: spawned coroutines may borrow from the
//! caller's stack, but the scope waits for all children before returning, so a
//! stackful coroutine cannot outlive the stack frame it borrowed from. Each
//! coroutine has a small guarded stack allocated by `corosensei`, so stack
//! overflows trap instead of corrupting adjacent memory. [`Scope::spawn_with_stack_size`]
//! overrides the inherited default stack size for one coroutine. [`RuntimeContext::stack_usage`]
//! and [`JoinHandle::join_with_stack_usage`] expose current and final stack usage
//! so applications can tune stack sizes. [`JoinHandle::join`] retrieves the
//! coroutine return value. If a coroutine panic escapes the spawned closure, the
//! panic is propagated to the parent when the handle is joined or when the scope
//! finishes. Unjoined coroutines that are still blocked when their scope body has
//! no runnable work left are canceled before the scope returns.
//!
//! The scheduler is deliberately flat. A coroutine that blocks on a join, scope,
//! channel receive, or I/O operation records an internal waiter and suspends with
//! `yielder.suspend`; it never recursively drives the scheduler from inside that
//! coroutine. Only the root context drives the scheduler loop. Waking a waiter
//! requeues the suspended task by ID, and the scheduler later resumes it on its
//! existing stack.
//!
//! # Scheduling and io_uring
//!
//! The scheduler keeps a ready queue of coroutine task IDs, a table of live
//! coroutine stacks, and one io_uring instance. [`RingEnterPolicy`] controls how
//! often ready tasks are polled before the runtime enters the ring:
//!
//! - [`RingEnterPolicy::AfterReadyBatch`] runs the tasks that were ready at the
//!   beginning of the scheduler tick, then submits and reaps without blocking.
//! - [`RingEnterPolicy::AfterReadyTasks`] runs up to the configured number of
//!   ready tasks, then submits and reaps without blocking.
//!
//! If no tasks are ready but I/O is in flight, [`BusyPoll`] controls whether the
//! root scheduler enters the ring in wait mode or repeatedly checks for
//! completions without blocking the OS thread. If tasks are ready, ring entry is
//! nonblocking so the runtime does not stall runnable coroutines behind kernel
//! work.
//!
//! Completion handling is split into two phases to avoid reentrant scheduler
//! borrows. While the scheduler is mutably borrowed it submits SQEs, drains CQEs
//! into scheduler-owned scratch storage, and updates the in-flight count. After
//! releasing that borrow, completions are applied to their operation state and
//! waiters are woken.
//!
//! Companion runtimes use hidden unstable no-payload I/O hooks to submit
//! scheduler-owned no-op and timeout operations without a user buffer. These
//! handles reuse normal `IoState` lifecycle, waiter, cancellation, detach, and
//! recycle paths; they do not create a second scheduler or allow nested
//! scheduler driving. A hidden scheduler wake handle only interrupts the root
//! io_uring wait so the flat scheduler loop can observe external work.
//!
//! # I/O APIs
//!
//! [`RuntimeContext::read`], [`RuntimeContext::write`],
//! [`RuntimeContext::send`], [`RuntimeContext::recv`],
//! [`RuntimeContext::sendmsg`], [`RuntimeContext::recvmsg`],
//! [`RuntimeContext::accept`], [`RuntimeContext::connect`],
//! [`RuntimeContext::shutdown`], [`RuntimeContext::open`],
//! [`RuntimeContext::link`], [`RuntimeContext::symlink`],
//! [`RuntimeContext::mkdir`], [`RuntimeContext::rmdir`],
//! [`RuntimeContext::unlink`], [`RuntimeContext::rename`],
//! [`RuntimeContext::fadvise`], [`RuntimeContext::madvise`],
//! [`RuntimeContext::fallocate`], [`RuntimeContext::fsync`],
//! [`RuntimeContext::sync_file_range`], [`RuntimeContext::close`],
//! [`RuntimeContext::nop`], and [`RuntimeContext::sleep`] are
//! blocking-from-the-coroutine operations. They submit one SQE, park only the
//! current stackful coroutine, and return after the CQE has been reaped.
//! [`RuntimeContext::socket`] uses io_uring when the kernel supports
//! `IORING_OP_SOCKET` and otherwise falls back to `socket(2)`. [`RuntimeContext::bind`]
//! and [`RuntimeContext::listen`] are synchronous setup helpers because Linux has
//! no io_uring operations for them.
//!
//! [`RuntimeContext::read_async`] and [`RuntimeContext::write_async`] take an
//! [`IoFd`] and return an [`IoResult`]. `IoFd` consumes an [`OwnedFd`] once and
//! clones cheaply without duplicating the kernel fd, so async operations can keep
//! the descriptor alive until the CQE is reaped. `try_get` observes a completion
//! that has already been reaped without entering io_uring or running other tasks.
//! `get` waits cooperatively by parking the current coroutine.
//! [`Cancellable::cancel`] requests cancellation without waiting for the kernel
//! CQE. [`RuntimeContext::join`] waits for multiple [`Waitable`] values at once
//! and can optionally use an io_uring timeout; timed out operations remain pending
//! and their handles can still be completed later.
//!
//! Async I/O takes ownership of buffers instead of borrowing them. This keeps the
//! safe API sound even if a handle is dropped or forgotten while the kernel still
//! has a pointer. Buffer ownership is abstracted with [`IoReadBuffer`] and
//! [`IoWriteBuffer`]; `Vec<u8>` is the default buffer type and `Box<[u8]>` is
//! also supported. Successful read/write completions return the original buffer
//! in [`ReadOutput`] or [`WriteOutput`]. If an [`IoResult`] is canceled or
//! dropped while its operation is still pending, the runtime requests kernel
//! cancellation, detaches the operation from the user handle, and reclaims the
//! backing buffer after the CQE is reaped. Operation state still owns a cheap
//! clone of the submitted [`IoFd`] until that reaping point. Forgetting an
//! [`IoResult`] leaks the handle and buffer deliberately; complete pending
//! results with `get`, `try_get`, `cancel`, or `detach` when reclamation matters.
//!
//! [`Runtime::with_registered_resources`] configures io_uring fixed-file and
//! fixed-buffer tables for lower-overhead I/O. [`RuntimeContext::register_fd`]
//! returns a [`RegisteredFd`] that owns an [`OwnedFd`] in a fixed-file slot.
//! [`RuntimeContext::register_buffer`] returns a [`RegisteredBuffer`] whose
//! backing memory is registered in a fixed-buffer slot. Fixed-resource I/O
//! records resource leases in the operation state and releases them at CQE
//! completion, so dropping a registered fd while I/O is pending retires the slot
//! only after the kernel is done with it. Forgetting a pending async result leaks
//! the registered handle, which preserves safety by keeping the kernel-visible
//! fd or buffer alive.
//!
//! # Synchronization primitives
//!
//! The crate provides stackful, cooperative synchronization primitives in
//! separate modules rather than embedding them in the runtime core:
//!
//! - [`mutex`] provides [`Mutex`] and [`MutexGuard`].
//! - [`semaphore`] provides [`Semaphore`] and [`SemaphorePermit`].
//! - [`notify`] provides [`Notify`] and [`Notified`].
//! - [`watch`] provides a latest-value watch channel.
//! - [`barrier`] provides [`Barrier`].
//! - [`rwlock`] provides [`RwLock`] with read and write guards.
//! - [`channel`] provides bounded and unbounded multi-producer/multi-consumer
//!   channels, including [`channel::cross_thread`] endpoints for crossing
//!   runtime, OS-thread, and Tokio-compatible task boundaries.
//! - [`once`] provides a single-send channel.
//!
//! These primitives never block the OS thread. When a stackful coroutine cannot
//! make progress, it records a waiter in the primitive and parks through the
//! runtime scheduler. When a primitive changes state, it wakes one or more
//! waiters, which requeues their coroutine task IDs.
//!
//! # Waiting on heterogeneous conditions
//!
//! [`Waitable`] is the common readiness abstraction used by I/O results,
//! [`JoinHandle`], and synchronization primitives. It is intentionally
//! readiness-only: [`RuntimeContext::wait_any`] returns the index of a ready
//! condition and [`RuntimeContext::wait_all`] returns when every condition is
//! ready. Values are consumed with typed APIs afterward, such as
//! [`IoResult::try_get`], [`JoinHandle::try_join`], [`Mutex::try_lock`], channel
//! `try_recv`, or semaphore `try_acquire`.
//!
//! Wait APIs take `&dyn Waitable` references rather than values. Taking
//! references allows heterogeneous conditions to be combined without erasing
//! their result types, avoids consuming pending operations on timeout, and keeps
//! ownership with the typed handle that knows how to recover its value or guard.
//!
//! # Allocation model
//!
//! Setup paths allocate: the runtime owns scheduler containers and io_uring,
//! initial coroutine spawns allocate guarded stacks and join state, scopes own
//! their structured-concurrency state, and channels own their shared state.
//! Completed coroutine stacks and completed scope states are retained in bounded
//! runtime-local caches so hot spawn/scope paths can reuse guarded stacks and
//! scope bookkeeping without paying setup allocation per short-lived operation.
//! These allocations are expected and bounded by runtime structure.
//! [`RuntimeContext::pool_diagnostics`] exposes a read-only snapshot of retained
//! scheduler pool capacity for observability; it is not allocator accounting.
//!
//! Hot I/O paths are designed to avoid Rust heap allocation after warmup. The
//! scheduler reuses I/O operation state through an internal pool, reuses
//! completion scratch storage across ring entries, and stores the common single
//! waiter inline before falling back to an overflow vector. Tests install a
//! test-only counting allocator and assert that warmed blocking and async
//! pipe-ping-pong loops perform zero Rust allocations. Kernel allocations and
//! io_uring mappings are outside those tests' scope.
//!
//! The crate does not install a global allocator for downstream users. Repository
//! performance harnesses may choose an allocator such as mimalloc, but allocator
//! selection is intentionally left to the final application.
//!
//! # Scope and limitations
//!
//! The runtime is currently single-threaded and Linux/io_uring oriented. It is
//! intended for code that wants explicit control over cooperative scheduling and
//! stackful call chains. Tokio-compatible integration is intentionally limited to
//! explicit interop surfaces such as [`channel::cross_thread`].

use std::any::Any;
use std::cell::{Cell, RefCell, UnsafeCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::fmt;
use std::io;
use std::marker::PhantomData;
use std::mem::{ManuallyDrop, MaybeUninit};
use std::ops::{Deref, DerefMut};
use std::os::fd::FromRawFd;
use std::panic::{self, AssertUnwindSafe};
use std::ptr::NonNull;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex as StdMutex};
use std::time::{Duration, Instant};

use corosensei::stack::{DefaultStack, MIN_STACK_SIZE, Stack as CoroStack};
use corosensei::{Coroutine, CoroutineResult, Yielder};
pub use rustix::event::epoll::{
    Event as EpollEvent, EventData as EpollEventData, EventFlags as EpollEventFlags,
};
use rustix::event::{EventfdFlags, PollFlags, eventfd};
pub use rustix::fd::OwnedFd;
use rustix::fd::{AsFd, AsRawFd, BorrowedFd};
use rustix::fs;
pub use rustix::fs::{
    Advice as FileAdvice, AtFlags, FallocateFlags, Mode, OFlags, RenameFlags, ResolveFlags, Statx,
    StatxFlags,
};
use rustix::io as rustix_io;
pub use rustix::io_uring::{
    FutexWaitFlags, FutexWaitvFlags, MsgHdr, SpliceFlags as UringSpliceFlags, iovec as IoVec,
};
use rustix::io_uring::{IoringOp, io_uring_user_data};
pub use rustix::mm::Advice as MemoryAdvice;
use rustix::net::{self, addr::SocketAddrArg};
pub use rustix::net::{
    AddressFamily, Protocol, RecvFlags, SendFlags, Shutdown, SocketType, ipproto,
};
use rustix::path;
pub use rustix::pipe::SpliceFlags;
use rustix::{mm, param};
pub use rustix_uring::Errno;
use rustix_uring::opcode;
use rustix_uring::{
    Probe,
    cqueue::Entry as Cqe,
    squeue::{Entry128 as Sqe, Flags},
    types::{CancelBuilder, Fd, Fixed, FutexWaitV, OpenHow, Timespec},
};

pub mod barrier;
pub mod channel;
pub mod mutex;
pub mod notify;
pub mod once;
pub mod runtime_api;
pub mod rwlock;
pub mod semaphore;
pub mod wait;
pub mod watch;

pub use barrier::{Barrier, BarrierWaitResult};
pub use mutex::{Mutex, MutexGuard};
pub use notify::{Notified, Notify};
pub use runtime_api::{
    IoRuntime, RuntimeCapabilities, RuntimeCapability, RuntimeFamily, RuntimeIoError,
    RuntimeIoErrorKind, RuntimeReadResult, RuntimeSocket, RuntimeWaitError, RuntimeWaitable,
    RuntimeWaitableAdapter, RuntimeWriteResult, SocketIoRuntime, StackRuntime, StackRuntimeContext,
    StackfulWaitContext, StackfulWaitRegistration, StackfulWaiter, StackfulWaiterHandle,
    UnsupportedCapability,
};
pub use rwlock::{ReadLock, RwLock, RwLockReadGuard, RwLockWriteGuard, WriteLock};
pub use semaphore::{Semaphore, SemaphorePermit};
pub use wait::{IoHandle, IoJoinError, WaitError, Waitable};
pub use watch::{WatchReceiver, WatchSender};

const DEFAULT_STACK_SIZE: usize = 64 * 1024;
const DEFAULT_RING_ENTRIES: u32 = 128;
const DEFAULT_STACK_CACHE_CAPACITY: usize = 1024;
const DEFAULT_SCOPE_STATE_CACHE_CAPACITY: usize = 1024;
const DEFAULT_SCOPE_STATE_COLLECTION_CACHE_CAPACITY: usize = 32;
const WAITER_COMPACT_THRESHOLD: usize = 32;

thread_local! {
    static CURRENT_CONTEXT: Cell<*const ()> = const { Cell::new(std::ptr::null()) };
}

#[cfg(test)]
#[global_allocator]
static TEST_ALLOCATOR: allocation_tracking::CountingAllocator =
    allocation_tracking::CountingAllocator;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct TaskKey {
    slot: usize,
    generation: u32,
}

impl TaskKey {
    fn index(self) -> usize {
        self.slot
    }
}

type PanicPayload = Box<dyn Any + Send + 'static>;
type KernelIoResult = Result<u32, Errno>;
type IoUring = rustix_uring::IoUring<Sqe, Cqe>;

struct TaskCanceled;

/// A handle for kernel work that can be canceled without blocking.
///
/// Cancellation is best-effort at the submission boundary: the method requests
/// cancellation, detaches user interest, and returns immediately. The runtime
/// still drains the eventual completion before [`Runtime::block_on`] returns, so
/// cancellation never leaves an io_uring operation owned by a dropped scheduler.
pub trait Cancellable {
    /// Requests cancellation of the pending operation.
    fn cancel(&mut self);
}

fn effective_stack_size(stack_size: usize) -> usize {
    let page_size = param::page_size();
    stack_size
        .max(MIN_STACK_SIZE)
        .checked_add(page_size - 1)
        .expect("stack size overflow")
        & !(page_size - 1)
}

/// Stack usage information for one stackful coroutine.
///
/// `used` is a byte-level estimate from the current stack pointer. `high_water`
/// is a conservative page-granular resident-stack estimate plus the largest
/// current usage observed while taking the measurement. It may count a whole
/// page after only part of that page was used, which is useful when tuning stack
/// sizes because stacks are mapped and protected at page granularity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StackUsage {
    size: usize,
    used: usize,
    high_water: usize,
}

impl StackUsage {
    fn new(size: usize, used: usize, high_water: usize) -> Self {
        debug_assert!(used <= size);
        debug_assert!(high_water <= size);
        Self {
            size,
            used,
            high_water: high_water.max(used),
        }
    }

    /// Returns usable stack bytes for this coroutine.
    pub fn size(self) -> usize {
        self.size
    }

    /// Returns the currently used stack bytes.
    pub fn used(self) -> usize {
        self.used
    }

    /// Returns currently unused stack bytes.
    pub fn remaining(self) -> usize {
        self.size - self.used
    }

    /// Returns the observed high-water stack usage.
    pub fn high_water(self) -> usize {
        self.high_water
    }

    /// Returns stack bytes not reached by the observed high-water mark.
    pub fn high_water_remaining(self) -> usize {
        self.size - self.high_water
    }
}

/// Captured stack information for live stackful coroutines.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StackDump {
    tasks: Vec<TaskStackDump>,
}

impl StackDump {
    /// Returns one entry per live stackful coroutine known to the scheduler.
    pub fn tasks(&self) -> &[TaskStackDump] {
        &self.tasks
    }

    /// Returns `true` if no live stackful coroutine was present.
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }
}

/// Captured stack information for a single stackful coroutine.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskStackDump {
    task_id: usize,
    state: TaskStackState,
    stack_usage: StackUsage,
    backtrace: Option<String>,
}

impl TaskStackDump {
    fn not_started(task_id: TaskKey, stack: StackTracker) -> Self {
        Self {
            task_id: task_id.index(),
            state: TaskStackState::NotStarted,
            stack_usage: stack.usage_without_current(),
            backtrace: None,
        }
    }

    fn suspended(interrupt: TaskInterrupt) -> Self {
        let stack_usage = interrupt.stack_usage();
        Self {
            task_id: interrupt.task_id(),
            state: TaskStackState::Suspended,
            stack_usage,
            backtrace: Some(std::backtrace::Backtrace::force_capture().to_string()),
        }
    }

    /// Returns the runtime-local task id.
    pub fn task_id(&self) -> usize {
        self.task_id
    }

    /// Returns the observed coroutine state.
    pub fn state(&self) -> TaskStackState {
        self.state
    }

    /// Returns stack usage observed for this coroutine.
    pub fn stack_usage(&self) -> StackUsage {
        self.stack_usage
    }

    /// Returns the captured backtrace, if this task had started and was suspended.
    pub fn backtrace(&self) -> Option<&str> {
        self.backtrace.as_deref()
    }
}

/// State of a stackful coroutine observed by [`RuntimeContext::dump_stacks`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskStackState {
    /// The coroutine exists but has not been resumed yet, so there is no stackful
    /// call chain to capture.
    NotStarted,
    /// The coroutine is suspended at a yield or park point and supplied a
    /// backtrace from its own stack.
    Suspended,
}

/// Operation for [`RuntimeContext::epoll_ctl`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EpollCtlOp {
    /// Add a file descriptor to an epoll interest list.
    Add = 1,
    /// Remove a file descriptor from an epoll interest list.
    Delete = 2,
    /// Modify an existing epoll interest.
    Modify = 3,
}

fn memory_advice_for_uring(advice: MemoryAdvice) -> Option<FileAdvice> {
    match advice {
        MemoryAdvice::Normal => Some(FileAdvice::Normal),
        MemoryAdvice::Sequential => Some(FileAdvice::Sequential),
        MemoryAdvice::Random => Some(FileAdvice::Random),
        MemoryAdvice::WillNeed => Some(FileAdvice::WillNeed),
        MemoryAdvice::LinuxDontNeed => Some(FileAdvice::DontNeed),
        _ => None,
    }
}

/// Runs stackful coroutines on the current OS thread.
#[derive(Debug)]
pub struct Runtime {
    config: RuntimeConfig,
}

/// Runtime configuration.
///
/// This alpha API is non-exhaustive so runtime-owned tuning knobs can grow
/// without forcing downstream struct-literal updates. Use
/// `RuntimeConfig::default()` with field update syntax.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct RuntimeConfig {
    /// Usable stack bytes for each stackful coroutine.
    pub stack_size: usize,
    /// Maximum completed coroutine stacks retained for reuse by this runtime.
    ///
    /// Reusing guarded stacks keeps hot spawn paths from paying `mmap`,
    /// `mprotect`, and `munmap` on every short-lived coroutine.
    pub max_cached_stacks: usize,
    /// Number of entries in the io_uring submission queue.
    pub ring_entries: u32,
    /// Policy controlling how often the scheduler enters the ring.
    pub ring_enter_policy: RingEnterPolicy,
    /// Policy controlling whether pending I/O completions are busy-polled.
    pub busy_poll: BusyPoll,
    /// Number of fixed-file slots to register with io_uring.
    pub registered_file_slots: u32,
    /// Number of fixed-buffer slots to register with io_uring.
    pub registered_buffer_slots: u16,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            stack_size: DEFAULT_STACK_SIZE,
            max_cached_stacks: DEFAULT_STACK_CACHE_CAPACITY,
            ring_entries: DEFAULT_RING_ENTRIES,
            ring_enter_policy: RingEnterPolicy::default(),
            busy_poll: BusyPoll::default(),
            registered_file_slots: 0,
            registered_buffer_slots: 0,
        }
    }
}

/// Runtime I/O lifecycle counters.
///
/// These counters are snapshots intended for tests and diagnostics. They track
/// detach/cancel lifecycle events inside one [`Runtime::block_on`] execution and
/// are not reset while that runtime invocation is active.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IoCounters {
    /// Number of detached operations that have not been reaped yet.
    pub detached_pending: usize,
    /// Total operations detached from user handles.
    pub detached_total: usize,
    /// Cancel SQEs submitted for pending operations.
    pub cancel_requests: usize,
    /// Cancel requests that could not be submitted because the opcode is unsupported.
    pub unsupported_cancels: usize,
    /// Cancel SQEs that have completed.
    pub cancel_completions: usize,
    /// Cancel SQEs that completed with an error result.
    pub cancel_completion_errors: usize,
    /// Original operation CQEs observed after cancellation had been requested.
    pub original_completions_after_cancel: usize,
}

/// Snapshot of runtime-owned scheduler pool state.
///
/// This diagnostic surface reports retained capacity owned by the active
/// [`Runtime::block_on`] invocation. It is intended for observability and tests,
/// not as exact allocator accounting. Fields may grow as this alpha runtime adds
/// more internal pools.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct PoolDiagnostics {
    /// Number of task slots retained by the scheduler.
    pub task_slots: usize,
    /// Capacity of the task-slot vector.
    pub task_slot_capacity: usize,
    /// Number of reusable task slots currently on the free list.
    pub free_task_slots: usize,
    /// Number of waiter token slots retained by the scheduler.
    pub wait_token_slots: usize,
    /// Capacity of the waiter-token vector.
    pub wait_token_slot_capacity: usize,
    /// Number of reusable waiter token slots currently on the free list.
    pub free_wait_token_slots: usize,
    /// Number of completed coroutine stacks currently cached.
    pub cached_stacks: usize,
    /// Maximum completed coroutine stacks retained by this runtime.
    pub max_cached_stacks: usize,
    /// Number of completed scope states currently cached.
    pub cached_scope_states: usize,
    /// Maximum completed scope states retained by this runtime.
    pub max_cached_scope_states: usize,
    /// Number of completed I/O states currently cached.
    pub cached_io_states: usize,
    /// Capacity of the I/O state pool.
    pub io_state_pool_capacity: usize,
    /// Number of completion scratch records currently held.
    pub completion_scratch_len: usize,
    /// Capacity of the reusable completion scratch storage.
    pub completion_scratch_capacity: usize,
    /// Number of detached I/O operations awaiting completion/reap.
    pub detached_pending_io: usize,
    /// Registered fixed-file slots currently free for reuse.
    pub registered_file_free_slots: usize,
    /// Registered fixed-buffer slots currently free for reuse.
    pub registered_buffer_free_slots: usize,
}

/// Controls whether the scheduler busy-polls for pending I/O completions.
///
/// Busy polling avoids putting the OS thread to sleep while an io_uring
/// operation is in flight, which can reduce tail latency for short operations at
/// the cost of burning CPU. It only affects the root scheduler's behavior while
/// I/O is pending and no stackful coroutine is ready to run.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BusyPoll {
    /// Never busy poll; suspend the OS thread while waiting for I/O completions.
    #[default]
    Never,
    /// Always busy poll while I/O is pending.
    Always,
    /// Busy poll until `duration` has elapsed since the runtime started.
    Until(Duration),
}

impl From<Option<Duration>> for BusyPoll {
    fn from(value: Option<Duration>) -> Self {
        match value {
            Some(duration) => Self::Until(duration),
            None => Self::Never,
        }
    }
}

/// Policy controlling how often the scheduler submits and reaps io_uring work.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RingEnterPolicy {
    /// Run the tasks that were ready when the scheduler tick began, then submit
    /// and reap without blocking.
    #[default]
    AfterReadyBatch,
    /// Run up to the configured number of ready tasks, then submit and reap
    /// without blocking.
    AfterReadyTasks(usize),
}

impl RingEnterPolicy {
    fn ready_budget(self, ready_len: usize) -> usize {
        match self {
            Self::AfterReadyBatch => ready_len,
            Self::AfterReadyTasks(limit) => limit.max(1).min(ready_len),
        }
    }
}

impl Runtime {
    /// Creates a runtime that uses a small guarded stack for each coroutine.
    pub fn new() -> Self {
        Self {
            config: RuntimeConfig::default(),
        }
    }

    /// Creates a runtime with a custom usable stack size for each coroutine.
    ///
    /// The stack allocator adds a guard page so stack overflow does not corrupt
    /// adjacent memory.
    pub fn with_stack_size(stack_size: usize) -> Self {
        Self {
            config: RuntimeConfig {
                stack_size,
                ..RuntimeConfig::default()
            },
        }
    }

    /// Creates a runtime with custom busy-poll behavior.
    pub fn with_busy_poll(busy_poll: BusyPoll) -> Self {
        Self {
            config: RuntimeConfig {
                busy_poll,
                ..RuntimeConfig::default()
            },
        }
    }

    /// Creates a runtime with custom configuration.
    pub fn with_config(config: RuntimeConfig) -> Self {
        Self { config }
    }

    /// Creates a runtime configured with fixed-file and fixed-buffer slots.
    pub fn with_registered_resources(
        registered_file_slots: u32,
        registered_buffer_slots: u16,
    ) -> Self {
        Self {
            config: RuntimeConfig {
                registered_file_slots,
                registered_buffer_slots,
                ..RuntimeConfig::default()
            },
        }
    }

    /// Runs `main` to completion on the current OS thread.
    pub fn block_on<F, T>(&mut self, main: F) -> T
    where
        F: FnOnce(&RuntimeContext<'_>) -> T,
    {
        let core = Rc::new(SchedulerCell::new(Scheduler::new(self.config)));
        let cx = RuntimeContext {
            core: Rc::clone(&core),
            stack_size: effective_stack_size(self.config.stack_size),
            current: CurrentTask::Root,
            active_scopes: RefCell::new(Vec::new()),
        };
        let output = {
            let _current_context = CurrentContextGuard::enter(&cx);
            panic::catch_unwind(AssertUnwindSafe(|| main(&cx)))
        };
        drain_scheduler_io(&core);

        let scheduler = core.borrow();
        debug_assert!(
            scheduler.is_empty(),
            "all stackful coroutines should be scoped"
        );
        debug_assert_eq!(
            scheduler.in_flight_io, 0,
            "io_uring operations should complete before Runtime::block_on returns"
        );
        debug_assert_eq!(
            scheduler.detached_io.len(),
            0,
            "detached io_uring operations should be reaped before Runtime::block_on returns"
        );

        match output {
            Ok(output) => output,
            Err(payload) => panic::resume_unwind(payload),
        }
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

/// Methods available to code running inside a [`Runtime`].
pub struct RuntimeContext<'cx> {
    core: Rc<SchedulerCell>,
    stack_size: usize,
    current: CurrentTask<'cx>,
    active_scopes: RefCell<Vec<Rc<ScopeState>>>,
}

/// Borrowed view of the currently executing [`RuntimeContext`].
///
/// The borrow is valid only for the duration of the callback passed to
/// [`RuntimeContext::with_current`].
pub struct CurrentRuntimeContext<'cx> {
    inner: &'cx RuntimeContext<'cx>,
}

impl<'cx> Deref for CurrentRuntimeContext<'cx> {
    type Target = RuntimeContext<'cx>;

    fn deref(&self) -> &Self::Target {
        self.inner
    }
}

struct CurrentContextGuard {
    previous: *const (),
}

impl CurrentContextGuard {
    fn enter(cx: &RuntimeContext<'_>) -> Self {
        let current = (cx as *const RuntimeContext<'_>).cast::<()>();
        let previous = CURRENT_CONTEXT.with(|slot| slot.replace(current));
        Self { previous }
    }
}

impl Drop for CurrentContextGuard {
    fn drop(&mut self) {
        CURRENT_CONTEXT.with(|slot| slot.set(self.previous));
    }
}

struct CurrentContextClearGuard {
    previous: *const (),
}

impl CurrentContextClearGuard {
    fn enter() -> Self {
        let previous = CURRENT_CONTEXT.with(|slot| slot.replace(std::ptr::null()));
        Self { previous }
    }
}

impl Drop for CurrentContextClearGuard {
    fn drop(&mut self) {
        CURRENT_CONTEXT.with(|slot| slot.set(self.previous));
    }
}

fn with_current_context_cleared<T>(f: impl FnOnce() -> T) -> T {
    let _clear_context = CurrentContextClearGuard::enter();
    f()
}

fn suspend_with_current_context_cleared(
    yielder: &Yielder<ResumeAction, Suspend>,
    suspend: Suspend,
) -> ResumeAction {
    let _clear_context = CurrentContextClearGuard::enter();
    yielder.suspend(suspend)
}

impl RuntimeContext<'_> {
    /// Invokes `f` with the runtime context currently executing on this OS thread.
    ///
    /// This is hidden from public API docs because it exists for synchronous
    /// callback integrations. It returns `None` outside a running kimojio-stack
    /// context and while the root scheduler is driving other stackful work.
    #[doc(hidden)]
    pub fn with_current<T>(f: impl for<'cx> FnOnce(CurrentRuntimeContext<'cx>) -> T) -> Option<T> {
        let current = CURRENT_CONTEXT.with(Cell::get);
        if current.is_null() {
            return None;
        }

        unsafe fn call_with_current<T>(
            current: *const (),
            f: impl for<'cx> FnOnce(CurrentRuntimeContext<'cx>) -> T,
        ) -> T {
            struct ErasedContext(*const ());

            impl ErasedContext {
                unsafe fn get<'cx>(&self) -> CurrentRuntimeContext<'cx> {
                    CurrentRuntimeContext {
                        // SAFETY: `CURRENT_CONTEXT` is set only by
                        // `CurrentContextGuard` while a root or coroutine
                        // `RuntimeContext` is actively executing user code on
                        // this OS thread. The higher-ranked callback prevents a
                        // safe caller from returning a borrow tied to this
                        // context.
                        inner: unsafe { &*(self.0.cast::<RuntimeContext<'cx>>()) },
                    }
                }
            }

            // SAFETY: The erased pointer came from `CurrentContextGuard::enter`
            // for the currently executing runtime context on this OS thread.
            f(unsafe { ErasedContext(current).get() })
        }

        // SAFETY: The pointer is either null (handled above) or installed by
        // `CurrentContextGuard::enter` for this OS thread.
        Some(unsafe { call_with_current(current, f) })
    }

    /// Creates a structured concurrency scope.
    ///
    /// All stackful coroutines spawned through the scope finish before this
    /// method returns.
    pub fn scope<'env, F, T>(&'env self, f: F) -> T
    where
        F: for<'scope> FnOnce(&'scope Scope<'scope, 'env>) -> T,
    {
        let state = self.core.borrow_mut().acquire_scope_state();
        self.active_scopes.borrow_mut().push(Rc::clone(&state));
        let result = {
            let scope = Scope {
                core: Rc::clone(&self.core),
                state: Rc::clone(&state),
                stack_size: self.stack_size,
                _scope: PhantomData,
                _env: PhantomData,
            };
            let result = panic::catch_unwind(AssertUnwindSafe(|| f(&scope)));
            self.wait_for_scope(&state);
            result
        };

        let popped = self
            .active_scopes
            .borrow_mut()
            .pop()
            .expect("active scope stack underflow");
        debug_assert!(Rc::ptr_eq(&popped, &state));
        drop(popped);
        let child_panic_payload = state.take_panic_payload();
        self.core.borrow_mut().recycle_scope_state(state);

        match result {
            Ok(output) => {
                if let Some(payload) = child_panic_payload {
                    panic::resume_unwind(payload);
                }
                output
            }
            Err(payload) => panic::resume_unwind(payload),
        }
    }

    /// Returns the default usable stack size inherited by new scopes.
    ///
    /// Root contexts report the runtime default. Stackful coroutine contexts
    /// report that coroutine's configured stack size, which nested scopes inherit
    /// unless a spawn overrides it with [`Scope::spawn_with_stack_size`].
    pub fn stack_size(&self) -> usize {
        self.stack_size
    }

    /// Returns stack usage for the current stackful coroutine.
    ///
    /// The root context runs on the caller's OS thread stack, not on a guarded
    /// runtime stack, so it returns `None`.
    pub fn stack_usage(&self) -> Option<StackUsage> {
        match self.current {
            CurrentTask::Root => None,
            CurrentTask::Coroutine { stack, .. } => Some(stack.usage()),
        }
    }

    #[cfg(test)]
    fn scheduler_task_slots(&self) -> usize {
        self.core.borrow().task_slot_len()
    }

    #[cfg(test)]
    fn cached_stack_count(&self) -> usize {
        self.core.borrow().cached_stack_count()
    }

    #[cfg(test)]
    fn reused_stack_count(&self) -> usize {
        self.core.borrow().reused_stack_count()
    }

    /// Cooperatively yields the current stackful coroutine.
    pub fn yield_now(&self) {
        let _ = self.internal_scheduler_tick();
    }

    /// Drives one scheduler tick and reports whether scheduler progress was made.
    ///
    /// This is hidden from the public API docs because it exists for companion
    /// schedulers that need to multiplex their own work queues with this
    /// runtime's stackful tasks and io_uring completions.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn internal_scheduler_tick(&self) -> bool {
        let made_progress = match self.current {
            CurrentTask::Root => with_current_context_cleared(|| drive_scheduler(&self.core)),
            CurrentTask::Coroutine { id, yielder, stack } => {
                suspend_task(id, yielder, stack, Suspend::Ready);
                true
            }
        };
        self.resume_active_scope_panic();
        made_progress
    }

    /// Captures stack traces for live stackful coroutines.
    ///
    /// This must be called from the root context. Started coroutines are resumed
    /// with an interrupt action that captures a [`std::backtrace::Backtrace`]
    /// on the coroutine stack and then immediately suspends again without
    /// advancing normal user work. Coroutines that have not started yet are
    /// reported as [`TaskStackState::NotStarted`] because they do not have a
    /// stackful call chain to inspect.
    pub fn dump_stacks(&self) -> StackDump {
        assert!(
            matches!(self.current, CurrentTask::Root),
            "dump_stacks must be called from the root RuntimeContext"
        );

        self.core.borrow_mut().dump_stacks()
    }

    /// Returns an unstable internal wake handle for companion runtimes.
    ///
    /// This is hidden from the public API docs because it exists to let runtime
    /// integration code interrupt this runtime's root io_uring wait without
    /// registering a stackful waiter. Calling the handle only wakes the root
    /// wait path; companion runtimes must still let this scheduler drive
    /// completions from its normal root loop.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn internal_scheduler_wake(&self) -> SchedulerWake {
        let external_wake = self.core.borrow().external_wake();
        SchedulerWake::new(external_wake)
    }

    /// Submits an io_uring no-op and waits for its completion.
    pub fn nop(&self) -> Result<(), Errno> {
        self.submit_and_wait_for_io(opcode::Nop::new().build())
            .map(|_| ())
    }

    /// Submits an unstable internal no-payload io_uring no-op without waiting.
    ///
    /// The returned handle owns normal scheduler operation state and must be
    /// waited, canceled, or detached so the state can be recycled after the CQE
    /// is reaped.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn submit_no_payload_nop(&self) -> NoPayloadIo {
        let state = self.acquire_io_state();
        let user_data = self.submit_io_state(opcode::Nop::new().build(), Rc::clone(&state));
        NoPayloadIo::new(
            Rc::downgrade(&self.core),
            state,
            IoCancelKind::Async(user_data),
            NoPayloadResultKind::Unit,
        )
    }

    /// Starts an internal no-payload no-op linked to a timeout.
    ///
    /// The timeout CQE carries null user data and is reaped only for in-flight
    /// accounting. The returned handle completes with the no-op result, or
    /// `Err(Errno::TIME)` if the linked timeout cancels the operation first.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn submit_no_payload_nop_with_timeout(&self, duration: Duration) -> NoPayloadIo {
        let state = self.acquire_io_state();
        let user_data = self.submit_io_state_with_link_timeout(
            opcode::Nop::new().build(),
            Rc::clone(&state),
            duration,
        );
        NoPayloadIo::new(
            Rc::downgrade(&self.core),
            state,
            IoCancelKind::Async(user_data),
            NoPayloadResultKind::Unit,
        )
    }

    /// Submits an unstable internal no-payload io_uring timeout without waiting.
    ///
    /// The returned handle follows the same timeout result, cancellation, and
    /// recycling rules as public timeout operations, but carries no user payload.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn submit_no_payload_timeout(&self, duration: Duration) -> NoPayloadIo {
        let timeout = self.submit_timeout(duration);
        NoPayloadIo::new(
            Rc::downgrade(&self.core),
            timeout.state,
            IoCancelKind::Timeout(timeout.user_data),
            NoPayloadResultKind::Timeout,
        )
    }

    /// Submits an unstable internal read operation without waiting.
    ///
    /// # Safety
    ///
    /// `buffer` must point to writable memory of at least `len` bytes. The
    /// descriptor and buffer must remain valid until the returned handle
    /// completes. If the operation is canceled, detached, or dropped before
    /// completion, callers must use [`RawIo::cancel_with_payload`] or
    /// [`RawIo::detach_with_payload`] so the descriptor/buffer owner is retained
    /// until the kernel completion is reaped; bare cancellation/detach/drop of a
    /// pending raw read is rejected to prevent the kernel from writing through a
    /// freed pointer.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub unsafe fn submit_raw_read(&self, fd: &impl AsFd, buffer: *mut u8, len: usize) -> RawIo {
        let len = u32::try_from(len).expect("io_uring read length exceeds u32::MAX");
        let fd = fd.as_fd().as_raw_fd();
        let entry = opcode::Read::new(Fd(fd), buffer, len)
            .offset(u64::MAX)
            .build();
        self.submit_raw_io(entry, true, true)
    }

    /// Submits an internal read linked to a timeout.
    ///
    /// # Safety
    ///
    /// `buffer` must satisfy the same requirements as
    /// [`RuntimeContext::submit_raw_read`].
    #[doc(hidden)]
    #[allow(dead_code)]
    pub unsafe fn submit_raw_read_with_timeout(
        &self,
        fd: &impl AsFd,
        buffer: *mut u8,
        len: usize,
        timeout: Duration,
    ) -> RawIo {
        let len = u32::try_from(len).expect("io_uring read length exceeds u32::MAX");
        let fd = fd.as_fd().as_raw_fd();
        let entry = opcode::Read::new(Fd(fd), buffer, len)
            .offset(u64::MAX)
            .build();
        self.submit_raw_io_with_link_timeout(entry, true, true, timeout)
    }

    /// Submits an unstable internal write operation without waiting.
    ///
    /// # Safety
    ///
    /// `buffer` must point to readable memory of at least `len` bytes. The
    /// descriptor and buffer must remain valid until the returned handle
    /// completes. If the operation is canceled, detached, or dropped before
    /// completion, callers must use [`RawIo::cancel_with_payload`] or
    /// [`RawIo::detach_with_payload`] so the descriptor/buffer owner is retained
    /// until the kernel completion is reaped; bare cancellation/detach/drop of a
    /// pending raw write is rejected to prevent the kernel from reading through a
    /// freed pointer.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub unsafe fn submit_raw_write(&self, fd: &impl AsFd, buffer: *const u8, len: usize) -> RawIo {
        let len = u32::try_from(len).expect("io_uring write length exceeds u32::MAX");
        let fd = fd.as_fd().as_raw_fd();
        let entry = opcode::Write::new(Fd(fd), buffer, len)
            .offset(u64::MAX)
            .build();
        self.submit_raw_io(entry, true, true)
    }

    /// Submits an internal write linked to a timeout.
    ///
    /// # Safety
    ///
    /// `buffer` must satisfy the same requirements as
    /// [`RuntimeContext::submit_raw_write`].
    #[doc(hidden)]
    #[allow(dead_code)]
    pub unsafe fn submit_raw_write_with_timeout(
        &self,
        fd: &impl AsFd,
        buffer: *const u8,
        len: usize,
        timeout: Duration,
    ) -> RawIo {
        let len = u32::try_from(len).expect("io_uring write length exceeds u32::MAX");
        let fd = fd.as_fd().as_raw_fd();
        let entry = opcode::Write::new(Fd(fd), buffer, len)
            .offset(u64::MAX)
            .build();
        self.submit_raw_io_with_link_timeout(entry, true, true, timeout)
    }

    /// Submits an unstable internal shutdown operation without waiting.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn submit_raw_shutdown(&self, fd: &impl AsFd, how: Shutdown) -> RawIo {
        let fd = fd.as_fd().as_raw_fd();
        self.submit_raw_io(
            opcode::Shutdown::new(Fd(fd), how as i32).build(),
            false,
            true,
        )
    }

    /// Submits an unstable internal close operation without waiting.
    ///
    /// The returned raw close handle is detach-on-drop: once the [`OwnedFd`] has
    /// been consumed by the close SQE, dropping or canceling the handle does not
    /// issue async-cancel, because a canceled close would leave no Rust owner for
    /// the still-open descriptor.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn submit_raw_close(&self, fd: OwnedFd) -> RawIo {
        let fd = ManuallyDrop::new(fd);
        let fd = fd.as_fd().as_raw_fd();
        self.submit_raw_io(opcode::Close::new(Fd(fd)).build(), false, false)
    }

    fn submit_raw_io(
        &self,
        entry: impl Into<Sqe>,
        requires_payload_on_detach: bool,
        cancelable: bool,
    ) -> RawIo {
        let state = self.acquire_io_state();
        let user_data = self.submit_io_state(entry, Rc::clone(&state));
        RawIo::new(
            Rc::downgrade(&self.core),
            state,
            IoCancelKind::Async(user_data),
            requires_payload_on_detach,
            cancelable,
        )
    }

    fn submit_raw_io_with_link_timeout(
        &self,
        entry: impl Into<Sqe>,
        requires_payload_on_detach: bool,
        cancelable: bool,
        timeout: Duration,
    ) -> RawIo {
        let state = self.acquire_io_state();
        let user_data = self.submit_io_state_with_link_timeout(entry, Rc::clone(&state), timeout);
        RawIo::new(
            Rc::downgrade(&self.core),
            state,
            IoCancelKind::Async(user_data),
            requires_payload_on_detach,
            cancelable,
        )
    }

    /// Parks the current stackful coroutine until `duration` has elapsed.
    pub fn sleep(&self, duration: Duration) -> Result<(), Errno> {
        let timeout = self.submit_timeout(duration);
        let result = self
            .wait_for_submitted_io_state(timeout.state, IoCancelKind::Timeout(timeout.user_data));

        match result {
            Ok(_) | Err(Errno::TIME) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Registers `fd` in the runtime's fixed-file table.
    ///
    /// The runtime must be configured with at least one registered file slot.
    pub fn register_fd(&self, fd: OwnedFd) -> Result<RegisteredFd, Errno> {
        RegisteredFd::new(Rc::downgrade(&self.core), fd)
    }

    /// Registers an owned buffer in the runtime's fixed-buffer table.
    ///
    /// The runtime must be configured with at least one registered buffer slot.
    pub fn register_buffer<B>(&self, mut buffer: B) -> Result<RegisteredBuffer<B>, Errno>
    where
        B: IoReadBuffer + IoWriteBuffer + 'static,
    {
        let ptr = buffer.io_buffer_mut_ptr();
        let len = IoReadBuffer::io_buffer_len(&buffer);
        RegisteredBuffer::new(Rc::downgrade(&self.core), buffer, ptr, len)
    }

    /// Returns whether the current kernel reports support for an io_uring opcode.
    pub fn supports_io_uring_opcode(&self, opcode: IoringOp) -> bool {
        self.core.borrow().supports_opcode(opcode)
    }

    /// Opens `path` relative to the process current working directory using
    /// io_uring.
    pub fn open<P: path::Arg>(&self, path: P, flags: OFlags, mode: Mode) -> Result<OwnedFd, Errno> {
        self.openat(&fs::CWD, path, flags, mode)
    }

    /// Opens `path` relative to `dirfd` using io_uring.
    pub fn openat<P: path::Arg>(
        &self,
        dirfd: &impl AsFd,
        path: P,
        flags: OFlags,
        mode: Mode,
    ) -> Result<OwnedFd, Errno> {
        if !self.core.borrow().supports_opcode(opcode::OpenAt::CODE) {
            return fs::openat(dirfd.as_fd(), path, flags, mode);
        }

        path.into_with_c_str(|path| {
            let fd = self.submit_and_wait_for_io(
                opcode::OpenAt::new(Fd(dirfd.as_fd().as_raw_fd()), path.as_ptr())
                    .flags(flags)
                    .mode(mode)
                    .build(),
            )?;

            Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
        })
    }

    /// Opens `path` relative to the process current working directory using
    /// `openat2` through io_uring.
    pub fn open2<P: path::Arg>(
        &self,
        path: P,
        flags: OFlags,
        mode: Mode,
        resolve: ResolveFlags,
    ) -> Result<OwnedFd, Errno> {
        self.openat2(&fs::CWD, path, flags, mode, resolve)
    }

    /// Opens `path` relative to `dirfd` using `openat2` through io_uring.
    pub fn openat2<P: path::Arg>(
        &self,
        dirfd: &impl AsFd,
        path: P,
        flags: OFlags,
        mode: Mode,
        resolve: ResolveFlags,
    ) -> Result<OwnedFd, Errno> {
        if !self.core.borrow().supports_opcode(opcode::OpenAt2::CODE) {
            return fs::openat2(dirfd.as_fd(), path, flags, mode, resolve);
        }

        let how = OpenHow::new().flags(flags).mode(mode).resolve(resolve);
        path.into_with_c_str(|path| {
            let fd = self.submit_and_wait_for_io(
                opcode::OpenAt2::new(Fd(dirfd.as_fd().as_raw_fd()), path.as_ptr(), &how).build(),
            )?;

            Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
        })
    }

    /// Gets extended file status for `path` in the process current working
    /// directory using io_uring.
    pub fn statx<P: path::Arg>(
        &self,
        path: P,
        flags: AtFlags,
        mask: StatxFlags,
    ) -> Result<Statx, Errno> {
        self.statxat(&fs::CWD, path, flags, mask)
    }

    /// Gets extended file status relative to `dirfd` using io_uring.
    pub fn statxat<P: path::Arg>(
        &self,
        dirfd: &impl AsFd,
        path: P,
        flags: AtFlags,
        mask: StatxFlags,
    ) -> Result<Statx, Errno> {
        if !self.core.borrow().supports_opcode(opcode::Statx::CODE) {
            return fs::statx(dirfd.as_fd(), path, flags, mask);
        }

        path.into_with_c_str(|path| {
            let mut statx = MaybeUninit::<Statx>::zeroed();
            self.submit_and_wait_for_io(
                opcode::Statx::new(
                    Fd(dirfd.as_fd().as_raw_fd()),
                    path.as_ptr(),
                    statx.as_mut_ptr(),
                )
                .flags(flags)
                .mask(mask)
                .build(),
            )?;

            Ok(unsafe { statx.assume_init() })
        })
    }

    /// Creates a hard link using paths relative to the process current working
    /// directory.
    pub fn link<P: path::Arg, Q: path::Arg>(
        &self,
        oldpath: P,
        newpath: Q,
        flags: AtFlags,
    ) -> Result<(), Errno> {
        self.linkat(&fs::CWD, oldpath, &fs::CWD, newpath, flags)
    }

    /// Creates a hard link using io_uring.
    pub fn linkat<P: path::Arg, Q: path::Arg>(
        &self,
        olddirfd: &impl AsFd,
        oldpath: P,
        newdirfd: &impl AsFd,
        newpath: Q,
        flags: AtFlags,
    ) -> Result<(), Errno> {
        if !self.core.borrow().supports_opcode(opcode::LinkAt::CODE) {
            return fs::linkat(olddirfd.as_fd(), oldpath, newdirfd.as_fd(), newpath, flags);
        }

        oldpath.into_with_c_str(|oldpath| {
            newpath.into_with_c_str(|newpath| {
                self.submit_and_wait_for_io(
                    opcode::LinkAt::new(
                        Fd(olddirfd.as_fd().as_raw_fd()),
                        oldpath.as_ptr(),
                        Fd(newdirfd.as_fd().as_raw_fd()),
                        newpath.as_ptr(),
                    )
                    .flags(flags)
                    .build(),
                )
                .map(|_| ())
            })
        })
    }

    /// Creates a symbolic link in the process current working directory using
    /// io_uring.
    pub fn symlink<P: path::Arg, Q: path::Arg>(&self, target: P, linkpath: Q) -> Result<(), Errno> {
        self.symlinkat(target, &fs::CWD, linkpath)
    }

    /// Creates a symbolic link using io_uring.
    pub fn symlinkat<P: path::Arg, Q: path::Arg>(
        &self,
        target: P,
        newdirfd: &impl AsFd,
        linkpath: Q,
    ) -> Result<(), Errno> {
        if !self.core.borrow().supports_opcode(opcode::SymlinkAt::CODE) {
            return fs::symlinkat(target, newdirfd.as_fd(), linkpath);
        }

        target.into_with_c_str(|target| {
            linkpath.into_with_c_str(|linkpath| {
                self.submit_and_wait_for_io(
                    opcode::SymlinkAt::new(
                        Fd(newdirfd.as_fd().as_raw_fd()),
                        target.as_ptr(),
                        linkpath.as_ptr(),
                    )
                    .build(),
                )
                .map(|_| ())
            })
        })
    }

    /// Creates a directory relative to the process current working directory
    /// using io_uring.
    pub fn mkdir<P: path::Arg>(&self, path: P, mode: Mode) -> Result<(), Errno> {
        self.mkdirat(&fs::CWD, path, mode)
    }

    /// Creates a directory relative to `dirfd` using io_uring.
    pub fn mkdirat<P: path::Arg>(
        &self,
        dirfd: &impl AsFd,
        path: P,
        mode: Mode,
    ) -> Result<(), Errno> {
        if !self.core.borrow().supports_opcode(opcode::MkDirAt::CODE) {
            return fs::mkdirat(dirfd.as_fd(), path, mode);
        }

        path.into_with_c_str(|path| {
            self.submit_and_wait_for_io(
                opcode::MkDirAt::new(Fd(dirfd.as_fd().as_raw_fd()), path.as_ptr())
                    .mode(mode)
                    .build(),
            )
            .map(|_| ())
        })
    }

    /// Removes a directory using io_uring.
    pub fn rmdir<P: path::Arg>(&self, path: P) -> Result<(), Errno> {
        self.unlinkat(&fs::CWD, path, AtFlags::REMOVEDIR)
    }

    /// Removes a filesystem entry using io_uring.
    pub fn unlink<P: path::Arg>(&self, path: P) -> Result<(), Errno> {
        self.unlinkat(&fs::CWD, path, AtFlags::empty())
    }

    /// Removes a filesystem entry relative to `dirfd` using io_uring.
    pub fn unlinkat<P: path::Arg>(
        &self,
        dirfd: &impl AsFd,
        path: P,
        flags: AtFlags,
    ) -> Result<(), Errno> {
        if !self.core.borrow().supports_opcode(opcode::UnlinkAt::CODE) {
            return fs::unlinkat(dirfd.as_fd(), path, flags);
        }

        path.into_with_c_str(|path| {
            self.submit_and_wait_for_io(
                opcode::UnlinkAt::new(Fd(dirfd.as_fd().as_raw_fd()), path.as_ptr())
                    .flags(flags)
                    .build(),
            )
            .map(|_| ())
        })
    }

    /// Renames a filesystem entry using io_uring.
    pub fn rename<P: path::Arg, Q: path::Arg>(
        &self,
        oldpath: P,
        newpath: Q,
        flags: RenameFlags,
    ) -> Result<(), Errno> {
        self.renameat(&fs::CWD, oldpath, &fs::CWD, newpath, flags)
    }

    /// Renames a filesystem entry relative to directory fds using io_uring.
    pub fn renameat<P: path::Arg, Q: path::Arg>(
        &self,
        olddirfd: &impl AsFd,
        oldpath: P,
        newdirfd: &impl AsFd,
        newpath: Q,
        flags: RenameFlags,
    ) -> Result<(), Errno> {
        if !self.core.borrow().supports_opcode(opcode::RenameAt::CODE) {
            return fs::renameat_with(olddirfd.as_fd(), oldpath, newdirfd.as_fd(), newpath, flags);
        }

        oldpath.into_with_c_str(|oldpath| {
            newpath.into_with_c_str(|newpath| {
                self.submit_and_wait_for_io(
                    opcode::RenameAt::new(
                        Fd(olddirfd.as_fd().as_raw_fd()),
                        oldpath.as_ptr(),
                        Fd(newdirfd.as_fd().as_raw_fd()),
                        newpath.as_ptr(),
                    )
                    .flags(flags)
                    .build(),
                )
                .map(|_| ())
            })
        })
    }

    /// Declares an expected file access pattern using io_uring.
    pub fn fadvise(
        &self,
        fd: &impl AsFd,
        offset: u64,
        len: u32,
        advice: FileAdvice,
    ) -> Result<(), Errno> {
        if !self.core.borrow().supports_opcode(opcode::Fadvise::CODE) {
            return fs::fadvise(
                fd.as_fd(),
                offset,
                std::num::NonZeroU64::new(u64::from(len)),
                advice,
            );
        }

        let fd = fd.as_fd().as_raw_fd();
        self.submit_and_wait_for_io(
            opcode::Fadvise::new(Fd(fd), len, advice)
                .offset(offset)
                .build(),
        )
        .map(|_| ())
    }

    /// Gives advice about a memory range using io_uring.
    ///
    /// # Safety
    ///
    /// `addr..addr + len` must satisfy the safety requirements of
    /// `madvise(2)` for `advice` and remain valid until this call returns.
    pub unsafe fn madvise(
        &self,
        addr: *mut c_void,
        len: usize,
        advice: MemoryAdvice,
    ) -> Result<(), Errno> {
        let Some(uring_advice) = memory_advice_for_uring(advice) else {
            return unsafe { mm::madvise(addr, len, advice) };
        };

        if !self.core.borrow().supports_opcode(opcode::Madvise::CODE) {
            return unsafe { mm::madvise(addr, len, advice) };
        }

        let len = u32::try_from(len).expect("io_uring madvise length exceeds u32::MAX");
        self.submit_and_wait_for_io(
            opcode::Madvise::new(addr.cast_const(), len, uring_advice).build(),
        )
        .map(|_| ())
    }

    /// Preallocates or deallocates file space using io_uring.
    pub fn fallocate(
        &self,
        fd: &impl AsFd,
        mode: FallocateFlags,
        offset: u64,
        len: u64,
    ) -> Result<(), Errno> {
        if !self.core.borrow().supports_opcode(opcode::Fallocate::CODE) {
            return fs::fallocate(fd.as_fd(), mode, offset, len);
        }

        let fd = fd.as_fd().as_raw_fd();
        self.submit_and_wait_for_io(
            opcode::Fallocate::new(Fd(fd), len)
                .offset(offset)
                .mode(mode.bits() as i32)
                .build(),
        )
        .map(|_| ())
    }

    /// Sets the length of a file using io_uring.
    pub fn ftruncate(&self, fd: &impl AsFd, len: u64) -> Result<(), Errno> {
        if !self.core.borrow().supports_opcode(opcode::Ftruncate::CODE) {
            return fs::ftruncate(fd.as_fd(), len);
        }

        let fd = fd.as_fd().as_raw_fd();
        self.submit_and_wait_for_io(opcode::Ftruncate::new(Fd(fd), len).build())
            .map(|_| ())
    }

    /// Synchronizes file data and metadata using io_uring.
    pub fn fsync(&self, fd: &impl AsFd) -> Result<(), Errno> {
        if !self.core.borrow().supports_opcode(opcode::Fsync::CODE) {
            return fs::fsync(fd.as_fd());
        }

        let fd = fd.as_fd().as_raw_fd();
        self.submit_and_wait_for_io(opcode::Fsync::new(Fd(fd)).build())
            .map(|_| ())
    }

    /// Synchronizes a file range using io_uring.
    pub fn sync_file_range(
        &self,
        fd: &impl AsFd,
        offset: u64,
        len: u32,
        flags: u32,
    ) -> Result<(), Errno> {
        let fd = fd.as_fd().as_raw_fd();
        self.submit_and_wait_for_io(
            opcode::SyncFileRange::new(Fd(fd), len)
                .offset(offset)
                .flags(flags)
                .build(),
        )
        .map(|_| ())
    }

    /// Gets extended status for an open file descriptor using `statx(2)` with
    /// `AT_EMPTY_PATH`.
    pub fn fstat(&self, fd: &impl AsFd, mask: StatxFlags) -> Result<Statx, Errno> {
        if !self.core.borrow().supports_opcode(opcode::Statx::CODE) {
            return fs::statx(fd.as_fd(), c"", AtFlags::EMPTY_PATH, mask);
        }

        let mut statx = MaybeUninit::<Statx>::zeroed();
        self.submit_and_wait_for_io(
            opcode::Statx::new(Fd(fd.as_fd().as_raw_fd()), c"".as_ptr(), statx.as_mut_ptr())
                .flags(AtFlags::EMPTY_PATH)
                .mask(mask)
                .build(),
        )?;

        Ok(unsafe { statx.assume_init() })
    }

    /// Splices data between file descriptors using io_uring.
    pub fn splice(
        &self,
        fd_in: &impl AsFd,
        off_in: i64,
        fd_out: &impl AsFd,
        off_out: i64,
        len: u32,
        flags: UringSpliceFlags,
    ) -> Result<usize, Errno> {
        if !self.core.borrow().supports_opcode(opcode::Splice::CODE) {
            return Err(Errno::OPNOTSUPP);
        }

        let entry = opcode::Splice::new(
            Fd(fd_in.as_fd().as_raw_fd()),
            off_in,
            Fd(fd_out.as_fd().as_raw_fd()),
            off_out,
            len,
        )
        .flags(flags)
        .build();

        self.submit_and_wait_for_io(entry)
            .map(|result| result as usize)
    }

    /// Duplicates pipe data between file descriptors using io_uring.
    pub fn tee(
        &self,
        fd_in: &impl AsFd,
        fd_out: &impl AsFd,
        len: u32,
        flags: UringSpliceFlags,
    ) -> Result<usize, Errno> {
        if !self.core.borrow().supports_opcode(opcode::Tee::CODE) {
            return Err(Errno::OPNOTSUPP);
        }

        let entry = opcode::Tee::new(
            Fd(fd_in.as_fd().as_raw_fd()),
            Fd(fd_out.as_fd().as_raw_fd()),
            len,
        )
        .flags(flags)
        .build();

        self.submit_and_wait_for_io(entry)
            .map(|result| result as usize)
    }

    /// Waits for one poll event set on `fd` using io_uring.
    pub fn poll(&self, fd: &impl AsFd, flags: u32) -> Result<u32, Errno> {
        if !self.core.borrow().supports_opcode(opcode::PollAdd::CODE) {
            return Err(Errno::OPNOTSUPP);
        }

        let fd = fd.as_fd().as_raw_fd();
        self.submit_and_wait_for_io(opcode::PollAdd::new(Fd(fd), flags).build())
    }

    /// Removes a previously submitted poll request by user data.
    pub fn poll_remove(&self, user_data: io_uring_user_data) -> Result<(), Errno> {
        if !self.core.borrow().supports_opcode(opcode::PollRemove::CODE) {
            return Err(Errno::OPNOTSUPP);
        }

        self.submit_and_wait_for_io(opcode::PollRemove::new(user_data).build())
            .map(|_| ())
    }

    /// Modifies an epoll instance using io_uring.
    pub fn epoll_ctl(
        &self,
        epfd: &impl AsFd,
        fd: &impl AsFd,
        op: EpollCtlOp,
        event: &EpollEvent,
    ) -> Result<(), Errno> {
        if !self.core.borrow().supports_opcode(opcode::EpollCtl::CODE) {
            return Err(Errno::OPNOTSUPP);
        }

        self.submit_and_wait_for_io(
            opcode::EpollCtl::new(
                Fd(epfd.as_fd().as_raw_fd()),
                Fd(fd.as_fd().as_raw_fd()),
                op as i32,
                event,
            )
            .build(),
        )
        .map(|_| ())
    }

    /// Creates a socket.
    ///
    /// Uses io_uring when the kernel reports support for `IORING_OP_SOCKET`,
    /// otherwise falls back to `socket(2)` synchronously.
    pub fn socket(
        &self,
        domain: AddressFamily,
        socket_type: SocketType,
        protocol: Option<Protocol>,
    ) -> Result<OwnedFd, Errno> {
        if !self.core.borrow().supports_opcode(opcode::Socket::CODE) {
            return net::socket(domain, socket_type, protocol);
        }

        let domain = i32::from(domain.as_raw());
        let socket_type = socket_type.as_raw() as i32;
        let protocol = protocol
            .map(|protocol| u32::from(protocol.as_raw()) as i32)
            .unwrap_or(0);
        let fd = self
            .submit_and_wait_for_io(opcode::Socket::new(domain, socket_type, protocol).build())?;

        Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
    }

    /// Binds a socket to `address`.
    ///
    /// `bind(2)` is not currently an io_uring operation, so this is a synchronous
    /// setup helper.
    pub fn bind(&self, fd: &impl AsFd, address: &impl SocketAddrArg) -> Result<(), Errno> {
        net::bind(fd, address)
    }

    /// Marks a socket as accepting incoming connections.
    ///
    /// `listen(2)` is not currently an io_uring operation, so this is a
    /// synchronous setup helper.
    pub fn listen(&self, fd: &impl AsFd, backlog: i32) -> Result<(), Errno> {
        net::listen(fd, backlog)
    }

    /// Accepts one connection from a listening socket using io_uring.
    pub fn accept(&self, fd: &impl AsFd) -> Result<OwnedFd, Errno> {
        let fd = fd.as_fd().as_raw_fd();
        let accepted = self.submit_and_wait_for_io(
            opcode::Accept::new(Fd(fd), std::ptr::null_mut(), std::ptr::null_mut()).build(),
        )?;

        Ok(unsafe { OwnedFd::from_raw_fd(accepted as i32) })
    }

    /// Connects a socket to `address` using io_uring.
    pub fn connect(&self, fd: &impl AsFd, address: &impl SocketAddrArg) -> Result<(), Errno> {
        let fd = fd.as_fd().as_raw_fd();
        unsafe {
            address.with_sockaddr(|address, address_len| {
                self.submit_and_wait_for_io(
                    opcode::Connect::new(Fd(fd), address, address_len).build(),
                )
                .map(|_| ())
            })
        }
    }

    /// Reads from `fd` into `buf` using io_uring.
    ///
    /// This blocks only the current stackful coroutine. Pass a borrowed fd to
    /// keep ownership of it; use [`RuntimeContext::close`] to close an
    /// [`OwnedFd`] through io_uring.
    pub fn read(&self, fd: &impl AsFd, buf: &mut [u8]) -> Result<usize, Errno> {
        let len = u32::try_from(buf.len()).expect("io_uring read length exceeds u32::MAX");
        let fd = fd.as_fd().as_raw_fd();
        let entry = opcode::Read::new(Fd(fd), buf.as_mut_ptr(), len)
            .offset(u64::MAX)
            .build();

        self.submit_and_wait_for_io(entry)
            .map(|result| result as usize)
    }

    /// Reads from `fd` into `buf` at an explicit file offset using io_uring.
    ///
    /// This does not update the descriptor's current file offset.
    pub fn pread(&self, fd: &impl AsFd, buf: &mut [u8], offset: u64) -> Result<usize, Errno> {
        let len = u32::try_from(buf.len()).expect("io_uring read length exceeds u32::MAX");
        let fd = fd.as_fd().as_raw_fd();
        let entry = opcode::Read::new(Fd(fd), buf.as_mut_ptr(), len)
            .offset(offset)
            .build();

        self.submit_and_wait_for_io(entry)
            .map(|result| result as usize)
    }

    /// Vectored read from `fd` into `iovecs` using io_uring.
    pub fn readv(&self, fd: &impl AsFd, iovecs: &mut [IoVec]) -> Result<usize, Errno> {
        let len = u32::try_from(iovecs.len()).expect("io_uring readv iovec count exceeds u32::MAX");
        let fd = fd.as_fd().as_raw_fd();
        let entry = opcode::Readv::new(Fd(fd), iovecs.as_ptr(), len)
            .offset(u64::MAX)
            .build();

        self.submit_and_wait_for_io(entry)
            .map(|result| result as usize)
    }

    /// Reads from a registered fixed fd into `buf`.
    pub fn read_registered_fd(&self, fd: &RegisteredFd, buf: &mut [u8]) -> Result<usize, Errno> {
        let len = u32::try_from(buf.len()).expect("io_uring read length exceeds u32::MAX");
        let state = self.acquire_io_state();
        let user_data = self.submit_io_state(
            opcode::Read::new(Fixed(fd.slot()), buf.as_mut_ptr(), len)
                .offset(u64::MAX)
                .build(),
            Rc::clone(&state),
        );
        let result = self.wait_for_submitted_io_state(state, IoCancelKind::Async(user_data));
        result.map(|result| result as usize)
    }
    /// Reads from `fd` into a registered fixed buffer.
    pub fn read_registered_buffer<B>(
        &self,
        fd: &impl AsFd,
        buffer: &mut RegisteredBuffer<B>,
    ) -> Result<usize, Errno> {
        let len = u32::try_from(buffer.len()).expect("io_uring read length exceeds u32::MAX");
        let fd = fd.as_fd().as_raw_fd();
        let state = self.acquire_io_state();
        let user_data = self.submit_io_state(
            opcode::ReadFixed::new(Fd(fd), buffer.mut_ptr(), len, buffer.slot())
                .offset(u64::MAX)
                .build(),
            Rc::clone(&state),
        );
        let result = self.wait_for_submitted_io_state(state, IoCancelKind::Async(user_data));
        result.map(|result| result as usize)
    }

    /// Reads from a registered fixed fd into a registered fixed buffer.
    pub fn read_registered_fixed<B>(
        &self,
        fd: &RegisteredFd,
        buffer: &mut RegisteredBuffer<B>,
    ) -> Result<usize, Errno> {
        let len = u32::try_from(buffer.len()).expect("io_uring read length exceeds u32::MAX");
        let state = self.acquire_io_state();
        let user_data = self.submit_io_state(
            opcode::ReadFixed::new(Fixed(fd.slot()), buffer.mut_ptr(), len, buffer.slot())
                .offset(u64::MAX)
                .build(),
            Rc::clone(&state),
        );
        let result = self.wait_for_submitted_io_state(state, IoCancelKind::Async(user_data));
        result.map(|result| result as usize)
    }

    /// Starts a read into an owned buffer using io_uring.
    ///
    /// The returned [`IoResult`] owns `buffer` until completion, so it is safe to
    /// drop the handle while the kernel operation is still pending.
    pub fn read_async<B>(&self, fd: &IoFd, mut buffer: B) -> IoResult<ReadOutput<B>, B>
    where
        B: IoReadBuffer + 'static,
    {
        let len =
            u32::try_from(buffer.io_buffer_len()).expect("io_uring read length exceeds u32::MAX");
        let raw_fd = fd.raw_fd();
        let entry = opcode::Read::new(Fd(raw_fd), buffer.io_buffer_mut_ptr(), len)
            .offset(u64::MAX)
            .build();
        let state = self.acquire_io_state();
        state.attach_io_fd(fd.clone());

        self.submit_io_state(entry, Rc::clone(&state));
        IoResult::new(Rc::downgrade(&self.core), state, buffer, read_output::<B>)
    }

    /// Starts a read from a registered fixed fd into an owned buffer.
    pub fn read_registered_fd_async<B>(
        &self,
        fd: &RegisteredFd,
        mut buffer: B,
    ) -> IoResult<ReadOutput<B>, B>
    where
        B: IoReadBuffer + 'static,
    {
        let len =
            u32::try_from(buffer.io_buffer_len()).expect("io_uring read length exceeds u32::MAX");
        let state = self.acquire_io_state();
        state.attach_registered_resource(fd.resource());

        self.submit_io_state(
            opcode::Read::new(Fixed(fd.slot()), buffer.io_buffer_mut_ptr(), len)
                .offset(u64::MAX)
                .build(),
            Rc::clone(&state),
        );
        IoResult::new(Rc::downgrade(&self.core), state, buffer, read_output::<B>)
    }

    /// Starts a read from `fd` into a registered fixed buffer.
    pub fn read_registered_buffer_async<B: 'static>(
        &self,
        fd: &IoFd,
        buffer: RegisteredBuffer<B>,
    ) -> IoResult<ReadOutput<RegisteredBuffer<B>>, RegisteredBuffer<B>> {
        let len = u32::try_from(buffer.len()).expect("io_uring read length exceeds u32::MAX");
        let raw_fd = fd.raw_fd();
        let state = self.acquire_io_state();
        state.attach_io_fd(fd.clone());
        state.attach_registered_resource(buffer.resource());

        self.submit_io_state(
            opcode::ReadFixed::new(Fd(raw_fd), buffer.mut_ptr(), len, buffer.slot())
                .offset(u64::MAX)
                .build(),
            Rc::clone(&state),
        );
        IoResult::new(
            Rc::downgrade(&self.core),
            state,
            buffer,
            read_output::<RegisteredBuffer<B>>,
        )
    }

    /// Starts a read from a registered fixed fd into a registered fixed buffer.
    pub fn read_registered_fixed_async<B: 'static>(
        &self,
        fd: &RegisteredFd,
        buffer: RegisteredBuffer<B>,
    ) -> IoResult<ReadOutput<RegisteredBuffer<B>>, RegisteredBuffer<B>> {
        let len = u32::try_from(buffer.len()).expect("io_uring read length exceeds u32::MAX");
        let state = self.acquire_io_state();
        state.attach_registered_resource(fd.resource());
        state.attach_registered_resource(buffer.resource());

        self.submit_io_state(
            opcode::ReadFixed::new(Fixed(fd.slot()), buffer.mut_ptr(), len, buffer.slot())
                .offset(u64::MAX)
                .build(),
            Rc::clone(&state),
        );
        IoResult::new(
            Rc::downgrade(&self.core),
            state,
            buffer,
            read_output::<RegisteredBuffer<B>>,
        )
    }

    /// Writes `buf` to `fd` using io_uring.
    ///
    /// This blocks only the current stackful coroutine. Pass a borrowed fd to
    /// keep ownership of it; use [`RuntimeContext::close`] to close an
    /// [`OwnedFd`] through io_uring.
    pub fn write(&self, fd: &impl AsFd, buf: &[u8]) -> Result<usize, Errno> {
        let len = u32::try_from(buf.len()).expect("io_uring write length exceeds u32::MAX");
        let fd = fd.as_fd().as_raw_fd();
        let entry = opcode::Write::new(Fd(fd), buf.as_ptr(), len)
            .offset(u64::MAX)
            .build();

        self.submit_and_wait_for_io(entry)
            .map(|result| result as usize)
    }

    /// Writes `buf` to `fd` at an explicit file offset using io_uring.
    ///
    /// This does not update the descriptor's current file offset.
    pub fn pwrite(&self, fd: &impl AsFd, buf: &[u8], offset: u64) -> Result<usize, Errno> {
        let len = u32::try_from(buf.len()).expect("io_uring write length exceeds u32::MAX");
        let fd = fd.as_fd().as_raw_fd();
        let entry = opcode::Write::new(Fd(fd), buf.as_ptr(), len)
            .offset(offset)
            .build();

        self.submit_and_wait_for_io(entry)
            .map(|result| result as usize)
    }

    /// Vectored write from `iovecs` to `fd` using io_uring.
    pub fn writev(&self, fd: &impl AsFd, iovecs: &[IoVec]) -> Result<usize, Errno> {
        let len =
            u32::try_from(iovecs.len()).expect("io_uring writev iovec count exceeds u32::MAX");
        let fd = fd.as_fd().as_raw_fd();
        let entry = opcode::Writev::new(Fd(fd), iovecs.as_ptr(), len)
            .offset(u64::MAX)
            .build();

        self.submit_and_wait_for_io(entry)
            .map(|result| result as usize)
    }

    /// Writes `buf` to a registered fixed fd.
    pub fn write_registered_fd(&self, fd: &RegisteredFd, buf: &[u8]) -> Result<usize, Errno> {
        let len = u32::try_from(buf.len()).expect("io_uring write length exceeds u32::MAX");
        let state = self.acquire_io_state();
        let user_data = self.submit_io_state(
            opcode::Write::new(Fixed(fd.slot()), buf.as_ptr(), len)
                .offset(u64::MAX)
                .build(),
            Rc::clone(&state),
        );
        let result = self.wait_for_submitted_io_state(state, IoCancelKind::Async(user_data));
        result.map(|result| result as usize)
    }
    /// Writes from a registered fixed buffer to `fd`.
    pub fn write_registered_buffer<B>(
        &self,
        fd: &impl AsFd,
        buffer: &RegisteredBuffer<B>,
    ) -> Result<usize, Errno> {
        let len = u32::try_from(buffer.len()).expect("io_uring write length exceeds u32::MAX");
        let fd = fd.as_fd().as_raw_fd();
        let state = self.acquire_io_state();
        let user_data = self.submit_io_state(
            opcode::WriteFixed::new(Fd(fd), buffer.ptr(), len, buffer.slot())
                .offset(u64::MAX)
                .build(),
            Rc::clone(&state),
        );
        let result = self.wait_for_submitted_io_state(state, IoCancelKind::Async(user_data));
        result.map(|result| result as usize)
    }

    /// Writes from a registered fixed buffer to a registered fixed fd.
    pub fn write_registered_fixed<B>(
        &self,
        fd: &RegisteredFd,
        buffer: &RegisteredBuffer<B>,
    ) -> Result<usize, Errno> {
        let len = u32::try_from(buffer.len()).expect("io_uring write length exceeds u32::MAX");
        let state = self.acquire_io_state();
        let user_data = self.submit_io_state(
            opcode::WriteFixed::new(Fixed(fd.slot()), buffer.ptr(), len, buffer.slot())
                .offset(u64::MAX)
                .build(),
            Rc::clone(&state),
        );
        let result = self.wait_for_submitted_io_state(state, IoCancelKind::Async(user_data));
        result.map(|result| result as usize)
    }

    /// Sends bytes on a connected socket using io_uring.
    pub fn send(&self, fd: &impl AsFd, buf: &[u8]) -> Result<usize, Errno> {
        let len = u32::try_from(buf.len()).expect("io_uring send length exceeds u32::MAX");
        let fd = fd.as_fd().as_raw_fd();
        let entry = opcode::Send::new(Fd(fd), buf.as_ptr(), len).build();

        self.submit_and_wait_for_io(entry)
            .map(|result| result as usize)
    }

    /// Sends a message on a socket using io_uring.
    pub fn sendmsg(&self, fd: &impl AsFd, msg: &MsgHdr, flags: SendFlags) -> Result<usize, Errno> {
        let fd = fd.as_fd().as_raw_fd();
        let entry = opcode::SendMsg::new(Fd(fd), msg).flags(flags).build();

        self.submit_and_wait_for_io(entry)
            .map(|result| result as usize)
    }

    /// Receives bytes from a connected socket using io_uring.
    pub fn recv(&self, fd: &impl AsFd, buf: &mut [u8]) -> Result<usize, Errno> {
        let len = u32::try_from(buf.len()).expect("io_uring recv length exceeds u32::MAX");
        let fd = fd.as_fd().as_raw_fd();
        let entry = opcode::Recv::new(Fd(fd), buf.as_mut_ptr(), len).build();

        self.submit_and_wait_for_io(entry)
            .map(|result| result as usize)
    }

    /// Receives a message from a socket using io_uring.
    pub fn recvmsg(
        &self,
        fd: &impl AsFd,
        msg: &mut MsgHdr,
        flags: RecvFlags,
    ) -> Result<usize, Errno> {
        let fd = fd.as_fd().as_raw_fd();
        let entry = opcode::RecvMsg::new(Fd(fd), msg).flags(flags).build();

        self.submit_and_wait_for_io(entry)
            .map(|result| result as usize)
    }

    /// Starts a write from an owned buffer using io_uring.
    ///
    /// The returned [`IoResult`] owns `buffer` until completion, so it is safe to
    /// drop the handle while the kernel operation is still pending.
    pub fn write_async<B>(&self, fd: &IoFd, buffer: B) -> IoResult<WriteOutput<B>, B>
    where
        B: IoWriteBuffer + 'static,
    {
        let len =
            u32::try_from(buffer.io_buffer_len()).expect("io_uring write length exceeds u32::MAX");
        let raw_fd = fd.raw_fd();
        let entry = opcode::Write::new(Fd(raw_fd), buffer.io_buffer_ptr(), len)
            .offset(u64::MAX)
            .build();
        let state = self.acquire_io_state();
        state.attach_io_fd(fd.clone());

        self.submit_io_state(entry, Rc::clone(&state));
        IoResult::new(Rc::downgrade(&self.core), state, buffer, write_output::<B>)
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
        let len =
            u32::try_from(buffer.io_buffer_len()).expect("io_uring write length exceeds u32::MAX");
        let state = self.acquire_io_state();
        state.attach_registered_resource(fd.resource());

        self.submit_io_state(
            opcode::Write::new(Fixed(fd.slot()), buffer.io_buffer_ptr(), len)
                .offset(u64::MAX)
                .build(),
            Rc::clone(&state),
        );
        IoResult::new(Rc::downgrade(&self.core), state, buffer, write_output::<B>)
    }

    /// Starts a write from a registered fixed buffer to `fd`.
    pub fn write_registered_buffer_async<B: 'static>(
        &self,
        fd: &IoFd,
        buffer: RegisteredBuffer<B>,
    ) -> IoResult<WriteOutput<RegisteredBuffer<B>>, RegisteredBuffer<B>> {
        let len = u32::try_from(buffer.len()).expect("io_uring write length exceeds u32::MAX");
        let raw_fd = fd.raw_fd();
        let state = self.acquire_io_state();
        state.attach_io_fd(fd.clone());
        state.attach_registered_resource(buffer.resource());

        self.submit_io_state(
            opcode::WriteFixed::new(Fd(raw_fd), buffer.ptr(), len, buffer.slot())
                .offset(u64::MAX)
                .build(),
            Rc::clone(&state),
        );
        IoResult::new(
            Rc::downgrade(&self.core),
            state,
            buffer,
            write_output::<RegisteredBuffer<B>>,
        )
    }

    /// Starts a write from a registered fixed buffer to a registered fixed fd.
    pub fn write_registered_fixed_async<B: 'static>(
        &self,
        fd: &RegisteredFd,
        buffer: RegisteredBuffer<B>,
    ) -> IoResult<WriteOutput<RegisteredBuffer<B>>, RegisteredBuffer<B>> {
        let len = u32::try_from(buffer.len()).expect("io_uring write length exceeds u32::MAX");
        let state = self.acquire_io_state();
        state.attach_registered_resource(fd.resource());
        state.attach_registered_resource(buffer.resource());

        self.submit_io_state(
            opcode::WriteFixed::new(Fixed(fd.slot()), buffer.ptr(), len, buffer.slot())
                .offset(u64::MAX)
                .build(),
            Rc::clone(&state),
        );
        IoResult::new(
            Rc::downgrade(&self.core),
            state,
            buffer,
            write_output::<RegisteredBuffer<B>>,
        )
    }

    /// Closes `fd` using io_uring.
    ///
    /// The fd is consumed immediately and will not be closed again on drop.
    pub fn close(&self, fd: OwnedFd) -> Result<(), Errno> {
        let fd = ManuallyDrop::new(fd);
        let fd = fd.as_fd().as_raw_fd();
        let entry = opcode::Close::new(Fd(fd)).build();

        self.submit_and_wait_for_io(entry).map(|_| ())
    }

    /// Shuts down part or all of a connected socket using io_uring.
    pub fn shutdown(&self, fd: &impl AsFd, how: Shutdown) -> Result<(), Errno> {
        let fd = fd.as_fd().as_raw_fd();
        let entry = opcode::Shutdown::new(Fd(fd), how as i32).build();

        self.submit_and_wait_for_io(entry).map(|_| ())
    }

    /// Starts an io_uring timeout and returns a waitable handle.
    pub fn timeout(&self, duration: Duration) -> Timeout {
        Timeout::new(Rc::downgrade(&self.core), self.submit_timeout(duration))
    }

    /// Returns a snapshot of runtime I/O lifecycle counters.
    pub fn io_counters(&self) -> IoCounters {
        self.core.borrow().io_counters()
    }

    /// Returns a read-only snapshot of runtime-owned scheduler pool state.
    pub fn pool_diagnostics(&self) -> PoolDiagnostics {
        self.core.borrow().pool_diagnostics()
    }

    /// Attempts to cancel a pending [`IoResult`] without consuming its handle.
    ///
    /// Unlike [`IoResult::cancel`], this waits only for the cancel request and
    /// leaves the result handle responsible for eventual completion, detachment,
    /// or drop-based reaping.
    pub fn cancel_io<T, B>(&self, io: &IoResult<T, B>) -> Result<(), Errno> {
        let Some(state) = io.state.as_ref() else {
            return Ok(());
        };
        let Some(user_data) = io.user_data() else {
            return Ok(());
        };

        if !self
            .core
            .borrow()
            .supports_opcode(opcode::AsyncCancel::CODE)
        {
            return Err(Errno::OPNOTSUPP);
        }

        let Some(cancel_state) =
            self.submit_cancel_for_io_state(state, IoCancelKind::Async(user_data))
        else {
            return Ok(());
        };
        let result = self.wait_for_io_state_without_scope_panic(&cancel_state);
        self.recycle_io_state(cancel_state);
        result.map(|_| ())
    }

    /// Attempts to cancel a pending internal [`RawIo`] without consuming its handle.
    #[doc(hidden)]
    pub fn cancel_raw_io(&self, io: &RawIo) -> Result<(), Errno> {
        if !io.cancelable {
            return Ok(());
        }

        let Some(state) = io.state.as_ref() else {
            return Ok(());
        };

        if !self
            .core
            .borrow()
            .supports_opcode(opcode::AsyncCancel::CODE)
        {
            return Err(Errno::OPNOTSUPP);
        }

        let Some(cancel_state) = self.submit_cancel_for_io_state(state, io.cancel_kind) else {
            return Ok(());
        };
        let result = self.wait_for_io_state_without_scope_panic(&cancel_state);
        self.recycle_io_state(cancel_state);
        result.map(|_| ())
    }

    /// Attempts to cancel requests matched by `builder`.
    pub fn cancel_matching(&self, builder: CancelBuilder) -> Result<(), Errno> {
        if !self
            .core
            .borrow()
            .supports_opcode(opcode::AsyncCancel2::CODE)
        {
            return Err(Errno::OPNOTSUPP);
        }

        self.submit_and_wait_for_io(opcode::AsyncCancel2::new(builder).build())
            .map(|_| ())
    }

    /// Waits on one futex using io_uring.
    pub fn futex_wait(
        &self,
        futex: &AtomicU32,
        val: u64,
        mask: u64,
        futex_flags: FutexWaitFlags,
    ) -> Result<(), Errno> {
        if !self.core.borrow().supports_opcode(opcode::FutexWait::CODE) {
            return Err(Errno::OPNOTSUPP);
        }

        self.submit_and_wait_for_io(
            opcode::FutexWait::new(futex.as_ptr().cast_const(), val, mask, futex_flags).build(),
        )
        .map(|_| ())
    }

    /// Wakes waiters on one futex using io_uring.
    pub fn futex_wake(
        &self,
        futex: &AtomicU32,
        count: u64,
        mask: u64,
        futex_flags: FutexWaitFlags,
    ) -> Result<usize, Errno> {
        if !self.core.borrow().supports_opcode(opcode::FutexWake::CODE) {
            return Err(Errno::OPNOTSUPP);
        }

        self.submit_and_wait_for_io(
            opcode::FutexWake::new(futex.as_ptr().cast_const(), count, mask, futex_flags).build(),
        )
        .map(|result| result as usize)
    }

    /// Waits on any futex in `futexes` using io_uring.
    pub fn futex_waitv(&self, futexes: &[FutexWaitV]) -> Result<usize, Errno> {
        if !self.core.borrow().supports_opcode(opcode::FutexWaitV::CODE) {
            return Err(Errno::OPNOTSUPP);
        }

        let len =
            u32::try_from(futexes.len()).expect("io_uring futex_waitv count exceeds u32::MAX");
        self.submit_and_wait_for_io(opcode::FutexWaitV::new(futexes.as_ptr(), len).build())
            .map(|result| result as usize)
    }

    /// Issues a device-specific 80-byte io_uring command.
    ///
    /// # Safety
    ///
    /// The caller must ensure the command bytes, command opcode, optional fixed
    /// buffer index, and target fd satisfy the target device driver's
    /// requirements.
    pub unsafe fn uring_cmd80(
        &self,
        fd: &impl AsFd,
        cmd_op: u32,
        cmd: [u8; 80],
        buf_index: Option<u16>,
    ) -> Result<u32, Errno> {
        if !self.core.borrow().supports_opcode(opcode::UringCmd80::CODE) {
            return Err(Errno::OPNOTSUPP);
        }

        self.submit_and_wait_for_io(
            opcode::UringCmd80::new(Fd(fd.as_fd().as_raw_fd()), cmd_op)
                .cmd(cmd)
                .buf_index(buf_index)
                .build(),
        )
    }

    /// Issues a device-specific 80-byte io_uring command to a registered fd.
    ///
    /// # Safety
    ///
    /// The caller must ensure the command bytes, command opcode, optional fixed
    /// buffer index, and target fixed fd satisfy the target device driver's
    /// requirements.
    pub unsafe fn uring_cmd80_registered_fd(
        &self,
        fd: &RegisteredFd,
        cmd_op: u32,
        cmd: [u8; 80],
        buf_index: Option<u16>,
    ) -> Result<u32, Errno> {
        if !self.core.borrow().supports_opcode(opcode::UringCmd80::CODE) {
            return Err(Errno::OPNOTSUPP);
        }

        self.submit_and_wait_for_io(
            opcode::UringCmd80::new(Fixed(fd.slot()), cmd_op)
                .cmd(cmd)
                .buf_index(buf_index)
                .build(),
        )
    }

    fn wait_for_scope(&self, state: &Rc<ScopeState>) {
        let mut cancel_requested = false;
        while state.has_remaining_tasks() {
            self.prepare_scope_quiescence();
            let task_ids = state.task_ids();
            if self.core.borrow().has_ready_task(&task_ids) {
                self.yield_now();
                continue;
            }

            if !cancel_requested && !task_ids.is_empty() {
                self.cancel_scope_tasks(task_ids);
                cancel_requested = true;
                continue;
            }

            let registration = self.wait_registration();
            if let Some(waiter) = self.waiter(&registration) {
                state.push_waiter(waiter);
            }
            self.park();
        }
    }

    fn cancel_scope_tasks(&self, task_ids: Vec<TaskKey>) {
        interrupt_scheduler_tasks(&self.core, task_ids, |_| {
            Box::new(|_| panic::panic_any(TaskCanceled))
        });
    }

    fn prepare_scope_quiescence(&self) {
        {
            let mut scheduler = self.core.borrow_mut();
            scheduler.prepare_external_wake();
        }

        run_completed_io(&self.core, false);

        let mut scheduler = self.core.borrow_mut();
        scheduler.prepare_external_wake();
    }

    fn submit_and_wait_for_io(&self, entry: impl Into<Sqe>) -> KernelIoResult {
        let state = self.acquire_io_state();
        let user_data = self.submit_io_state(entry, Rc::clone(&state));
        self.wait_for_submitted_io_state(state, IoCancelKind::Async(user_data))
    }

    fn wait_for_submitted_io_state(
        &self,
        state: Rc<IoState>,
        cancel_kind: IoCancelKind,
    ) -> KernelIoResult {
        let result = panic::catch_unwind(AssertUnwindSafe(|| self.wait_for_io_state(&state)));
        match result {
            Ok(result) => {
                self.recycle_io_state(state);
                self.resume_active_scope_panic();
                result
            }
            Err(payload) => {
                self.cancel_and_wait_for_io_state(&state, cancel_kind);
                self.recycle_io_state(state);
                panic::resume_unwind(payload);
            }
        }
    }

    fn wait_for_io_state(&self, state: &Rc<IoState>) -> KernelIoResult {
        loop {
            if let Some(result) = state.take_result() {
                return result;
            }

            let registration = self.wait_registration();
            state.add_waiter_from(self, &registration);
            self.park();
            self.resume_active_scope_panic();
        }
    }

    fn wait_for_io_state_without_scope_panic(&self, state: &Rc<IoState>) -> KernelIoResult {
        loop {
            if let Some(result) = state.take_result() {
                return result;
            }

            let registration = self.wait_registration();
            state.add_waiter_from(self, &registration);
            self.park();
        }
    }

    fn cancel_and_wait_for_io_state(&self, state: &Rc<IoState>, cancel_kind: IoCancelKind) {
        let cancel_state = self.submit_cancel_for_io_state(state, cancel_kind);
        while !state.is_ready() || cancel_state.as_ref().is_some_and(|state| !state.is_ready()) {
            let registration = self.wait_registration();
            state.add_waiter_from(self, &registration);
            if let Some(cancel_state) = &cancel_state {
                cancel_state.add_waiter_from(self, &registration);
            }
            self.park();
        }

        if let Some(cancel_state) = cancel_state {
            self.recycle_io_state(cancel_state);
        }
    }

    fn submit_cancel_for_io_state(
        &self,
        state: &Rc<IoState>,
        cancel_kind: IoCancelKind,
    ) -> Option<Rc<IoState>> {
        self.core.borrow_mut().submit_cancel(state, cancel_kind)
    }

    fn submit_io_state(&self, entry: impl Into<Sqe>, state: Rc<IoState>) -> io_uring_user_data {
        self.core.borrow_mut().submit_io_state(entry, state)
    }

    fn submit_io_state_with_link_timeout(
        &self,
        entry: impl Into<Sqe>,
        state: Rc<IoState>,
        duration: Duration,
    ) -> io_uring_user_data {
        self.core
            .borrow_mut()
            .submit_io_state_with_link_timeout(entry, state, duration)
    }

    pub(crate) fn submit_timeout(&self, duration: Duration) -> TimeoutState {
        let state = self.acquire_io_state();
        state.set_timeout(Timespec::from(duration));
        let entry = state.timeout_entry();
        let user_data = self.submit_io_state(entry, Rc::clone(&state));

        TimeoutState { state, user_data }
    }

    pub(crate) fn cancel_timeout(&self, timeout: TimeoutState) {
        if timeout.state.is_ready() {
            self.recycle_io_state(timeout.state);
            return;
        }

        let cancel_state = self
            .submit_cancel_for_io_state(&timeout.state, IoCancelKind::Timeout(timeout.user_data));

        while !timeout.state.is_ready()
            || cancel_state.as_ref().is_some_and(|state| !state.is_ready())
        {
            let registration = self.wait_registration();
            timeout.state.add_waiter_from(self, &registration);
            if let Some(cancel_state) = &cancel_state {
                cancel_state.add_waiter_from(self, &registration);
            }
            self.park();
        }

        self.recycle_io_state(timeout.state);
        if let Some(cancel_state) = cancel_state {
            self.recycle_io_state(cancel_state);
        }
    }

    fn acquire_io_state(&self) -> Rc<IoState> {
        self.core.borrow_mut().acquire_io_state()
    }

    pub(crate) fn recycle_io_state(&self, state: Rc<IoState>) {
        self.core.borrow_mut().recycle_io_state(state);
    }

    pub(crate) fn park(&self) {
        match self.current {
            CurrentTask::Root => {
                assert!(
                    with_current_context_cleared(|| drive_scheduler(&self.core)),
                    "kimojio-stack runtime deadlocked: no runnable stackful coroutines"
                );
            }
            CurrentTask::Coroutine { id, yielder, stack } => {
                suspend_task(id, yielder, stack, Suspend::Parked);
            }
        }
    }

    pub(crate) fn wait_registration(&self) -> WaitRegistration {
        match self.current {
            CurrentTask::Root => WaitRegistration::root(),
            CurrentTask::Coroutine { .. } => {
                let token = self.core.borrow_mut().register_wait_token();
                WaitRegistration::new(Rc::downgrade(&self.core), token)
            }
        }
    }

    pub(crate) fn waiter(&self, registration: &WaitRegistration) -> Option<Waiter> {
        match self.current {
            CurrentTask::Root => None,
            CurrentTask::Coroutine { id, .. } => registration.token().map(|token| Waiter {
                core: Rc::downgrade(&self.core),
                task_id: id,
                token,
            }),
        }
    }

    fn resume_active_scope_panic(&self) {
        let payload = {
            let scopes = self.active_scopes.borrow();
            scopes
                .iter()
                .rev()
                .find_map(|scope| scope.take_panic_payload())
        };

        if let Some(payload) = payload {
            panic::resume_unwind(payload);
        }
    }

    pub(crate) fn external_wait_registration(&self) -> Option<ExternalWaitRegistration> {
        match self.current {
            CurrentTask::Root => None,
            CurrentTask::Coroutine { id, .. } => {
                let external_wake = self.core.borrow().external_wake();
                Some(ExternalWaitRegistration::new(&external_wake, id))
            }
        }
    }
}

impl fmt::Debug for RuntimeContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuntimeContext").finish_non_exhaustive()
    }
}

#[derive(Clone, Copy)]
enum CurrentTask<'cx> {
    Root,
    Coroutine {
        id: TaskKey,
        yielder: &'cx Yielder<ResumeAction, Suspend>,
        stack: StackTracker,
    },
}

/// A structured concurrency scope for stackful coroutines.
pub struct Scope<'scope, 'env: 'scope> {
    core: Rc<SchedulerCell>,
    state: Rc<ScopeState>,
    stack_size: usize,
    _scope: PhantomData<&'scope mut &'scope ()>,
    _env: PhantomData<&'env mut &'env ()>,
}

impl<'scope, 'env: 'scope> Scope<'scope, 'env> {
    /// Spawns a stackful coroutine in this scope.
    ///
    /// The returned handle can be joined to retrieve the coroutine's return
    /// value. If the handle is dropped, the coroutine is still driven until it
    /// completes or is canceled as the scope exits.
    pub fn spawn<F, T>(&self, f: F) -> JoinHandle<'scope, T>
    where
        F: FnOnce(&RuntimeContext<'_>) -> T + 'scope,
        T: 'scope,
    {
        self.spawn_with_stack_size(self.stack_size, f)
    }

    /// Spawns a stackful coroutine with a custom usable stack size.
    ///
    /// The stack allocator may round the requested size up to the next page and
    /// adds a guard page that is not included in [`StackUsage::size`].
    pub fn spawn_with_stack_size<F, T>(&self, stack_size: usize, f: F) -> JoinHandle<'scope, T>
    where
        F: FnOnce(&RuntimeContext<'_>) -> T + 'scope,
        T: 'scope,
    {
        let stack_size = effective_stack_size(stack_size);
        let id = self.core.borrow_mut().reserve_task();
        let state = Rc::clone(&self.state);
        let task_state = Rc::clone(&state);
        let panic_payload = Rc::new(RefCell::new(None));
        let task_panic_payload = Rc::clone(&panic_payload);
        let scope_panic_payload = Rc::clone(&panic_payload);
        let join = Rc::new(JoinState::new(panic_payload, Rc::downgrade(&state), id));
        let task_join = Rc::clone(&join);
        let core = Rc::clone(&self.core);
        let stack = self.core.borrow_mut().acquire_stack(stack_size);
        let stack_tracker = StackTracker::new(&stack);

        let coroutine: Coroutine<ResumeAction, Suspend, ()> = unsafe {
            // SAFETY: spawned closures and their return slots are bounded by the
            // public scope lifetime. `RuntimeContext::scope` waits for every task
            // counted in `ScopeState` before returning, and `JoinHandle` cannot
            // escape the scope that created it.
            Coroutine::with_stack_unchecked(stack, move |yielder, action| {
                let cx = RuntimeContext {
                    core: Rc::clone(&core),
                    stack_size,
                    current: CurrentTask::Coroutine {
                        id,
                        yielder,
                        stack: stack_tracker,
                    },
                    active_scopes: RefCell::new(Vec::new()),
                };

                let result = panic::catch_unwind(AssertUnwindSafe(|| {
                    handle_resume_action(id, yielder, stack_tracker, Suspend::Ready, action);
                    let _current_context = CurrentContextGuard::enter(&cx);
                    f(&cx)
                }));
                let result = match result {
                    Ok(value) => TaskOutcome::Value(value),
                    Err(payload) if payload.is::<TaskCanceled>() => TaskOutcome::Canceled,
                    Err(payload) => {
                        task_join.store_panic_payload(payload);
                        TaskOutcome::Panicked
                    }
                };
                let panicked = matches!(result, TaskOutcome::Panicked);
                let stack_usage = cx
                    .stack_usage()
                    .expect("stackful coroutine missing stack usage");
                task_join.complete(result, stack_usage);
                task_state.task_finished(id, &task_panic_payload, panicked);
            })
        };

        state.task_started(id, scope_panic_payload);
        self.core.borrow_mut().insert_task(
            id,
            Task {
                coroutine,
                stack: stack_tracker,
            },
        );

        JoinHandle {
            state: join,
            taken: Cell::new(false),
            _scope: PhantomData,
        }
    }
}

impl fmt::Debug for Scope<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scope").finish_non_exhaustive()
    }
}

/// A handle returned by [`Scope::spawn`].
pub struct JoinHandle<'scope, T> {
    state: Rc<JoinState<T>>,
    taken: Cell<bool>,
    _scope: PhantomData<&'scope mut T>,
}

impl<T> JoinHandle<'_, T> {
    /// Returns the completed stackful coroutine result if it is ready.
    pub fn try_join(&self) -> Option<T> {
        self.try_join_with_stack_usage().map(|(result, _)| result)
    }

    /// Returns final stack usage if the coroutine has completed.
    pub fn stack_usage(&self) -> Option<StackUsage> {
        self.state.stack_usage()
    }

    /// Returns the completed coroutine result and final stack usage if ready.
    pub fn try_join_with_stack_usage(&self) -> Option<(T, StackUsage)> {
        assert!(!self.taken.get(), "JoinHandle value already taken");

        let outcome = self.state.take_result()?;
        let stack_usage = self
            .state
            .stack_usage()
            .expect("completed JoinHandle missing stack usage");
        self.taken.set(true);
        match outcome {
            TaskOutcome::Value(result) => Some((result, stack_usage)),
            TaskOutcome::Panicked => self.state.resume_panic(),
            TaskOutcome::Canceled => panic!("stackful coroutine canceled before JoinHandle joined"),
        }
    }

    /// Waits for the stackful coroutine to finish and returns its result.
    pub fn join(self, cx: &RuntimeContext<'_>) -> T {
        loop {
            if let Some(result) = self.try_join() {
                return result;
            }

            let registration = cx.wait_registration();
            self.state.add_waiter_from(cx, &registration);
            cx.park();
        }
    }

    /// Waits for the coroutine to finish and returns its result plus final stack usage.
    pub fn join_with_stack_usage(self, cx: &RuntimeContext<'_>) -> (T, StackUsage) {
        loop {
            if let Some(result) = self.try_join_with_stack_usage() {
                return result;
            }

            let registration = cx.wait_registration();
            self.state.add_waiter_from(cx, &registration);
            cx.park();
        }
    }
}

impl<T> Waitable for JoinHandle<'_, T> {
    fn is_ready(&self) -> bool {
        self.taken.get() || self.state.is_ready()
    }

    fn add_waiter(&self, cx: &RuntimeContext<'_>, registration: &WaitRegistration) {
        self.state.add_waiter_from(cx, registration);
    }
}

impl<T> fmt::Debug for JoinHandle<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JoinHandle").finish_non_exhaustive()
    }
}

/// An owned buffer that can receive bytes from io_uring.
///
/// # Safety
///
/// Implementors must return a pointer that remains valid for writes of
/// [`IoReadBuffer::io_buffer_len`] bytes until the buffer is dropped. Moving the
/// buffer value, including into the runtime's detached-operation storage, must
/// not invalidate the pointer previously submitted to the kernel. If an
/// [`IoResult`] is leaked while pending, leaking the buffer value must keep the
/// submitted memory valid.
pub unsafe trait IoReadBuffer {
    /// Returns the writable buffer pointer submitted to io_uring.
    fn io_buffer_mut_ptr(&mut self) -> *mut u8;

    /// Returns the number of bytes available at [`IoReadBuffer::io_buffer_mut_ptr`].
    fn io_buffer_len(&self) -> usize;
}

/// An owned buffer that can provide bytes to io_uring.
///
/// # Safety
///
/// Implementors must return a pointer that remains valid for reads of
/// [`IoWriteBuffer::io_buffer_len`] bytes until the buffer is dropped. Moving the
/// buffer value, including into the runtime's detached-operation storage, must
/// not invalidate the pointer previously submitted to the kernel. If an
/// [`IoResult`] is leaked while pending, leaking the buffer value must keep the
/// submitted memory valid.
pub unsafe trait IoWriteBuffer {
    /// Returns the readable buffer pointer submitted to io_uring.
    fn io_buffer_ptr(&self) -> *const u8;

    /// Returns the number of bytes available at [`IoWriteBuffer::io_buffer_ptr`].
    fn io_buffer_len(&self) -> usize;
}

unsafe impl IoReadBuffer for Vec<u8> {
    fn io_buffer_mut_ptr(&mut self) -> *mut u8 {
        self.as_mut_ptr()
    }

    fn io_buffer_len(&self) -> usize {
        self.len()
    }
}

unsafe impl IoWriteBuffer for Vec<u8> {
    fn io_buffer_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    fn io_buffer_len(&self) -> usize {
        self.len()
    }
}

unsafe impl IoReadBuffer for Box<[u8]> {
    fn io_buffer_mut_ptr(&mut self) -> *mut u8 {
        self.as_mut_ptr()
    }

    fn io_buffer_len(&self) -> usize {
        self.len()
    }
}

unsafe impl IoWriteBuffer for Box<[u8]> {
    fn io_buffer_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    fn io_buffer_len(&self) -> usize {
        self.len()
    }
}

/// Completed read data.
#[derive(Debug, Eq, PartialEq)]
pub struct ReadOutput<B = Vec<u8>> {
    /// Number of bytes read into `buffer`.
    pub bytes: usize,
    /// Buffer owned by the read operation.
    pub buffer: B,
}

/// Completed write data.
#[derive(Debug, Eq, PartialEq)]
pub struct WriteOutput<B = Vec<u8>> {
    /// Number of bytes written from `buffer`.
    pub bytes: usize,
    /// Buffer owned by the write operation.
    pub buffer: B,
}

/// Runtime-owned file descriptor identity for unregistered async I/O.
///
/// `IoFd` consumes an [`OwnedFd`] once and clones by incrementing a local
/// reference count. Cloning does not call `dup(2)` or otherwise touch the
/// kernel. Async operations clone this handle into operation state so the
/// underlying descriptor cannot close before the kernel CQE is reaped.
#[derive(Clone)]
pub struct IoFd {
    state: Rc<IoFdState>,
}

impl IoFd {
    /// Takes ownership of `fd` for use with unregistered async I/O.
    pub fn from_owned(fd: OwnedFd) -> Self {
        Self {
            state: Rc::new(IoFdState { fd }),
        }
    }

    /// Returns the raw fd for SQE construction.
    fn raw_fd(&self) -> i32 {
        self.state.fd.as_fd().as_raw_fd()
    }

    /// Converts back into the owned fd if no clones or pending operations exist.
    pub fn into_owned(self) -> Result<OwnedFd, Self> {
        match Rc::try_unwrap(self.state) {
            Ok(state) => Ok(state.fd),
            Err(state) => Err(Self { state }),
        }
    }
}

impl AsFd for IoFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.state.fd.as_fd()
    }
}

impl fmt::Debug for IoFd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IoFd").finish_non_exhaustive()
    }
}

struct IoFdState {
    fd: OwnedFd,
}

/// A file descriptor registered in the runtime's fixed-file table.
pub struct RegisteredFd {
    state: Rc<RegisteredFdState>,
}

impl RegisteredFd {
    fn new(core: Weak<SchedulerCell>, fd: OwnedFd) -> Result<Self, Errno> {
        let core = core.upgrade().expect("runtime scheduler dropped");
        let slot = core.borrow_mut().register_fixed_fd(&fd)?;
        let state = Rc::new(RegisteredFdState {
            core: Rc::downgrade(&core),
            slot,
            fd: Cell::new(Some(fd)),
            in_flight: Cell::new(0),
            retired: Cell::new(false),
            active: Cell::new(true),
        });

        Ok(Self { state })
    }

    fn slot(&self) -> u32 {
        self.state.slot
    }

    fn resource(&self) -> Rc<dyn RegisteredResource> {
        self.state.clone()
    }
}

impl Drop for RegisteredFd {
    fn drop(&mut self) {
        self.state.retire();
    }
}

impl fmt::Debug for RegisteredFd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegisteredFd")
            .field("slot", &self.state.slot)
            .field("in_flight", &self.state.in_flight.get())
            .finish()
    }
}

/// An owned buffer registered in the runtime's fixed-buffer table.
pub struct RegisteredBuffer<B> {
    state: Rc<RegisteredBufferState<B>>,
}

impl<B> RegisteredBuffer<B> {
    fn new(core: Weak<SchedulerCell>, buffer: B, ptr: *mut u8, len: usize) -> Result<Self, Errno> {
        let core = core.upgrade().expect("runtime scheduler dropped");
        let slot = core.borrow_mut().register_fixed_buffer(ptr, len)?;
        let state = Rc::new(RegisteredBufferState {
            core: Rc::downgrade(&core),
            slot,
            buffer: UnsafeCell::new(buffer),
            ptr,
            len,
            in_flight: Cell::new(0),
            retired: Cell::new(false),
            active: Cell::new(true),
        });

        Ok(Self { state })
    }

    fn slot(&self) -> u16 {
        self.state.slot
    }

    fn ptr(&self) -> *const u8 {
        self.state.ptr.cast_const()
    }

    fn mut_ptr(&self) -> *mut u8 {
        self.state.ptr
    }

    fn len(&self) -> usize {
        self.state.len
    }

    /// Returns the registered buffer.
    pub fn buffer(&self) -> &B {
        assert_eq!(
            self.state.in_flight.get(),
            0,
            "registered buffer is in use by io_uring"
        );
        unsafe { &*self.state.buffer.get() }
    }

    /// Returns the registered buffer mutably.
    pub fn buffer_mut(&mut self) -> &mut B {
        assert_eq!(
            self.state.in_flight.get(),
            0,
            "registered buffer is in use by io_uring"
        );
        unsafe { &mut *self.state.buffer.get() }
    }
}

impl<B: 'static> RegisteredBuffer<B> {
    fn resource(&self) -> Rc<dyn RegisteredResource> {
        self.state.clone()
    }
}

impl<B> Drop for RegisteredBuffer<B> {
    fn drop(&mut self) {
        self.state.retire();
    }
}

impl<B> fmt::Debug for RegisteredBuffer<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegisteredBuffer")
            .field("slot", &self.state.slot)
            .field("len", &self.state.len)
            .field("in_flight", &self.state.in_flight.get())
            .finish()
    }
}

unsafe impl<B> IoReadBuffer for RegisteredBuffer<B> {
    fn io_buffer_mut_ptr(&mut self) -> *mut u8 {
        self.mut_ptr()
    }

    fn io_buffer_len(&self) -> usize {
        self.len()
    }
}

unsafe impl<B> IoWriteBuffer for RegisteredBuffer<B> {
    fn io_buffer_ptr(&self) -> *const u8 {
        self.ptr()
    }

    fn io_buffer_len(&self) -> usize {
        self.len()
    }
}

trait RegisteredResource {
    fn start_io(&self);
    fn complete_io(&self);
}

struct RegisteredFdState {
    core: Weak<SchedulerCell>,
    slot: u32,
    fd: Cell<Option<OwnedFd>>,
    in_flight: Cell<usize>,
    retired: Cell<bool>,
    active: Cell<bool>,
}

impl RegisteredFdState {
    fn retire(&self) {
        self.retired.set(true);
        self.maybe_cleanup();
    }

    fn maybe_cleanup(&self) {
        if !self.active.get() || !self.retired.get() || self.in_flight.get() != 0 {
            return;
        }

        self.active.set(false);
        if let Some(core) = self.core.upgrade() {
            core.borrow_mut().unregister_fixed_fd(self.slot);
        }
        self.fd.take();
    }
}

impl RegisteredResource for RegisteredFdState {
    fn start_io(&self) {
        assert!(self.active.get(), "registered fd slot is inactive");
        assert!(!self.retired.get(), "registered fd is retired");
        self.in_flight.set(self.in_flight.get() + 1);
    }

    fn complete_io(&self) {
        self.in_flight.set(
            self.in_flight
                .get()
                .checked_sub(1)
                .expect("registered fd in-flight underflow"),
        );
        self.maybe_cleanup();
    }
}

struct RegisteredBufferState<B> {
    core: Weak<SchedulerCell>,
    slot: u16,
    buffer: UnsafeCell<B>,
    ptr: *mut u8,
    len: usize,
    in_flight: Cell<usize>,
    retired: Cell<bool>,
    active: Cell<bool>,
}

impl<B> RegisteredBufferState<B> {
    fn retire(&self) {
        self.retired.set(true);
        self.maybe_cleanup();
    }

    fn maybe_cleanup(&self) {
        if !self.active.get() || !self.retired.get() || self.in_flight.get() != 0 {
            return;
        }

        self.active.set(false);
        if let Some(core) = self.core.upgrade() {
            core.borrow_mut().unregister_fixed_buffer(self.slot);
        }
    }
}

impl<B> RegisteredResource for RegisteredBufferState<B> {
    fn start_io(&self) {
        assert!(self.active.get(), "registered buffer slot is inactive");
        assert!(!self.retired.get(), "registered buffer is retired");
        self.in_flight.set(self.in_flight.get() + 1);
    }

    fn complete_io(&self) {
        self.in_flight.set(
            self.in_flight
                .get()
                .checked_sub(1)
                .expect("registered buffer in-flight underflow"),
        );
        self.maybe_cleanup();
    }
}

struct RegisteredResources {
    first: Option<Rc<dyn RegisteredResource>>,
    second: Option<Rc<dyn RegisteredResource>>,
}

impl RegisteredResources {
    fn new() -> Self {
        Self {
            first: None,
            second: None,
        }
    }

    fn push(&mut self, resource: Rc<dyn RegisteredResource>) {
        resource.start_io();
        if self.first.is_none() {
            self.first = Some(resource);
        } else {
            assert!(
                self.second.is_none(),
                "too many registered resources on one IO"
            );
            self.second = Some(resource);
        }
    }

    fn complete_all(&mut self) {
        if let Some(resource) = self.first.take() {
            resource.complete_io();
        }
        if let Some(resource) = self.second.take() {
            resource.complete_io();
        }
    }

    fn clear(&mut self) {
        self.first = None;
        self.second = None;
    }
}

struct DetachedBuffer {
    ptr: NonNull<()>,
    drop_fn: unsafe fn(NonNull<()>),
}

impl DetachedBuffer {
    fn new<B>(buffer: B) -> Self {
        let ptr = Box::into_raw(Box::new(buffer)).cast::<()>();
        Self {
            ptr: NonNull::new(ptr).expect("Box::into_raw returned null"),
            drop_fn: drop_detached_buffer::<B>,
        }
    }
}

impl Drop for DetachedBuffer {
    fn drop(&mut self) {
        // SAFETY: DetachedBuffer::new stores a pointer and drop function for the
        // same concrete buffer type, and no other owner can drop that allocation.
        unsafe {
            (self.drop_fn)(self.ptr);
        }
    }
}

unsafe fn drop_detached_buffer<B>(ptr: NonNull<()>) {
    // SAFETY: `ptr` was produced by `Box::into_raw(Box::new(buffer))` for this
    // same concrete `B` in `DetachedBuffer::new`.
    unsafe {
        drop(Box::from_raw(ptr.as_ptr().cast::<B>()));
    }
}

struct DetachedIo {
    state: Rc<IoState>,
    cancel_state: Option<Rc<IoState>>,
    buffer: Option<DetachedBuffer>,
}

impl DetachedIo {
    fn new(
        state: Rc<IoState>,
        cancel_state: Option<Rc<IoState>>,
        buffer: Option<DetachedBuffer>,
    ) -> Self {
        Self {
            state,
            cancel_state,
            buffer,
        }
    }

    fn is_ready(&self) -> bool {
        self.state.is_ready()
            && self
                .cancel_state
                .as_ref()
                .is_none_or(|state| state.is_ready())
    }

    fn reap(mut self, core: &Weak<SchedulerCell>) {
        drop(self.buffer.take());
        recycle_io_state(core, self.state);
        if let Some(cancel_state) = self.cancel_state {
            recycle_io_state(core, cancel_state);
        }
    }
}

/// A pending io_uring operation.
#[must_use = "pending IO should be completed with get, try_get, RuntimeContext::join, cancel, or detach"]
pub struct IoResult<T, B = Vec<u8>> {
    core: Weak<SchedulerCell>,
    state: Option<Rc<IoState>>,
    buffer: ManuallyDrop<B>,
    output: fn(B, u32) -> T,
    taken: bool,
    _output: PhantomData<T>,
}

impl<T, B> IoResult<T, B> {
    fn new(
        core: Weak<SchedulerCell>,
        state: Rc<IoState>,
        buffer: B,
        output: fn(B, u32) -> T,
    ) -> Self {
        Self {
            core,
            state: Some(state),
            buffer: ManuallyDrop::new(buffer),
            output,
            taken: false,
            _output: PhantomData,
        }
    }

    /// Returns the completed value if the CQE has already been reaped.
    ///
    /// This does not enter io_uring or otherwise drive the scheduler.
    pub fn try_get(&mut self) -> Option<Result<T, Errno>> {
        assert!(!self.taken, "IoResult value already taken");

        let result = self.state.as_ref()?.take_result()?;
        let state = self.state.take().expect("IoResult state missing");
        self.taken = true;

        let output = match result {
            Ok(value) => {
                let buffer = unsafe { ManuallyDrop::take(&mut self.buffer) };
                Ok((self.output)(buffer, value))
            }
            Err(error) => {
                unsafe {
                    ManuallyDrop::drop(&mut self.buffer);
                }
                Err(error)
            }
        };
        recycle_io_state(&self.core, state);

        Some(output)
    }

    /// Waits for the operation to complete and returns its value.
    pub fn get(mut self, cx: &RuntimeContext<'_>) -> Result<T, Errno> {
        loop {
            if let Some(result) = self.try_get() {
                return result;
            }

            let registration = cx.wait_registration();
            self.state
                .as_ref()
                .expect("IoResult state missing")
                .add_waiter_from(cx, &registration);
            cx.park();
            cx.resume_active_scope_panic();
        }
    }

    fn user_data(&self) -> Option<io_uring_user_data> {
        self.state.as_ref().map(io_state_user_data)
    }

    /// Requests cancellation of this operation and detaches this result handle.
    ///
    /// After direct cancellation, this handle is taken and must not be drained
    /// for a completion result. Runtime-neutral code that needs a drainable
    /// canceled read/write result should call `SocketIoRuntime::cancel_read` or
    /// `SocketIoRuntime::cancel_write` on the runtime context instead.
    pub fn cancel(&mut self) {
        self.detach_pending(true);
    }

    /// Detaches this operation without requesting cancellation.
    ///
    /// The runtime keeps the fd and buffer alive until the CQE is reaped, then
    /// discards the result.
    pub fn detach(mut self) {
        self.detach_pending(false);
    }

    fn detach_pending(&mut self, cancel: bool) {
        if self.taken {
            return;
        }

        let Some(state) = self.state.take() else {
            return;
        };

        if state.is_ready() {
            // SAFETY: `taken` is false, so the buffer is still initialized, and a
            // ready state means the kernel no longer holds the submitted pointer.
            unsafe {
                ManuallyDrop::drop(&mut self.buffer);
            }
            self.taken = true;
            recycle_io_state(&self.core, state);
            return;
        }

        // SAFETY: `taken` is false, so the buffer has not been moved or dropped.
        let buffer = unsafe { ManuallyDrop::take(&mut self.buffer) };
        let cancel_kind = cancel.then(|| IoCancelKind::Async(io_state_user_data(&state)));
        detach_io_state(
            &self.core,
            state,
            cancel_kind,
            Some(DetachedBuffer::new(buffer)),
        );
        self.taken = true;
    }
}

impl<T, B> Cancellable for IoResult<T, B> {
    fn cancel(&mut self) {
        self.detach_pending(true);
    }
}

impl<T, B> Waitable for IoResult<T, B> {
    fn is_ready(&self) -> bool {
        self.taken || self.state.as_ref().is_none_or(|state| state.is_ready())
    }

    fn add_waiter(&self, cx: &RuntimeContext<'_>, registration: &WaitRegistration) {
        if let Some(state) = &self.state {
            state.add_waiter_from(cx, registration);
        }
    }
}

impl<T, B> fmt::Debug for IoResult<T, B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IoResult")
            .field("ready", &Waitable::is_ready(self))
            .field("taken", &self.taken)
            .finish()
    }
}

impl<T, B> Drop for IoResult<T, B> {
    fn drop(&mut self) {
        self.detach_pending(true);
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

enum TaskOutcome<T> {
    Value(T),
    Panicked,
    Canceled,
}

struct JoinState<T> {
    inner: RefCell<JoinStateInner<T>>,
    panic_payload: Rc<RefCell<Option<PanicPayload>>>,
    scope_state: Weak<ScopeState>,
    task_id: TaskKey,
}

impl<T> JoinState<T> {
    fn new(
        panic_payload: Rc<RefCell<Option<PanicPayload>>>,
        scope_state: Weak<ScopeState>,
        task_id: TaskKey,
    ) -> Self {
        Self {
            inner: RefCell::new(JoinStateInner {
                result: None,
                stack_usage: None,
                waiters: Waiters::default(),
            }),
            panic_payload,
            scope_state,
            task_id,
        }
    }

    fn complete(&self, result: TaskOutcome<T>, stack_usage: StackUsage) {
        let mut inner = self.inner.borrow_mut();
        inner.stack_usage = Some(stack_usage);
        inner.result = Some(result);
        inner.waiters.wake_all();
    }

    fn store_panic_payload(&self, payload: PanicPayload) {
        *self.panic_payload.borrow_mut() = Some(payload);
    }

    fn add_waiter_from(&self, cx: &RuntimeContext<'_>, registration: &WaitRegistration) {
        if let Some(waiter) = cx.waiter(registration) {
            self.inner.borrow_mut().waiters.push(waiter);
        }
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.inner.borrow().result.is_some()
    }

    fn stack_usage(&self) -> Option<StackUsage> {
        self.inner.borrow().stack_usage
    }

    fn take_result(&self) -> Option<TaskOutcome<T>> {
        self.inner.borrow_mut().result.take()
    }

    fn resume_panic(&self) -> ! {
        let payload = self
            .panic_payload
            .borrow_mut()
            .take()
            .unwrap_or_else(|| Box::new("stackful coroutine panic payload already consumed"));
        if let Some(scope_state) = self.scope_state.upgrade() {
            scope_state.remove_panic_payload(self.task_id, &self.panic_payload);
        }
        panic::resume_unwind(payload);
    }
}

struct JoinStateInner<T> {
    result: Option<TaskOutcome<T>>,
    stack_usage: Option<StackUsage>,
    waiters: Waiters,
}

pub(crate) struct TimeoutState {
    pub(crate) state: Rc<IoState>,
    user_data: io_uring_user_data,
}

/// A waitable io_uring timeout.
#[must_use = "timeouts should be waited on, canceled, detached, or allowed to complete"]
pub struct Timeout {
    core: Weak<SchedulerCell>,
    state: Option<Rc<IoState>>,
    user_data: io_uring_user_data,
}

impl Timeout {
    fn new(core: Weak<SchedulerCell>, state: TimeoutState) -> Self {
        Self {
            core,
            state: Some(state.state),
            user_data: state.user_data,
        }
    }

    /// Returns the completed timeout result if ready.
    pub fn try_wait(&mut self) -> Option<Result<(), Errno>> {
        let result = self.state.as_ref()?.take_result()?;
        let state = self.state.take().expect("timeout state missing");
        recycle_io_state(&self.core, state);
        Some(timeout_result(result))
    }

    /// Waits until the timeout completes.
    pub fn wait(mut self, cx: &RuntimeContext<'_>) -> Result<(), Errno> {
        loop {
            if let Some(result) = self.try_wait() {
                return result;
            }

            let registration = cx.wait_registration();
            self.state
                .as_ref()
                .expect("timeout state missing")
                .add_waiter_from(cx, &registration);
            cx.park();
            cx.resume_active_scope_panic();
        }
    }

    /// Updates this timeout to a new relative duration.
    pub fn update(&self, cx: &RuntimeContext<'_>, duration: Duration) -> Result<(), Errno> {
        let state = cx.acquire_io_state();
        state.set_timeout(Timespec::from(duration));
        let entry = state.timeout_update_entry(self.user_data);

        let user_data = cx.submit_io_state(entry, Rc::clone(&state));
        cx.wait_for_submitted_io_state(state, IoCancelKind::Async(user_data))
            .map(|_| ())
    }

    /// Requests cancellation of this timeout without waiting for completion.
    pub fn cancel(&mut self) {
        self.detach_pending(true);
    }

    /// Detaches this timeout without requesting cancellation.
    pub fn detach(mut self) {
        self.detach_pending(false);
    }

    /// Cancels this timeout and waits for the cancel request to complete.
    pub fn cancel_and_wait(mut self, cx: &RuntimeContext<'_>) -> Result<(), Errno> {
        let Some(state) = self.state.take() else {
            return Ok(());
        };

        if state.is_ready() {
            cx.recycle_io_state(state);
            return Ok(());
        }

        let cancel_state =
            cx.submit_cancel_for_io_state(&state, IoCancelKind::Timeout(self.user_data));

        while !state.is_ready() || cancel_state.as_ref().is_some_and(|state| !state.is_ready()) {
            let registration = cx.wait_registration();
            state.add_waiter_from(cx, &registration);
            if let Some(cancel_state) = &cancel_state {
                cancel_state.add_waiter_from(cx, &registration);
            }
            cx.park();
        }

        let cancel_result = cancel_state
            .as_ref()
            .and_then(|state| state.take_result())
            .unwrap_or(Ok(0));
        cx.recycle_io_state(state);
        if let Some(cancel_state) = cancel_state {
            cx.recycle_io_state(cancel_state);
        }
        cancel_result.map(|_| ())
    }

    fn detach_pending(&mut self, cancel: bool) {
        let Some(state) = self.state.take() else {
            return;
        };

        if state.is_ready() {
            recycle_io_state(&self.core, state);
            return;
        }

        let cancel_kind = cancel.then_some(IoCancelKind::Timeout(self.user_data));
        detach_io_state(&self.core, state, cancel_kind, None);
    }
}

impl Cancellable for Timeout {
    fn cancel(&mut self) {
        self.detach_pending(true);
    }
}

impl Waitable for Timeout {
    fn is_ready(&self) -> bool {
        self.state.as_ref().is_none_or(|state| state.is_ready())
    }

    fn add_waiter(&self, cx: &RuntimeContext<'_>, registration: &WaitRegistration) {
        if let Some(state) = &self.state {
            state.add_waiter_from(cx, registration);
        }
    }
}

impl RuntimeWaitable for Timeout {
    fn is_ready(&self) -> bool {
        Waitable::is_ready(self)
    }

    fn add_stackful_waiter(&self, waiter: StackfulWaiterHandle) -> bool {
        let Some(state) = &self.state else {
            return false;
        };
        state.add_stackful_waiter(waiter)
    }
}

impl fmt::Debug for Timeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Timeout")
            .field("ready", &Waitable::is_ready(self))
            .finish()
    }
}

impl Drop for Timeout {
    fn drop(&mut self) {
        self.detach_pending(true);
    }
}

fn timeout_result(result: KernelIoResult) -> Result<(), Errno> {
    match result {
        Ok(_) | Err(Errno::TIME) => Ok(()),
        Err(error) => Err(error),
    }
}

/// Unstable internal handle that can interrupt a root scheduler io_uring wait.
#[doc(hidden)]
#[allow(dead_code)]
pub struct SchedulerWake {
    external_wake: Arc<ExternalWake>,
}

impl SchedulerWake {
    fn new(external_wake: Arc<ExternalWake>) -> Self {
        external_wake.register_driver_waker();
        Self { external_wake }
    }

    /// Signals the associated runtime's scheduler wake source.
    #[doc(hidden)]
    pub fn wake(&self) {
        self.external_wake.driver_wake();
    }
}

impl Clone for SchedulerWake {
    fn clone(&self) -> Self {
        Self::new(Arc::clone(&self.external_wake))
    }
}

impl Drop for SchedulerWake {
    fn drop(&mut self) {
        self.external_wake.unregister_driver_waker();
    }
}

impl fmt::Debug for SchedulerWake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SchedulerWake").finish_non_exhaustive()
    }
}

/// Unstable internal waitable operation for no-payload io_uring work.
#[doc(hidden)]
#[must_use = "no-payload I/O should be waited on, canceled, or detached"]
#[allow(dead_code)]
pub struct NoPayloadIo {
    core: Weak<SchedulerCell>,
    state: Option<Rc<IoState>>,
    cancel_kind: IoCancelKind,
    result_kind: NoPayloadResultKind,
}

#[derive(Clone, Copy)]
#[allow(dead_code)]
enum NoPayloadResultKind {
    Unit,
    Timeout,
}

impl NoPayloadIo {
    fn new(
        core: Weak<SchedulerCell>,
        state: Rc<IoState>,
        cancel_kind: IoCancelKind,
        result_kind: NoPayloadResultKind,
    ) -> Self {
        Self {
            core,
            state: Some(state),
            cancel_kind,
            result_kind,
        }
    }

    /// Returns the completed result if the operation is ready.
    #[doc(hidden)]
    pub fn try_wait(&mut self) -> Option<Result<(), Errno>> {
        let result = self.state.as_ref()?.take_result()?;
        let state = self.state.take().expect("no-payload state missing");
        recycle_io_state(&self.core, state);
        Some(self.result_kind.map(result))
    }

    /// Waits until the operation completes.
    #[doc(hidden)]
    pub fn wait(mut self, cx: &RuntimeContext<'_>) -> Result<(), Errno> {
        loop {
            if let Some(result) = self.try_wait() {
                return result;
            }

            let registration = cx.wait_registration();
            self.state
                .as_ref()
                .expect("no-payload state missing")
                .add_waiter_from(cx, &registration);
            cx.park();
            cx.resume_active_scope_panic();
        }
    }

    /// Requests cancellation without waiting for completion.
    #[doc(hidden)]
    pub fn cancel(&mut self) {
        self.detach_pending(true);
    }

    /// Detaches this operation without requesting cancellation.
    #[doc(hidden)]
    pub fn detach(mut self) {
        self.detach_pending(false);
    }

    /// Cancels this operation and waits for the cancel request to complete.
    #[doc(hidden)]
    pub fn cancel_and_wait(mut self, cx: &RuntimeContext<'_>) -> Result<(), Errno> {
        let Some(state) = self.state.take() else {
            return Ok(());
        };

        if state.is_ready() {
            cx.recycle_io_state(state);
            return Ok(());
        }

        let cancel_state = cx.submit_cancel_for_io_state(&state, self.cancel_kind);
        while !state.is_ready() || cancel_state.as_ref().is_some_and(|state| !state.is_ready()) {
            let registration = cx.wait_registration();
            state.add_waiter_from(cx, &registration);
            if let Some(cancel_state) = &cancel_state {
                cancel_state.add_waiter_from(cx, &registration);
            }
            cx.park();
        }

        let cancel_result = cancel_state
            .as_ref()
            .and_then(|state| state.take_result())
            .unwrap_or(Ok(0));
        cx.recycle_io_state(state);
        if let Some(cancel_state) = cancel_state {
            cx.recycle_io_state(cancel_state);
        }
        cancel_result.map(|_| ())
    }

    fn detach_pending(&mut self, cancel: bool) {
        let Some(state) = self.state.take() else {
            return;
        };

        if state.is_ready() {
            recycle_io_state(&self.core, state);
            return;
        }

        detach_io_state(&self.core, state, cancel.then_some(self.cancel_kind), None);
    }
}

impl Waitable for NoPayloadIo {
    fn is_ready(&self) -> bool {
        self.state.as_ref().is_none_or(|state| state.is_ready())
    }

    fn add_waiter(&self, cx: &RuntimeContext<'_>, registration: &WaitRegistration) {
        if let Some(state) = &self.state {
            state.add_waiter_from(cx, registration);
        }
    }
}

impl RuntimeWaitable for NoPayloadIo {
    fn is_ready(&self) -> bool {
        Waitable::is_ready(self)
    }

    fn add_stackful_waiter(&self, waiter: StackfulWaiterHandle) -> bool {
        let Some(state) = &self.state else {
            return false;
        };
        state.add_stackful_waiter(waiter)
    }
}

/// Unstable internal waitable operation that returns the raw io_uring result.
#[doc(hidden)]
#[must_use = "raw I/O should be waited on, canceled, or detached"]
#[allow(dead_code)]
pub struct RawIo {
    core: Weak<SchedulerCell>,
    state: Option<Rc<IoState>>,
    cancel_kind: IoCancelKind,
    requires_payload_on_detach: bool,
    cancelable: bool,
}

impl RawIo {
    fn new(
        core: Weak<SchedulerCell>,
        state: Rc<IoState>,
        cancel_kind: IoCancelKind,
        requires_payload_on_detach: bool,
        cancelable: bool,
    ) -> Self {
        Self {
            core,
            state: Some(state),
            cancel_kind,
            requires_payload_on_detach,
            cancelable,
        }
    }

    /// Returns the completed raw result if the operation is ready.
    #[doc(hidden)]
    pub fn try_wait(&mut self) -> Option<Result<u32, Errno>> {
        let result = self.state.as_ref()?.take_result()?;
        let state = self.state.take().expect("raw I/O state missing");
        recycle_io_state(&self.core, state);
        Some(result)
    }

    /// Waits until the operation completes.
    #[doc(hidden)]
    pub fn wait(mut self, cx: &RuntimeContext<'_>) -> Result<u32, Errno> {
        loop {
            if let Some(result) = self.try_wait() {
                return result;
            }

            let registration = cx.wait_registration();
            self.state
                .as_ref()
                .expect("raw I/O state missing")
                .add_waiter_from(cx, &registration);
            cx.park();
            cx.resume_active_scope_panic();
        }
    }

    /// Requests cancellation without waiting for completion.
    ///
    /// Pending raw read/write operations submitted with borrowed pointers must
    /// use [`RawIo::cancel_with_payload`] instead so their descriptor/buffer
    /// owner stays alive until the kernel completion is reaped.
    #[doc(hidden)]
    pub fn cancel(&mut self) {
        self.detach_pending(true);
    }

    /// Requests cancellation while retaining `payload` until the operation is reaped.
    #[doc(hidden)]
    pub fn cancel_with_payload<B>(&mut self, payload: B)
    where
        B: 'static,
    {
        self.detach_pending_with_buffer(true, Some(DetachedBuffer::new(payload)));
    }

    /// Detaches this operation without requesting cancellation.
    ///
    /// Pending raw read/write operations submitted with borrowed pointers must
    /// use [`RawIo::detach_with_payload`] instead so their descriptor/buffer
    /// owner stays alive until the kernel completion is reaped.
    #[doc(hidden)]
    pub fn detach(mut self) {
        self.detach_pending(false);
    }

    /// Detaches while retaining `payload` until the operation is reaped.
    #[doc(hidden)]
    pub fn detach_with_payload<B>(mut self, payload: B)
    where
        B: 'static,
    {
        self.detach_pending_with_buffer(false, Some(DetachedBuffer::new(payload)));
    }

    fn detach_pending(&mut self, cancel: bool) {
        self.detach_pending_with_buffer(cancel, None);
    }

    fn detach_pending_with_buffer(&mut self, cancel: bool, buffer: Option<DetachedBuffer>) {
        assert!(
            !self.requires_payload_on_detach
                || buffer.is_some()
                || self.state.as_ref().is_none_or(|state| state.is_ready()),
            "pending raw read/write cancellation or detach requires payload retention"
        );

        let Some(state) = self.state.take() else {
            return;
        };

        if state.is_ready() {
            recycle_io_state(&self.core, state);
            return;
        }

        detach_io_state(
            &self.core,
            state,
            (cancel && self.cancelable).then_some(self.cancel_kind),
            buffer,
        );
    }
}

impl Waitable for RawIo {
    fn is_ready(&self) -> bool {
        self.state.as_ref().is_none_or(|state| state.is_ready())
    }

    fn add_waiter(&self, cx: &RuntimeContext<'_>, registration: &WaitRegistration) {
        if let Some(state) = &self.state {
            state.add_waiter_from(cx, registration);
        }
    }
}

impl RuntimeWaitable for RawIo {
    fn is_ready(&self) -> bool {
        Waitable::is_ready(self)
    }

    fn add_stackful_waiter(&self, waiter: StackfulWaiterHandle) -> bool {
        let Some(state) = &self.state else {
            return false;
        };
        state.add_stackful_waiter(waiter)
    }
}

impl fmt::Debug for RawIo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RawIo")
            .field(
                "ready",
                &self.state.as_ref().is_none_or(|state| state.is_ready()),
            )
            .finish()
    }
}

impl Drop for RawIo {
    fn drop(&mut self) {
        self.detach_pending(true);
    }
}

#[allow(dead_code)]
impl NoPayloadResultKind {
    fn map(self, result: KernelIoResult) -> Result<(), Errno> {
        match self {
            Self::Unit => result.map(|_| ()),
            Self::Timeout => timeout_result(result),
        }
    }
}

impl fmt::Debug for NoPayloadIo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NoPayloadIo")
            .field(
                "ready",
                &self.state.as_ref().is_none_or(|state| state.is_ready()),
            )
            .finish()
    }
}

impl Drop for NoPayloadIo {
    fn drop(&mut self) {
        self.detach_pending(true);
    }
}

pub(crate) struct IoState {
    inner: RefCell<IoStateInner>,
}

impl IoState {
    fn new() -> Self {
        Self {
            inner: RefCell::new(IoStateInner::new()),
        }
    }

    fn reset_for_reuse(&self) {
        self.inner.borrow_mut().reset_for_reuse();
    }

    fn complete(&self, result: KernelIoResult) {
        let mut inner = self.inner.borrow_mut();
        inner.result = Some(inner.map_completion_result(result));
        inner.registered_resources.complete_all();
        inner.waiters.wake_all();
        inner.stackful_waiters.wake_all();
    }

    pub(crate) fn add_waiter_from(&self, cx: &RuntimeContext<'_>, registration: &WaitRegistration) {
        if let Some(waiter) = cx.waiter(registration) {
            self.inner.borrow_mut().waiters.push(waiter);
        }
    }

    pub(crate) fn add_stackful_waiter(&self, waiter: StackfulWaiterHandle) -> bool {
        let mut inner = self.inner.borrow_mut();
        if inner.result.is_some() {
            false
        } else {
            inner.stackful_waiters.push(waiter);
            true
        }
    }

    fn is_ready(&self) -> bool {
        self.inner.borrow().result.is_some()
    }

    fn take_result(&self) -> Option<KernelIoResult> {
        self.inner.borrow_mut().result.take()
    }

    fn set_timeout(&self, timeout: Timespec) {
        self.inner.borrow_mut().timeout = Some(timeout);
    }

    fn timeout_entry(&self) -> Sqe {
        let inner = self.inner.borrow();
        opcode::Timeout::new(inner.timeout.as_ref().expect("timeout missing timespec"))
            .build()
            .into()
    }

    fn link_timeout_entry(&self) -> Sqe {
        let inner = self.inner.borrow();
        opcode::LinkTimeout::new(inner.timeout.as_ref().expect("timeout missing timespec"))
            .build()
            .into()
    }

    fn timeout_update_entry(&self, user_data: io_uring_user_data) -> Sqe {
        let inner = self.inner.borrow();
        opcode::TimeoutUpdate::new(
            user_data,
            inner.timeout.as_ref().expect("timeout missing timespec"),
        )
        .build()
        .into()
    }

    fn attach_registered_resource(&self, resource: Rc<dyn RegisteredResource>) {
        self.inner.borrow_mut().registered_resources.push(resource);
    }

    fn attach_io_fd(&self, fd: IoFd) {
        let mut inner = self.inner.borrow_mut();
        debug_assert!(inner.io_fd.is_none(), "IO state already has an IoFd");
        inner.io_fd = Some(fd);
    }

    fn mark_cancel_requested(&self) -> bool {
        let mut inner = self.inner.borrow_mut();
        let already_requested = inner.cancel_requested;
        inner.cancel_requested = true;
        !already_requested
    }

    fn mark_cancel_operation(&self) {
        self.inner.borrow_mut().cancel_operation = true;
    }

    fn mark_linked_timeout_operation(&self) {
        self.inner.borrow_mut().linked_timeout_operation = true;
    }

    fn cancel_requested(&self) -> bool {
        self.inner.borrow().cancel_requested
    }

    fn is_cancel_operation(&self) -> bool {
        self.inner.borrow().cancel_operation
    }

    fn result(&self) -> Option<KernelIoResult> {
        self.inner.borrow().result
    }
}

struct IoStateInner {
    result: Option<KernelIoResult>,
    waiters: Waiters,
    stackful_waiters: StackfulWaiters,
    timeout: Option<Timespec>,
    io_fd: Option<IoFd>,
    registered_resources: RegisteredResources,
    cancel_requested: bool,
    cancel_operation: bool,
    linked_timeout_operation: bool,
}

impl IoStateInner {
    fn new() -> Self {
        Self {
            result: None,
            waiters: Waiters::default(),
            stackful_waiters: StackfulWaiters::default(),
            timeout: None,
            io_fd: None,
            registered_resources: RegisteredResources::new(),
            cancel_requested: false,
            cancel_operation: false,
            linked_timeout_operation: false,
        }
    }

    fn reset_for_reuse(&mut self) {
        self.result = None;
        self.waiters.clear();
        self.stackful_waiters.clear();
        self.timeout = None;
        self.io_fd = None;
        self.registered_resources.clear();
        self.cancel_requested = false;
        self.cancel_operation = false;
        self.linked_timeout_operation = false;
    }

    fn map_completion_result(&self, result: KernelIoResult) -> KernelIoResult {
        if self.linked_timeout_operation && !self.cancel_requested && result == Err(Errno::CANCELED)
        {
            Err(Errno::TIME)
        } else {
            result
        }
    }
}

#[derive(Default)]
pub(crate) struct StackfulWaiters {
    first: Option<StackfulWaiterHandle>,
    second: Option<StackfulWaiterHandle>,
    rest: Vec<StackfulWaiterHandle>,
}

impl StackfulWaiters {
    pub(crate) fn push(&mut self, waiter: StackfulWaiterHandle) {
        if self
            .first
            .as_ref()
            .is_some_and(|waiter| !waiter.is_active())
            || self
                .second
                .as_ref()
                .is_some_and(|waiter| !waiter.is_active())
            || self.rest.len() > WAITER_COMPACT_THRESHOLD
        {
            self.compact_inactive();
        }

        if self.first.is_none() {
            self.first = Some(waiter);
        } else if self.second.is_none() {
            self.second = Some(waiter);
        } else {
            self.rest.push(waiter);
        }
    }

    pub(crate) fn wake_all(&mut self) {
        if let Some(waiter) = self.first.take()
            && waiter.mark_ready()
        {
            waiter.wake_ready();
        }
        if let Some(waiter) = self.second.take()
            && waiter.mark_ready()
        {
            waiter.wake_ready();
        }

        for waiter in self.rest.drain(..) {
            if waiter.mark_ready() {
                waiter.wake_ready();
            }
        }
    }

    pub(crate) fn clear(&mut self) {
        self.first = None;
        self.second = None;
        self.rest.clear();
    }

    fn compact_inactive(&mut self) {
        if self
            .first
            .as_ref()
            .is_some_and(|waiter| !waiter.is_active())
        {
            self.first = None;
        }
        if self
            .second
            .as_ref()
            .is_some_and(|waiter| !waiter.is_active())
        {
            self.second = None;
        }

        self.rest.retain(|waiter| waiter.is_active());
        if self.first.is_none() {
            self.first = self.second.take().or_else(|| {
                if self.rest.is_empty() {
                    None
                } else {
                    Some(self.rest.remove(0))
                }
            });
        }
        if self.second.is_none() && !self.rest.is_empty() {
            self.second = Some(self.rest.remove(0));
        }
    }
}

#[derive(Default)]
struct ScopeState {
    inner: RefCell<ScopeStateInner>,
}

impl ScopeState {
    fn task_started(&self, id: TaskKey, payload: Rc<RefCell<Option<PanicPayload>>>) {
        let mut inner = self.inner.borrow_mut();
        inner.remaining += 1;
        let inserted = inner.task_ids.insert(id);
        debug_assert!(inserted, "started task was already tracked by its scope");
        let previous = inner.panic_payloads.insert(id, payload);
        debug_assert!(
            previous.is_none(),
            "started task already had a scope panic payload slot"
        );
    }

    fn has_remaining_tasks(&self) -> bool {
        self.inner.borrow().remaining != 0
    }

    fn push_waiter(&self, waiter: Waiter) {
        self.inner.borrow_mut().waiters.push(waiter);
    }

    fn task_ids(&self) -> Vec<TaskKey> {
        self.inner.borrow().task_ids.iter().copied().collect()
    }

    #[cfg(test)]
    fn tracked_task_count(&self) -> usize {
        self.inner.borrow().task_ids.len()
    }

    #[cfg(test)]
    fn tracked_panic_payload_count(&self) -> usize {
        self.inner.borrow().panic_payloads.len()
    }

    fn take_panic_payload(&self) -> Option<PanicPayload> {
        let inner = self.inner.borrow();
        for payload in inner.panic_payloads.values() {
            if let Some(payload) = payload.borrow_mut().take() {
                return Some(payload);
            }
        }
        None
    }

    fn task_finished(
        &self,
        id: TaskKey,
        panic_payload: &Rc<RefCell<Option<PanicPayload>>>,
        keep_panic_payload: bool,
    ) {
        let mut inner = self.inner.borrow_mut();
        inner.remaining = inner
            .remaining
            .checked_sub(1)
            .expect("scope task count underflow");
        let removed = inner.task_ids.remove(&id);
        debug_assert!(removed, "finished task was not tracked by its scope");

        if keep_panic_payload {
            debug_assert!(
                inner
                    .panic_payloads
                    .get(&id)
                    .is_some_and(|payload| Rc::ptr_eq(payload, panic_payload)),
                "finished task panic payload slot did not match its join state"
            );
        } else if let Some(removed) = inner.panic_payloads.remove(&id) {
            debug_assert!(
                Rc::ptr_eq(&removed, panic_payload),
                "finished task panic payload slot did not match its join state"
            );
        } else {
            debug_assert!(false, "finished task had no scope panic payload slot");
        }

        if inner.remaining == 0 {
            inner.waiters.wake_all();
        }
    }

    fn reset_for_reuse(&self) {
        let mut inner = self.inner.borrow_mut();
        debug_assert_eq!(inner.remaining, 0, "recycling active scope state");
        inner.waiters.clear();
        inner.panic_payloads.clear();
        if inner.panic_payloads.capacity() > DEFAULT_SCOPE_STATE_COLLECTION_CACHE_CAPACITY {
            inner.panic_payloads.shrink_to(0);
        }
        inner.task_ids.clear();
        if inner.task_ids.capacity() > DEFAULT_SCOPE_STATE_COLLECTION_CACHE_CAPACITY {
            inner.task_ids.shrink_to(0);
        }
    }

    fn remove_panic_payload(&self, id: TaskKey, panic_payload: &Rc<RefCell<Option<PanicPayload>>>) {
        let mut inner = self.inner.borrow_mut();
        if let Some(removed) = inner.panic_payloads.remove(&id) {
            debug_assert!(
                Rc::ptr_eq(&removed, panic_payload),
                "observed task panic payload slot did not match its join state"
            );
        }
    }
}

#[derive(Default)]
struct ScopeStateInner {
    remaining: usize,
    waiters: Waiters,
    panic_payloads: HashMap<TaskKey, Rc<RefCell<Option<PanicPayload>>>>,
    task_ids: HashSet<TaskKey>,
}

#[derive(Clone)]
pub(crate) struct Waiter {
    core: Weak<SchedulerCell>,
    task_id: TaskKey,
    token: WaitToken,
}

impl Waiter {
    pub(crate) fn wake(self) -> bool {
        self.core.upgrade().is_some_and(|core| {
            let mut scheduler = core.borrow_mut();
            scheduler.take_wait_token(self.token) && scheduler.schedule(self.task_id)
        })
    }

    fn is_active(&self) -> bool {
        self.core
            .upgrade()
            .is_some_and(|core| core.borrow().is_wait_token_active(self.token))
    }
}

#[doc(hidden)]
pub struct WaitRegistration {
    core: Weak<SchedulerCell>,
    token: Option<WaitToken>,
    external: RefCell<Option<ExternalWaitRegistration>>,
}

impl WaitRegistration {
    fn root() -> Self {
        Self {
            core: Weak::new(),
            token: None,
            external: RefCell::new(None),
        }
    }

    fn new(core: Weak<SchedulerCell>, token: WaitToken) -> Self {
        Self {
            core,
            token: Some(token),
            external: RefCell::new(None),
        }
    }

    fn token(&self) -> Option<WaitToken> {
        self.token
    }

    /// Returns a stackful waiter tied to this registration for first-party
    /// external wait queues.
    #[doc(hidden)]
    pub fn external_waiter(&self, cx: &RuntimeContext<'_>) -> Option<StackfulWaiterHandle> {
        let mut external = self.external.borrow_mut();
        if external.is_none() {
            *external = cx.external_wait_registration();
        }
        external
            .as_ref()
            .map(|registration| StackfulWaiterHandle::external(registration.waiter()))
    }
}

impl Drop for WaitRegistration {
    fn drop(&mut self) {
        let Some(token) = self.token.take() else {
            return;
        };
        if let Some(core) = self.core.upgrade() {
            core.borrow_mut().release_wait_token(token);
        }
    }
}

#[derive(Clone, Copy)]
struct WaitToken {
    slot: usize,
    generation: u64,
}

struct WaitTokenSlot {
    generation: u64,
    active: bool,
    leased: bool,
}

const EXTERNAL_WAIT_ACTIVE: u8 = 0;
const EXTERNAL_WAIT_READY: u8 = 1;
const EXTERNAL_WAIT_CANCELED: u8 = 2;

pub(crate) struct ExternalWaitRegistration {
    registration: Arc<ExternalWaiterRegistration>,
}

impl ExternalWaitRegistration {
    fn new(external_wake: &Arc<ExternalWake>, task_id: TaskKey) -> Self {
        external_wake.register_waiter();
        Self {
            registration: Arc::new(ExternalWaiterRegistration {
                external_wake: Arc::downgrade(external_wake),
                task_id,
                state: AtomicU8::new(EXTERNAL_WAIT_ACTIVE),
            }),
        }
    }

    pub(crate) fn waiter(&self) -> ExternalWaiter {
        ExternalWaiter {
            registration: Arc::clone(&self.registration),
        }
    }
}

impl Drop for ExternalWaitRegistration {
    fn drop(&mut self) {
        self.registration.cancel();
    }
}

#[derive(Clone)]
pub(crate) struct ExternalWaiter {
    registration: Arc<ExternalWaiterRegistration>,
}

impl ExternalWaiter {
    #[cfg(test)]
    pub(crate) fn wake(self) -> bool {
        if !self.mark_ready() {
            return false;
        }

        self.wake_ready()
    }

    pub(crate) fn is_active(&self) -> bool {
        self.registration.is_active()
    }

    pub(crate) fn mark_ready(&self) -> bool {
        self.registration.mark_ready()
    }

    pub(crate) fn wake_ready(self) -> bool {
        let registration = self.registration;
        if let Some(external_wake) = registration.external_wake.upgrade() {
            external_wake.wake_task(ExternalReady {
                task_id: registration.task_id,
                registration,
            });
            true
        } else {
            false
        }
    }
}

struct ExternalWaiterRegistration {
    external_wake: std::sync::Weak<ExternalWake>,
    task_id: TaskKey,
    state: AtomicU8,
}

impl ExternalWaiterRegistration {
    fn mark_ready(&self) -> bool {
        self.state
            .compare_exchange(
                EXTERNAL_WAIT_ACTIVE,
                EXTERNAL_WAIT_READY,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn cancel(&self) {
        if self
            .state
            .compare_exchange(
                EXTERNAL_WAIT_ACTIVE,
                EXTERNAL_WAIT_CANCELED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return;
        }

        if let Some(external_wake) = self.external_wake.upgrade() {
            external_wake.unregister_waiter();
        }
    }

    fn is_active(&self) -> bool {
        self.state.load(Ordering::Acquire) == EXTERNAL_WAIT_ACTIVE
    }

    fn is_ready(&self) -> bool {
        self.state.load(Ordering::Acquire) == EXTERNAL_WAIT_READY
    }
}

impl Drop for ExternalWaiterRegistration {
    fn drop(&mut self) {
        self.cancel();
    }
}

struct ExternalReady {
    task_id: TaskKey,
    registration: Arc<ExternalWaiterRegistration>,
}

struct ExternalWake {
    inner: StdMutex<ExternalWakeInner>,
    wake_fd: OwnedFd,
    ready: Condvar,
}

impl ExternalWake {
    fn new() -> Self {
        Self {
            inner: StdMutex::new(ExternalWakeInner::default()),
            wake_fd: eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)
                .expect("failed to create external wake eventfd"),
            ready: Condvar::new(),
        }
    }

    fn register_waiter(&self) {
        let mut inner = self.inner();
        inner.waiters += 1;
    }

    fn unregister_waiter(&self) {
        let mut inner = self.inner();
        inner.waiters = inner
            .waiters
            .checked_sub(1)
            .expect("external waiter count underflow");
        drop(inner);
        self.signal();
    }

    #[allow(dead_code)]
    fn register_driver_waker(&self) {
        let mut inner = self.inner();
        inner.driver_wakers = inner
            .driver_wakers
            .checked_add(1)
            .expect("external driver waker count overflow");
        drop(inner);
        self.signal();
    }

    #[allow(dead_code)]
    fn unregister_driver_waker(&self) {
        let mut inner = self.inner();
        inner.driver_wakers = inner
            .driver_wakers
            .checked_sub(1)
            .expect("external driver waker count underflow");
        drop(inner);
        self.signal();
    }

    #[allow(dead_code)]
    fn driver_wake(&self) {
        let mut inner = self.inner();
        inner.driver_wakes = inner
            .driver_wakes
            .checked_add(1)
            .expect("external driver wake count overflow");
        drop(inner);
        self.signal();
    }

    fn wake_task(&self, ready: ExternalReady) {
        let mut inner = self.inner();
        inner.waiters = inner
            .waiters
            .checked_sub(1)
            .expect("external waiter count underflow");
        inner.ready.push_back(ready);
        drop(inner);
        self.signal();
    }

    fn take_ready(&self) -> VecDeque<ExternalReady> {
        let mut inner = self.inner();
        std::mem::take(&mut inner.ready)
    }

    fn has_waiters_or_ready(&self) -> bool {
        let inner = self.inner();
        inner.waiters != 0 || !inner.ready.is_empty()
    }

    fn has_io_wake_interest(&self) -> bool {
        let inner = self.inner();
        inner.waiters != 0
            || inner.driver_wakers != 0
            || inner.driver_wakes != 0
            || !inner.ready.is_empty()
    }

    fn take_driver_wakes(&self) -> bool {
        let mut inner = self.inner();
        let has_driver_wakes = inner.driver_wakes != 0;
        inner.driver_wakes = 0;
        has_driver_wakes
    }

    fn wake_fd(&self) -> &OwnedFd {
        &self.wake_fd
    }

    fn drain_signal(&self) {
        let mut buffer = [0_u8; 8];
        loop {
            match rustix_io::read(&self.wake_fd, &mut buffer) {
                Ok(8) => {}
                Ok(bytes) => panic!("short read from external wake eventfd: {bytes} bytes"),
                Err(rustix_io::Errno::AGAIN) => return,
                Err(rustix_io::Errno::INTR) => {}
                Err(error) => panic!("error reading external wake eventfd: {error:?}"),
            }
        }
    }

    fn signal(&self) {
        let buffer = 1_u64.to_ne_bytes();
        loop {
            match rustix_io::write(&self.wake_fd, &buffer) {
                Ok(8) => break,
                Ok(bytes) => panic!("short write to external wake eventfd: {bytes} bytes"),
                Err(rustix_io::Errno::AGAIN) => break,
                Err(rustix_io::Errno::INTR) => {}
                Err(error) => panic!("error writing external wake eventfd: {error:?}"),
            }
        }

        self.ready.notify_one();
    }

    fn wait_for_ready(&self, timeout: Option<Duration>) {
        let inner = self.inner();
        match timeout {
            Some(timeout) => {
                let _guard = self
                    .ready
                    .wait_timeout_while(inner, timeout, |inner| {
                        inner.ready.is_empty() && inner.waiters != 0
                    })
                    .expect("external wake mutex poisoned");
            }
            None => {
                let _guard = self
                    .ready
                    .wait_while(inner, |inner| inner.ready.is_empty() && inner.waiters != 0)
                    .expect("external wake mutex poisoned");
            }
        }
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, ExternalWakeInner> {
        self.inner.lock().expect("external wake mutex poisoned")
    }
}

#[derive(Default)]
struct ExternalWakeInner {
    ready: VecDeque<ExternalReady>,
    waiters: usize,
    driver_wakers: usize,
    driver_wakes: usize,
}

#[derive(Default)]
pub(crate) struct Waiters {
    first: Option<Waiter>,
    rest: Vec<Waiter>,
}

impl Waiters {
    pub(crate) fn push(&mut self, waiter: Waiter) {
        if self
            .first
            .as_ref()
            .is_some_and(|waiter| !waiter.is_active())
            || self.rest.len() > WAITER_COMPACT_THRESHOLD
        {
            self.compact_inactive();
        }

        if self.first.is_none() {
            self.first = Some(waiter);
        } else {
            self.rest.push(waiter);
        }
    }

    pub(crate) fn wake_all(&mut self) {
        if let Some(waiter) = self.first.take() {
            waiter.wake();
        }

        for waiter in self.rest.drain(..) {
            waiter.wake();
        }
    }

    pub(crate) fn wake_one(&mut self) {
        if let Some(waiter) = self.first.take()
            && waiter.wake()
        {
            return;
        }

        while !self.rest.is_empty() {
            if self.rest.remove(0).wake() {
                return;
            }
        }
    }

    pub(crate) fn clear(&mut self) {
        self.first = None;
        self.rest.clear();
    }

    fn compact_inactive(&mut self) {
        if self
            .first
            .as_ref()
            .is_some_and(|waiter| !waiter.is_active())
        {
            self.first = None;
        }

        self.rest.retain(Waiter::is_active);
        if self.first.is_none() && !self.rest.is_empty() {
            self.first = Some(self.rest.remove(0));
        }
    }
}

#[derive(Clone, Copy)]
enum Suspend {
    Ready,
    Parked,
}

struct Task {
    coroutine: Coroutine<ResumeAction, Suspend, ()>,
    stack: StackTracker,
}

impl Task {
    fn into_finished_stack(self) -> DefaultStack {
        self.coroutine.into_stack()
    }
}

struct StackPool {
    capacity: usize,
    stacks: Vec<CachedStack>,
    #[cfg(test)]
    reused: usize,
}

struct CachedStack {
    size: usize,
    stack: DefaultStack,
}

impl StackPool {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            stacks: Vec::new(),
            #[cfg(test)]
            reused: 0,
        }
    }

    fn acquire(&mut self, stack_size: usize) -> DefaultStack {
        if let Some(index) = self
            .stacks
            .iter()
            .position(|stack| stack.size == stack_size)
        {
            #[cfg(test)]
            {
                self.reused += 1;
            }
            self.stacks.swap_remove(index).stack
        } else {
            DefaultStack::new(stack_size).expect("failed to allocate stackful coroutine stack")
        }
    }

    fn recycle(&mut self, stack: DefaultStack) {
        if self.stacks.len() < self.capacity {
            self.stacks.push(CachedStack {
                size: StackTracker::new(&stack).size,
                stack,
            });
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.stacks.len()
    }

    #[cfg(test)]
    fn reused(&self) -> usize {
        self.reused
    }
}

enum ResumeAction {
    Run,
    Interrupt(TaskInterruptFn),
}

type TaskInterruptFn = Box<dyn FnOnce(TaskInterrupt)>;

#[derive(Clone, Copy)]
struct TaskInterrupt {
    task_id: TaskKey,
    stack: StackTracker,
}

impl TaskInterrupt {
    fn new(task_id: TaskKey, stack: StackTracker) -> Self {
        Self { task_id, stack }
    }

    fn task_id(self) -> usize {
        self.task_id.index()
    }

    fn stack_usage(self) -> StackUsage {
        self.stack.usage()
    }
}

#[derive(Clone, Copy)]
struct StackTracker {
    usable_low: usize,
    base: usize,
    size: usize,
}

impl StackTracker {
    fn new(stack: &DefaultStack) -> Self {
        let limit = stack.limit().get();
        let base = stack.base().get();
        let usable_low = limit
            .checked_add(param::page_size())
            .expect("stack lower bound overflow");
        assert!(usable_low < base, "stack must contain writable pages");

        Self {
            usable_low,
            base,
            size: base - usable_low,
        }
    }

    fn usage(self) -> StackUsage {
        let marker = 0_u8;
        self.usage_at((&marker as *const u8).addr())
    }

    fn usage_without_current(self) -> StackUsage {
        StackUsage::new(self.size, 0, self.resident_high_water())
    }

    fn usage_at(self, stack_addr: usize) -> StackUsage {
        let used = if stack_addr <= self.usable_low {
            self.size
        } else {
            self.base.saturating_sub(stack_addr)
        };

        StackUsage::new(self.size, used, self.resident_high_water())
    }

    fn resident_high_water(self) -> usize {
        let page_size = param::page_size();
        debug_assert_eq!(self.usable_low % page_size, 0);
        debug_assert_eq!(self.size % page_size, 0);

        let page_count = self.size / page_size;
        const INLINE_RESIDENCY_PAGES: usize = 256;

        if page_count <= INLINE_RESIDENCY_PAGES {
            let mut residency = [0_u8; INLINE_RESIDENCY_PAGES];
            self.fill_residency(&mut residency[..page_count]);
            return self.high_water_from_residency(&residency[..page_count], page_size);
        }

        let mut residency = vec![0_u8; page_count];
        self.fill_residency(&mut residency);
        self.high_water_from_residency(&residency, page_size)
    }

    fn fill_residency(self, residency: &mut [u8]) {
        let result = unsafe {
            libc::mincore(
                self.usable_low as *mut c_void,
                self.size,
                residency.as_mut_ptr().cast(),
            )
        };
        assert_eq!(
            result,
            0,
            "failed to inspect stack residency with mincore: {}",
            io::Error::last_os_error()
        );
    }

    fn high_water_from_residency(self, residency: &[u8], page_size: usize) -> usize {
        residency
            .iter()
            .position(|byte| byte & 1 != 0)
            .map_or(0, |first_resident| self.size - first_resident * page_size)
    }
}

struct SchedulerCell {
    scheduler: UnsafeCell<Scheduler>,
    #[cfg(debug_assertions)]
    borrow_state: Cell<isize>,
}

impl SchedulerCell {
    fn new(scheduler: Scheduler) -> Self {
        Self {
            scheduler: UnsafeCell::new(scheduler),
            #[cfg(debug_assertions)]
            borrow_state: Cell::new(0),
        }
    }

    fn borrow(&self) -> SchedulerRef<'_> {
        #[cfg(debug_assertions)]
        {
            let borrow_state = self.borrow_state.get();
            assert!(borrow_state >= 0, "already mutably borrowed: BorrowError");
            self.borrow_state.set(
                borrow_state
                    .checked_add(1)
                    .expect("too many shared scheduler borrows"),
            );
        }

        SchedulerRef { cell: self }
    }

    fn borrow_mut(&self) -> SchedulerRefMut<'_> {
        #[cfg(debug_assertions)]
        {
            let borrow_state = self.borrow_state.get();
            assert_eq!(borrow_state, 0, "already borrowed: BorrowMutError");
            self.borrow_state.set(-1);
        }

        SchedulerRefMut { cell: self }
    }
}

struct SchedulerRef<'cell> {
    cell: &'cell SchedulerCell,
}

impl Deref for SchedulerRef<'_> {
    type Target = Scheduler;

    fn deref(&self) -> &Self::Target {
        // SAFETY: Runtime and all scheduler handles are `Rc`-backed and confined to
        // one OS thread. Callers must not keep scheduler references across coroutine
        // resume/user-code boundaries; the scheduler loop scopes those accesses.
        unsafe { &*self.cell.scheduler.get() }
    }
}

#[cfg(debug_assertions)]
impl Drop for SchedulerRef<'_> {
    fn drop(&mut self) {
        let borrow_state = self.cell.borrow_state.get();
        debug_assert!(
            borrow_state > 0,
            "scheduler shared borrow underflow: {borrow_state}"
        );
        self.cell.borrow_state.set(borrow_state - 1);
    }
}

struct SchedulerRefMut<'cell> {
    cell: &'cell SchedulerCell,
}

impl Deref for SchedulerRefMut<'_> {
    type Target = Scheduler;

    fn deref(&self) -> &Self::Target {
        // SAFETY: See `SchedulerRef::deref`.
        unsafe { &*self.cell.scheduler.get() }
    }
}

impl DerefMut for SchedulerRefMut<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: See `SchedulerRef::deref`. Mutable accesses are manually scoped
        // so no scheduler reference is live while coroutine/user code can re-enter
        // scheduler APIs; debug builds enforce that invariant with borrow_state.
        unsafe { &mut *self.cell.scheduler.get() }
    }
}

#[cfg(debug_assertions)]
impl Drop for SchedulerRefMut<'_> {
    fn drop(&mut self) {
        let borrow_state = self.cell.borrow_state.get();
        debug_assert_eq!(
            borrow_state, -1,
            "scheduler mutable borrow state corrupted: {borrow_state}"
        );
        self.cell.borrow_state.set(0);
    }
}

struct Scheduler {
    tasks: Vec<Option<Task>>,
    task_generations: Vec<u32>,
    free_tasks: Vec<usize>,
    wait_tokens: Vec<WaitTokenSlot>,
    free_wait_tokens: Vec<usize>,
    queued: Vec<bool>,
    ready: VecDeque<TaskKey>,
    ring: IoUring,
    probe: Probe,
    registered_file_free: Vec<u32>,
    registered_buffer_free: Vec<u16>,
    stack_pool: StackPool,
    scope_state_pool: Vec<Rc<ScopeState>>,
    io_state_pool: Vec<Rc<IoState>>,
    completed_io: Vec<CompletedIo>,
    detached_io: Vec<DetachedIo>,
    in_flight_io: usize,
    io_counters: IoCounters,
    ring_enter_policy: RingEnterPolicy,
    busy_poll: BusyPoll,
    busy_poll_started: Instant,
    external_wake: Arc<ExternalWake>,
    external_wake_io: Option<Rc<IoState>>,
    #[cfg(test)]
    poll_ring_entries: usize,
    #[cfg(test)]
    wait_ring_entries: usize,
}

impl Scheduler {
    fn new(config: RuntimeConfig) -> Self {
        let ring = IoUring::builder()
            .build(config.ring_entries)
            .expect("failed to create io_uring");
        let mut probe = Probe::new();
        let _ = ring.submitter().register_probe(&mut probe);
        if config.registered_file_slots != 0 {
            ring.submitter()
                .register_files_sparse(config.registered_file_slots)
                .expect("failed to register io_uring fixed-file table");
        }
        if config.registered_buffer_slots != 0 {
            ring.submitter()
                .register_buffers_sparse(u32::from(config.registered_buffer_slots))
                .expect("failed to register io_uring fixed-buffer table");
        }

        Self {
            tasks: Vec::new(),
            task_generations: Vec::new(),
            free_tasks: Vec::new(),
            wait_tokens: Vec::new(),
            free_wait_tokens: Vec::new(),
            queued: Vec::new(),
            ready: VecDeque::new(),
            ring,
            probe,
            registered_file_free: (0..config.registered_file_slots).rev().collect(),
            registered_buffer_free: (0..config.registered_buffer_slots).rev().collect(),
            stack_pool: StackPool::new(config.max_cached_stacks),
            scope_state_pool: Vec::with_capacity(DEFAULT_SCOPE_STATE_CACHE_CAPACITY),
            io_state_pool: Vec::with_capacity(config.ring_entries as usize),
            completed_io: Vec::with_capacity(config.ring_entries as usize),
            detached_io: Vec::new(),
            in_flight_io: 0,
            io_counters: IoCounters::default(),
            ring_enter_policy: config.ring_enter_policy,
            busy_poll: config.busy_poll,
            busy_poll_started: Instant::now(),
            external_wake: Arc::new(ExternalWake::new()),
            external_wake_io: None,
            #[cfg(test)]
            poll_ring_entries: 0,
            #[cfg(test)]
            wait_ring_entries: 0,
        }
    }

    fn reserve_task(&mut self) -> TaskKey {
        let slot = if let Some(slot) = self.free_tasks.pop() {
            slot
        } else {
            let slot = self.tasks.len();
            self.tasks.push(None);
            self.task_generations.push(0);
            self.queued.push(false);
            slot
        };

        assert!(
            self.tasks[slot].is_none(),
            "free task slot should not contain a task"
        );
        assert!(!self.queued[slot], "free task slot should not be queued");

        TaskKey {
            slot,
            generation: self.task_generations[slot],
        }
    }

    fn acquire_scope_state(&mut self) -> Rc<ScopeState> {
        self.scope_state_pool
            .pop()
            .unwrap_or_else(|| Rc::new(ScopeState::default()))
    }

    fn recycle_scope_state(&mut self, state: Rc<ScopeState>) {
        if Rc::strong_count(&state) == 1
            && self.scope_state_pool.len() < DEFAULT_SCOPE_STATE_CACHE_CAPACITY
        {
            state.reset_for_reuse();
            self.scope_state_pool.push(state);
        }
    }

    fn acquire_stack(&mut self, stack_size: usize) -> DefaultStack {
        self.stack_pool.acquire(stack_size)
    }

    fn insert_task(&mut self, id: TaskKey, task: Task) {
        assert!(self.is_current_task_key(id), "stale task key inserted");
        assert!(
            self.tasks[id.index()].is_none(),
            "task slot already occupied"
        );
        self.tasks[id.index()] = Some(task);
        self.schedule(id);
    }

    fn register_wait_token(&mut self) -> WaitToken {
        let slot = if let Some(slot) = self.free_wait_tokens.pop() {
            slot
        } else {
            let slot = self.wait_tokens.len();
            self.wait_tokens.push(WaitTokenSlot {
                generation: 0,
                active: false,
                leased: false,
            });
            slot
        };

        let entry = &mut self.wait_tokens[slot];
        debug_assert!(!entry.leased, "free wait token slot is still leased");
        entry.active = true;
        entry.leased = true;
        WaitToken {
            slot,
            generation: entry.generation,
        }
    }

    fn is_wait_token_active(&self, token: WaitToken) -> bool {
        self.wait_tokens
            .get(token.slot)
            .is_some_and(|entry| entry.generation == token.generation && entry.active)
    }

    fn take_wait_token(&mut self, token: WaitToken) -> bool {
        let Some(entry) = self.wait_tokens.get_mut(token.slot) else {
            return false;
        };
        if entry.generation != token.generation || !entry.active {
            return false;
        }
        entry.active = false;
        true
    }

    fn release_wait_token(&mut self, token: WaitToken) {
        let Some(entry) = self.wait_tokens.get_mut(token.slot) else {
            return;
        };
        if entry.generation != token.generation || !entry.leased {
            return;
        }
        entry.active = false;
        entry.leased = false;
        entry.generation = entry.generation.wrapping_add(1);
        self.free_wait_tokens.push(token.slot);
    }

    fn schedule(&mut self, id: TaskKey) -> bool {
        if self
            .tasks
            .get(id.index())
            .and_then(Option::as_ref)
            .is_some()
            && self.is_current_task_key(id)
            && !self.queued[id.index()]
        {
            self.queued[id.index()] = true;
            self.ready.push_back(id);
            true
        } else {
            false
        }
    }

    fn pop_ready(&mut self) -> Option<(TaskKey, Task)> {
        while let Some(id) = self.ready.pop_front() {
            if !self.is_current_task_key(id) {
                continue;
            }
            self.queued[id.index()] = false;
            if let Some(task) = self.tasks[id.index()].take() {
                return Some((id, task));
            }
        }

        None
    }

    fn ready_len(&self) -> usize {
        self.ready.len()
    }

    fn has_ready_task(&self, task_ids: &[TaskKey]) -> bool {
        task_ids
            .iter()
            .copied()
            .any(|id| self.is_current_task_key(id) && self.queued[id.index()])
    }

    #[cfg(test)]
    fn task_slot_len(&self) -> usize {
        self.tasks.len()
    }

    #[cfg(test)]
    fn cached_stack_count(&self) -> usize {
        self.stack_pool.len()
    }

    #[cfg(test)]
    fn reused_stack_count(&self) -> usize {
        self.stack_pool.reused()
    }

    fn is_current_task_key(&self, id: TaskKey) -> bool {
        self.task_generations
            .get(id.index())
            .is_some_and(|generation| *generation == id.generation)
    }

    fn task_ref(&self, id: TaskKey) -> Option<&Task> {
        if self.is_current_task_key(id) {
            self.tasks.get(id.index()).and_then(Option::as_ref)
        } else {
            None
        }
    }

    fn take_task(&mut self, id: TaskKey) -> Option<Task> {
        if self.is_current_task_key(id) {
            self.tasks.get_mut(id.index()).and_then(Option::take)
        } else {
            None
        }
    }

    fn restore_task(&mut self, id: TaskKey, task: Task) {
        if self.is_current_task_key(id) {
            self.tasks[id.index()] = Some(task);
        }
    }

    fn finish_task(&mut self, id: TaskKey, task: Task) {
        if !self.is_current_task_key(id) {
            return;
        }

        let slot = id.index();
        self.tasks[slot] = None;
        self.queued[slot] = false;
        self.task_generations[slot] = self.task_generations[slot].wrapping_add(1);
        self.free_tasks.push(slot);
        self.stack_pool.recycle(task.into_finished_stack());
    }

    fn is_empty(&self) -> bool {
        self.tasks.iter().all(Option::is_none)
    }

    fn dump_stacks(&mut self) -> StackDump {
        let captures = Rc::new(RefCell::new(Vec::new()));
        let mut dump = StackDump::default();

        for (slot, task) in self.tasks.iter().enumerate() {
            if let Some(task) = task
                && !task.coroutine.started()
            {
                let id = TaskKey {
                    slot,
                    generation: self.task_generations[slot],
                };
                dump.tasks.push(TaskStackDump::not_started(id, task.stack));
            }
        }

        self.interrupt_started_tasks(|_| {
            let task_captures = Rc::clone(&captures);
            Box::new(move |interrupt| {
                task_captures
                    .borrow_mut()
                    .push(TaskStackDump::suspended(interrupt));
            })
        });
        dump.tasks.extend(captures.borrow_mut().drain(..));
        dump.tasks.sort_by_key(TaskStackDump::task_id);

        dump
    }

    fn interrupt_started_tasks<F>(&mut self, interrupt: F)
    where
        F: FnMut(TaskKey) -> TaskInterruptFn,
    {
        let ids = self
            .tasks
            .iter()
            .enumerate()
            .filter_map(|(slot, task)| {
                task.as_ref().map(|_| TaskKey {
                    slot,
                    generation: self.task_generations[slot],
                })
            })
            .collect::<Vec<_>>();
        self.interrupt_tasks(ids, interrupt);
    }

    fn interrupt_tasks<I, F>(&mut self, ids: I, mut interrupt: F)
    where
        I: IntoIterator<Item = TaskKey>,
        F: FnMut(TaskKey) -> TaskInterruptFn,
    {
        for id in ids {
            let Some(task) = self.task_ref(id) else {
                continue;
            };
            if !task.coroutine.started() {
                continue;
            }

            let interrupt = interrupt(id);
            let Some(mut task) = self.take_task(id) else {
                continue;
            };

            let result = task.coroutine.resume(ResumeAction::Interrupt(interrupt));
            match result {
                CoroutineResult::Yield(_) => {
                    self.restore_task(id, task);
                }
                CoroutineResult::Return(()) => {
                    self.finish_task(id, task);
                }
            }
        }
    }

    fn acquire_io_state(&mut self) -> Rc<IoState> {
        let state = self
            .io_state_pool
            .pop()
            .unwrap_or_else(|| Rc::new(IoState::new()));
        state.reset_for_reuse();
        state
    }

    fn recycle_io_state(&mut self, state: Rc<IoState>) {
        if Rc::strong_count(&state) == 1 {
            state.reset_for_reuse();
            self.io_state_pool.push(state);
        }
    }

    fn detach_io_state(
        &mut self,
        state: Rc<IoState>,
        cancel_kind: Option<IoCancelKind>,
        buffer: Option<DetachedBuffer>,
    ) {
        let cancel_state =
            cancel_kind.and_then(|cancel_kind| self.submit_cancel(&state, cancel_kind));
        self.io_counters.detached_total += 1;
        self.detached_io
            .push(DetachedIo::new(state, cancel_state, buffer));
    }

    fn take_ready_detached_io(&mut self) -> Vec<DetachedIo> {
        let mut ready = Vec::new();
        let mut index = 0;
        while index < self.detached_io.len() {
            if self.detached_io[index].is_ready() {
                ready.push(self.detached_io.swap_remove(index));
            } else {
                index += 1;
            }
        }
        ready
    }

    fn io_counters(&self) -> IoCounters {
        IoCounters {
            detached_pending: self.detached_io.len(),
            ..self.io_counters
        }
    }

    fn pool_diagnostics(&self) -> PoolDiagnostics {
        PoolDiagnostics {
            task_slots: self.tasks.len(),
            task_slot_capacity: self.tasks.capacity(),
            free_task_slots: self.free_tasks.len(),
            wait_token_slots: self.wait_tokens.len(),
            wait_token_slot_capacity: self.wait_tokens.capacity(),
            free_wait_token_slots: self.free_wait_tokens.len(),
            cached_stacks: self.stack_pool.stacks.len(),
            max_cached_stacks: self.stack_pool.capacity,
            cached_scope_states: self.scope_state_pool.len(),
            max_cached_scope_states: DEFAULT_SCOPE_STATE_CACHE_CAPACITY,
            cached_io_states: self.io_state_pool.len(),
            io_state_pool_capacity: self.io_state_pool.capacity(),
            completion_scratch_len: self.completed_io.len(),
            completion_scratch_capacity: self.completed_io.capacity(),
            detached_pending_io: self.detached_io.len(),
            registered_file_free_slots: self.registered_file_free.len(),
            registered_buffer_free_slots: self.registered_buffer_free.len(),
        }
    }

    fn record_io_completion(&mut self, state: &IoState) {
        if state.is_cancel_operation() {
            self.io_counters.cancel_completions += 1;
            if state.result().is_some_and(|result| result.is_err()) {
                self.io_counters.cancel_completion_errors += 1;
            }
        } else if state.cancel_requested() {
            self.io_counters.original_completions_after_cancel += 1;
        }
    }

    fn supports_opcode(&self, opcode: IoringOp) -> bool {
        self.probe.is_supported(opcode)
    }

    fn submit_io_state(&mut self, entry: impl Into<Sqe>, state: Rc<IoState>) -> io_uring_user_data {
        let user_data = io_state_user_data(&state);
        let _ = Rc::into_raw(Rc::clone(&state));
        let entry = entry.into().user_data(user_data);
        self.submit_io(&entry);
        user_data
    }

    fn submit_io_state_with_link_timeout(
        &mut self,
        entry: impl Into<Sqe>,
        state: Rc<IoState>,
        duration: Duration,
    ) -> io_uring_user_data {
        assert!(
            self.supports_opcode(opcode::LinkTimeout::CODE),
            "io_uring link timeout opcode is not supported by this kernel"
        );
        let user_data = io_state_user_data(&state);
        let _ = Rc::into_raw(Rc::clone(&state));
        state.set_timeout(Timespec::from(duration));
        state.mark_linked_timeout_operation();
        let entries = [
            entry.into().user_data(user_data).flags(Flags::IO_LINK),
            state.link_timeout_entry(),
        ];
        self.submit_linked_io(&entries);
        user_data
    }

    fn submit_cancel(
        &mut self,
        state: &Rc<IoState>,
        cancel_kind: IoCancelKind,
    ) -> Option<Rc<IoState>> {
        if state.is_ready() || !state.mark_cancel_requested() {
            return None;
        }

        if !cancel_kind.is_supported(self) {
            self.io_counters.unsupported_cancels += 1;
            return None;
        }

        self.io_counters.cancel_requests += 1;
        let cancel_state = self.acquire_io_state();
        cancel_state.mark_cancel_operation();
        let entry = cancel_kind.build();
        self.submit_io_state(entry, Rc::clone(&cancel_state));
        Some(cancel_state)
    }

    fn external_wake(&self) -> Arc<ExternalWake> {
        Arc::clone(&self.external_wake)
    }

    fn has_external_io_wake_interest(&self) -> bool {
        self.external_wake.has_io_wake_interest()
    }

    fn drain_external_ready(&mut self) -> bool {
        let ready = self.external_wake.take_ready();
        let mut scheduled = false;
        for ready in ready {
            if ready.registration.is_ready() {
                scheduled |= self.schedule(ready.task_id);
            }
        }
        scheduled
    }

    fn prepare_external_wake(&mut self) -> bool {
        let finished_wake_io = self.finish_external_wake_io();
        let scheduled = self.drain_external_ready();
        finished_wake_io || scheduled
    }

    fn take_external_driver_wakes(&mut self) -> bool {
        self.external_wake.take_driver_wakes()
    }

    fn arm_external_wake_io(&mut self) -> bool {
        if self.external_wake_io.is_some()
            || !self.external_wake.has_io_wake_interest()
            || !self.supports_opcode(opcode::PollAdd::CODE)
        {
            return false;
        }

        self.external_wake.drain_signal();
        if self.external_wake.take_driver_wakes() || self.drain_external_ready() {
            return true;
        }

        let state = self.acquire_io_state();
        let fd = self.external_wake.wake_fd().as_fd().as_raw_fd();
        let entry = opcode::PollAdd::new(Fd(fd), u32::from(PollFlags::IN.bits())).build();
        self.submit_io_state(entry, Rc::clone(&state));
        self.external_wake_io = Some(state);
        false
    }

    fn finish_external_wake_io(&mut self) -> bool {
        let Some(state) = self.external_wake_io.as_ref() else {
            return false;
        };
        if !state.is_ready() {
            return false;
        }

        let state = self
            .external_wake_io
            .take()
            .expect("external wake io state missing");
        let _ = state.take_result();
        self.external_wake.drain_signal();
        self.recycle_io_state(state);
        true
    }

    fn register_fixed_fd(&mut self, fd: &OwnedFd) -> Result<u32, Errno> {
        let slot = self.registered_file_free.pop().ok_or(Errno::NOBUFS)?;
        let raw_fd = fd.as_fd().as_raw_fd();
        match self.ring.submitter().register_files_update(slot, &[raw_fd]) {
            Ok(()) => Ok(slot),
            Err(error) => {
                self.registered_file_free.push(slot);
                Err(error)
            }
        }
    }

    fn unregister_fixed_fd(&mut self, slot: u32) {
        let _ = self.ring.submitter().register_files_update(slot, &[-1]);
        self.registered_file_free.push(slot);
    }

    fn register_fixed_buffer(&mut self, ptr: *mut u8, len: usize) -> Result<u16, Errno> {
        let slot = self.registered_buffer_free.pop().ok_or(Errno::NOBUFS)?;
        let iovec = IoVec {
            iov_base: ptr.cast(),
            iov_len: len,
        };
        let update = unsafe {
            self.ring
                .submitter()
                .register_buffers_update(u32::from(slot), &[iovec], None)
        };
        match update {
            Ok(()) => Ok(slot),
            Err(error) => {
                self.registered_buffer_free.push(slot);
                Err(error)
            }
        }
    }

    fn unregister_fixed_buffer(&mut self, slot: u16) {
        let iovec = IoVec {
            iov_base: std::ptr::null_mut(),
            iov_len: 0,
        };
        let _ = unsafe {
            self.ring
                .submitter()
                .register_buffers_update(u32::from(slot), &[iovec], None)
        };
        self.registered_buffer_free.push(slot);
    }

    fn submit_io(&mut self, entry: &Sqe) {
        let push_result = unsafe { self.ring.submission().push(entry) };
        if push_result.is_err() {
            self.submit_and_wait(0);
            unsafe { self.ring.submission().push(entry) }
                .expect("failed to push io_uring SQE after submitting pending entries");
        }

        self.in_flight_io += 1;
    }

    fn submit_linked_io(&mut self, entries: &[Sqe; 2]) {
        let push_result = unsafe { self.ring.submission().push_multiple(entries) };
        if push_result.is_err() {
            self.submit_and_wait(0);
            unsafe { self.ring.submission().push_multiple(entries) }
                .expect("failed to push linked io_uring SQEs after submitting pending entries");
        }

        self.in_flight_io += entries.len();
    }

    fn submit_and_wait(&self, want: usize) -> usize {
        match self.ring.submitter().submit_and_wait(want) {
            Ok(submitted) => submitted,
            Err(Errno::AGAIN | Errno::BUSY | Errno::INTR) => 0,
            Err(error) => panic!("error submitting or waiting for io_uring work: {error:?}"),
        }
    }

    fn should_enter(&mut self, want: usize) -> bool {
        if want > 0 {
            return true;
        }

        let submission = self.ring.submission();
        !submission.is_empty() || submission.cq_overflow()
    }

    fn enter_ring(&mut self, want: usize) -> usize {
        #[cfg(test)]
        if want == 0 {
            self.poll_ring_entries += 1;
        } else {
            self.wait_ring_entries += 1;
        }

        let submitted = if self.should_enter(want) {
            self.submit_and_wait(want)
        } else {
            0
        };

        for cqe in self.ring.completion() {
            self.in_flight_io = self
                .in_flight_io
                .checked_sub(1)
                .expect("io_uring in-flight count underflow");
            let state = cqe.user_data_ptr() as *const IoState;
            if state.is_null() {
                continue;
            }
            self.completed_io.push(CompletedIo {
                state,
                result: cqe.result(),
            });
        }

        submitted
    }

    fn should_busy_poll_io(&self) -> bool {
        match self.busy_poll {
            BusyPoll::Never => false,
            BusyPoll::Always => true,
            BusyPoll::Until(duration) => self.busy_poll_started.elapsed() <= duration,
        }
    }

    #[cfg(test)]
    fn ring_enter_counts(&self) -> (usize, usize) {
        (self.poll_ring_entries, self.wait_ring_entries)
    }
}

struct CompletedIo {
    state: *const IoState,
    result: KernelIoResult,
}

#[derive(Clone, Copy)]
enum IoCancelKind {
    Async(io_uring_user_data),
    Timeout(io_uring_user_data),
}

impl IoCancelKind {
    fn is_supported(self, scheduler: &Scheduler) -> bool {
        match self {
            Self::Async(_) => scheduler.supports_opcode(opcode::AsyncCancel::CODE),
            Self::Timeout(_) => scheduler.supports_opcode(opcode::TimeoutRemove::CODE),
        }
    }

    fn build(self) -> Sqe {
        match self {
            Self::Async(user_data) => opcode::AsyncCancel::new(user_data).build().into(),
            Self::Timeout(user_data) => opcode::TimeoutRemove::new(user_data).build().into(),
        }
    }
}

fn io_state_user_data(state: &Rc<IoState>) -> io_uring_user_data {
    io_uring_user_data::from_ptr(Rc::as_ptr(state).cast_mut().cast())
}

fn drain_scheduler_io(core: &Rc<SchedulerCell>) {
    while core.borrow().in_flight_io != 0 {
        assert!(
            drive_scheduler(core),
            "kimojio-stack runtime deadlocked while draining io_uring operations"
        );
    }
}

fn drive_scheduler(core: &Rc<SchedulerCell>) -> bool {
    {
        let mut scheduler = core.borrow_mut();
        scheduler.prepare_external_wake();
    }

    let (ready_len, ring_enter_policy) = {
        let scheduler = core.borrow();
        (scheduler.ready_len(), scheduler.ring_enter_policy)
    };

    if ready_len != 0 {
        let mut ran_task = false;
        for _ in 0..ring_enter_policy.ready_budget(ready_len) {
            ran_task |= run_one(core);
        }

        return run_completed_io(core, false) || ran_task;
    }

    let (should_wait_for_io, busy_poll_io) = {
        let scheduler = core.borrow();
        (scheduler.in_flight_io != 0, scheduler.should_busy_poll_io())
    };
    if should_wait_for_io {
        if core.borrow().has_external_io_wake_interest() {
            if run_completed_io(core, false) {
                let mut scheduler = core.borrow_mut();
                scheduler.finish_external_wake_io();
                scheduler.drain_external_ready();
                return true;
            }

            {
                let mut scheduler = core.borrow_mut();
                if scheduler.prepare_external_wake() {
                    return true;
                }
                if scheduler.take_external_driver_wakes() {
                    return true;
                }
                if scheduler.arm_external_wake_io() {
                    return true;
                }
            }
            run_completed_io(core, !busy_poll_io);
            let scheduled = {
                let mut scheduler = core.borrow_mut();
                let finished_wake_io = scheduler.finish_external_wake_io();
                let driver_wake = scheduler.take_external_driver_wakes();
                let scheduled = scheduler.drain_external_ready();
                finished_wake_io || driver_wake || scheduled
            };
            let completed = run_completed_io(core, false);
            scheduled || completed || core.borrow().in_flight_io != 0
        } else {
            run_completed_io(core, !busy_poll_io);
            true
        }
    } else {
        if run_completed_io(core, false) {
            return true;
        }

        let external_wake = core.borrow().external_wake();
        if external_wake.has_waiters_or_ready() {
            external_wake.wait_for_ready(None);
            let mut scheduler = core.borrow_mut();
            scheduler.finish_external_wake_io();
            scheduler.drain_external_ready()
        } else {
            false
        }
    }
}

fn run_one(core: &Rc<SchedulerCell>) -> bool {
    let Some((id, mut task)) = core.borrow_mut().pop_ready() else {
        return false;
    };

    let result = task.coroutine.resume(ResumeAction::Run);
    let mut scheduler = core.borrow_mut();
    match result {
        CoroutineResult::Yield(Suspend::Ready) => {
            scheduler.restore_task(id, task);
            scheduler.schedule(id);
        }
        CoroutineResult::Yield(Suspend::Parked) => {
            scheduler.restore_task(id, task);
        }
        CoroutineResult::Return(()) => {
            scheduler.finish_task(id, task);
        }
    }

    true
}

fn interrupt_scheduler_tasks<I, F>(core: &Rc<SchedulerCell>, ids: I, mut interrupt: F)
where
    I: IntoIterator<Item = TaskKey>,
    F: FnMut(TaskKey) -> TaskInterruptFn,
{
    for id in ids {
        let Some(mut task) = ({
            let mut scheduler = core.borrow_mut();
            let Some(task) = scheduler.task_ref(id) else {
                continue;
            };
            if !task.coroutine.started() {
                continue;
            }
            scheduler.take_task(id)
        }) else {
            continue;
        };

        let result = task
            .coroutine
            .resume(ResumeAction::Interrupt(interrupt(id)));
        let mut scheduler = core.borrow_mut();
        match result {
            CoroutineResult::Yield(_) => {
                scheduler.restore_task(id, task);
            }
            CoroutineResult::Return(()) => {
                scheduler.finish_task(id, task);
            }
        }
    }
}

fn suspend_task(
    id: TaskKey,
    yielder: &Yielder<ResumeAction, Suspend>,
    stack: StackTracker,
    suspend: Suspend,
) {
    let action = suspend_with_current_context_cleared(yielder, suspend);
    handle_resume_action(id, yielder, stack, suspend, action);
}

fn handle_resume_action(
    id: TaskKey,
    yielder: &Yielder<ResumeAction, Suspend>,
    stack: StackTracker,
    suspend: Suspend,
    mut action: ResumeAction,
) {
    loop {
        match action {
            ResumeAction::Run => return,
            ResumeAction::Interrupt(interrupt) => {
                with_current_context_cleared(|| interrupt(TaskInterrupt::new(id, stack)));
                action = suspend_with_current_context_cleared(yielder, suspend);
            }
        }
    }
}

fn run_completed_io(core: &Rc<SchedulerCell>, wait: bool) -> bool {
    let (submitted, mut completed) = {
        let mut scheduler = core.borrow_mut();
        if wait {
            debug_assert!(
                scheduler.ready.is_empty(),
                "io_uring wait requested while tasks are ready"
            );
            debug_assert_ne!(
                scheduler.in_flight_io, 0,
                "io_uring wait requested with no in-flight IO"
            );
        }

        let submitted = scheduler.enter_ring(usize::from(wait));
        let completed = std::mem::take(&mut scheduler.completed_io);
        (submitted, completed)
    };

    let had_completion = !completed.is_empty();
    for completion in completed.drain(..) {
        let state = unsafe { Rc::from_raw(completion.state) };
        state.complete(completion.result);
        core.borrow_mut().record_io_completion(&state);
        if Rc::strong_count(&state) == 1 {
            recycle_io_state(&Rc::downgrade(core), state);
        }
    }

    let ready_detached = if had_completion {
        let mut scheduler = core.borrow_mut();
        scheduler.completed_io = completed;
        scheduler.take_ready_detached_io()
    } else {
        core.borrow_mut().completed_io = completed;
        Vec::new()
    };

    let core = Rc::downgrade(core);
    for detached in ready_detached {
        detached.reap(&core);
    }

    submitted != 0 || had_completion
}

fn detach_io_state(
    core: &Weak<SchedulerCell>,
    state: Rc<IoState>,
    cancel_kind: Option<IoCancelKind>,
    buffer: Option<DetachedBuffer>,
) {
    if let Some(core) = core.upgrade() {
        core.borrow_mut()
            .detach_io_state(state, cancel_kind, buffer);
    }
}

fn recycle_io_state(core: &Weak<SchedulerCell>, state: Rc<IoState>) {
    if let Some(core) = core.upgrade() {
        core.borrow_mut().recycle_io_state(state);
    }
}

#[cfg(test)]
mod allocation_tracking {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    thread_local! {
        static ACTIVE: Cell<bool> = const { Cell::new(false) };
    }

    static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
    static ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);
    static DEALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
    static DEALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);
    static REALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
    static REALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);
    static MEASUREMENT_LOCK: StdMutex<()> = StdMutex::new(());

    pub struct CountingAllocator;

    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct AllocationCounts {
        pub allocations: usize,
        pub allocated_bytes: usize,
        pub deallocations: usize,
        pub deallocated_bytes: usize,
        pub reallocations: usize,
        pub reallocated_bytes: usize,
    }

    impl AllocationCounts {
        pub fn allocating_operations(self) -> usize {
            self.allocations + self.reallocations
        }

        pub fn allocated_or_reallocated_bytes(self) -> usize {
            self.allocated_bytes + self.reallocated_bytes
        }
    }

    pub fn measure<T>(f: impl FnOnce() -> T) -> (T, AllocationCounts) {
        let _measurement = MEASUREMENT_LOCK.lock().unwrap();

        ACTIVE.with(|active| {
            assert!(!active.get(), "nested allocation measurement");
        });
        reset();

        ACTIVE.with(|active| active.set(true));
        let guard = MeasurementGuard;
        let output = f();
        drop(guard);

        (output, current())
    }

    fn reset() {
        ALLOCATIONS.store(0, Ordering::Relaxed);
        ALLOCATED_BYTES.store(0, Ordering::Relaxed);
        DEALLOCATIONS.store(0, Ordering::Relaxed);
        DEALLOCATED_BYTES.store(0, Ordering::Relaxed);
        REALLOCATIONS.store(0, Ordering::Relaxed);
        REALLOCATED_BYTES.store(0, Ordering::Relaxed);
    }

    fn current() -> AllocationCounts {
        AllocationCounts {
            allocations: ALLOCATIONS.load(Ordering::Relaxed),
            allocated_bytes: ALLOCATED_BYTES.load(Ordering::Relaxed),
            deallocations: DEALLOCATIONS.load(Ordering::Relaxed),
            deallocated_bytes: DEALLOCATED_BYTES.load(Ordering::Relaxed),
            reallocations: REALLOCATIONS.load(Ordering::Relaxed),
            reallocated_bytes: REALLOCATED_BYTES.load(Ordering::Relaxed),
        }
    }

    fn record_allocation(bytes: usize) {
        ACTIVE.with(|active| {
            if active.get() {
                ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
                ALLOCATED_BYTES.fetch_add(bytes, Ordering::Relaxed);
            }
        });
    }

    fn record_deallocation(bytes: usize) {
        ACTIVE.with(|active| {
            if active.get() {
                DEALLOCATIONS.fetch_add(1, Ordering::Relaxed);
                DEALLOCATED_BYTES.fetch_add(bytes, Ordering::Relaxed);
            }
        });
    }

    fn record_reallocation(bytes: usize) {
        ACTIVE.with(|active| {
            if active.get() {
                REALLOCATIONS.fetch_add(1, Ordering::Relaxed);
                REALLOCATED_BYTES.fetch_add(bytes, Ordering::Relaxed);
            }
        });
    }

    struct MeasurementGuard;

    impl Drop for MeasurementGuard {
        fn drop(&mut self) {
            ACTIVE.with(|active| active.set(false));
        }
    }

    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            record_allocation(layout.size());
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            record_allocation(layout.size());
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            record_deallocation(layout.size());
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            record_reallocation(new_size);
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AddressFamily, AtFlags, BusyPoll, Cancellable, DEFAULT_STACK_SIZE, EpollCtlOp, EpollEvent,
        EpollEventData, EpollEventFlags, Errno, FallocateFlags, FileAdvice, FutexWaitFlags, IoFd,
        IoJoinError, IoReadBuffer, IoRuntime, IoVec, MemoryAdvice, Mode, MsgHdr, OFlags, RecvFlags,
        RenameFlags, ResolveFlags, Runtime, RuntimeCapabilities, RuntimeCapability, RuntimeConfig,
        RuntimeContext, RuntimeFamily, RuntimeIoError, RuntimeWaitError, RuntimeWaitable,
        RuntimeWaitableAdapter, SendFlags, Shutdown, SocketIoRuntime, SocketType,
        StackfulWaitContext, StackfulWaiterHandle, StackfulWaiters, StatxFlags, TaskStackState,
        UringSpliceFlags, WaitError, WaitRegistration, Waitable, Waiters, allocation_tracking,
        ipproto, once,
    };
    use rustix::event::{PollFlags, epoll};
    use rustix::fd::AsFd;
    use rustix::mm::{MapFlags, ProtFlags};
    use rustix::net;
    use rustix::pipe::pipe;
    use std::cell::{Cell, RefCell};
    use std::ffi::c_void;
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
    use std::ptr;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    fn send_all(cx: &RuntimeContext<'_>, fd: &impl AsFd, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let written = cx.send(fd, bytes).unwrap();
            assert!(written != 0);
            bytes = &bytes[written..];
        }
    }

    fn recv_to_end(cx: &RuntimeContext<'_>, fd: &impl AsFd, expected_len: usize) -> Vec<u8> {
        let mut output = Vec::with_capacity(expected_len);
        let mut buffer = [0_u8; 3];

        loop {
            let read = cx.recv(fd, &mut buffer).unwrap();
            if read == 0 {
                return output;
            }
            output.extend_from_slice(&buffer[..read]);
        }
    }

    trait TestRuntimeSocketExt {
        fn as_stack_io_fd_for_test(&self) -> Option<&IoFd>;
    }

    impl TestRuntimeSocketExt for IoFd {
        fn as_stack_io_fd_for_test(&self) -> Option<&IoFd> {
            Some(self)
        }
    }

    #[test]
    fn runtime_context_reports_explicit_capabilities() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            assert_eq!(cx.runtime_family(), RuntimeFamily::Stack);
            assert!(cx.supports(RuntimeCapability::StackfulWait));
            assert!(cx.supports(RuntimeCapability::SocketIo));
            assert!(!cx.supports(RuntimeCapability::ExplicitRingIo));
            cx.require_socket_io().unwrap();

            let error = cx
                .require_capability(RuntimeCapability::ExplicitRingIo)
                .unwrap_err();
            assert_eq!(error.capability(), "explicit-ring-io");
            assert_eq!(error.family(), RuntimeFamily::Stack);
        });
    }

    struct NoSocketIoRuntime;

    struct NoSocketSleep;

    impl RuntimeWaitable for NoSocketSleep {
        fn is_ready(&self) -> bool {
            true
        }

        fn add_stackful_waiter(&self, _waiter: StackfulWaiterHandle) -> bool {
            false
        }
    }

    impl RuntimeCapabilities for NoSocketIoRuntime {
        fn runtime_family(&self) -> RuntimeFamily {
            RuntimeFamily::Other("no-socket-io-test")
        }

        fn supports(&self, capability: RuntimeCapability) -> bool {
            matches!(capability, RuntimeCapability::StackfulWait)
        }
    }

    impl IoRuntime for NoSocketIoRuntime {
        type Sleep = NoSocketSleep;

        fn sleep_async(&self, _duration: Duration) -> Result<Self::Sleep, RuntimeIoError> {
            Ok(NoSocketSleep)
        }

        fn sleep_for(&self, _duration: Duration) -> Result<(), RuntimeIoError> {
            Ok(())
        }
    }

    #[test]
    fn socket_io_requirement_reports_unsupported_capability() {
        let runtime = NoSocketIoRuntime;

        let error = runtime.require_socket_io().unwrap_err();
        assert_eq!(error.capability(), "socket-io");
        assert_eq!(error.family(), RuntimeFamily::Other("no-socket-io-test"));
    }

    #[test]
    fn runtime_socket_io_contract_maps_to_stack_io() {
        let (read_fd, write_fd) = pipe().unwrap();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let read_fd = SocketIoRuntime::socket_from_owned_fd(cx, read_fd).unwrap();
            let write_fd = SocketIoRuntime::socket_from_owned_fd(cx, write_fd).unwrap();

            assert!(read_fd.as_stack_io_fd_for_test().is_some());

            assert_eq!(SocketIoRuntime::write(cx, &write_fd, b"hi").unwrap(), 2);

            let mut read_buf = [0_u8; 2];
            assert_eq!(
                SocketIoRuntime::read(cx, &read_fd, &mut read_buf).unwrap(),
                2
            );
            assert_eq!(&read_buf, b"hi");

            let read = SocketIoRuntime::read_async(cx, &read_fd, vec![0_u8; 2]).unwrap();
            let write = SocketIoRuntime::write_async(cx, &write_fd, b"ok".to_vec()).unwrap();

            let written = write.get(cx).unwrap();
            assert_eq!(written.bytes, 2);
            assert_eq!(written.buffer, b"ok");

            let read = read.get(cx).unwrap();
            assert_eq!(read.bytes, 2);
            assert_eq!(&read.buffer[..read.bytes], b"ok");

            SocketIoRuntime::close(cx, read_fd).unwrap();
            SocketIoRuntime::close(cx, write_fd).unwrap();
        });
    }

    struct DropCountingReadBuffer {
        bytes: Vec<u8>,
        drops: Rc<Cell<usize>>,
    }

    impl DropCountingReadBuffer {
        fn new(len: usize, drops: Rc<Cell<usize>>) -> Self {
            Self {
                bytes: vec![0; len],
                drops,
            }
        }
    }

    impl Drop for DropCountingReadBuffer {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
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

    fn drain_detached_io(cx: &RuntimeContext<'_>) {
        for _ in 0..32 {
            if cx.io_counters().detached_pending == 0 {
                return;
            }
            cx.nop().unwrap();
        }
        assert_eq!(cx.io_counters().detached_pending, 0);
    }

    struct ManualWait {
        ready: Cell<bool>,
        waiters: RefCell<Waiters>,
    }

    impl ManualWait {
        fn new() -> Self {
            Self {
                ready: Cell::new(false),
                waiters: RefCell::new(Waiters::default()),
            }
        }

        fn wait(&self, cx: &RuntimeContext<'_>) {
            loop {
                if self.ready.get() {
                    return;
                }

                let registration = cx.wait_registration();
                if let Some(waiter) = cx.waiter(&registration) {
                    self.waiters.borrow_mut().push(waiter);
                }
                cx.park();
            }
        }

        fn wake_one(&self) {
            self.ready.set(true);
            self.waiters.borrow_mut().wake_one();
        }

        fn waiter_slots(&self) -> usize {
            let waiters = self.waiters.borrow();
            usize::from(waiters.first.is_some()) + waiters.rest.len()
        }

        fn active_waiters(&self) -> usize {
            let waiters = self.waiters.borrow();
            waiters
                .first
                .as_ref()
                .is_some_and(|waiter| waiter.is_active()) as usize
                + waiters
                    .rest
                    .iter()
                    .filter(|waiter| waiter.is_active())
                    .count()
        }
    }

    impl Waitable for ManualWait {
        fn is_ready(&self) -> bool {
            self.ready.get()
        }

        fn add_waiter(&self, cx: &RuntimeContext<'_>, registration: &WaitRegistration) {
            if !self.ready.get()
                && let Some(waiter) = cx.waiter(registration)
            {
                self.waiters.borrow_mut().push(waiter);
            }
        }
    }

    struct ManualRuntimeWait {
        ready: Cell<bool>,
        waiters: RefCell<StackfulWaiters>,
        registrations: Cell<usize>,
    }

    impl ManualRuntimeWait {
        fn new(ready: bool) -> Self {
            Self {
                ready: Cell::new(ready),
                waiters: RefCell::new(StackfulWaiters::default()),
                registrations: Cell::new(0),
            }
        }

        fn wake_all(&self) {
            self.ready.set(true);
            self.waiters.borrow_mut().wake_all();
        }
    }

    impl RuntimeWaitable for ManualRuntimeWait {
        fn is_ready(&self) -> bool {
            self.ready.get()
        }

        fn add_stackful_waiter(&self, waiter: StackfulWaiterHandle) -> bool {
            self.registrations.set(self.registrations.get() + 1);
            if self.ready.get() {
                false
            } else {
                self.waiters.borrow_mut().push(waiter);
                true
            }
        }
    }

    struct ReadyDuringRuntimeRegistration {
        ready: Cell<bool>,
        registrations: Cell<usize>,
    }

    impl ReadyDuringRuntimeRegistration {
        fn new() -> Self {
            Self {
                ready: Cell::new(false),
                registrations: Cell::new(0),
            }
        }
    }

    impl RuntimeWaitable for ReadyDuringRuntimeRegistration {
        fn is_ready(&self) -> bool {
            self.ready.get()
        }

        fn add_stackful_waiter(&self, _waiter: StackfulWaiterHandle) -> bool {
            self.registrations.set(self.registrations.get() + 1);
            self.ready.set(true);
            false
        }
    }

    struct NoStackfulWaitContext;

    impl RuntimeCapabilities for NoStackfulWaitContext {
        fn runtime_family(&self) -> RuntimeFamily {
            RuntimeFamily::Other("no-stackful-wait-test")
        }

        fn supports(&self, _capability: RuntimeCapability) -> bool {
            false
        }
    }

    impl StackfulWaitContext for NoStackfulWaitContext {
        fn stackful_wait_registration(
            &self,
        ) -> Option<Box<dyn super::StackfulWaitRegistration + '_>> {
            None
        }

        fn park_stackful(&self) {
            panic!("unsupported context must not park");
        }
    }

    #[test]
    fn runtime_wait_ready_before_registration_does_not_require_stackful_context() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let waitable = ManualRuntimeWait::new(true);
            cx.wait_stackful(&waitable).unwrap();
            assert_eq!(waitable.registrations.get(), 0);
        });
    }

    #[test]
    fn runtime_wait_pending_root_context_reports_no_stackful_context() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let waitable = ManualRuntimeWait::new(false);
            assert_eq!(
                cx.wait_stackful(&waitable),
                Err(RuntimeWaitError::NoStackfulContext {
                    family: RuntimeFamily::Stack,
                })
            );
            assert_eq!(waitable.registrations.get(), 0);
        });
    }

    #[test]
    fn runtime_wait_pending_unsupported_context_reports_capability_error() {
        let waitable = ManualRuntimeWait::new(false);
        let context = NoStackfulWaitContext;

        let error = context.wait_stackful(&waitable).unwrap_err();
        match error {
            RuntimeWaitError::Unsupported(error) => {
                assert_eq!(error.capability(), "stackful-wait");
                assert_eq!(
                    error.family(),
                    RuntimeFamily::Other("no-stackful-wait-test")
                );
            }
            other => panic!("unexpected runtime wait error: {other:?}"),
        }
    }

    #[test]
    fn runtime_wait_parks_and_wakes_stackful_task() {
        let waitable = ManualRuntimeWait::new(false);
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let worker = scope.spawn(|cx| {
                    cx.wait_stackful(&waitable).unwrap();
                    42
                });

                while waitable.registrations.get() == 0 {
                    cx.yield_now();
                }
                waitable.wake_all();

                assert_eq!(worker.join(cx), 42);
            });
        });
    }

    #[test]
    fn runtime_neutral_waiter_creation_after_registration_is_allocation_free() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let worker = scope.spawn(|cx| {
                    let registration = cx
                        .stackful_wait_registration()
                        .expect("spawned task should provide stackful wait registration");

                    let (_, counts) = allocation_tracking::measure(|| {
                        for _ in 0..8 {
                            let waiter = registration.waiter();
                            std::hint::black_box(waiter.is_active());
                        }
                    });

                    assert_eq!(counts.allocating_operations(), 0, "{counts:?}");
                    assert_eq!(counts.allocated_or_reallocated_bytes(), 0, "{counts:?}");
                });

                worker.join(cx);
            });
        });
    }

    #[test]
    fn runtime_wait_any_rechecks_ready_before_registration_race() {
        let race = ReadyDuringRuntimeRegistration::new();
        let pending = ManualRuntimeWait::new(false);
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let worker = scope.spawn(|cx| cx.wait_any_stackful(&[&race, &pending]).unwrap());

                while pending.registrations.get() == 0 {
                    cx.yield_now();
                }

                pending.wake_all();
                assert_eq!(worker.join(cx), 0);
            });
        });
        assert_eq!(race.registrations.get(), 1);
    }

    #[test]
    fn runtime_waitable_adapter_preserves_stack_core_timeout_waits() {
        let waitable = ManualRuntimeWait::new(false);
        let adapter = RuntimeWaitableAdapter::new(&waitable);
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let waitables: [&dyn Waitable; 1] = [&adapter];
            assert_eq!(
                cx.wait_all(&waitables, Some(Duration::from_millis(1))),
                Err(WaitError::TimedOut)
            );
        });
    }

    #[test]
    fn runtime_waitable_adapter_wakes_ready_before_registration_race() {
        let race = ReadyDuringRuntimeRegistration::new();
        let adapter = RuntimeWaitableAdapter::new(&race);
        let pending = ManualWait::new();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let worker = scope.spawn(|cx| {
                    let waitables: [&dyn Waitable; 2] = [&adapter, &pending];
                    cx.wait_any(&waitables, Some(Duration::from_millis(10)))
                        .unwrap()
                });

                while pending.waiter_slots() == 0 {
                    cx.yield_now();
                }
                pending.wake_one();

                assert_eq!(worker.join(cx), 0);
            });
        });
        assert_eq!(race.registrations.get(), 1);
    }

    fn external_waiter_count(cx: &RuntimeContext<'_>) -> usize {
        let external_wake = { cx.core.borrow().external_wake() };
        external_wake.inner().waiters
    }

    fn unique_temp_dir(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "kimojio-stack-{name}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn runtime_context_addr(cx: &RuntimeContext<'_>) -> usize {
        (cx as *const RuntimeContext<'_>).cast::<()>() as usize
    }

    #[test]
    fn current_context_is_visible_only_while_runtime_code_executes() {
        assert!(RuntimeContext::with_current(|_| ()).is_none());

        let mut runtime = Runtime::new();
        let visible = runtime.block_on(|cx| {
            let root_addr = runtime_context_addr(cx);
            let root_current =
                RuntimeContext::with_current(|current| runtime_context_addr(&current));
            let child_current = cx.scope(|scope| {
                scope
                    .spawn(|cx| {
                        RuntimeContext::with_current(|current| runtime_context_addr(&current))
                            == Some(runtime_context_addr(cx))
                    })
                    .join(cx)
            });

            root_current == Some(root_addr) && child_current
        });

        assert!(visible);
        assert!(RuntimeContext::with_current(|_| ()).is_none());
    }

    #[test]
    fn current_context_is_cleared_while_coroutine_is_parked() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let (ready_tx, ready_rx) = once::channel();
                let (release_tx, release_rx) = once::channel();
                let sleeper = scope.spawn(move |cx| {
                    let sleeper_addr = runtime_context_addr(cx);
                    ready_tx.send(sleeper_addr).unwrap();
                    release_rx.recv(cx).unwrap();
                    RuntimeContext::with_current(|current| runtime_context_addr(&current))
                        == Some(sleeper_addr)
                });

                let sleeper_addr = ready_rx.recv(cx).unwrap();
                cx.yield_now();

                let observer = scope.spawn(move |cx| {
                    RuntimeContext::with_current(|current| runtime_context_addr(&current))
                        == Some(runtime_context_addr(cx))
                        && RuntimeContext::with_current(|current| runtime_context_addr(&current))
                            != Some(sleeper_addr)
                });
                assert!(observer.join(cx));

                release_tx.send(()).unwrap();
                assert!(sleeper.join(cx));
            });
        });
    }

    #[test]
    fn allocation_counter_reports_zero_for_noop() {
        let _ = allocation_tracking::measure(|| ());
        let (_, counts) = allocation_tracking::measure(|| std::hint::black_box(42));

        assert_eq!(counts.allocating_operations(), 0);
        assert_eq!(counts.allocated_or_reallocated_bytes(), 0);
    }

    #[test]
    fn allocation_counter_detects_vec_allocation() {
        let (_, counts) = allocation_tracking::measure(|| {
            let values = vec![1_u8];
            std::hint::black_box(values);
        });

        assert!(counts.allocating_operations() != 0);
        assert!(counts.allocated_or_reallocated_bytes() != 0);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn scheduler_cell_debug_borrows_are_released_on_drop() {
        let cell = super::SchedulerCell::new(super::Scheduler::new(RuntimeConfig::default()));

        {
            let _shared = cell.borrow();
            let _another_shared = cell.borrow();
        }

        {
            let _exclusive = cell.borrow_mut();
        }

        let _shared = cell.borrow();
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "already borrowed: BorrowMutError")]
    fn scheduler_cell_debug_rejects_mut_borrow_during_shared_borrow() {
        let cell = super::SchedulerCell::new(super::Scheduler::new(RuntimeConfig::default()));
        let _shared = cell.borrow();

        let _exclusive = cell.borrow_mut();
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "already mutably borrowed: BorrowError")]
    fn scheduler_cell_debug_rejects_shared_borrow_during_mut_borrow() {
        let cell = super::SchedulerCell::new(super::Scheduler::new(RuntimeConfig::default()));
        let _exclusive = cell.borrow_mut();

        let _shared = cell.borrow();
    }

    #[test]
    fn external_waiter_wakes_parked_stackful_task_from_os_thread() {
        let (waiter_tx, waiter_rx) = mpsc::channel::<super::ExternalWaiter>();
        let waker = thread::spawn(move || {
            let waiter = waiter_rx.recv().unwrap();
            waiter.wake();
        });
        let mut runtime = Runtime::new();

        let output = runtime.block_on(|cx| {
            cx.scope(|scope| {
                let worker = scope.spawn(|cx| {
                    let registration = cx
                        .external_wait_registration()
                        .expect("stackful coroutine should have an external waiter");
                    assert!(waiter_tx.send(registration.waiter()).is_ok());
                    cx.park();
                    42
                });

                worker.join(cx)
            })
        });

        waker.join().unwrap();
        assert_eq!(output, 42);
    }

    #[test]
    fn external_wake_delivered_before_park_is_observed() {
        let mut runtime = Runtime::new();

        let output = runtime.block_on(|cx| {
            cx.scope(|scope| {
                let worker = scope.spawn(|cx| {
                    let registration = cx
                        .external_wait_registration()
                        .expect("stackful coroutine should have an external waiter");
                    registration.waiter().wake();
                    cx.park();
                    7
                });

                worker.join(cx)
            })
        });

        assert_eq!(output, 7);
    }

    #[test]
    fn external_waiter_wakes_stackful_task_while_root_waits_in_io_uring() {
        let (waiter_tx, waiter_rx) = mpsc::channel::<super::ExternalWaiter>();
        let waker = thread::spawn(move || {
            let waiter = waiter_rx.recv().unwrap();
            waiter.wake();
        });
        let mut runtime = Runtime::new();

        let output = runtime.block_on(|cx| {
            let mut timeout = cx.timeout(Duration::from_secs(60));

            let output = cx.scope(|scope| {
                let worker = scope.spawn(|cx| {
                    let registration = cx
                        .external_wait_registration()
                        .expect("stackful coroutine should have an external waiter");
                    assert!(waiter_tx.send(registration.waiter()).is_ok());
                    cx.park();
                    99
                });

                worker.join(cx)
            });

            timeout.cancel();
            output
        });

        waker.join().unwrap();
        assert_eq!(output, 99);
    }

    #[test]
    fn scheduler_wake_interrupts_root_io_wait_without_external_waiter() {
        let (start_tx, start_rx) = mpsc::channel();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let wake = cx.internal_scheduler_wake();
            let waker_wake = wake.clone();
            let waker = thread::spawn(move || {
                start_rx.recv().unwrap();
                thread::sleep(Duration::from_millis(20));
                waker_wake.wake();
            });
            let timeout = cx.submit_no_payload_timeout(Duration::from_secs(1));
            let started = Instant::now();
            start_tx.send(()).unwrap();
            cx.yield_now();
            assert!(started.elapsed() < Duration::from_millis(500));
            timeout.cancel_and_wait(cx).unwrap();
            waker.join().unwrap();
        });
    }

    #[test]
    fn scheduler_wake_delivered_before_io_wait_is_observed() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let wake = cx.internal_scheduler_wake();
            let timeout = cx.submit_no_payload_timeout(Duration::from_secs(1));
            wake.wake();
            let started = Instant::now();
            cx.yield_now();
            assert!(started.elapsed() < Duration::from_millis(500));
            timeout.cancel_and_wait(cx).unwrap();
        });
    }

    #[test]
    fn no_payload_nop_and_timeout_complete() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.submit_no_payload_nop().wait(cx).unwrap();
            cx.submit_no_payload_timeout(Duration::ZERO)
                .wait(cx)
                .unwrap();
        });
    }

    #[test]
    fn linked_timeout_nop_completes_before_timeout() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            if !cx.supports_io_uring_opcode(rustix_uring::opcode::LinkTimeout::CODE) {
                return;
            }
            cx.submit_no_payload_nop_with_timeout(Duration::from_secs(1))
                .wait(cx)
                .unwrap();
        });
    }

    #[test]
    fn linked_timeout_maps_canceled_primary_to_timeout() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            if !cx.supports_io_uring_opcode(rustix_uring::opcode::LinkTimeout::CODE) {
                return;
            }
            let (read_fd, write_fd) = pipe().unwrap();
            let mut buffer = [0_u8; 1];
            let result = unsafe {
                cx.submit_raw_read_with_timeout(
                    &read_fd,
                    buffer.as_mut_ptr(),
                    buffer.len(),
                    Duration::from_millis(1),
                )
            }
            .wait(cx);
            assert_eq!(result, Err(Errno::TIME));
            cx.close(read_fd).unwrap();
            cx.close(write_fd).unwrap();
        });
    }

    #[test]
    fn no_payload_cancel_and_drop_reclaim_state() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let timeout = cx.submit_no_payload_timeout(Duration::from_secs(60));
            timeout.cancel_and_wait(cx).unwrap();

            let dropped = cx.submit_no_payload_timeout(Duration::from_secs(60));
            drop(dropped);
            drain_detached_io(cx);
            assert_eq!(cx.io_counters().detached_pending, 0);
        });
    }

    #[test]
    fn stale_external_waiter_does_not_wake_reused_task_slot() {
        let (waiter_tx, waiter_rx) = mpsc::channel::<super::ExternalWaiter>();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let first_tx = waiter_tx.clone();
                let first = scope.spawn(move |cx| {
                    let registration = cx
                        .external_wait_registration()
                        .expect("stackful coroutine should have an external waiter");
                    first_tx
                        .send(registration.waiter())
                        .expect("failed to send first waiter");
                    1
                });
                assert_eq!(first.join(cx), 1);
                let stale_waiter = waiter_rx.recv().expect("missing stale waiter");

                let second_tx = waiter_tx.clone();
                let second = scope.spawn(move |cx| {
                    let registration = cx
                        .external_wait_registration()
                        .expect("stackful coroutine should have an external waiter");
                    second_tx
                        .send(registration.waiter())
                        .expect("failed to send second waiter");
                    cx.park();
                    2
                });
                cx.yield_now();
                let live_waiter = waiter_rx.recv().expect("missing live waiter");
                assert!(!stale_waiter.wake());
                assert_eq!(second.try_join(), None);

                assert!(live_waiter.wake());
                assert_eq!(second.join(cx), 2);
            });
        });
    }

    #[test]
    fn canceled_parked_task_releases_external_waiter_count() {
        let (registered_tx, registered_rx) = mpsc::channel();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                scope.spawn(|cx| {
                    let _registration = cx
                        .external_wait_registration()
                        .expect("stackful coroutine should have an external waiter");
                    registered_tx.send(()).unwrap();
                    cx.park();
                });
                cx.yield_now();
                registered_rx.recv().unwrap();
                assert_eq!(external_waiter_count(cx), 1);
            });

            assert_eq!(external_waiter_count(cx), 0);
        });
    }

    #[test]
    fn scope_exit_observes_completed_timeout_before_canceling_child() {
        let completed = Rc::new(Cell::new(false));
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let completed = Rc::clone(&completed);
                scope.spawn(move |cx| {
                    cx.sleep(Duration::from_millis(25)).unwrap();
                    completed.set(true);
                });

                cx.yield_now();
                cx.nop().unwrap();
                thread::sleep(Duration::from_millis(75));
            });
        });

        assert!(completed.get());
    }

    #[test]
    fn once_channel_between_stackful_threads() {
        let mut runtime = Runtime::new();

        let output = runtime.block_on(|cx| {
            let (tx, rx) = once::channel();

            cx.scope(|scope| {
                let receiver = scope.spawn(move |cx| rx.recv(cx).unwrap() + 1);
                let sender = scope.spawn(move |_| {
                    tx.send(41).unwrap();
                    "sent"
                });

                assert_eq!(sender.join(cx), "sent");
                receiver.join(cx)
            })
        });

        assert_eq!(output, 42);
    }

    #[test]
    fn spawn_handle_and_block_on_return_values() {
        let mut runtime = Runtime::with_stack_size(32 * 1024);

        let output = runtime.block_on(|cx| {
            let base = 10;
            cx.scope(|scope| {
                let first = scope.spawn(|_| base + 1);
                let second = scope.spawn(|_| 30);

                first.join(cx) + second.join(cx)
            })
        });

        assert_eq!(output, 41);
    }

    #[test]
    fn completed_task_slots_are_reused_after_spawn_churn() {
        let mut runtime = Runtime::new();

        let slots = runtime.block_on(|cx| {
            for i in 0..64 {
                let output = cx.scope(|scope| {
                    let worker = scope.spawn(move |_| i);
                    worker.join(cx)
                });
                assert_eq!(output, i);
            }

            cx.scheduler_task_slots()
        });

        assert_eq!(slots, 1);
    }

    #[test]
    fn completed_scope_states_are_reused_after_scope_churn() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            assert_eq!(cx.core.borrow().scope_state_pool.len(), 0);
            for _ in 0..64 {
                cx.scope(|_| ());
            }
            assert_eq!(cx.core.borrow().scope_state_pool.len(), 1);
        });
    }

    #[test]
    fn pool_diagnostics_reports_retained_scheduler_pools() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            max_cached_stacks: 1,
            registered_file_slots: 2,
            registered_buffer_slots: 3,
            ..RuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            let initial = cx.pool_diagnostics();
            assert_eq!(initial.cached_scope_states, 0);
            assert_eq!(
                initial.max_cached_scope_states,
                super::DEFAULT_SCOPE_STATE_CACHE_CAPACITY
            );
            assert_eq!(initial.cached_stacks, 0);
            assert_eq!(initial.max_cached_stacks, 1);
            assert_eq!(initial.registered_file_free_slots, 2);
            assert_eq!(initial.registered_buffer_free_slots, 3);

            cx.scope(|_| ());
            let after_scope = cx.pool_diagnostics();
            assert_eq!(after_scope.cached_scope_states, 1);

            cx.scope(|scope| {
                let worker = scope.spawn(|_| 1);
                assert_eq!(worker.join(cx), 1);
            });

            let after_spawn = cx.pool_diagnostics();
            assert_eq!(after_spawn.task_slots, 1);
            assert_eq!(after_spawn.free_task_slots, 1);
            assert_eq!(after_spawn.cached_stacks, 1);
            assert_eq!(after_spawn.max_cached_stacks, 1);
            assert_eq!(after_spawn.cached_scope_states, 1);
            assert!(after_spawn.task_slot_capacity >= after_spawn.task_slots);
            assert!(after_spawn.wait_token_slot_capacity >= after_spawn.wait_token_slots);
            assert!(after_spawn.io_state_pool_capacity >= after_spawn.cached_io_states);
            assert!(after_spawn.completion_scratch_capacity >= after_spawn.completion_scratch_len);
        });
    }

    #[test]
    fn pool_diagnostics_does_not_mutate_io_counters() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let before = cx.io_counters();
            let diagnostics = cx.pool_diagnostics();
            let after = cx.io_counters();

            assert_eq!(before, after);
            assert_eq!(diagnostics.detached_pending_io, before.detached_pending);
        });
    }

    #[test]
    fn recycled_scope_states_do_not_keep_large_tracking_maps() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let mut handles = Vec::new();
                for _ in 0..128 {
                    handles.push(scope.spawn(|_| ()));
                }
                for handle in handles {
                    handle.join(cx);
                }
            });

            let scheduler = cx.core.borrow();
            let state = scheduler
                .scope_state_pool
                .last()
                .expect("scope state should be recycled");
            let inner = state.inner.borrow();
            assert!(
                inner.panic_payloads.capacity()
                    <= super::DEFAULT_SCOPE_STATE_COLLECTION_CACHE_CAPACITY
            );
            assert!(
                inner.task_ids.capacity() <= super::DEFAULT_SCOPE_STATE_COLLECTION_CACHE_CAPACITY
            );
        });
    }

    #[test]
    fn completed_coroutine_stacks_are_reused_after_spawn_churn() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            max_cached_stacks: 1,
            ..RuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            assert_eq!(cx.cached_stack_count(), 0);
            assert_eq!(cx.reused_stack_count(), 0);

            cx.scope(|scope| {
                let worker = scope.spawn(|_| 1);
                assert_eq!(worker.join(cx), 1);
            });
            assert_eq!(cx.cached_stack_count(), 1);
            assert_eq!(cx.reused_stack_count(), 0);

            cx.scope(|scope| {
                let worker = scope.spawn(|_| 2);
                assert_eq!(worker.join(cx), 2);
            });
            assert_eq!(cx.cached_stack_count(), 1);
            assert_eq!(cx.reused_stack_count(), 1);
        });
    }

    #[test]
    fn stale_waiter_does_not_wake_reused_task_slot() {
        let (waiter_tx, waiter_rx) = mpsc::channel::<super::Waiter>();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let first_tx = waiter_tx.clone();
                let first = scope.spawn(move |cx| {
                    let registration = cx.wait_registration();
                    first_tx
                        .send(
                            cx.waiter(&registration)
                                .expect("stackful task should have waiter"),
                        )
                        .expect("failed to send first waiter");
                    1
                });
                assert_eq!(first.join(cx), 1);
                let stale_waiter = waiter_rx.recv().expect("missing stale waiter");

                let second_tx = waiter_tx.clone();
                let second = scope.spawn(move |cx| {
                    let registration = cx.wait_registration();
                    second_tx
                        .send(
                            cx.waiter(&registration)
                                .expect("stackful task should have waiter"),
                        )
                        .expect("failed to send second waiter");
                    cx.park();
                    2
                });
                cx.yield_now();
                let live_waiter = waiter_rx.recv().expect("missing live waiter");

                stale_waiter.wake();
                cx.yield_now();
                assert_eq!(second.try_join(), None);

                live_waiter.wake();
                assert_eq!(second.join(cx), 2);
            });
        });
    }

    #[test]
    fn timed_out_waiter_does_not_consume_later_wake() {
        let gate = ManualWait::new();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let waitables: [&dyn Waitable; 1] = [&gate];
            assert_eq!(
                cx.wait_any(&waitables, Some(Duration::from_millis(1))),
                Err(WaitError::TimedOut)
            );
            assert_eq!(gate.active_waiters(), 0);

            cx.scope(|scope| {
                let waiter = scope.spawn(|cx| {
                    gate.wait(cx);
                    7
                });
                cx.yield_now();
                assert_eq!(gate.active_waiters(), 1);

                gate.wake_one();
                assert_eq!(waiter.join(cx), 7);
            });
        });
    }

    #[test]
    fn wait_any_non_winning_waiter_does_not_consume_later_wake() {
        let first = ManualWait::new();
        let second = ManualWait::new();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let waiter = scope.spawn(|cx| {
                    let waitables: [&dyn Waitable; 2] = [&first, &second];
                    cx.wait_any(&waitables, None).unwrap()
                });

                cx.yield_now();
                assert_eq!(first.active_waiters(), 1);
                assert_eq!(second.active_waiters(), 1);

                first.wake_one();
                assert_eq!(waiter.join(cx), 0);
                assert_eq!(second.active_waiters(), 0);

                second.wake_one();
            });
        });
    }

    #[test]
    fn timed_out_multi_waiters_do_not_consume_later_wakes() {
        let first = ManualWait::new();
        let second = ManualWait::new();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let waitables: [&dyn Waitable; 2] = [&first, &second];
            assert_eq!(
                cx.wait_any(&waitables, Some(Duration::from_millis(1))),
                Err(WaitError::TimedOut)
            );
            assert_eq!(first.active_waiters(), 0);
            assert_eq!(second.active_waiters(), 0);

            cx.scope(|scope| {
                let waiter = scope.spawn(|cx| {
                    second.wait(cx);
                    8
                });
                cx.yield_now();
                second.wake_one();
                assert_eq!(waiter.join(cx), 8);
            });
        });
    }

    #[test]
    fn canceled_parked_task_releases_active_waiter() {
        let gate = ManualWait::new();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                scope.spawn(|cx| gate.wait(cx));
                cx.yield_now();
                assert_eq!(gate.active_waiters(), 1);
            });

            assert_eq!(gate.active_waiters(), 0);
            assert!(gate.waiter_slots() <= 1);
        });
    }

    #[test]
    fn repeated_timeout_churn_keeps_waiter_tombstones_bounded() {
        let gate = ManualWait::new();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let waitables: [&dyn Waitable; 1] = [&gate];
            for _ in 0..32 {
                assert_eq!(
                    cx.wait_any(&waitables, Some(Duration::from_millis(1))),
                    Err(WaitError::TimedOut)
                );
            }

            assert_eq!(gate.active_waiters(), 0);
            assert!(
                gate.waiter_slots() <= 1,
                "stale waiters should be compacted, found {} slots",
                gate.waiter_slots()
            );
        });
    }

    #[test]
    fn root_context_reports_default_stack_size_without_stack_usage() {
        let mut runtime = Runtime::with_stack_size(96 * 1024);

        runtime.block_on(|cx| {
            assert_eq!(cx.stack_size(), 96 * 1024);
            assert_eq!(cx.stack_usage(), None);
        });
    }

    #[test]
    fn spawn_with_stack_size_overrides_inherited_default() {
        let mut runtime = Runtime::with_stack_size(64 * 1024);

        let output = runtime.block_on(|cx| {
            cx.scope(|scope| {
                let default = scope.spawn(|cx| {
                    let usage = cx.stack_usage().expect("stackful task missing usage");
                    assert_eq!(cx.stack_size(), 64 * 1024);
                    usage.size()
                });
                let custom = scope.spawn_with_stack_size(96 * 1024, |cx| {
                    let usage = cx.stack_usage().expect("stackful task missing usage");
                    assert_eq!(cx.stack_size(), 96 * 1024);
                    usage.size()
                });

                (default.join(cx), custom.join(cx))
            })
        });

        assert_eq!(output, (64 * 1024, 96 * 1024));
    }

    #[test]
    fn join_with_stack_usage_reports_final_high_water() {
        let mut runtime = Runtime::new();

        let final_usage = runtime.block_on(|cx| {
            cx.scope(|scope| {
                let worker = scope.spawn_with_stack_size(128 * 1024, |cx| {
                    let before = cx.stack_usage().expect("stackful task missing usage");
                    assert_eq!(before.size(), 128 * 1024);
                    assert!(before.used() != 0);

                    stack_usage_touch::<{ 12 * 1024 }>();

                    let after = cx.stack_usage().expect("stackful task missing usage");
                    assert_eq!(after.size(), 128 * 1024);
                    assert!(after.high_water() >= 12 * 1024, "{after:?}");
                    assert!(after.high_water() >= after.used(), "{after:?}");
                    7
                });

                let (result, usage) = worker.join_with_stack_usage(cx);
                assert_eq!(result, 7);
                usage
            })
        });

        assert_eq!(final_usage.size(), 128 * 1024);
        assert!(final_usage.high_water() >= 12 * 1024, "{final_usage:?}");
        assert!(final_usage.high_water_remaining() < final_usage.size());
    }

    #[test]
    fn completed_handle_exposes_stack_usage_before_result_is_taken() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let worker = scope.spawn(|cx| {
                    stack_usage_touch::<4096>();
                    cx.stack_usage()
                        .expect("stackful task missing usage")
                        .size()
                });

                cx.yield_now();
                let usage = worker
                    .stack_usage()
                    .expect("completed task missing stack usage");
                let (result, usage_from_join) = worker
                    .try_join_with_stack_usage()
                    .expect("worker should have completed");

                assert_eq!(result, DEFAULT_STACK_SIZE);
                assert_eq!(usage, usage_from_join);
                assert!(usage.high_water() >= 4096, "{usage:?}");
            });
        });
    }

    #[inline(never)]
    fn stack_usage_touch<const N: usize>() {
        let mut buffer = [0_u8; N];
        buffer.fill(0x5a);
        std::hint::black_box(&mut buffer);
    }

    #[test]
    fn yield_now_reschedules_task() {
        let mut runtime = Runtime::new();

        let output = runtime.block_on(|cx| {
            cx.scope(|scope| {
                let yielding = scope.spawn(|cx| {
                    cx.yield_now();
                    2
                });
                let other = scope.spawn(|_| 40);

                other.join(cx) + yielding.join(cx)
            })
        });

        assert_eq!(output, 42);
    }

    #[test]
    fn dump_stacks_reports_not_started_stackful_tasks() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let handle = scope.spawn(|_| 42);
                let dump = cx.dump_stacks();

                assert_eq!(dump.tasks().len(), 1);
                let task = &dump.tasks()[0];
                assert_eq!(task.state(), TaskStackState::NotStarted);
                assert_eq!(task.stack_usage().size(), DEFAULT_STACK_SIZE);
                assert_eq!(task.stack_usage().used(), 0);
                assert_eq!(task.backtrace(), None);
                assert_eq!(handle.join(cx), 42);
            });
        });
    }

    #[test]
    fn dump_stacks_captures_suspended_stackful_task() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let handle = scope.spawn(|cx| {
                    stack_dump_test_leaf(cx);
                    42
                });

                cx.yield_now();
                let dump = cx.dump_stacks();

                assert_eq!(dump.tasks().len(), 1);
                let task = &dump.tasks()[0];
                assert_eq!(task.state(), TaskStackState::Suspended);
                assert_eq!(task.stack_usage().size(), DEFAULT_STACK_SIZE);
                assert!(task.stack_usage().used() != 0);
                let backtrace = task.backtrace().expect("missing suspended task backtrace");
                assert!(!backtrace.trim().is_empty());
                assert_eq!(handle.join(cx), 42);
            });
        });
    }

    #[inline(never)]
    fn stack_dump_test_leaf(cx: &RuntimeContext<'_>) {
        cx.yield_now();
    }

    #[test]
    fn interrupted_stackful_task_panics_propagate_to_joiner() {
        let result = std::panic::catch_unwind(|| {
            let mut runtime = Runtime::new();

            runtime.block_on(|cx| {
                cx.scope(|scope| {
                    let handle = scope.spawn(|cx| {
                        cx.yield_now();
                        42
                    });

                    cx.yield_now();
                    cx.core
                        .borrow_mut()
                        .interrupt_started_tasks(|_| Box::new(|_| panic!("interrupted")));

                    handle.join(cx);
                });
            });
        });

        let payload = result.expect_err("interrupted task should panic through join");
        let message = payload.downcast_ref::<&str>().copied();
        assert_eq!(message, Some("interrupted"));
    }

    #[test]
    fn unobserved_stackful_task_panic_propagates_when_parent_exits_scope_body() {
        let result = std::panic::catch_unwind(|| {
            let mut runtime = Runtime::new();

            runtime.block_on(|cx| {
                cx.scope(|scope| {
                    scope.spawn(|cx| {
                        cx.yield_now();
                        panic!("unjoined child panic");
                    });
                });
            });
        });

        let payload = result.expect_err("unjoined task panic should propagate through scope");
        let message = payload.downcast_ref::<&str>().copied();
        assert_eq!(message, Some("unjoined child panic"));
    }

    #[test]
    fn unjoined_child_can_use_outer_scope_for_nested_spawn_after_scope_body_returns() {
        let mut runtime = Runtime::new();

        let output = runtime.block_on(|cx| {
            let mut nested_output = 0;
            cx.scope(|scope| {
                scope.spawn(|cx| {
                    cx.yield_now();
                    let nested = scope.spawn(|_| 7);
                    nested_output = nested.join(cx);
                });
            });
            nested_output
        });

        assert_eq!(output, 7);
    }

    #[test]
    fn unobserved_stackful_task_panic_propagates_to_parent_waiting_on_io() {
        let result = std::panic::catch_unwind(|| {
            let mut runtime = Runtime::new();

            runtime.block_on(|cx| {
                cx.scope(|scope| {
                    scope.spawn(|cx| {
                        cx.yield_now();
                        panic!("unjoined child panic during parent io wait");
                    });

                    let mut sleeps = 0;
                    loop {
                        cx.sleep(Duration::from_millis(1)).unwrap();
                        sleeps += 1;
                        assert!(
                            sleeps < 100,
                            "parent loop should have observed the child panic"
                        );
                    }
                });
            });
        });

        let payload = result.expect_err("unjoined task panic should propagate while parent waits");
        let message = payload.downcast_ref::<&str>().copied();
        assert_eq!(message, Some("unjoined child panic during parent io wait"));
    }

    #[test]
    fn nested_scopes_finish_before_parent_continues() {
        let mut runtime = Runtime::new();

        let output = runtime.block_on(|cx| {
            cx.scope(|scope| {
                let parent = scope.spawn(|cx| {
                    let mut child_output = 0;
                    cx.scope(|scope| {
                        let child = scope.spawn(|_| 5);
                        child_output = child.join(cx);
                    });
                    child_output + 1
                });

                parent.join(cx)
            })
        });

        assert_eq!(output, 6);
    }

    #[test]
    fn io_uring_pipe_ping_pong_between_stackful_tasks() {
        const ROUNDS: u8 = 8;

        let mut runtime = Runtime::new();

        let output = runtime.block_on(|cx| {
            let (ping_read, ping_write) = pipe().unwrap();
            let (pong_read, pong_write) = pipe().unwrap();

            cx.scope(|scope| {
                let initiator = scope.spawn(move |cx| {
                    let mut received = [0];

                    for round in 0..ROUNDS {
                        assert_eq!(cx.write(&ping_write, &[round]).unwrap(), 1);
                        assert_eq!(cx.read(&pong_read, &mut received).unwrap(), 1);
                        assert_eq!(received[0], round.wrapping_add(1));
                    }

                    cx.close(ping_write).unwrap();
                    cx.close(pong_read).unwrap();
                    ROUNDS as usize
                });

                let responder = scope.spawn(move |cx| {
                    let mut received = [0];

                    for _ in 0..ROUNDS {
                        assert_eq!(cx.read(&ping_read, &mut received).unwrap(), 1);
                        assert_eq!(
                            cx.write(&pong_write, &[received[0].wrapping_add(1)])
                                .unwrap(),
                            1
                        );
                    }

                    cx.close(ping_read).unwrap();
                    cx.close(pong_write).unwrap();
                    ROUNDS as usize
                });

                initiator.join(cx) + responder.join(cx)
            })
        });

        assert_eq!(output, (ROUNDS as usize) * 2);
    }

    #[test]
    fn registered_fd_pipe_read_write() {
        let mut runtime = Runtime::with_registered_resources(2, 0);

        let output = runtime.block_on(|cx| {
            let (read_fd, write_fd) = pipe().unwrap();
            let read_fd = cx.register_fd(read_fd).unwrap();
            let write_fd = cx.register_fd(write_fd).unwrap();
            let mut buffer = [0_u8; 1];

            assert_eq!(cx.write_registered_fd(&write_fd, &[42]).unwrap(), 1);
            assert_eq!(cx.read_registered_fd(&read_fd, &mut buffer).unwrap(), 1);
            buffer[0]
        });

        assert_eq!(output, 42);
    }

    #[test]
    fn registered_buffer_pipe_read_write() {
        let mut runtime = Runtime::with_registered_resources(0, 2);

        let output = runtime.block_on(|cx| {
            let (read_fd, write_fd) = pipe().unwrap();
            let write_buffer = cx.register_buffer(vec![7_u8]).unwrap();
            let mut read_buffer = cx.register_buffer(vec![0_u8]).unwrap();

            assert_eq!(
                cx.write_registered_buffer(&write_fd, &write_buffer)
                    .unwrap(),
                1
            );
            assert_eq!(
                cx.read_registered_buffer(&read_fd, &mut read_buffer)
                    .unwrap(),
                1
            );
            cx.close(read_fd).unwrap();
            cx.close(write_fd).unwrap();

            read_buffer.buffer()[0]
        });

        assert_eq!(output, 7);
    }

    #[test]
    fn registered_fd_and_buffer_pipe_read_write() {
        let mut runtime = Runtime::with_registered_resources(2, 2);

        let output = runtime.block_on(|cx| {
            let (read_fd, write_fd) = pipe().unwrap();
            let read_fd = cx.register_fd(read_fd).unwrap();
            let write_fd = cx.register_fd(write_fd).unwrap();
            let write_buffer = cx.register_buffer(vec![9_u8]).unwrap();
            let mut read_buffer = cx.register_buffer(vec![0_u8]).unwrap();

            assert_eq!(
                cx.write_registered_fixed(&write_fd, &write_buffer).unwrap(),
                1
            );
            assert_eq!(
                cx.read_registered_fixed(&read_fd, &mut read_buffer)
                    .unwrap(),
                1
            );

            read_buffer.buffer()[0]
        });

        assert_eq!(output, 9);
    }

    #[test]
    fn registered_fixed_async_io_returns_buffers() {
        let mut runtime = Runtime::with_registered_resources(2, 2);

        let output = runtime.block_on(|cx| {
            let (read_fd, write_fd) = pipe().unwrap();
            let read_fd = cx.register_fd(read_fd).unwrap();
            let write_fd = cx.register_fd(write_fd).unwrap();
            let read_buffer = cx.register_buffer(vec![0_u8]).unwrap();
            let write_buffer = cx.register_buffer(vec![11_u8]).unwrap();

            let read = cx.read_registered_fixed_async(&read_fd, read_buffer);
            let write = cx.write_registered_fixed_async(&write_fd, write_buffer);
            let pending: [&dyn Waitable; 2] = [&read, &write];
            cx.join(&pending, Some(Duration::from_secs(1))).unwrap();

            let read = read.get(cx).unwrap();
            let write = write.get(cx).unwrap();

            assert_eq!(write.bytes, 1);
            assert_eq!(read.bytes, 1);
            read.buffer.buffer()[0]
        });

        assert_eq!(output, 11);
    }

    #[test]
    fn registered_resource_slots_are_reused_after_drop() {
        let mut runtime = Runtime::with_registered_resources(1, 1);

        runtime.block_on(|cx| {
            let (first_read, first_write) = pipe().unwrap();
            drop(cx.register_fd(first_read).unwrap());
            cx.close(first_write).unwrap();

            let (second_read, second_write) = pipe().unwrap();
            drop(cx.register_fd(second_read).unwrap());
            cx.close(second_write).unwrap();

            drop(cx.register_buffer(vec![1_u8]).unwrap());
            drop(cx.register_buffer(vec![2_u8]).unwrap());
        });
    }

    #[test]
    fn dropped_registered_fd_pending_async_io_reclaims_slot_after_completion() {
        let mut runtime = Runtime::with_registered_resources(1, 0);

        runtime.block_on(|cx| {
            let (read_fd, write_fd) = pipe().unwrap();
            let read_fd = cx.register_fd(read_fd).unwrap();
            let read = cx.read_registered_fd_async(&read_fd, vec![0_u8]);
            drop(read_fd);

            let (second_read, second_write) = pipe().unwrap();
            assert_eq!(cx.register_fd(second_read).unwrap_err(), Errno::NOBUFS);
            cx.close(second_write).unwrap();

            assert_eq!(cx.write(&write_fd, &[91]).unwrap(), 1);
            let output = read.get(cx).unwrap();
            assert_eq!(output.bytes, 1);
            assert_eq!(output.buffer[0], 91);
            cx.close(write_fd).unwrap();

            let (third_read, third_write) = pipe().unwrap();
            drop(cx.register_fd(third_read).unwrap());
            cx.close(third_write).unwrap();
        });
    }

    #[test]
    fn io_uring_nop_and_sleep_complete() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.nop().unwrap();
            cx.sleep(Duration::from_millis(1)).unwrap();
        });
    }

    #[test]
    fn busy_poll_never_waits_for_io_completion() {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            busy_poll: BusyPoll::Never,
            ..RuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            cx.sleep(Duration::from_millis(1)).unwrap();
            let (poll_entries, wait_entries) = cx.core.borrow().ring_enter_counts();
            assert_eq!(poll_entries, 0);
            assert!(wait_entries != 0);
        });
    }

    #[test]
    fn busy_poll_always_does_not_wait_for_io_completion() {
        let mut runtime = Runtime::with_busy_poll(BusyPoll::Always);

        runtime.block_on(|cx| {
            cx.sleep(Duration::from_millis(1)).unwrap();
            let (poll_entries, wait_entries) = cx.core.borrow().ring_enter_counts();
            assert!(poll_entries != 0);
            assert_eq!(wait_entries, 0);
        });
    }

    #[test]
    fn busy_poll_until_polls_then_waits_after_duration() {
        let mut runtime = Runtime::with_busy_poll(BusyPoll::Until(Duration::from_millis(100)));

        runtime.block_on(|cx| {
            cx.sleep(Duration::from_millis(1)).unwrap();
            let (poll_entries, wait_entries) = cx.core.borrow().ring_enter_counts();
            assert!(poll_entries != 0);
            assert_eq!(wait_entries, 0);

            thread::sleep(Duration::from_millis(120));
            cx.sleep(Duration::from_millis(1)).unwrap();
            let (_, wait_entries) = cx.core.borrow().ring_enter_counts();
            assert!(wait_entries != 0);
        });
    }

    #[test]
    fn positioned_io_preserves_file_offset_and_fstat_reports_size() {
        let root = unique_temp_dir("positioned");
        let file = root.join("file");
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

            let mut positioned = [0_u8; 5];
            assert_eq!(cx.pread(&fd, &mut positioned, 5).unwrap(), 5);
            assert_eq!(&positioned, b"world");

            let mut suffix = [0_u8; 5];
            assert_eq!(cx.read(&fd, &mut suffix).unwrap(), 5);
            assert_eq!(&suffix, b"world");

            cx.close(fd).unwrap();
            cx.unlink(&file).unwrap();
            cx.rmdir(&root).unwrap();
        });

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn io_uring_filesystem_and_advisory_operations() {
        let root = unique_temp_dir("fs");
        let file = root.join("file");
        let renamed = root.join("renamed");
        let hard_link = root.join("hard-link");
        let symlink = root.join("symlink");
        let subdir = root.join("subdir");
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
            assert_eq!(cx.write(&fd, b"hello").unwrap(), 5);

            cx.fadvise(&fd, 0, 5, FileAdvice::Normal).unwrap();
            cx.fallocate(&fd, FallocateFlags::empty(), 0, 4096).unwrap();
            cx.sync_file_range(&fd, 0, 5, 0).unwrap();
            cx.fsync(&fd).unwrap();
            cx.close(fd).unwrap();

            cx.rename(&file, &renamed, RenameFlags::empty()).unwrap();
            cx.link(&renamed, &hard_link, AtFlags::empty()).unwrap();
            cx.symlink(&renamed, &symlink).unwrap();
            cx.unlink(&symlink).unwrap();
            cx.unlink(&hard_link).unwrap();
            cx.mkdir(&subdir, Mode::RWXU).unwrap();
            cx.rmdir(&subdir).unwrap();

            let mapping_len = 4096;
            let mapping = unsafe {
                rustix::mm::mmap_anonymous(
                    ptr::null_mut(),
                    mapping_len,
                    ProtFlags::READ | ProtFlags::WRITE,
                    MapFlags::PRIVATE,
                )
                .unwrap()
            };
            unsafe {
                cx.madvise(mapping, mapping_len, MemoryAdvice::Normal)
                    .unwrap();
                rustix::mm::munmap(mapping, mapping_len).unwrap();
            }

            cx.unlink(&renamed).unwrap();
            cx.rmdir(&root).unwrap();
        });

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn io_uring_openat2_statx_ftruncate_and_vectored_io() {
        let root = unique_temp_dir("fs-extra");
        let file = root.join("file");
        let _ = std::fs::remove_dir_all(&root);

        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.mkdir(&root, Mode::RWXU).unwrap();
            let fd = cx
                .open2(
                    &file,
                    OFlags::CREATE | OFlags::RDWR | OFlags::TRUNC,
                    Mode::RUSR | Mode::WUSR,
                    ResolveFlags::empty(),
                )
                .unwrap();
            assert_eq!(cx.write(&fd, b"abcdef").unwrap(), 6);
            cx.ftruncate(&fd, 3).unwrap();
            let stat = cx
                .statx(&file, AtFlags::empty(), StatxFlags::BASIC_STATS)
                .unwrap();
            assert_eq!(stat.stx_size, 3);
            cx.close(fd).unwrap();

            let (read_fd, write_fd) = pipe().unwrap();
            let first = b"vec";
            let second = b"tor";
            let write_iov = [
                IoVec {
                    iov_base: first.as_ptr().cast::<c_void>().cast_mut(),
                    iov_len: first.len(),
                },
                IoVec {
                    iov_base: second.as_ptr().cast::<c_void>().cast_mut(),
                    iov_len: second.len(),
                },
            ];
            assert_eq!(cx.writev(&write_fd, &write_iov).unwrap(), 6);

            let mut left = [0_u8; 3];
            let mut right = [0_u8; 3];
            let mut read_iov = [
                IoVec {
                    iov_base: left.as_mut_ptr().cast::<c_void>(),
                    iov_len: left.len(),
                },
                IoVec {
                    iov_base: right.as_mut_ptr().cast::<c_void>(),
                    iov_len: right.len(),
                },
            ];
            assert_eq!(cx.readv(&read_fd, &mut read_iov).unwrap(), 6);
            assert_eq!([left.as_slice(), right.as_slice()].concat(), b"vector");
            cx.close(read_fd).unwrap();
            cx.close(write_fd).unwrap();

            cx.unlink(&file).unwrap();
            cx.rmdir(&root).unwrap();
        });

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn io_uring_splice_tee_poll_and_epoll_ctl() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            if cx.supports_io_uring_opcode(rustix_uring::opcode::Splice::CODE) {
                let (src_read, src_write) = pipe().unwrap();
                let (dst_read, dst_write) = pipe().unwrap();
                assert_eq!(cx.write(&src_write, b"abc").unwrap(), 3);
                assert_eq!(
                    cx.splice(&src_read, -1, &dst_write, -1, 3, UringSpliceFlags::empty())
                        .unwrap(),
                    3
                );
                let mut out = [0_u8; 3];
                assert_eq!(cx.read(&dst_read, &mut out).unwrap(), 3);
                assert_eq!(&out, b"abc");
                cx.close(src_read).unwrap();
                cx.close(src_write).unwrap();
                cx.close(dst_read).unwrap();
                cx.close(dst_write).unwrap();
            }

            if cx.supports_io_uring_opcode(rustix_uring::opcode::Tee::CODE) {
                let (src_read, src_write) = pipe().unwrap();
                let (dst_read, dst_write) = pipe().unwrap();
                assert_eq!(cx.write(&src_write, b"xy").unwrap(), 2);
                assert_eq!(
                    cx.tee(&src_read, &dst_write, 2, UringSpliceFlags::empty())
                        .unwrap(),
                    2
                );
                let mut dst = [0_u8; 2];
                let mut src = [0_u8; 2];
                assert_eq!(cx.read(&dst_read, &mut dst).unwrap(), 2);
                assert_eq!(cx.read(&src_read, &mut src).unwrap(), 2);
                assert_eq!(&dst, b"xy");
                assert_eq!(&src, b"xy");
                cx.close(src_read).unwrap();
                cx.close(src_write).unwrap();
                cx.close(dst_read).unwrap();
                cx.close(dst_write).unwrap();
            }

            if cx.supports_io_uring_opcode(rustix_uring::opcode::PollAdd::CODE) {
                let (read_fd, write_fd) = pipe().unwrap();
                assert_eq!(cx.write(&write_fd, &[1]).unwrap(), 1);
                let events = cx.poll(&read_fd, PollFlags::IN.bits().into()).unwrap();
                assert_ne!(events & u32::from(PollFlags::IN.bits()), 0);
                cx.close(read_fd).unwrap();
                cx.close(write_fd).unwrap();
            }

            if cx.supports_io_uring_opcode(rustix_uring::opcode::EpollCtl::CODE) {
                let (read_fd, write_fd) = pipe().unwrap();
                let epoll_fd = epoll::create(epoll::CreateFlags::CLOEXEC).unwrap();
                let event = EpollEvent {
                    flags: EpollEventFlags::IN,
                    data: EpollEventData::new_u64(42),
                };
                cx.epoll_ctl(&epoll_fd, &read_fd, EpollCtlOp::Add, &event)
                    .unwrap();
                cx.close(read_fd).unwrap();
                cx.close(write_fd).unwrap();
                cx.close(epoll_fd).unwrap();
            }
        });
    }

    #[test]
    fn io_uring_timeout_cancel_and_futex_operations() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let mut timeout = cx.timeout(Duration::from_secs(60));
            timeout.cancel();

            if cx.supports_io_uring_opcode(rustix_uring::opcode::AsyncCancel::CODE) {
                let (read_fd, write_fd) = pipe().unwrap();
                let read_fd = IoFd::from_owned(read_fd);
                let read = cx.read_async(&read_fd, vec![0]);
                cx.cancel_io(&read).unwrap();
                assert!(read.get(cx).is_err());
                cx.close(write_fd).unwrap();
            }

            if cx.supports_io_uring_opcode(rustix_uring::opcode::FutexWait::CODE)
                && cx.supports_io_uring_opcode(rustix_uring::opcode::FutexWake::CODE)
            {
                let futex = AtomicU32::new(0);
                cx.scope(|scope| {
                    let waiter = scope.spawn(|cx| {
                        cx.futex_wait(
                            &futex,
                            0,
                            u64::from(u32::MAX),
                            FutexWaitFlags::SIZE_U32 | FutexWaitFlags::PRIVATE,
                        )
                        .unwrap();
                        futex.load(Ordering::Acquire)
                    });
                    let waker = scope.spawn(|cx| {
                        cx.yield_now();
                        futex.store(1, Ordering::Release);
                        cx.futex_wake(
                            &futex,
                            1,
                            u64::from(u32::MAX),
                            FutexWaitFlags::SIZE_U32 | FutexWaitFlags::PRIVATE,
                        )
                        .unwrap()
                    });

                    assert_eq!(waker.join(cx), 1);
                    assert_eq!(waiter.join(cx), 1);
                });
            }
        });
    }

    #[test]
    fn dropped_timeout_is_canceled_before_block_on_returns() {
        let mut runtime = Runtime::new();
        let start = Instant::now();

        let supported = runtime.block_on(|cx| {
            if !cx.supports_io_uring_opcode(rustix_uring::opcode::TimeoutRemove::CODE) {
                return false;
            }

            drop(cx.timeout(Duration::from_secs(60)));
            true
        });

        if supported {
            assert!(start.elapsed() < Duration::from_secs(5));
        }
    }

    #[test]
    fn scope_exit_cancels_child_sleeping_for_a_long_time() {
        let mut runtime = Runtime::new();
        let start = Instant::now();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                scope.spawn(|cx| {
                    cx.sleep(Duration::from_secs(60 * 60)).unwrap();
                });
            });
        });

        assert!(start.elapsed() < Duration::from_secs(60));
    }

    #[test]
    fn nested_scope_cancels_blocked_child_after_ready_scope_children_park() {
        let gate = ManualWait::new();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let parent = scope.spawn(|cx| {
                    cx.scope(|nested| {
                        nested.spawn(|cx| gate.wait(cx));
                    });
                    1
                });

                let unrelated = scope.spawn(|cx| {
                    cx.yield_now();
                    2
                });

                assert_eq!(parent.join(cx), 1);
                assert_eq!(unrelated.join(cx), 2);
            });
        });

        assert_eq!(gate.active_waiters(), 0);
    }

    #[test]
    fn dropped_io_result_is_canceled_before_block_on_returns() {
        let mut runtime = Runtime::new();

        let fds = runtime.block_on(|cx| {
            if !cx.supports_io_uring_opcode(rustix_uring::opcode::AsyncCancel::CODE) {
                return None;
            }

            let (read_fd, write_fd) = pipe().unwrap();
            let read_fd = IoFd::from_owned(read_fd);
            drop(cx.read_async(&read_fd, vec![0]));
            Some((read_fd, write_fd))
        });

        drop(fds);
    }

    #[test]
    fn dropped_pending_io_result_reclaims_buffer_after_drain() {
        let drops = Rc::new(Cell::new(0));
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let (read_fd, write_fd) = pipe().unwrap();
            let read_fd = IoFd::from_owned(read_fd);
            let read = cx.read_async(&read_fd, DropCountingReadBuffer::new(1, Rc::clone(&drops)));
            drop(read);

            assert_eq!(drops.get(), 0);
            assert_eq!(cx.io_counters().detached_pending, 1);

            assert_eq!(cx.write(&write_fd, &[33]).unwrap(), 1);
            cx.close(write_fd).unwrap();
        });

        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn detached_pending_io_result_reclaims_buffer_without_cancel() {
        let drops = Rc::new(Cell::new(0));
        let mut runtime = Runtime::new();

        let counters = runtime.block_on(|cx| {
            let (read_fd, write_fd) = pipe().unwrap();
            let read_fd = IoFd::from_owned(read_fd);
            let read = cx.read_async(&read_fd, DropCountingReadBuffer::new(1, Rc::clone(&drops)));
            read.detach();

            let counters = cx.io_counters();
            assert_eq!(counters.detached_pending, 1);
            assert_eq!(counters.cancel_requests, 0);

            assert_eq!(cx.write(&write_fd, &[34]).unwrap(), 1);
            cx.close(write_fd).unwrap();
            drain_detached_io(cx);
            cx.io_counters()
        });

        assert_eq!(drops.get(), 1);
        assert_eq!(counters.detached_pending, 0);
        assert_eq!(counters.detached_total, 1);
        assert_eq!(counters.cancel_requests, 0);
        assert_eq!(counters.original_completions_after_cancel, 0);
    }

    #[test]
    fn io_lifecycle_counters_track_dropped_cancel_completion() {
        let mut runtime = Runtime::new();

        let counters = runtime.block_on(|cx| {
            let (read_fd, write_fd) = pipe().unwrap();
            let read_fd = IoFd::from_owned(read_fd);
            let read = cx.read_async(&read_fd, vec![0_u8]);
            drop(read);

            assert_eq!(cx.io_counters().detached_pending, 1);
            assert_eq!(cx.write(&write_fd, &[35]).unwrap(), 1);
            cx.close(write_fd).unwrap();
            drain_detached_io(cx);
            cx.io_counters()
        });

        assert_eq!(counters.detached_pending, 0);
        assert_eq!(counters.detached_total, 1);
        assert!(
            counters.original_completions_after_cancel >= 1,
            "expected original CQE after cancellation, counters: {counters:?}"
        );
        if counters.cancel_requests != 0 {
            assert_eq!(counters.cancel_completions, counters.cancel_requests);
        } else {
            assert!(counters.unsupported_cancels <= 1);
        }
    }

    #[test]
    fn detached_timeout_reaps_without_cancel_request() {
        let mut runtime = Runtime::new();

        let counters = runtime.block_on(|cx| {
            cx.timeout(Duration::from_millis(1)).detach();
            let counters = cx.io_counters();
            assert_eq!(counters.detached_pending, 1);
            assert_eq!(counters.cancel_requests, 0);

            cx.sleep(Duration::from_millis(1)).unwrap();
            drain_detached_io(cx);
            cx.io_counters()
        });

        assert_eq!(counters.detached_pending, 0);
        assert_eq!(counters.detached_total, 1);
        assert_eq!(counters.cancel_requests, 0);
    }

    #[test]
    fn dropped_registered_fd_async_result_reclaims_slot_after_drain() {
        let mut runtime = Runtime::with_registered_resources(1, 0);

        runtime.block_on(|cx| {
            let (read_fd, write_fd) = pipe().unwrap();
            let read_fd = cx.register_fd(read_fd).unwrap();
            let read = cx.read_registered_fd_async(&read_fd, vec![0_u8]);
            drop(read_fd);
            drop(read);

            let (second_read, second_write) = pipe().unwrap();
            assert_eq!(cx.register_fd(second_read).unwrap_err(), Errno::NOBUFS);
            cx.close(second_write).unwrap();

            match cx.write(&write_fd, &[36]) {
                Ok(1) | Err(Errno::PIPE) => {}
                other => panic!("unexpected write result after dropped registered read: {other:?}"),
            }
            cx.close(write_fd).unwrap();
            drain_detached_io(cx);

            let (third_read, third_write) = pipe().unwrap();
            drop(cx.register_fd(third_read).unwrap());
            cx.close(third_write).unwrap();
        });
    }

    #[test]
    fn dropped_registered_buffer_async_result_reclaims_slot_after_drain() {
        let mut runtime = Runtime::with_registered_resources(0, 1);

        runtime.block_on(|cx| {
            let (read_fd, write_fd) = pipe().unwrap();
            let read_fd = IoFd::from_owned(read_fd);
            let read_buffer = cx.register_buffer(vec![0_u8]).unwrap();
            let read = cx.read_registered_buffer_async(&read_fd, read_buffer);
            drop(read);

            assert_eq!(cx.register_buffer(vec![1_u8]).unwrap_err(), Errno::NOBUFS);

            match cx.write(&write_fd, &[37]) {
                Ok(1) | Err(Errno::PIPE) => {}
                other => panic!("unexpected write result after dropped registered read: {other:?}"),
            }
            cx.close(write_fd).unwrap();
            drain_detached_io(cx);

            drop(cx.register_buffer(vec![2_u8]).unwrap());
        });
    }

    #[test]
    fn cancellable_trait_requests_timeout_and_io_result_cancellation() {
        let mut runtime = Runtime::new();

        let fds = runtime.block_on(|cx| {
            if !cx.supports_io_uring_opcode(rustix_uring::opcode::AsyncCancel::CODE)
                || !cx.supports_io_uring_opcode(rustix_uring::opcode::TimeoutRemove::CODE)
            {
                return None;
            }

            let mut timeout = cx.timeout(Duration::from_secs(60));
            Cancellable::cancel(&mut timeout);

            let (read_fd, write_fd) = pipe().unwrap();
            let read_fd = IoFd::from_owned(read_fd);
            let mut read = cx.read_async(&read_fd, vec![0]);
            Cancellable::cancel(&mut read);

            Some((read_fd, write_fd))
        });

        drop(fds);
    }

    #[test]
    fn successful_child_metadata_is_removed_in_long_lived_scope() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                for _ in 0..128 {
                    scope.spawn(|_| ()).join(cx);
                    let scope_state = cx
                        .active_scopes
                        .borrow()
                        .last()
                        .expect("scope should be active")
                        .clone();
                    assert_eq!(scope_state.tracked_task_count(), 0);
                    assert_eq!(scope_state.tracked_panic_payload_count(), 0);
                }
            });
        });
    }

    #[test]
    fn observed_child_panic_metadata_is_removed_in_long_lived_scope() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                for _ in 0..16 {
                    let child = scope.spawn(|_| panic!("expected child panic"));
                    assert!(
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| child.join(cx)))
                            .is_err()
                    );

                    let scope_state = cx
                        .active_scopes
                        .borrow()
                        .last()
                        .expect("scope should be active")
                        .clone();
                    assert_eq!(scope_state.tracked_task_count(), 0);
                    assert_eq!(scope_state.tracked_panic_payload_count(), 0);
                }
            });
        });
    }

    #[test]
    fn io_uring_sendmsg_and_recvmsg_between_stackful_tasks() {
        let mut runtime = Runtime::new();

        let received = runtime.block_on(|cx| {
            let (left, right) = net::socketpair(
                AddressFamily::UNIX,
                SocketType::STREAM,
                net::SocketFlags::empty(),
                None,
            )
            .unwrap();

            cx.scope(|scope| {
                let sender = scope.spawn(move |cx| {
                    let first = b"hello ";
                    let second = b"msg";
                    let mut iov = [
                        IoVec {
                            iov_base: first.as_ptr().cast::<c_void>().cast_mut(),
                            iov_len: first.len(),
                        },
                        IoVec {
                            iov_base: second.as_ptr().cast::<c_void>().cast_mut(),
                            iov_len: second.len(),
                        },
                    ];
                    let msg = MsgHdr {
                        msg_name: ptr::null_mut(),
                        msg_namelen: 0,
                        msg_iov: iov.as_mut_ptr(),
                        msg_iovlen: iov.len(),
                        msg_control: ptr::null_mut(),
                        msg_controllen: 0,
                        msg_flags: RecvFlags::empty(),
                    };

                    assert_eq!(cx.sendmsg(&left, &msg, SendFlags::empty()).unwrap(), 9);
                    cx.close(left).unwrap();
                });

                let receiver = scope.spawn(move |cx| {
                    let mut first = [0_u8; 5];
                    let mut second = [0_u8; 4];
                    let mut iov = [
                        IoVec {
                            iov_base: first.as_mut_ptr().cast::<c_void>(),
                            iov_len: first.len(),
                        },
                        IoVec {
                            iov_base: second.as_mut_ptr().cast::<c_void>(),
                            iov_len: second.len(),
                        },
                    ];
                    let mut msg = MsgHdr {
                        msg_name: ptr::null_mut(),
                        msg_namelen: 0,
                        msg_iov: iov.as_mut_ptr(),
                        msg_iovlen: iov.len(),
                        msg_control: ptr::null_mut(),
                        msg_controllen: 0,
                        msg_flags: RecvFlags::empty(),
                    };

                    assert_eq!(cx.recvmsg(&right, &mut msg, RecvFlags::empty()).unwrap(), 9);
                    cx.close(right).unwrap();
                    [first.as_slice(), second.as_slice()].concat()
                });

                sender.join(cx);
                receiver.join(cx)
            })
        });

        assert_eq!(received, b"hello msg");
    }

    #[test]
    fn io_uring_tcp_echo_server_and_client_short_reads() {
        let mut runtime = Runtime::new();

        let echoed = runtime.block_on(|cx| {
            let (addr_tx, addr_rx) = once::channel();
            let message = b"stackful socket echo through io_uring".to_vec();

            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let listener = cx
                        .socket(AddressFamily::INET, SocketType::STREAM, Some(ipproto::TCP))
                        .unwrap();
                    let loopback = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
                    cx.bind(&listener, &loopback).unwrap();
                    cx.listen(&listener, 8).unwrap();
                    let local_addr: SocketAddr =
                        net::getsockname(&listener).unwrap().try_into().unwrap();
                    addr_tx.send(local_addr).unwrap();

                    let connection = cx.accept(&listener).unwrap();
                    let mut buffer = [0_u8; 5];

                    loop {
                        let read = cx.recv(&connection, &mut buffer).unwrap();
                        if read == 0 {
                            break;
                        }
                        send_all(cx, &connection, &buffer[..read]);
                    }

                    cx.shutdown(&connection, Shutdown::Write).unwrap();
                    cx.close(connection).unwrap();
                    cx.close(listener).unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let addr = addr_rx.recv(cx).unwrap();
                    let socket = cx
                        .socket(AddressFamily::INET, SocketType::STREAM, Some(ipproto::TCP))
                        .unwrap();
                    cx.connect(&socket, &addr).unwrap();

                    for chunk in message.chunks(4) {
                        send_all(cx, &socket, chunk);
                    }
                    cx.shutdown(&socket, Shutdown::Write).unwrap();

                    let echoed = recv_to_end(cx, &socket, message.len());
                    cx.close(socket).unwrap();
                    assert_eq!(echoed, message);
                    echoed
                });

                let echoed = client.join(cx);
                server.join(cx);
                echoed
            })
        });

        assert_eq!(echoed.as_slice(), b"stackful socket echo through io_uring");
    }

    #[test]
    fn async_io_results_can_be_tried_joined_and_timed_out() {
        let mut runtime = Runtime::new();

        let output = runtime.block_on(|cx| {
            let (first_read, first_write) = pipe().unwrap();
            let (second_read, second_write) = pipe().unwrap();
            let first_read = IoFd::from_owned(first_read);
            let first_write = IoFd::from_owned(first_write);
            let second_read = IoFd::from_owned(second_read);
            let second_write = IoFd::from_owned(second_write);

            let mut first = cx.read_async(&first_read, vec![0; 1]);
            let mut second = cx.read_async(&second_read, vec![0; 1]);
            assert!(first.try_get().is_none());
            assert!(second.try_get().is_none());

            let mut first_write_result = cx.write_async(&first_write, b"a".to_vec());
            let mut second_write_result = cx.write_async(&second_write, b"b".to_vec());

            let pending: [&dyn Waitable; 4] =
                [&first, &second, &first_write_result, &second_write_result];
            cx.join(&pending, Some(Duration::from_secs(1))).unwrap();

            assert_eq!(first_write_result.try_get().unwrap().unwrap().bytes, 1);
            assert_eq!(second_write_result.try_get().unwrap().unwrap().bytes, 1);

            let first = first.get(cx).unwrap();
            let second = second.get(cx).unwrap();

            (first.buffer[0], second.buffer[0])
        });

        assert_eq!(output, (b'a', b'b'));
    }

    #[test]
    fn async_io_accepts_non_vec_owned_buffers() {
        let mut runtime = Runtime::new();

        let output = runtime.block_on(|cx| {
            let (read_fd, write_fd) = pipe().unwrap();
            let read_fd = IoFd::from_owned(read_fd);
            let write_fd = IoFd::from_owned(write_fd);
            let read_buffer = vec![0_u8; 1].into_boxed_slice();
            let write_buffer: Box<[u8]> = Box::new([b'x']);

            let read = cx.read_async(&read_fd, read_buffer);
            let write = cx.write_async(&write_fd, write_buffer);
            let pending: [&dyn Waitable; 2] = [&read, &write];
            cx.join(&pending, Some(Duration::from_secs(1))).unwrap();

            let read = read.get(cx).unwrap();
            let write = write.get(cx).unwrap();

            (read.buffer[0], write.bytes)
        });

        assert_eq!(output, (b'x', 1));
    }

    #[test]
    fn async_io_get_parks_stackful_tasks() {
        const ROUNDS: u8 = 4;

        let mut runtime = Runtime::new();

        let output = runtime.block_on(|cx| {
            let (ping_read, ping_write) = pipe().unwrap();
            let (pong_read, pong_write) = pipe().unwrap();
            let ping_read = IoFd::from_owned(ping_read);
            let ping_write = IoFd::from_owned(ping_write);
            let pong_read = IoFd::from_owned(pong_read);
            let pong_write = IoFd::from_owned(pong_write);

            cx.scope(|scope| {
                let initiator = scope.spawn(move |cx| {
                    let mut total = 0;

                    for round in 0..ROUNDS {
                        assert_eq!(
                            cx.write_async(&ping_write, vec![round])
                                .get(cx)
                                .unwrap()
                                .bytes,
                            1
                        );
                        let pong = cx.read_async(&pong_read, vec![0]).get(cx).unwrap();
                        assert_eq!(pong.buffer[0], round + 1);
                        total += pong.buffer[0] as usize;
                    }

                    total
                });

                let responder = scope.spawn(move |cx| {
                    for _ in 0..ROUNDS {
                        let ping = cx.read_async(&ping_read, vec![0]).get(cx).unwrap();
                        let response = vec![ping.buffer[0] + 1];
                        assert_eq!(
                            cx.write_async(&pong_write, response).get(cx).unwrap().bytes,
                            1
                        );
                    }
                });

                responder.join(cx);
                initiator.join(cx)
            })
        });

        assert_eq!(output, (1..=ROUNDS).map(usize::from).sum::<usize>());
    }

    #[test]
    fn async_io_join_timeout_leaves_result_usable() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let (read_fd, write_fd) = pipe().unwrap();
            let read_fd = IoFd::from_owned(read_fd);
            let write_fd = IoFd::from_owned(write_fd);
            let read = cx.read_async(&read_fd, vec![0; 1]);

            let pending: [&dyn Waitable; 1] = [&read];
            assert_eq!(
                cx.join(&pending, Some(Duration::from_millis(1))),
                Err(IoJoinError::TimedOut)
            );

            let write = cx.write_async(&write_fd, b"z".to_vec());
            let pending: [&dyn Waitable; 2] = [&read, &write];
            cx.join(&pending, Some(Duration::from_secs(1))).unwrap();

            let read = read.get(cx).unwrap();
            let write = write.get(cx).unwrap();
            assert_eq!(read.buffer[0], b'z');
            assert_eq!(write.bytes, 1);
        });
    }

    #[test]
    fn io_fd_pending_async_read_keeps_fd_alive_after_user_handles_drop() {
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let (read_fd, write_fd) = pipe().unwrap();
            let read_fd = IoFd::from_owned(read_fd);
            let read = cx.read_async(&read_fd, vec![0_u8]);

            let read_fd = match read_fd.into_owned() {
                Ok(_) => panic!("pending async read should keep an IoFd lease"),
                Err(read_fd) => read_fd,
            };
            drop(read_fd);

            assert_eq!(cx.write(&write_fd, &[73]).unwrap(), 1);
            let output = read.get(cx).unwrap();
            assert_eq!(output.bytes, 1);
            assert_eq!(output.buffer[0], 73);

            cx.close(write_fd).unwrap();
        });
    }

    #[test]
    fn allocation_pipe_ping_pong_hot_path_no_allocations() {
        const WARMUP_ROUNDS: u8 = 4;
        const MEASURED_ROUNDS: u8 = 64;

        let mut runtime = Runtime::new();

        let counts = runtime.block_on(|cx| {
            let (ping_read, ping_write) = pipe().unwrap();
            let (pong_read, pong_write) = pipe().unwrap();

            cx.scope(|scope| {
                let responder = scope.spawn(move |cx| {
                    let mut received = [0];

                    for _ in 0..WARMUP_ROUNDS {
                        assert_eq!(cx.read(&ping_read, &mut received).unwrap(), 1);
                        assert_eq!(
                            cx.write(&pong_write, &[received[0].wrapping_add(1)])
                                .unwrap(),
                            1
                        );
                    }

                    for _ in 0..MEASURED_ROUNDS {
                        assert_eq!(cx.read(&ping_read, &mut received).unwrap(), 1);
                        assert_eq!(
                            cx.write(&pong_write, &[received[0].wrapping_add(1)])
                                .unwrap(),
                            1
                        );
                    }

                    cx.close(ping_read).unwrap();
                    cx.close(pong_write).unwrap();
                });

                let initiator = scope.spawn(move |cx| {
                    let mut received = [0];

                    for round in 0..WARMUP_ROUNDS {
                        assert_eq!(cx.write(&ping_write, &[round]).unwrap(), 1);
                        assert_eq!(cx.read(&pong_read, &mut received).unwrap(), 1);
                        assert_eq!(received[0], round.wrapping_add(1));
                    }

                    let (_, counts) = allocation_tracking::measure(|| {
                        for round in 0..MEASURED_ROUNDS {
                            assert_eq!(cx.write(&ping_write, &[round]).unwrap(), 1);
                            assert_eq!(cx.read(&pong_read, &mut received).unwrap(), 1);
                            assert_eq!(received[0], round.wrapping_add(1));
                        }
                    });

                    cx.close(ping_write).unwrap();
                    cx.close(pong_read).unwrap();
                    counts
                });

                let counts = initiator.join(cx);
                responder.join(cx);
                counts
            })
        });

        assert_eq!(counts.allocating_operations(), 0, "{counts:?}");
        assert_eq!(counts.allocated_or_reallocated_bytes(), 0, "{counts:?}");
    }

    #[test]
    fn allocation_async_pipe_ping_pong_reused_buffers_no_allocations() {
        const WARMUP_ROUNDS: u8 = 4;
        const MEASURED_ROUNDS: u8 = 64;

        let mut runtime = Runtime::new();

        let counts = runtime.block_on(|cx| {
            let (ping_read, ping_write) = pipe().unwrap();
            let (pong_read, pong_write) = pipe().unwrap();
            let ping_read = IoFd::from_owned(ping_read);
            let ping_write = IoFd::from_owned(ping_write);
            let pong_read = IoFd::from_owned(pong_read);
            let pong_write = IoFd::from_owned(pong_write);

            cx.scope(|scope| {
                scope.spawn(|_| ()).join(cx);

                let responder = scope.spawn(move |cx| {
                    let mut read_buffer = vec![0];
                    let mut write_buffer = vec![0];

                    for _ in 0..WARMUP_ROUNDS {
                        let read = cx.read_async(&ping_read, read_buffer).get(cx).unwrap();
                        read_buffer = read.buffer;
                        write_buffer[0] = read_buffer[0].wrapping_add(1);
                        let write = cx.write_async(&pong_write, write_buffer).get(cx).unwrap();
                        write_buffer = write.buffer;
                    }

                    for _ in 0..MEASURED_ROUNDS {
                        let read = cx.read_async(&ping_read, read_buffer).get(cx).unwrap();
                        read_buffer = read.buffer;
                        write_buffer[0] = read_buffer[0].wrapping_add(1);
                        let write = cx.write_async(&pong_write, write_buffer).get(cx).unwrap();
                        write_buffer = write.buffer;
                    }
                });

                let initiator = scope.spawn(move |cx| {
                    let mut write_buffer = vec![0];
                    let mut read_buffer = vec![0];

                    for round in 0..WARMUP_ROUNDS {
                        write_buffer[0] = round;
                        let write = cx.write_async(&ping_write, write_buffer).get(cx).unwrap();
                        write_buffer = write.buffer;
                        let read = cx.read_async(&pong_read, read_buffer).get(cx).unwrap();
                        assert_eq!(read.buffer[0], round.wrapping_add(1));
                        read_buffer = read.buffer;
                    }

                    let (_, counts) = allocation_tracking::measure(|| {
                        for round in 0..MEASURED_ROUNDS {
                            write_buffer[0] = round;
                            let write = cx.write_async(&ping_write, write_buffer).get(cx).unwrap();
                            write_buffer = write.buffer;
                            let read = cx.read_async(&pong_read, read_buffer).get(cx).unwrap();
                            assert_eq!(read.buffer[0], round.wrapping_add(1));
                            read_buffer = read.buffer;
                        }
                    });

                    counts
                });

                let counts = initiator.join(cx);
                responder.join(cx);
                counts
            })
        });

        assert_eq!(counts.allocating_operations(), 0, "{counts:?}");
        assert_eq!(counts.allocated_or_reallocated_bytes(), 0, "{counts:?}");
    }
}
