// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Stackful coroutines with structured concurrency.
//!
//! This crate is intentionally not an async runtime. It runs stackful coroutines
//! cooperatively on the current OS thread and exposes scoped spawning so
//! coroutines cannot outlive the scope that created them.
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
//! [`RuntimeContext::read_async`] and [`RuntimeContext::write_async`] return an
//! [`IoResult`]. `try_get` observes a completion that has already been reaped
//! without entering io_uring or running other tasks. `get` waits cooperatively by
//! parking the current coroutine. [`Cancellable::cancel`] requests cancellation
//! without waiting for the kernel CQE. [`RuntimeContext::join`] waits for
//! multiple [`Waitable`] values at once and can optionally use an io_uring
//! timeout; timed out operations remain pending and their handles can still be
//! completed later.
//!
//! Async I/O takes ownership of buffers instead of borrowing them. This keeps the
//! safe API sound even if a handle is dropped or forgotten while the kernel still
//! has a pointer. Buffer ownership is abstracted with [`IoReadBuffer`] and
//! [`IoWriteBuffer`]; `Vec<u8>` is the default buffer type and `Box<[u8]>` is
//! also supported. Successful read/write completions return the original buffer
//! in [`ReadOutput`] or [`WriteOutput`]. If an [`IoResult`] is canceled or
//! dropped while its operation is still pending, the runtime requests kernel
//! cancellation and intentionally leaks the backing buffer rather than allowing
//! io_uring to complete into freed memory. Complete pending results with `get`
//! or `try_get` when buffer reclamation matters.
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
//! Setup paths allocate: the runtime owns scheduler containers and io_uring, each
//! spawned coroutine allocates a guarded stack and join state, and channels own
//! their shared state. These allocations are expected and bounded by runtime
//! structure.
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
use std::collections::VecDeque;
use std::ffi::c_void;
use std::fmt;
use std::io;
use std::marker::PhantomData;
use std::mem::{ManuallyDrop, MaybeUninit};
use std::ops::{Deref, DerefMut};
use std::os::fd::FromRawFd;
use std::panic::{self, AssertUnwindSafe};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex as StdMutex};
use std::time::{Duration, Instant};

use corosensei::stack::{DefaultStack, MIN_STACK_SIZE, Stack as CoroStack};
use corosensei::{Coroutine, CoroutineResult, Yielder};
pub use rustix::event::epoll::{
    Event as EpollEvent, EventData as EpollEventData, EventFlags as EpollEventFlags,
};
use rustix::event::{EventfdFlags, PollFlags, eventfd};
pub use rustix::fd::OwnedFd;
use rustix::fd::{AsFd, AsRawFd};
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
    squeue::Entry128 as Sqe,
    types::{CancelBuilder, Fd, Fixed, FutexWaitV, OpenHow, Timespec},
};

pub mod barrier;
pub mod channel;
pub mod mutex;
pub mod notify;
pub mod once;
pub mod rwlock;
pub mod semaphore;
pub mod wait;
pub mod watch;

pub use barrier::{Barrier, BarrierWaitResult};
pub use mutex::{Mutex, MutexGuard};
pub use notify::{Notified, Notify};
pub use rwlock::{ReadLock, RwLock, RwLockReadGuard, RwLockWriteGuard, WriteLock};
pub use semaphore::{Semaphore, SemaphorePermit};
pub use wait::{IoHandle, IoJoinError, WaitError, Waitable};
pub use watch::{WatchReceiver, WatchSender};

const DEFAULT_STACK_SIZE: usize = 64 * 1024;
const DEFAULT_RING_ENTRIES: u32 = 128;

#[cfg(test)]
#[global_allocator]
static TEST_ALLOCATOR: allocation_tracking::CountingAllocator =
    allocation_tracking::CountingAllocator;

type TaskId = usize;
type PanicPayload = Box<dyn Any + Send + 'static>;
type KernelIoResult = Result<u32, Errno>;
type IoUring = rustix_uring::IoUring<Sqe, Cqe>;

struct TaskCanceled;

/// A handle for kernel work that can be canceled without blocking.
///
/// Cancellation is best-effort at the submission boundary: the method requests
/// cancellation and returns immediately. The runtime still drains the eventual
/// completion before [`Runtime::block_on`] returns, so cancellation never leaves
/// an io_uring operation owned by a dropped scheduler.
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
    fn not_started(task_id: TaskId, stack: StackTracker) -> Self {
        Self {
            task_id,
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeConfig {
    /// Usable stack bytes for each stackful coroutine.
    pub stack_size: usize,
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
            ring_entries: DEFAULT_RING_ENTRIES,
            ring_enter_policy: RingEnterPolicy::default(),
            busy_poll: BusyPoll::default(),
            registered_file_slots: 0,
            registered_buffer_slots: 0,
        }
    }
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
        let output = panic::catch_unwind(AssertUnwindSafe(|| main(&cx)));
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

impl RuntimeContext<'_> {
    /// Creates a structured concurrency scope.
    ///
    /// All stackful coroutines spawned through the scope finish before this
    /// method returns.
    pub fn scope<'env, F, T>(&'env self, f: F) -> T
    where
        F: for<'scope> FnOnce(&'scope Scope<'scope, 'env>) -> T,
    {
        let state = Rc::new(ScopeState::default());
        self.active_scopes.borrow_mut().push(Rc::clone(&state));
        let scope = Scope {
            core: Rc::clone(&self.core),
            state: Rc::clone(&state),
            stack_size: self.stack_size,
            _scope: PhantomData,
            _env: PhantomData,
        };

        let result = panic::catch_unwind(AssertUnwindSafe(|| f(&scope)));
        self.wait_for_scope(&state);
        let popped = self
            .active_scopes
            .borrow_mut()
            .pop()
            .expect("active scope stack underflow");
        debug_assert!(Rc::ptr_eq(&popped, &state));

        match result {
            Ok(output) => {
                if let Some(payload) = state.take_panic_payload() {
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

    /// Cooperatively yields the current stackful coroutine.
    pub fn yield_now(&self) {
        match self.current {
            CurrentTask::Root => {
                drive_scheduler(&self.core);
            }
            CurrentTask::Coroutine { id, yielder, stack } => {
                suspend_task(id, yielder, stack, Suspend::Ready);
            }
        }
        self.resume_active_scope_panic();
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

    /// Submits an io_uring no-op and waits for its completion.
    pub fn nop(&self) -> Result<(), Errno> {
        self.submit_and_wait_for_io(opcode::Nop::new().build())
            .map(|_| ())
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
    pub fn read_async<B>(&self, fd: &impl AsFd, mut buffer: B) -> IoResult<ReadOutput<B>, B>
    where
        B: IoReadBuffer,
    {
        let len =
            u32::try_from(buffer.io_buffer_len()).expect("io_uring read length exceeds u32::MAX");
        let fd = fd.as_fd().as_raw_fd();
        let entry = opcode::Read::new(Fd(fd), buffer.io_buffer_mut_ptr(), len)
            .offset(u64::MAX)
            .build();
        let state = self.acquire_io_state();

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
        B: IoReadBuffer,
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
        fd: &impl AsFd,
        buffer: RegisteredBuffer<B>,
    ) -> IoResult<ReadOutput<RegisteredBuffer<B>>, RegisteredBuffer<B>> {
        let len = u32::try_from(buffer.len()).expect("io_uring read length exceeds u32::MAX");
        let fd = fd.as_fd().as_raw_fd();
        let state = self.acquire_io_state();
        state.attach_registered_resource(buffer.resource());

        self.submit_io_state(
            opcode::ReadFixed::new(Fd(fd), buffer.mut_ptr(), len, buffer.slot())
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
    pub fn write_async<B>(&self, fd: &impl AsFd, buffer: B) -> IoResult<WriteOutput<B>, B>
    where
        B: IoWriteBuffer,
    {
        let len =
            u32::try_from(buffer.io_buffer_len()).expect("io_uring write length exceeds u32::MAX");
        let fd = fd.as_fd().as_raw_fd();
        let entry = opcode::Write::new(Fd(fd), buffer.io_buffer_ptr(), len)
            .offset(u64::MAX)
            .build();
        let state = self.acquire_io_state();

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
        B: IoWriteBuffer,
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
        fd: &impl AsFd,
        buffer: RegisteredBuffer<B>,
    ) -> IoResult<WriteOutput<RegisteredBuffer<B>>, RegisteredBuffer<B>> {
        let len = u32::try_from(buffer.len()).expect("io_uring write length exceeds u32::MAX");
        let fd = fd.as_fd().as_raw_fd();
        let state = self.acquire_io_state();
        state.attach_registered_resource(buffer.resource());

        self.submit_io_state(
            opcode::WriteFixed::new(Fd(fd), buffer.ptr(), len, buffer.slot())
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

    /// Attempts to cancel a pending [`IoResult`].
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
            if !cancel_requested && self.core.borrow().ready_len() == 0 {
                self.cancel_scope_tasks(state);
                cancel_requested = true;
                continue;
            }

            if let Some(waiter) = self.waiter() {
                state.push_waiter(waiter);
            }
            self.park();
        }
    }

    fn cancel_scope_tasks(&self, state: &ScopeState) {
        let task_ids = state.task_ids();
        interrupt_scheduler_tasks(&self.core, task_ids, |_| {
            Box::new(|_| panic::panic_any(TaskCanceled))
        });
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

            state.add_waiter_from(self);
            self.park();
            self.resume_active_scope_panic();
        }
    }

    fn wait_for_io_state_without_scope_panic(&self, state: &Rc<IoState>) -> KernelIoResult {
        loop {
            if let Some(result) = state.take_result() {
                return result;
            }

            state.add_waiter_from(self);
            self.park();
        }
    }

    fn cancel_and_wait_for_io_state(&self, state: &Rc<IoState>, cancel_kind: IoCancelKind) {
        let cancel_state = self.submit_cancel_for_io_state(state, cancel_kind);
        while !state.is_ready() || cancel_state.as_ref().is_some_and(|state| !state.is_ready()) {
            state.add_waiter_from(self);
            if let Some(cancel_state) = &cancel_state {
                cancel_state.add_waiter_from(self);
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
            timeout.state.add_waiter_from(self);
            if let Some(cancel_state) = &cancel_state {
                cancel_state.add_waiter_from(self);
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
                    drive_scheduler(&self.core),
                    "kimojio-stack runtime deadlocked: no runnable stackful coroutines"
                );
            }
            CurrentTask::Coroutine { id, yielder, stack } => {
                suspend_task(id, yielder, stack, Suspend::Parked);
            }
        }
    }

    pub(crate) fn waiter(&self) -> Option<Waiter> {
        match self.current {
            CurrentTask::Root => None,
            CurrentTask::Coroutine { id, .. } => Some(Waiter {
                core: Rc::downgrade(&self.core),
                task_id: id,
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

    #[allow(
        dead_code,
        reason = "phase 1 infrastructure consumed by cross-thread channel phases"
    )]
    pub(crate) fn external_waiter(&self) -> Option<ExternalWaiter> {
        match self.current {
            CurrentTask::Root => None,
            CurrentTask::Coroutine { id, .. } => {
                let external_wake = self.core.borrow().external_wake();
                Some(ExternalWaiter::new(&external_wake, id))
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
        id: TaskId,
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
        state.register_panic_payload(Rc::clone(&panic_payload));
        let join = Rc::new(JoinState::new(panic_payload));
        let task_join = Rc::clone(&join);
        let core = Rc::clone(&self.core);
        let stack =
            DefaultStack::new(stack_size).expect("failed to allocate stackful coroutine stack");
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
                let stack_usage = cx
                    .stack_usage()
                    .expect("stackful coroutine missing stack usage");
                task_join.complete(result, stack_usage);
                task_state.task_finished();
            })
        };

        state.task_started(id);
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

            self.state.add_waiter_from(cx);
            cx.park();
        }
    }

    /// Waits for the coroutine to finish and returns its result plus final stack usage.
    pub fn join_with_stack_usage(self, cx: &RuntimeContext<'_>) -> (T, StackUsage) {
        loop {
            if let Some(result) = self.try_join_with_stack_usage() {
                return result;
            }

            self.state.add_waiter_from(cx);
            cx.park();
        }
    }
}

impl<T> Waitable for JoinHandle<'_, T> {
    fn is_ready(&self) -> bool {
        self.taken.get() || self.state.is_ready()
    }

    fn add_waiter(&self, cx: &RuntimeContext<'_>) {
        self.state.add_waiter_from(cx);
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
/// buffer value must not invalidate the pointer previously submitted to the
/// kernel. If an [`IoResult`] is leaked or dropped while pending, leaking the
/// buffer value must keep the submitted memory valid.
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
/// buffer value must not invalidate the pointer previously submitted to the
/// kernel. If an [`IoResult`] is leaked or dropped while pending, leaking the
/// buffer value must keep the submitted memory valid.
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

/// A pending io_uring operation.
#[must_use = "pending IO should be completed with get, try_get, or RuntimeContext::join"]
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

            self.state
                .as_ref()
                .expect("IoResult state missing")
                .add_waiter_from(cx);
            cx.park();
            cx.resume_active_scope_panic();
        }
    }

    fn user_data(&self) -> Option<io_uring_user_data> {
        self.state.as_ref().map(io_state_user_data)
    }

    /// Requests cancellation of this operation without waiting for completion.
    pub fn cancel(&mut self) {
        self.cancel_pending();
    }

    fn cancel_pending(&mut self) {
        if self.taken {
            return;
        }

        let Some(state) = self.state.take() else {
            return;
        };

        if state.is_ready() {
            unsafe {
                ManuallyDrop::drop(&mut self.buffer);
            }
            self.taken = true;
            recycle_io_state(&self.core, state);
            return;
        }

        submit_cancel(
            &self.core,
            &state,
            IoCancelKind::Async(io_state_user_data(&state)),
        );
        self.taken = true;
    }
}

impl<T, B> Cancellable for IoResult<T, B> {
    fn cancel(&mut self) {
        self.cancel_pending();
    }
}

impl<T, B> Waitable for IoResult<T, B> {
    fn is_ready(&self) -> bool {
        self.taken || self.state.as_ref().is_none_or(|state| state.is_ready())
    }

    fn add_waiter(&self, cx: &RuntimeContext<'_>) {
        if let Some(state) = &self.state {
            state.add_waiter_from(cx);
        }
    }
}

impl<T, B> fmt::Debug for IoResult<T, B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IoResult")
            .field("ready", &self.is_ready())
            .field("taken", &self.taken)
            .finish()
    }
}

impl<T, B> Drop for IoResult<T, B> {
    fn drop(&mut self) {
        self.cancel_pending();
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
}

impl<T> JoinState<T> {
    fn new(panic_payload: Rc<RefCell<Option<PanicPayload>>>) -> Self {
        Self {
            inner: RefCell::new(JoinStateInner {
                result: None,
                stack_usage: None,
                waiters: Waiters::default(),
            }),
            panic_payload,
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

    fn add_waiter_from(&self, cx: &RuntimeContext<'_>) {
        if let Some(waiter) = cx.waiter() {
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
#[must_use = "timeouts should be waited on, canceled, or allowed to complete"]
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

            self.state
                .as_ref()
                .expect("timeout state missing")
                .add_waiter_from(cx);
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
        self.cancel_pending();
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
            state.add_waiter_from(cx);
            if let Some(cancel_state) = &cancel_state {
                cancel_state.add_waiter_from(cx);
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

    fn cancel_pending(&mut self) {
        let Some(state) = self.state.take() else {
            return;
        };

        if state.is_ready() {
            recycle_io_state(&self.core, state);
            return;
        }

        submit_cancel(&self.core, &state, IoCancelKind::Timeout(self.user_data));
    }
}

impl Cancellable for Timeout {
    fn cancel(&mut self) {
        self.cancel_pending();
    }
}

impl Waitable for Timeout {
    fn is_ready(&self) -> bool {
        self.state.as_ref().is_none_or(|state| state.is_ready())
    }

    fn add_waiter(&self, cx: &RuntimeContext<'_>) {
        if let Some(state) = &self.state {
            state.add_waiter_from(cx);
        }
    }
}

impl fmt::Debug for Timeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Timeout")
            .field("ready", &self.is_ready())
            .finish()
    }
}

impl Drop for Timeout {
    fn drop(&mut self) {
        self.cancel_pending();
    }
}

fn timeout_result(result: KernelIoResult) -> Result<(), Errno> {
    match result {
        Ok(_) | Err(Errno::TIME) => Ok(()),
        Err(error) => Err(error),
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
        inner.result = Some(result);
        inner.registered_resources.complete_all();
        inner.waiters.wake_all();
    }

    pub(crate) fn add_waiter_from(&self, cx: &RuntimeContext<'_>) {
        if let Some(waiter) = cx.waiter() {
            self.inner.borrow_mut().waiters.push(waiter);
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

    fn mark_cancel_requested(&self) -> bool {
        let mut inner = self.inner.borrow_mut();
        let already_requested = inner.cancel_requested;
        inner.cancel_requested = true;
        !already_requested
    }
}

struct IoStateInner {
    result: Option<KernelIoResult>,
    waiters: Waiters,
    timeout: Option<Timespec>,
    registered_resources: RegisteredResources,
    cancel_requested: bool,
}

impl IoStateInner {
    fn new() -> Self {
        Self {
            result: None,
            waiters: Waiters::default(),
            timeout: None,
            registered_resources: RegisteredResources::new(),
            cancel_requested: false,
        }
    }

    fn reset_for_reuse(&mut self) {
        self.result = None;
        self.waiters.clear();
        self.timeout = None;
        self.registered_resources.clear();
        self.cancel_requested = false;
    }
}

#[derive(Default)]
struct ScopeState {
    inner: RefCell<ScopeStateInner>,
}

impl ScopeState {
    fn register_panic_payload(&self, payload: Rc<RefCell<Option<PanicPayload>>>) {
        self.inner.borrow_mut().panic_payloads.push(payload);
    }

    fn task_started(&self, id: TaskId) {
        let mut inner = self.inner.borrow_mut();
        inner.remaining += 1;
        inner.task_ids.push(id);
    }

    fn has_remaining_tasks(&self) -> bool {
        self.inner.borrow().remaining != 0
    }

    fn push_waiter(&self, waiter: Waiter) {
        self.inner.borrow_mut().waiters.push(waiter);
    }

    fn task_ids(&self) -> Vec<TaskId> {
        self.inner.borrow().task_ids.clone()
    }

    fn take_panic_payload(&self) -> Option<PanicPayload> {
        let inner = self.inner.borrow();
        for payload in &inner.panic_payloads {
            if let Some(payload) = payload.borrow_mut().take() {
                return Some(payload);
            }
        }
        None
    }

    fn task_finished(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.remaining = inner
            .remaining
            .checked_sub(1)
            .expect("scope task count underflow");

        if inner.remaining == 0 {
            inner.waiters.wake_all();
        }
    }
}

#[derive(Default)]
struct ScopeStateInner {
    remaining: usize,
    waiters: Waiters,
    panic_payloads: Vec<Rc<RefCell<Option<PanicPayload>>>>,
    task_ids: Vec<TaskId>,
}

#[derive(Clone)]
pub(crate) struct Waiter {
    core: Weak<SchedulerCell>,
    task_id: TaskId,
}

impl Waiter {
    pub(crate) fn wake(self) {
        if let Some(core) = self.core.upgrade() {
            core.borrow_mut().schedule(self.task_id);
        }
    }
}

#[allow(
    dead_code,
    reason = "phase 1 infrastructure consumed by cross-thread channel phases"
)]
#[derive(Clone)]
pub(crate) struct ExternalWaiter {
    registration: Arc<ExternalWaiterRegistration>,
}

#[allow(
    dead_code,
    reason = "phase 1 infrastructure consumed by cross-thread channel phases"
)]
impl ExternalWaiter {
    fn new(external_wake: &Arc<ExternalWake>, task_id: TaskId) -> Self {
        external_wake.register_waiter();
        Self {
            registration: Arc::new(ExternalWaiterRegistration {
                external_wake: Arc::downgrade(external_wake),
                task_id,
                active: AtomicBool::new(true),
            }),
        }
    }

    pub(crate) fn wake(self) {
        self.registration.wake();
    }
}

#[allow(
    dead_code,
    reason = "phase 1 infrastructure consumed by cross-thread channel phases"
)]
struct ExternalWaiterRegistration {
    external_wake: std::sync::Weak<ExternalWake>,
    task_id: TaskId,
    active: AtomicBool,
}

impl ExternalWaiterRegistration {
    fn wake(&self) {
        if !self.active.swap(false, Ordering::AcqRel) {
            return;
        }

        if let Some(external_wake) = self.external_wake.upgrade() {
            external_wake.wake_task(self.task_id);
        }
    }
}

impl Drop for ExternalWaiterRegistration {
    fn drop(&mut self) {
        if !self.active.swap(false, Ordering::AcqRel) {
            return;
        }

        if let Some(external_wake) = self.external_wake.upgrade() {
            external_wake.unregister_waiter();
        }
    }
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

    #[allow(
        dead_code,
        reason = "phase 1 infrastructure consumed by cross-thread channel phases"
    )]
    fn register_waiter(&self) {
        let mut inner = self.inner();
        inner.waiters += 1;
    }

    #[allow(
        dead_code,
        reason = "phase 1 infrastructure consumed by cross-thread channel phases"
    )]
    fn unregister_waiter(&self) {
        let mut inner = self.inner();
        inner.waiters = inner
            .waiters
            .checked_sub(1)
            .expect("external waiter count underflow");
        drop(inner);
        self.signal();
    }

    #[allow(
        dead_code,
        reason = "phase 1 infrastructure consumed by cross-thread channel phases"
    )]
    fn wake_task(&self, task_id: TaskId) {
        let mut inner = self.inner();
        inner.waiters = inner
            .waiters
            .checked_sub(1)
            .expect("external waiter count underflow");
        inner.ready.push_back(task_id);
        drop(inner);
        self.signal();
    }

    fn take_ready(&self) -> VecDeque<TaskId> {
        let mut inner = self.inner();
        std::mem::take(&mut inner.ready)
    }

    fn has_waiters_or_ready(&self) -> bool {
        let inner = self.inner();
        inner.waiters != 0 || !inner.ready.is_empty()
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
    ready: VecDeque<TaskId>,
    waiters: usize,
}

#[derive(Default)]
pub(crate) struct Waiters {
    first: Option<Waiter>,
    rest: Vec<Waiter>,
}

impl Waiters {
    pub(crate) fn push(&mut self, waiter: Waiter) {
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
        if let Some(waiter) = self.first.take() {
            waiter.wake();
        } else if !self.rest.is_empty() {
            self.rest.remove(0).wake();
        }
    }

    pub(crate) fn clear(&mut self) {
        self.first = None;
        self.rest.clear();
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

enum ResumeAction {
    Run,
    Interrupt(TaskInterruptFn),
}

type TaskInterruptFn = Box<dyn FnOnce(TaskInterrupt)>;

#[derive(Clone, Copy)]
struct TaskInterrupt {
    task_id: TaskId,
    stack: StackTracker,
}

impl TaskInterrupt {
    fn new(task_id: TaskId, stack: StackTracker) -> Self {
        Self { task_id, stack }
    }

    fn task_id(self) -> usize {
        self.task_id
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
    queued: Vec<bool>,
    ready: VecDeque<TaskId>,
    ring: IoUring,
    probe: Probe,
    registered_file_free: Vec<u32>,
    registered_buffer_free: Vec<u16>,
    io_state_pool: Vec<Rc<IoState>>,
    completed_io: Vec<CompletedIo>,
    in_flight_io: usize,
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
            queued: Vec::new(),
            ready: VecDeque::new(),
            ring,
            probe,
            registered_file_free: (0..config.registered_file_slots).rev().collect(),
            registered_buffer_free: (0..config.registered_buffer_slots).rev().collect(),
            io_state_pool: Vec::with_capacity(config.ring_entries as usize),
            completed_io: Vec::with_capacity(config.ring_entries as usize),
            in_flight_io: 0,
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

    fn reserve_task(&mut self) -> TaskId {
        let id = self.tasks.len();
        self.tasks.push(None);
        self.queued.push(false);
        id
    }

    fn insert_task(&mut self, id: TaskId, task: Task) {
        assert!(self.tasks[id].is_none(), "task slot already occupied");
        self.tasks[id] = Some(task);
        self.schedule(id);
    }

    fn schedule(&mut self, id: TaskId) -> bool {
        if self.tasks.get(id).and_then(Option::as_ref).is_some() && !self.queued[id] {
            self.queued[id] = true;
            self.ready.push_back(id);
            true
        } else {
            false
        }
    }

    fn pop_ready(&mut self) -> Option<(TaskId, Task)> {
        while let Some(id) = self.ready.pop_front() {
            self.queued[id] = false;
            if let Some(task) = self.tasks[id].take() {
                return Some((id, task));
            }
        }

        None
    }

    fn ready_len(&self) -> usize {
        self.ready.len()
    }

    fn is_empty(&self) -> bool {
        self.tasks.iter().all(Option::is_none)
    }

    fn dump_stacks(&mut self) -> StackDump {
        let captures = Rc::new(RefCell::new(Vec::new()));
        let mut dump = StackDump::default();

        for id in 0..self.tasks.len() {
            if let Some(task) = &self.tasks[id]
                && !task.coroutine.started()
            {
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
        F: FnMut(TaskId) -> TaskInterruptFn,
    {
        self.interrupt_tasks(0..self.tasks.len(), interrupt);
    }

    fn interrupt_tasks<I, F>(&mut self, ids: I, mut interrupt: F)
    where
        I: IntoIterator<Item = TaskId>,
        F: FnMut(TaskId) -> TaskInterruptFn,
    {
        for id in ids {
            let Some(task) = &self.tasks[id] else {
                continue;
            };
            if !task.coroutine.started() {
                continue;
            }

            let interrupt = interrupt(id);
            let Some(mut task) = self.tasks[id].take() else {
                continue;
            };

            let result = task.coroutine.resume(ResumeAction::Interrupt(interrupt));
            match result {
                CoroutineResult::Yield(_) => {
                    self.tasks[id] = Some(task);
                }
                CoroutineResult::Return(()) => {}
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

    fn submit_cancel(
        &mut self,
        state: &Rc<IoState>,
        cancel_kind: IoCancelKind,
    ) -> Option<Rc<IoState>> {
        if state.is_ready() || !cancel_kind.is_supported(self) || !state.mark_cancel_requested() {
            return None;
        }

        let cancel_state = self.acquire_io_state();
        let entry = cancel_kind.build();
        self.submit_io_state(entry, Rc::clone(&cancel_state));
        Some(cancel_state)
    }

    fn external_wake(&self) -> Arc<ExternalWake> {
        Arc::clone(&self.external_wake)
    }

    fn has_external_waiters_or_ready(&self) -> bool {
        self.external_wake.has_waiters_or_ready()
    }

    fn drain_external_ready(&mut self) -> bool {
        let ready = self.external_wake.take_ready();
        let mut scheduled = false;
        for id in ready {
            scheduled |= self.schedule(id);
        }
        scheduled
    }

    fn prepare_external_wake(&mut self) -> bool {
        let finished_wake_io = self.finish_external_wake_io();
        let scheduled = self.drain_external_ready();
        finished_wake_io || scheduled
    }

    fn arm_external_wake_io(&mut self) -> bool {
        if self.external_wake_io.is_some()
            || !self.external_wake.has_waiters_or_ready()
            || !self.supports_opcode(opcode::PollAdd::CODE)
        {
            return false;
        }

        self.external_wake.drain_signal();
        if self.drain_external_ready() {
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
            let state = cqe.user_data_ptr() as *const IoState;
            assert!(!state.is_null(), "io_uring CQE missing user data");
            self.in_flight_io = self
                .in_flight_io
                .checked_sub(1)
                .expect("io_uring in-flight count underflow");
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
        if core.borrow().has_external_waiters_or_ready() {
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
                if scheduler.arm_external_wake_io() {
                    return true;
                }
            }
            run_completed_io(core, !busy_poll_io);
            let scheduled = {
                let mut scheduler = core.borrow_mut();
                scheduler.finish_external_wake_io();
                scheduler.drain_external_ready()
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
            scheduler.tasks[id] = Some(task);
            scheduler.schedule(id);
        }
        CoroutineResult::Yield(Suspend::Parked) => {
            scheduler.tasks[id] = Some(task);
        }
        CoroutineResult::Return(()) => {}
    }

    true
}

fn interrupt_scheduler_tasks<I, F>(core: &Rc<SchedulerCell>, ids: I, mut interrupt: F)
where
    I: IntoIterator<Item = TaskId>,
    F: FnMut(TaskId) -> TaskInterruptFn,
{
    for id in ids {
        let Some(mut task) = ({
            let mut scheduler = core.borrow_mut();
            let Some(task) = scheduler.tasks.get(id).and_then(Option::as_ref) else {
                continue;
            };
            if !task.coroutine.started() {
                continue;
            }
            scheduler.tasks[id].take()
        }) else {
            continue;
        };

        let result = task
            .coroutine
            .resume(ResumeAction::Interrupt(interrupt(id)));
        let mut scheduler = core.borrow_mut();
        match result {
            CoroutineResult::Yield(_) => {
                scheduler.tasks[id] = Some(task);
            }
            CoroutineResult::Return(()) => {}
        }
    }
}

fn suspend_task(
    id: TaskId,
    yielder: &Yielder<ResumeAction, Suspend>,
    stack: StackTracker,
    suspend: Suspend,
) {
    let action = yielder.suspend(suspend);
    handle_resume_action(id, yielder, stack, suspend, action);
}

fn handle_resume_action(
    id: TaskId,
    yielder: &Yielder<ResumeAction, Suspend>,
    stack: StackTracker,
    suspend: Suspend,
    mut action: ResumeAction,
) {
    loop {
        match action {
            ResumeAction::Run => return,
            ResumeAction::Interrupt(interrupt) => {
                interrupt(TaskInterrupt::new(id, stack));
                action = yielder.suspend(suspend);
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
        if Rc::strong_count(&state) == 1 {
            recycle_io_state(&Rc::downgrade(core), state);
        }
    }

    core.borrow_mut().completed_io = completed;

    submitted != 0 || had_completion
}

fn submit_cancel(
    core: &Weak<SchedulerCell>,
    state: &Rc<IoState>,
    cancel_kind: IoCancelKind,
) -> Option<Rc<IoState>> {
    core.upgrade()
        .and_then(|core| core.borrow_mut().submit_cancel(state, cancel_kind))
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
        EpollEventData, EpollEventFlags, FallocateFlags, FileAdvice, FutexWaitFlags, IoJoinError,
        IoVec, MemoryAdvice, Mode, MsgHdr, OFlags, RecvFlags, RenameFlags, ResolveFlags, Runtime,
        RuntimeConfig, RuntimeContext, SendFlags, Shutdown, SocketType, StatxFlags, TaskStackState,
        UringSpliceFlags, Waitable, allocation_tracking, ipproto, once,
    };
    use rustix::event::{PollFlags, epoll};
    use rustix::fd::AsFd;
    use rustix::mm::{MapFlags, ProtFlags};
    use rustix::net;
    use rustix::pipe::pipe;
    use std::ffi::c_void;
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
    use std::ptr;
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
                    let waiter = cx
                        .external_waiter()
                        .expect("stackful coroutine should have an external waiter");
                    assert!(waiter_tx.send(waiter).is_ok());
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
                    let waiter = cx
                        .external_waiter()
                        .expect("stackful coroutine should have an external waiter");
                    waiter.wake();
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
                    let waiter = cx
                        .external_waiter()
                        .expect("stackful coroutine should have an external waiter");
                    assert!(waiter_tx.send(waiter).is_ok());
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
                let read = cx.read_async(&read_fd, vec![0]);
                cx.cancel_io(&read).unwrap();
                assert!(read.get(cx).is_err());
                cx.close(read_fd).unwrap();
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
    fn dropped_io_result_is_canceled_before_block_on_returns() {
        let mut runtime = Runtime::new();

        let fds = runtime.block_on(|cx| {
            if !cx.supports_io_uring_opcode(rustix_uring::opcode::AsyncCancel::CODE) {
                return None;
            }

            let (read_fd, write_fd) = pipe().unwrap();
            drop(cx.read_async(&read_fd, vec![0]));
            Some((read_fd, write_fd))
        });

        drop(fds);
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
            let mut read = cx.read_async(&read_fd, vec![0]);
            Cancellable::cancel(&mut read);

            Some((read_fd, write_fd))
        });

        drop(fds);
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

            cx.close(first_read).unwrap();
            cx.close(first_write).unwrap();
            cx.close(second_read).unwrap();
            cx.close(second_write).unwrap();

            (first.buffer[0], second.buffer[0])
        });

        assert_eq!(output, (b'a', b'b'));
    }

    #[test]
    fn async_io_accepts_non_vec_owned_buffers() {
        let mut runtime = Runtime::new();

        let output = runtime.block_on(|cx| {
            let (read_fd, write_fd) = pipe().unwrap();
            let read_buffer = vec![0_u8; 1].into_boxed_slice();
            let write_buffer: Box<[u8]> = Box::new([b'x']);

            let read = cx.read_async(&read_fd, read_buffer);
            let write = cx.write_async(&write_fd, write_buffer);
            let pending: [&dyn Waitable; 2] = [&read, &write];
            cx.join(&pending, Some(Duration::from_secs(1))).unwrap();

            let read = read.get(cx).unwrap();
            let write = write.get(cx).unwrap();
            cx.close(read_fd).unwrap();
            cx.close(write_fd).unwrap();

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

                    cx.close(ping_write).unwrap();
                    cx.close(pong_read).unwrap();
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

                    cx.close(ping_read).unwrap();
                    cx.close(pong_write).unwrap();
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

            cx.close(read_fd).unwrap();
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

            cx.scope(|scope| {
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

                    cx.close(ping_read).unwrap();
                    cx.close(pong_write).unwrap();
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
}
