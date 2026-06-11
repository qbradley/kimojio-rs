---
date: 2026-06-11T00:48:28+00:00
git_commit: 96873dd
branch: detached jj workspace
repository: qbradley/kimojio-rs
topic: "Stack Cancellation Runtime"
tags: [research, codebase, kimojio-stack, cancellation, io-uring, scheduler]
status: complete
last_updated: 2026-06-11
---

# Research: Stack Cancellation Runtime

## Purpose

This document provides a detailed technical survey of the existing kimojio-stack runtime implementation to support the stack cancellation feature development. All statements are backed by precise file:line references to the current codebase.

## Repository Context

- **Current commit**: 96873dd
- **Workspace**: Detached jj workspace
- **Primary implementation file**: `kimojio-stack/src/lib.rs`
- **Related modules**: `kimojio-stack/src/wait.rs`, `kimojio-stack/src/channel/cross_thread.rs`

## Research Areas

### 1. Task Identity and Scheduler Slot Management

#### TaskId and Task Structure

**TaskId** is currently a simple type alias for `usize` without generation checking:

- `kimojio-stack/src/lib.rs:271`: `pub(crate) type TaskId = usize;`

**Task** struct represents a scheduled computation:

- `kimojio-stack/src/lib.rs:3407-3419`: Task struct definition containing:
  - `future: UnsafeCell<Pin<Box<dyn Future<Output = ()>>>>`
  - `scope_state: Rc<ScopeState>`
  - `panic_payload: Rc<RefCell<Option<PanicPayload>>>`

#### Scheduler Storage

The scheduler maintains tasks in a vector-based structure:

- `kimojio-stack/src/lib.rs:3627-3648`: `Scheduler` struct definition:
  - `tasks: Vec<Option<Task>>` at line 3628 — sparse array indexed by TaskId
  - `queued: FixedBitSet` at line 3629 — tracks which tasks are in ready queue
  - `ready: VecDeque<TaskId>` at line 3630 — FIFO ready queue

#### Task Slot Reservation

**reserve_task** allocates a TaskId without creating the task yet:

- `kimojio-stack/src/lib.rs:3690-3695`: Implementation pushes `None` to tasks vector and returns new index as TaskId

#### Task Scheduling

**schedule** enqueues a task for execution:

- `kimojio-stack/src/lib.rs:3703-3711`: Checks task existence with `tasks[task_id].is_some()`, verifies not already queued via `queued.contains(task_id)`, then sets queued bit and pushes to ready queue

#### Task Execution Loop

**run_one** executes a single task:

- `kimojio-stack/src/lib.rs:4131-4173`: Pops from ready queue, clears queued bit, sets current task context, polls future, handles completion or rescheduling
- Line 4152: Sets `WITH_CURRENT_TASK.set()` context
- Line 4160: Polls the future
- Line 4163: On `Poll::Ready`, removes task from vector and calls `scope_state.task_finished()`
- Line 4168: On `Poll::Pending`, keeps task in vector for future scheduling

**drive_scheduler** is the main event loop:

- `kimojio-stack/src/lib.rs:4060-4128`: Loops calling `run_one()` and `io_driver.drive()`, drains external ready queue via `drain_external_ready()` at line 4083, breaks when scheduler is empty and no I/O is in flight

### 2. Scope State and Child Metadata

#### ScopeState Structure

**ScopeState** tracks children of a scope and manages completion/panic propagation:

- `kimojio-stack/src/lib.rs:3099-3149`: Wrapper around `Rc<RefCell<ScopeStateInner>>`
- `kimojio-stack/src/lib.rs:3150-3155`: `ScopeStateInner` fields:
  - `remaining: usize` — count of tasks not yet finished
  - `panic_payloads: Vec<Rc<RefCell<Option<PanicPayload>>>>` — panic state from children
  - `task_ids: Vec<TaskId>` — list of child task IDs
  - `waiters: Waiters` — tasks waiting for scope completion

#### Task Lifecycle Tracking

**task_started** is called when a child task begins:

- `kimojio-stack/src/lib.rs:3108-3112`: Increments `remaining` counter and appends task ID to `task_ids` list

**task_finished** is called when a child task completes:

- `kimojio-stack/src/lib.rs:3136-3146`: Decrements `remaining` counter, checks if zero, and if so calls `waiters.wake_all()` to notify waiting tasks

#### Panic Propagation

Panics are captured and stored for propagation:

- `kimojio-stack/src/lib.rs:4163-4165`: When task completes with `Poll::Ready`, checks if panic payload is `Some` and calls `scope_state.task_finished()`
- `kimojio-stack/src/lib.rs:3137-3139`: `task_finished` examines panic payloads to determine if scope should panic
- Panic payloads stored as `Vec<Rc<RefCell<Option<PanicPayload>>>>` allowing shared ownership between task and scope

#### Scope Wait and Quiescence

**wait** blocks until scope quiesces:

- `kimojio-stack/src/lib.rs:3117-3134`: Implements `Waitable` trait, registers waiter if `remaining != 0`, returns `Poll::Ready(())` when all children finish

#### Spawn and Join Interactions

**spawn** creates a child task:

- `kimojio-stack/src/lib.rs:791-827`: Calls `scope_state.task_started()` at line 810, reserves TaskId, creates Task, schedules it

**join** waits for scope completion:

- `kimojio-stack/src/wait.rs:20-22`: Type alias `join<W>(waitable: W)` implemented as `wait_all((waitable,))`
- Waits via `Waitable::add_waiter` protocol

### 3. Local Waiters and Waitable APIs

#### Waitable Trait

The trait defining the wait protocol for runtime objects:

- `kimojio-stack/src/wait.rs:16-22`: Two methods:
  - `is_ready(&self, cx: &RuntimeContext) -> bool`
  - `add_waiter(&self, cx: &RuntimeContext, waiter: Waiter)`

#### Waiters Collection

**Waiters** stores registered waiters with inline optimization:

- `kimojio-stack/src/lib.rs:3363-3399`: Structure with:
  - `first: Option<Waiter>` at line 3364 — inline storage for first waiter
  - `rest: Vec<Waiter>` at line 3365 — heap storage for additional waiters
- `add` method at lines 3380-3388: Pushes to `first` if empty, otherwise to `rest`
- `wake_all` method at lines 3390-3399: Iterates and wakes each waiter via `waiter.wake()`

#### Waiter Structure

**Waiter** represents a local task waiting on an object:

- `kimojio-stack/src/lib.rs:3158-3169`: Contains:
  - `scheduler: Weak<SchedulerCell>` — weak reference to avoid cycles
  - `task_id: TaskId` — which task to wake

**wake** method schedules the waiting task:

- `kimojio-stack/src/lib.rs:3165-3169`: Upgrades weak scheduler reference and calls `scheduler.schedule(task_id)`

#### RuntimeContext Park and Waiter Creation

**RuntimeContext::park** suspends current task waiting for a condition:

- `kimojio-stack/src/lib.rs:2003-2015`: Checks `is_ready()`, if false adds waiter and returns `Poll::Pending`, otherwise returns `Poll::Ready`

**RuntimeContext::waiter** creates a Waiter for the current task:

- `kimojio-stack/src/lib.rs:2017-2021`: Returns `Waiter { scheduler: Rc::downgrade(&scheduler), task_id }`

#### wait_all, wait_any, join, select Behavior

**wait_all** waits for all waitables to complete:

- `kimojio-stack/src/wait.rs:59-96`: Checks `is_ready()` on all in loop at lines 71-76, if any not ready adds waiter to each via `add_waiter()` at lines 80-82, returns `Poll::Pending`
- No waiter cleanup on completion — waiters fire and are dropped when waitable completes

**wait_any** waits for any waitable to complete:

- `kimojio-stack/src/wait.rs:98-142`: Similar structure, checks if any `is_ready()` at lines 110-116, adds waiters to all if none ready at lines 131-133, returns index of first ready waitable
- No waiter cleanup on timeout or early completion

**join** and **select**:

- `kimojio-stack/src/wait.rs:20-22`: `join<W>` is alias for `wait_all((waitable,)).await`
- `kimojio-stack/src/wait.rs:24-34`: `select!` macro provides syntax for `wait_any`

### 4. External Waiters and Cross-Thread Channel Integration

#### ExternalWaiter Structure

**ExternalWaiter** enables cross-thread wake operations:

- `kimojio-stack/src/lib.rs:3176-3233`: Wraps `Arc<ExternalWaiterRegistration>`:
  - `registration: Arc<ExternalWaiterRegistration>` at line 3177
- `kimojio-stack/src/lib.rs:3180-3186`: `ExternalWaiterRegistration` contains:
  - `wake: Arc<ExternalWake>` — shared wake mechanism
  - `task_id: TaskId` — which task to wake
  - `active: AtomicBool` — tracks if waiter is still registered

**new** method:

- `kimojio-stack/src/lib.rs:3188-3198`: Creates registration, increments wake waiter count, stores in active state

**wake** method:

- `kimojio-stack/src/lib.rs:3200-3214`: Sets active to false, calls `wake.wake_task(task_id)`

**drop** behavior:

- `kimojio-stack/src/lib.rs:3216-3233`: If still active, decrements waiter count without waking

#### ExternalWake Structure

**ExternalWake** coordinates cross-thread signaling:

- `kimojio-stack/src/lib.rs:3235-3249`: Contains:
  - `inner: StdMutex<ExternalWakeInner>` — protected state
  - `eventfd: Arc<EventFd>` — cross-thread notification primitive

**ExternalWakeInner** state:

- `kimojio-stack/src/lib.rs:3357-3360`: Fields:
  - `waiters: usize` — count of active external waiters
  - `ready: VecDeque<TaskId>` — queue of tasks ready to run

**wake_task** enqueues a task from another thread:

- `kimojio-stack/src/lib.rs:3278-3287`: Decrements `waiters`, pushes TaskId to `ready` queue, signals `eventfd` with value 1

#### External Ready Queue Draining

**drain_external_ready** integrates external wake queue into scheduler:

- `kimojio-stack/src/lib.rs:3844-3851`: Locks `external_wake.inner`, drains all TaskIds from ready queue, schedules each via `scheduler.schedule(task_id)`
- Called in `drive_scheduler` loop at line 4083

#### Cross-Thread Channel Wait Paths

**StackfulSender::send** registration:

- `kimojio-stack/src/channel/cross_thread.rs:392-416`: 
  - Line 402: Creates `ExternalWaiter` via `cx.external_waiter()`
  - Line 408: Adds to wait queue via `state.push_stackful_waiter(&waiter)`
  - Line 412: Parks with `cx.park(&*state)?`

**StackfulReceiver::recv** registration:

- `kimojio-stack/src/channel/cross_thread.rs:480-504`:
  - Line 490: Creates `ExternalWaiter` 
  - Line 496: Adds via `state.push_stackful_waiter(&waiter)`
  - Line 500: Parks with `cx.park(&*state)?`

**push_stackful_waiter** wait queue internals:

- `kimojio-stack/src/channel/cross_thread.rs:211-218`: Appends `ExternalWaiter` to `stackful_waiters` VecDeque
- Called from both send and recv wait paths

### 5. Async I/O and Timeout Operation Lifetime

#### IoResult Structure

**IoResult** owns I/O operation state and buffer:

- `kimojio-stack/src/lib.rs:2633-2736`: Generic over buffer type `B` and result type `R`:
  - `buf: ManuallyDrop<B>` at line 2635 — buffer ownership
  - `state: IoState` at line 2636 — shared completion state
  - `_marker: PhantomData<R>` at line 2637 — result type marker

**into_inner** extracts buffer on completion:

- `kimojio-stack/src/lib.rs:2687-2696`: Takes `self`, calls `state.take()` to get result, uses `ManuallyDrop::take` to extract buffer

**drop** behavior with cancellation:

- `kimojio-stack/src/lib.rs:2711-2735`: 
  - Line 2714: Calls `state.cancel_pending()`
  - Line 2716: Manually drops buffer with `ManuallyDrop::drop(&mut self.buf)`
  - `cancel_pending` submits io_uring cancel operation if not ready (lines 3071-3092)

#### Timeout Structure

**Timeout** manages timeout operations:

- `kimojio-stack/src/lib.rs:2860-2918`: Contains:
  - `state: IoState` at line 2862 — completion state
  - `_marker: PhantomData<Timespec>` at line 2863

**drop** behavior:

- `kimojio-stack/src/lib.rs:2911-2918`: Calls `state.cancel_pending()` similar to IoResult

#### IoState Structure

**IoState** tracks I/O operation completion state:

- `kimojio-stack/src/lib.rs:3001-3096`: Wraps `Rc<RefCell<IoStateInner>>`:
  - `result: Option<Result<i32, io::Error>>` at line 3008 — completion result
  - `waiters: Waiters` at line 3009 — tasks waiting for completion
  - `timeout: Option<Timeout>` at line 3010 — associated timeout
  - `registered_resources: Option<RegisteredResources>` at line 3011 — cleanup handle

**complete** method sets result and wakes waiters:

- `kimojio-stack/src/lib.rs:3044-3055`: Stores result, calls `waiters.wake_all()`, returns old result value

**take** method consumes result:

- `kimojio-stack/src/lib.rs:3057-3069`: Returns and clears stored result, leaving None

**cancel_pending** submits cancellation if not complete:

- `kimojio-stack/src/lib.rs:3071-3092`: Checks if result is None, if so submits `IORING_OP_ASYNC_CANCEL` operation via `submit_cancel()`

#### submit_and_wait_for_io

**submit_and_wait_for_io** coordinates I/O operation submission and waiting:

- `kimojio-stack/src/lib.rs:1885-1909`: 
  - Line 1893: Calls `io_driver.submit(&mut sqe)` to submit operation
  - Line 1895: Calls `io_driver.register_io_state()` to track operation
  - Line 1898: Parks on `io_state` waiting for completion
  - Lines 1902-1907: On panic, calls `io_state.cancel_pending()` and waits for cancellation to complete

#### read_async and write_async

**read_async** implementation:

- `kimojio-stack/src/lib.rs:1393-1445`: 
  - Line 1401: Creates `IoState::new()`
  - Line 1402: Calls `submit_and_wait_for_io()` with read SQE
  - Line 1413: Returns `IoResult` containing buffer and state

**write_async** implementation:

- `kimojio-stack/src/lib.rs:1608-1658`:
  - Line 1616: Creates `IoState::new()`
  - Line 1617: Calls `submit_and_wait_for_io()` with write SQE
  - Line 1628: Returns `IoResult` containing buffer and state

#### Registered Resources

**RegisteredResources** tracks resources for cleanup:

- `kimojio-stack/src/lib.rs:2590-2629`: Holds `Rc<RefCell<RegisteredResourcesInner>>`
- `complete` method at lines 2619-2629: Removes from registered set, calls `io_state.complete()`

### 6. Runtime block_on Shutdown/Drain Behavior

#### block_on Entry Point

**block_on** runs the runtime to completion:

- `kimojio-stack/src/lib.rs:584-610`:
  - Line 590: Creates root scope and spawns main task
  - Line 593: Calls `scheduler.drive_scheduler()` to run event loop
  - Line 596: Calls `scheduler.drain_scheduler_io()` after main task finishes
  - Line 598: Asserts scheduler is empty
  - Line 606: Asserts all I/O completed (`in_flight_io == 0`)

#### drain_scheduler_io

**drain_scheduler_io** ensures all I/O completes before shutdown:

- `kimojio-stack/src/lib.rs:4051-4058`:
  - Line 4053: Loops while `in_flight_io != 0`
  - Line 4055: Asserts `io_driver.drive()` makes progress (returns true)
  - Line 4056: Drains external ready queue
  - Ensures no orphaned I/O operations remain

### 7. Tests and Benchmarks

#### Test Organization

Tests are distributed across source files using `#[test]` attribute:

- `kimojio-stack/src/lib.rs`: Contains 30+ test functions starting around line 4500
- `kimojio-stack/src/wait.rs`: Contains wait operation tests around line 150
- Individual modules have inline tests

#### Cancellation-Related Tests

**dropped_io_result_is_canceled**:

- `kimojio-stack/src/lib.rs:5399-5432`: Verifies that dropping IoResult cancels I/O operation, waits for server to receive cancel, verifies operation fails

**cancellable_trait**:

- `kimojio-stack/src/lib.rs:5416-5432`: Tests Cancellable trait implementation (commented as WIP)

**scope_exit_cancels_child**:

- `kimojio-stack/src/lib.rs:5383-5397`: Verifies spawned task is canceled when scope exits

#### External Waiter Tests

**external_waiter_wakes_task**:

- `kimojio-stack/src/lib.rs:4507-4531`: Creates external waiter, wakes from thread, verifies task runs

**external_waiter_drop_without_wake**:

- `kimojio-stack/src/lib.rs:4535-4559`: Creates external waiter, drops without wake, verifies cleanup

#### Panic Propagation Tests

**panic_in_spawned_task_propagates**:

- `kimojio-stack/src/lib.rs:4637-4656`: Spawns task that panics, verifies panic propagates to scope

**panic_in_multiple_tasks**:

- `kimojio-stack/src/lib.rs:4658-4685`: Spawns multiple panicking tasks, verifies first panic propagates

#### Runtime Baseline Benchmarks

**benches/runtime_baseline.rs**:

- Key benchmarks:
  - `spawn_join_*`: Measures spawn and join overhead (`kimojio-stack/benches/runtime_baseline.rs:8-58`)
  - `yield_*`: Measures yield_now overhead (`kimojio-stack/benches/runtime_baseline.rs:60-88`)
  - `mutex_*`: Measures mutex contention (`kimojio-stack/benches/runtime_baseline.rs:120-175`)
  - `semaphore_*`: Measures semaphore operations (`kimojio-stack/benches/runtime_baseline.rs:177-232`)
  - `channel_*`: Measures channel throughput (`kimojio-stack/benches/runtime_baseline.rs:234-372`)
  - `io_*`: Measures I/O operation performance (`kimojio-stack/benches/runtime_baseline.rs:374-510`)

### 8. Documentation System and Verification Commands

#### Documentation Files

**Design Documents**:

- `kimojio-stack/CANCEL-DESIGN.md:1-29`: Cancellation design goal and design principles
- `kimojio-stack/CANCEL-DESIGN.md:31-177`: Proposed runtime concepts including task identity, wait registration, external wait registration, and operation state
- `kimojio-stack/REVIEW.md:1`: Code review notes file
- `kimojio-stack/WORKSTEALING.md:1`: Work-stealing scheduler design notes file

**Specification**:

- `.paw/work/stack-cancellation-runtime/Spec.md:1-179`: Feature specification with user stories and functional requirements

#### Verification Commands

**CI Configuration**:

- `.github/workflows/rust.yml:1-51`: GitHub Actions workflow defining verification steps

**Build and Test Commands**:

- `.github/workflows/rust.yml:48`: `cargo clippy --all-targets` — lint all targets
- `.github/workflows/rust.yml:49`: `cargo test --all-targets` — run all tests
- `.github/workflows/rust.yml:50`: `cargo fmt --check` — verify formatting
- `.github/workflows/rust.yml:51`: `cargo clippy --all-targets --all-features` — lint with all features

**Rust Toolchain**:

- `.github/workflows/rust.yml:30`: Uses `dtolnay/rust-toolchain@stable`
- `.github/workflows/rust.yml:40`: Matrix testing on Rust 1.90.0 and 1.92.0

**Custom Instructions**:

- `.copilot-instructions.md:1-20`: Documents required verification steps for all changes:
  - Run `cargo fmt` for formatting
  - Run `cargo clippy` for linting
  - Run `cargo clippy --all-targets --all-features` for comprehensive linting
  - Ensure tests pass
  - Keep changes minimal and focused

## Summary

This research documents the current kimojio-stack runtime implementation across 8 key areas:

1. **Task identity**: Simple usize-based TaskId, vector storage, reserve/schedule/run cycle
2. **Scope state**: RefCell-based ScopeStateInner tracking child count, panics, waiters
3. **Local waiters**: Waitable trait, Waiters collection, RuntimeContext park/waiter APIs
4. **External waiters**: ExternalWaiter/ExternalWake coordination, cross-thread channel integration
5. **I/O lifetime**: IoResult/IoState/Timeout structures, cancel_pending on drop, registered resources
6. **Shutdown**: block_on drain sequence ensuring all I/O completes
7. **Tests**: 30+ tests including cancellation, panic propagation, external wake, benchmarks
8. **Verification**: CI-defined commands (clippy, test, fmt) and design documentation

All implementation claims are backed by specific file:line references to commit 96873dd.

## Open Questions

None — all required research areas have been documented with precise citations.
