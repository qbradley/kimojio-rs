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
use std::fmt;
use std::marker::PhantomData;
use std::panic::{self, AssertUnwindSafe};
use std::rc::{Rc, Weak};

use corosensei::stack::DefaultStack;
use corosensei::{Coroutine, CoroutineResult, Yielder};

const DEFAULT_STACK_SIZE: usize = 64 * 1024;

type TaskId = usize;
type PanicPayload = Box<dyn Any + Send + 'static>;

/// Runs stackful coroutines on the current OS thread.
#[derive(Debug)]
pub struct Runtime {
    stack_size: usize,
}

impl Runtime {
    /// Creates a runtime that uses a small guarded stack for each coroutine.
    pub fn new() -> Self {
        Self {
            stack_size: DEFAULT_STACK_SIZE,
        }
    }

    /// Creates a runtime with a custom usable stack size for each coroutine.
    ///
    /// The stack allocator adds a guard page so stack overflow does not corrupt
    /// adjacent memory.
    pub fn with_stack_size(stack_size: usize) -> Self {
        Self { stack_size }
    }

    /// Runs `main` to completion on the current OS thread.
    pub fn block_on<F, T>(&mut self, main: F) -> T
    where
        F: FnOnce(&RuntimeContext<'_>) -> T,
    {
        let core = Rc::new(RefCell::new(Scheduler::default()));
        let cx = RuntimeContext {
            core: Rc::clone(&core),
            stack_size: self.stack_size,
            current: CurrentTask::Root,
        };
        let output = main(&cx);

        debug_assert!(
            core.borrow().is_empty(),
            "all stackful coroutines should be scoped"
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
                run_one(&self.core);
            }
            CurrentTask::Coroutine { yielder, .. } => {
                yielder.suspend(Suspend::Ready);
            }
        }
    }

    fn wait_for_scope(&self, state: &Rc<ScopeState>) {
        while state.remaining.get() != 0 {
            if let Some(waiter) = self.waiter() {
                state.waiters.borrow_mut().push(waiter);
            }
            self.park();
        }
    }

    fn park(&self) {
        match self.current {
            CurrentTask::Root => {
                assert!(
                    run_one(&self.core),
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

#[derive(Default)]
struct Scheduler {
    tasks: Vec<Option<Task>>,
    queued: Vec<bool>,
    ready: VecDeque<TaskId>,
}

impl Scheduler {
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

    fn is_empty(&self) -> bool {
        self.tasks.iter().all(Option::is_none)
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
}
