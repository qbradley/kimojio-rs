// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Stackful coroutines with structured concurrency.
//!
//! This crate is intentionally not an async runtime. It runs stackful coroutines
//! cooperatively on the current OS thread and exposes scoped spawning so
//! coroutines cannot outlive the scope that created them.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::fmt;
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::panic::{self, AssertUnwindSafe};
use std::rc::{Rc, Weak};

use corosensei::stack::DefaultStack;
use corosensei::{Coroutine, CoroutineResult, Yielder};
pub use rustix::fd::OwnedFd;
use rustix::fd::{AsFd, AsRawFd};
use rustix::io_uring::io_uring_user_data;
pub use rustix_uring::Errno;
use rustix_uring::opcode;
use rustix_uring::{cqueue::Entry as Cqe, squeue::Entry as Sqe, types::Fd};

const DEFAULT_STACK_SIZE: usize = 64 * 1024;
const DEFAULT_RING_ENTRIES: u32 = 128;

type TaskId = usize;
type PanicPayload = Box<dyn Any + Send + 'static>;
type IoResult = Result<u32, Errno>;
type IoUring = rustix_uring::IoUring<Sqe, Cqe>;

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
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            stack_size: DEFAULT_STACK_SIZE,
            ring_entries: DEFAULT_RING_ENTRIES,
            ring_enter_policy: RingEnterPolicy::default(),
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

    /// Creates a runtime with custom configuration.
    pub fn with_config(config: RuntimeConfig) -> Self {
        Self { config }
    }

    /// Runs `main` to completion on the current OS thread.
    pub fn block_on<F, T>(&mut self, main: F) -> T
    where
        F: FnOnce(&RuntimeContext<'_>) -> T,
    {
        let core = Rc::new(RefCell::new(Scheduler::new(self.config)));
        let cx = RuntimeContext {
            core: Rc::clone(&core),
            stack_size: self.config.stack_size,
            current: CurrentTask::Root,
        };
        let output = main(&cx);

        let scheduler = core.borrow();
        debug_assert!(
            scheduler.is_empty(),
            "all stackful coroutines should be scoped"
        );
        debug_assert_eq!(
            scheduler.in_flight_io, 0,
            "io_uring operations should complete before Runtime::block_on returns"
        );

        output
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

/// Methods available to code running inside a [`Runtime`].
pub struct RuntimeContext<'cx> {
    core: Rc<RefCell<Scheduler>>,
    stack_size: usize,
    current: CurrentTask<'cx>,
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
        let scope = Scope {
            core: Rc::clone(&self.core),
            state: Rc::clone(&state),
            stack_size: self.stack_size,
            _scope: PhantomData,
            _env: PhantomData,
        };

        let result = panic::catch_unwind(AssertUnwindSafe(|| f(&scope)));
        self.wait_for_scope(&state);

        match result {
            Ok(output) => output,
            Err(payload) => panic::resume_unwind(payload),
        }
    }

    /// Cooperatively yields the current stackful coroutine.
    pub fn yield_now(&self) {
        match self.current {
            CurrentTask::Root => {
                drive_scheduler(&self.core);
            }
            CurrentTask::Coroutine { yielder, .. } => {
                yielder.suspend(Suspend::Ready);
            }
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

    /// Closes `fd` using io_uring.
    ///
    /// The fd is consumed immediately and will not be closed again on drop.
    pub fn close(&self, fd: OwnedFd) -> Result<(), Errno> {
        let fd = ManuallyDrop::new(fd);
        let fd = fd.as_fd().as_raw_fd();
        let entry = opcode::Close::new(Fd(fd)).build();

        self.submit_and_wait_for_io(entry).map(|_| ())
    }

    fn wait_for_scope(&self, state: &Rc<ScopeState>) {
        while state.remaining.get() != 0 {
            if let Some(waiter) = self.waiter() {
                state.waiters.borrow_mut().push(waiter);
            }
            self.park();
        }
    }

    fn submit_and_wait_for_io(&self, entry: Sqe) -> IoResult {
        let state = Rc::new(IoState::new());
        let user_data =
            io_uring_user_data::from_ptr(Rc::into_raw(Rc::clone(&state)) as *mut c_void);
        let entry = entry.user_data(user_data);
        self.core.borrow_mut().submit_io(&entry);

        loop {
            if let Some(result) = state.take_result() {
                return result;
            }

            if let Some(waiter) = self.waiter() {
                state.set_waiter(waiter);
            }
            self.park();
        }
    }

    fn park(&self) {
        match self.current {
            CurrentTask::Root => {
                assert!(
                    drive_scheduler(&self.core),
                    "kimojio-stack runtime deadlocked: no runnable stackful coroutines"
                );
            }
            CurrentTask::Coroutine { yielder, .. } => {
                yielder.suspend(Suspend::Parked);
            }
        }
    }

    fn waiter(&self) -> Option<Waiter> {
        match self.current {
            CurrentTask::Root => None,
            CurrentTask::Coroutine { id, .. } => Some(Waiter {
                core: Rc::downgrade(&self.core),
                task_id: id,
            }),
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
        yielder: &'cx Yielder<(), Suspend>,
    },
}

/// A structured concurrency scope for stackful coroutines.
pub struct Scope<'scope, 'env: 'scope> {
    core: Rc<RefCell<Scheduler>>,
    state: Rc<ScopeState>,
    stack_size: usize,
    _scope: PhantomData<&'scope mut &'scope ()>,
    _env: PhantomData<&'env mut &'env ()>,
}

impl<'scope, 'env: 'scope> Scope<'scope, 'env> {
    /// Spawns a stackful coroutine in this scope.
    ///
    /// The returned handle can be joined to retrieve the coroutine's return
    /// value. If the handle is dropped, the coroutine still finishes before the
    /// scope returns.
    pub fn spawn<F, T>(&self, f: F) -> JoinHandle<'scope, T>
    where
        F: FnOnce(&RuntimeContext<'_>) -> T + 'scope,
        T: 'scope,
    {
        let id = self.core.borrow_mut().reserve_task();
        let state = Rc::clone(&self.state);
        let task_state = Rc::clone(&state);
        let join = Rc::new(JoinState::new());
        let task_join = Rc::clone(&join);
        let core = Rc::clone(&self.core);
        let stack_size = self.stack_size;
        let stack = DefaultStack::new(self.stack_size)
            .expect("failed to allocate stackful coroutine stack");

        let coroutine = unsafe {
            // SAFETY: spawned closures and their return slots are bounded by the
            // public scope lifetime. `RuntimeContext::scope` waits for every task
            // counted in `ScopeState` before returning, and `JoinHandle` cannot
            // escape the scope that created it.
            Coroutine::with_stack_unchecked(stack, move |yielder, ()| {
                let cx = RuntimeContext {
                    core: Rc::clone(&core),
                    stack_size,
                    current: CurrentTask::Coroutine { id, yielder },
                };

                let result = panic::catch_unwind(AssertUnwindSafe(|| f(&cx)))
                    .map_err(JoinError::from_payload);
                task_join.complete(result);
                task_state.task_finished();
            })
        };

        state.remaining.set(state.remaining.get() + 1);
        self.core.borrow_mut().insert_task(id, Task { coroutine });

        JoinHandle {
            state: join,
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
    _scope: PhantomData<&'scope mut T>,
}

impl<T> JoinHandle<'_, T> {
    /// Waits for the stackful coroutine to finish and returns its result.
    pub fn join(self, cx: &RuntimeContext<'_>) -> Result<T, JoinError> {
        loop {
            if let Some(result) = self.state.take_result() {
                return result;
            }

            if let Some(waiter) = cx.waiter() {
                self.state.waiters.borrow_mut().push(waiter);
            }
            cx.park();
        }
    }
}

impl<T> fmt::Debug for JoinHandle<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JoinHandle").finish_non_exhaustive()
    }
}

/// Error returned when a stackful coroutine panics.
pub struct JoinError {
    payload: PanicPayload,
}

impl JoinError {
    fn from_payload(payload: PanicPayload) -> Self {
        Self { payload }
    }

    /// Returns the panic payload.
    pub fn into_payload(self) -> PanicPayload {
        self.payload
    }
}

impl fmt::Debug for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JoinError").finish_non_exhaustive()
    }
}

impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("stackful coroutine panicked")
    }
}

impl std::error::Error for JoinError {}

struct JoinState<T> {
    result: RefCell<Option<Result<T, JoinError>>>,
    waiters: RefCell<Vec<Waiter>>,
}

impl<T> JoinState<T> {
    fn new() -> Self {
        Self {
            result: RefCell::new(None),
            waiters: RefCell::new(Vec::new()),
        }
    }

    fn complete(&self, result: Result<T, JoinError>) {
        *self.result.borrow_mut() = Some(result);
        for waiter in self.waiters.borrow_mut().drain(..) {
            waiter.wake();
        }
    }

    fn take_result(&self) -> Option<Result<T, JoinError>> {
        self.result.borrow_mut().take()
    }
}

struct IoState {
    result: RefCell<Option<IoResult>>,
    waiter: RefCell<Option<Waiter>>,
}

impl IoState {
    fn new() -> Self {
        Self {
            result: RefCell::new(None),
            waiter: RefCell::new(None),
        }
    }

    fn complete(&self, result: IoResult) {
        *self.result.borrow_mut() = Some(result);
        if let Some(waiter) = self.waiter.borrow_mut().take() {
            waiter.wake();
        }
    }

    fn set_waiter(&self, waiter: Waiter) {
        *self.waiter.borrow_mut() = Some(waiter);
    }

    fn take_result(&self) -> Option<IoResult> {
        self.result.borrow_mut().take()
    }
}

#[derive(Default)]
struct ScopeState {
    remaining: Cell<usize>,
    waiters: RefCell<Vec<Waiter>>,
}

impl ScopeState {
    fn task_finished(&self) {
        let remaining = self
            .remaining
            .get()
            .checked_sub(1)
            .expect("scope task count underflow");
        self.remaining.set(remaining);

        if remaining == 0 {
            for waiter in self.waiters.borrow_mut().drain(..) {
                waiter.wake();
            }
        }
    }
}

#[derive(Clone)]
struct Waiter {
    core: Weak<RefCell<Scheduler>>,
    task_id: TaskId,
}

impl Waiter {
    fn wake(self) {
        if let Some(core) = self.core.upgrade() {
            core.borrow_mut().schedule(self.task_id);
        }
    }
}

#[derive(Clone, Copy)]
enum Suspend {
    Ready,
    Parked,
}

struct Task {
    coroutine: Coroutine<(), Suspend, ()>,
}

struct Scheduler {
    tasks: Vec<Option<Task>>,
    queued: Vec<bool>,
    ready: VecDeque<TaskId>,
    ring: IoUring,
    in_flight_io: usize,
    ring_enter_policy: RingEnterPolicy,
}

impl Scheduler {
    fn new(config: RuntimeConfig) -> Self {
        Self {
            tasks: Vec::new(),
            queued: Vec::new(),
            ready: VecDeque::new(),
            ring: IoUring::new(config.ring_entries).expect("failed to create io_uring"),
            in_flight_io: 0,
            ring_enter_policy: config.ring_enter_policy,
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

    fn schedule(&mut self, id: TaskId) {
        if self.tasks.get(id).and_then(Option::as_ref).is_some() && !self.queued[id] {
            self.queued[id] = true;
            self.ready.push_back(id);
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

    fn enter_ring(&mut self, want: usize) -> (usize, Vec<CompletedIo>) {
        let submitted = if self.should_enter(want) {
            self.submit_and_wait(want)
        } else {
            0
        };

        let mut completed = Vec::new();
        for cqe in self.ring.completion() {
            let state = cqe.user_data_ptr() as *const IoState;
            assert!(!state.is_null(), "io_uring CQE missing user data");
            self.in_flight_io = self
                .in_flight_io
                .checked_sub(1)
                .expect("io_uring in-flight count underflow");
            completed.push(CompletedIo {
                state,
                result: cqe.result(),
            });
        }

        (submitted, completed)
    }
}

struct CompletedIo {
    state: *const IoState,
    result: IoResult,
}

fn drive_scheduler(core: &Rc<RefCell<Scheduler>>) -> bool {
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

    let should_wait_for_io = core.borrow().in_flight_io != 0;
    if should_wait_for_io {
        run_completed_io(core, true);
        true
    } else {
        run_completed_io(core, false)
    }
}

fn run_one(core: &Rc<RefCell<Scheduler>>) -> bool {
    let Some((id, mut task)) = core.borrow_mut().pop_ready() else {
        return false;
    };

    let result = task.coroutine.resume(());
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

fn run_completed_io(core: &Rc<RefCell<Scheduler>>, wait: bool) -> bool {
    let (submitted, completed) = {
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

        scheduler.enter_ring(usize::from(wait))
    };

    let had_completion = !completed.is_empty();
    for completion in completed {
        let state = unsafe { Rc::from_raw(completion.state) };
        state.complete(completion.result);
    }

    submitted != 0 || had_completion
}

/// A single-send channel for stackful coroutines.
pub mod once {
    use super::{RuntimeContext, Waiter};
    use std::cell::RefCell;
    use std::fmt;
    use std::rc::Rc;

    /// Creates a channel that can deliver one value.
    pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
        let inner = Rc::new(RefCell::new(Inner {
            value: None,
            sender_alive: true,
            receiver_alive: true,
            receiver_waiter: None,
        }));

        (
            Sender {
                inner: Rc::clone(&inner),
            },
            Receiver { inner },
        )
    }

    /// Sends one value to a [`Receiver`].
    pub struct Sender<T> {
        inner: Rc<RefCell<Inner<T>>>,
    }

    impl<T> Sender<T> {
        /// Sends `value` to the receiver.
        pub fn send(self, value: T) -> Result<(), SendError<T>> {
            let waiter = {
                let mut inner = self.inner.borrow_mut();
                if !inner.receiver_alive {
                    return Err(SendError(value));
                }

                inner.sender_alive = false;
                inner.value = Some(value);
                inner.receiver_waiter.take()
            };

            if let Some(waiter) = waiter {
                waiter.wake();
            }

            Ok(())
        }
    }

    impl<T> fmt::Debug for Sender<T> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Sender").finish_non_exhaustive()
        }
    }

    impl<T> Drop for Sender<T> {
        fn drop(&mut self) {
            let waiter = {
                let mut inner = self.inner.borrow_mut();
                inner.sender_alive = false;
                inner.receiver_waiter.take()
            };

            if let Some(waiter) = waiter {
                waiter.wake();
            }
        }
    }

    /// Receives one value from a [`Sender`].
    pub struct Receiver<T> {
        inner: Rc<RefCell<Inner<T>>>,
    }

    impl<T> Receiver<T> {
        /// Blocks cooperatively until the value is sent or the sender is dropped.
        pub fn recv(self, cx: &RuntimeContext<'_>) -> Result<T, RecvError> {
            loop {
                {
                    let mut inner = self.inner.borrow_mut();
                    if let Some(value) = inner.value.take() {
                        inner.receiver_alive = false;
                        return Ok(value);
                    }

                    if !inner.sender_alive {
                        inner.receiver_alive = false;
                        return Err(RecvError);
                    }

                    inner.receiver_waiter = cx.waiter();
                }

                cx.park();
            }
        }
    }

    impl<T> fmt::Debug for Receiver<T> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Receiver").finish_non_exhaustive()
        }
    }

    impl<T> Drop for Receiver<T> {
        fn drop(&mut self) {
            self.inner.borrow_mut().receiver_alive = false;
        }
    }

    struct Inner<T> {
        value: Option<T>,
        sender_alive: bool,
        receiver_alive: bool,
        receiver_waiter: Option<Waiter>,
    }

    /// Error returned when the receiver has been dropped.
    pub struct SendError<T>(pub T);

    impl<T> fmt::Debug for SendError<T> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_tuple("SendError").finish()
        }
    }

    impl<T> fmt::Display for SendError<T> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("receiver dropped")
        }
    }

    impl<T: fmt::Debug> std::error::Error for SendError<T> {}

    /// Error returned when the sender is dropped before sending.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct RecvError;

    impl fmt::Display for RecvError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("sender dropped")
        }
    }

    impl std::error::Error for RecvError {}
}

#[cfg(test)]
mod tests {
    use super::{Runtime, once};
    use rustix::pipe::pipe;

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

                assert_eq!(sender.join(cx).unwrap(), "sent");
                receiver.join(cx).unwrap()
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

                first.join(cx).unwrap() + second.join(cx).unwrap()
            })
        });

        assert_eq!(output, 41);
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

                other.join(cx).unwrap() + yielding.join(cx).unwrap()
            })
        });

        assert_eq!(output, 42);
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
                        child_output = child.join(cx).unwrap();
                    });
                    child_output + 1
                });

                parent.join(cx).unwrap()
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

                initiator.join(cx).unwrap() + responder.join(cx).unwrap()
            })
        });

        assert_eq!(output, (ROUNDS as usize) * 2);
    }
}
