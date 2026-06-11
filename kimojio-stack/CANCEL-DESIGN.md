# Cancellation, Lifetime, and Reclamation Design

## Goal

This document proposes a single design direction for the related must-fix risks found in the initial `kimojio-stack` review:

1. unregistered async I/O can outlive the borrowed file descriptor identity;
2. wait registration is not cancellation-safe;
3. scope quiescence can ignore externally staged ready tasks;
4. pending async I/O drop/cancel leaks buffers and resources in normal server cancellation paths;
5. long-lived scopes retain completed task metadata and scheduler slots.

The common problem is that several runtime resources are represented as one-way notifications:

- an I/O operation has a CQE waiter list, but the waiter cannot unregister;
- a stackful task can wait in local or external queues, but cancellation cannot retract all registrations;
- an async I/O handle can request cancellation, but ownership of kernel-visible fd and buffer resources is not represented strongly enough to reclaim them deterministically;
- a task slot is allocated forever, and scope state keeps task ids and panic payload slots after children finish.

The proposed fix is to introduce explicit lifecycle objects for tasks, waits, and I/O operations. Each lifecycle object has an identity, an owner, and a cleanup path that runs both on normal completion and on cancellation.

## Design principles

1. **Cancellation is a protocol, not a side effect.** Dropping a handle should request cancellation and detach the user-facing handle, but the runtime must still own enough state to drive the kernel and scheduler to a final state.
2. **Every wait registration returns a guard.** If a task times out, is canceled, or unwinds, dropping the guard unregisters or invalidates the waiter so it cannot consume a future wake.
3. **A wake must validate task generation.** Stale task ids must not reschedule a slot that has been canceled, completed, or reused.
4. **Kernel-visible resources must be owned by the operation state.** The operation state must keep fds, buffers, and registered resource leases alive until the CQE is reaped.
5. **Runtime memory scales with live work, not total historical work.** Completed tasks should free scheduler slots, and scope state should remove per-child metadata when the child reaches a terminal state.
6. **Keep the local-runtime fast path local.** The design should preserve the single-thread scheduler model and avoid atomics on local wait paths. Cross-thread wait paths can use atomic registration state because they already cross thread boundaries.

## Proposed runtime concepts

### `TaskKey`

Replace bare `TaskId` in waiters and external wake queues with:

```rust
struct TaskKey {
    slot: usize,
    generation: u32,
}
```

The scheduler stores each slot as:

```rust
struct TaskSlot {
    generation: u32,
    task: Option<Task>,
    queued: bool,
    canceling: bool,
    wait_set: TaskWaitSet,
}
```

`Scheduler::reserve_task` either reuses a free slot or pushes a new slot, then returns the current `TaskKey`. `schedule(TaskKey)` validates the generation and returns `false` for stale keys. Completion increments or tombstones the slot generation before placing the slot on `free_task_slots`.

This fixes stale waiters and external wakes rescheduling an unrelated task after slot reuse, and it makes task-slot reuse safe.

### `WaitRegistration`

Change `Waitable::add_waiter` from a fire-and-forget method into a registration-returning method:

```rust
pub trait Waitable {
    fn is_ready(&self) -> bool;

    #[doc(hidden)]
    fn register_waiter(&self, cx: &RuntimeContext<'_>) -> Option<WaitRegistration>;
}
```

`WaitRegistration` is a small RAII guard:

```rust
pub struct WaitRegistration {
    kind: WaitRegistrationKind,
    active: bool,
}

impl WaitRegistration {
    pub fn disarm(mut self) {
        self.active = false;
    }
}

impl Drop for WaitRegistration {
    fn drop(&mut self) {
        if self.active {
            self.kind.unregister_or_invalidate();
        }
    }
}
```

Local wait lists should not require O(N) removal from the hot path. Use token invalidation:

```rust
struct WaitToken {
    task: TaskKey,
    registration_id: u32,
}

struct Waiter {
    token: WaitToken,
}
```

Each task owns a `TaskWaitSet` containing active local registration ids. Registering a waiter allocates a per-task registration id and inserts it into the task wait set. Dropping the guard removes that id from the task wait set. Waking a waiter calls `Scheduler::schedule_if_wait_active(token)`, which succeeds only if:

- `token.task` is still the current task generation;
- the task is not canceled/completed;
- the registration id is still active.

If the token is stale, wake skips it and continues to the next waiter. `wake_one` must therefore loop until it schedules a live waiter or the queue is empty.

This makes timeout, cancellation, and panic-unwind cleanup safe without expensive removal from every local primitive.

### Cross-thread `ExternalWaitRegistration`

Cross-thread wait queues need explicit removal because they keep `Arc` registrations and manipulate external waiter counts. Replace `ExternalWaiter` with a two-part registration:

```rust
pub(crate) struct ExternalWaitRegistration {
    external_wake: Weak<ExternalWake>,
    task: TaskKey,
    state: Arc<AtomicExternalWaitState>,
}
```

States:

- `Waiting`: queue owns the registration and may wake it;
- `Woken`: external wake owns a staged ready task;
- `Canceled`: waiter was removed before wake;
- `Consumed`: scheduler drained the staged wake.

Registering a cross-thread waiter increments the external waiter count only after it is inserted in the channel wait queue. Dropping the RAII registration attempts `Waiting -> Canceled`; if successful, it removes or tombstones the queue entry and decrements the external waiter count. Waking attempts `Waiting -> Woken`; if successful, it decrements waiters, pushes the `TaskKey` into `ExternalWakeInner.ready`, and signals the eventfd/condvar.

The external ready queue stores `TaskKey`, not `TaskId`. Draining external ready calls `schedule(TaskKey)` and marks the external state `Consumed`. Stale or canceled ready entries are discarded.

This fixes cross-thread waiter leaks, underflows, and stale wakeups after task cancellation.

### `OperationState`

Replace `IoState`'s current mixed waiter/result/cancel/resource state with an operation lifecycle state:

```rust
struct OperationState {
    id: OperationId,
    phase: OperationPhase,
    result: Option<KernelIoResult>,
    waiters: WaitQueue,
    cancel: CancelState,
    resources: OperationResources,
}

enum OperationPhase {
    Building,
    Submitted,
    CancelRequested,
    Completed,
    Reaped,
}
```

`OperationResources` owns everything the kernel may still reference:

```rust
struct OperationResources {
    fd: Option<OperationFd>,
    buffer: Option<Box<dyn OperationBuffer>>,
    registered: RegisteredResources,
}
```

The operation state remains scheduler-owned until the CQE is reaped and the result is consumed or discarded. User-facing handles (`IoResult`, `Timeout`) hold an `Rc<OperationState>` plus typed output conversion, but dropping the handle does not drop kernel-visible resources. It only changes the user interest state.

## Addressing risk 1: fd lifetime for unregistered async I/O

### Current problem

`read_async` and `write_async` accept `&impl AsFd`, copy the raw fd into the SQE, and return an `IoResult` that does not carry the fd borrow or own an fd. If the caller closes or drops the fd before the SQE is submitted and consumed by the kernel, the numeric fd can be reused for unrelated I/O.

### Proposed API split

Keep synchronous operations borrowed because they do not return until CQE completion:

```rust
pub fn read(&self, fd: &impl AsFd, buf: &mut [u8]) -> Result<usize, Errno>;
pub fn write(&self, fd: &impl AsFd, buf: &[u8]) -> Result<usize, Errno>;
```

Change unregistered async operations to require a runtime-owned fd handle with cheap clone semantics:

```rust
pub struct IoFd { /* Rc<IoFdState> or scheduler fd-table handle */ }

impl IoFd {
    pub fn from_owned(fd: OwnedFd) -> Self;
}

impl Clone for IoFd {
    fn clone(&self) -> Self;
}

pub fn read_async<B>(&self, fd: &IoFd, buffer: B) -> IoResult<ReadOutput<B>, B>;
pub fn write_async<B>(&self, fd: &IoFd, buffer: B) -> IoResult<WriteOutput<B>, B>;
```

`IoFd` consumes an `OwnedFd` once at construction time and keeps that fd alive until all cloned handles and pending operations release it. The clone operation must not make a kernel call. It should initially be an `Rc<IoFdState>` because `kimojio-stack` is a local, thread-per-core runtime. If `Rc` overhead or cache locality becomes measurable, the same public type can be backed by a scheduler-owned fd table:

```rust
struct IoFd {
    table: Rc<FdTable>,
    slot: u32,
    generation: u32,
}
```

The fd-table variant makes `IoFd` a compact generational handle and centralizes fd reclamation in the scheduler. Both representations preserve the key invariant: an async operation clones or leases a runtime-owned fd identity cheaply, and the underlying `OwnedFd` cannot close until the operation is reaped.

Do not provide borrowed async fd APIs as the default. A borrowed async API cannot be both safe against fd reuse and cheap without requiring the caller to already hold a stable runtime-owned fd identity. If a convenience adapter is ever added, it should be explicit and outside the hot path, for example:

```rust
pub fn into_io_fd(fd: OwnedFd) -> IoFd;
```

Callers with existing `TcpStream`, `File`, or socket objects should transfer ownership into `IoFd` once and then cheaply clone `IoFd` for operations. They should not pay a syscall per read or write.

For fixed-file APIs, keep the existing registered fd path. `RegisteredFdState` already owns an `OwnedFd` and registered-resource leases hold the state until CQE completion. The design should add tests for pending registered handle drops, but the API shape is sounder than unregistered borrowed async I/O.

### Submission invariant

After `submit_io_state` returns, the `OperationState` owns all kernel-visible resources until CQE reaping:

- unregistered fd: cheap `IoFd` clone/lease that keeps the original `OwnedFd` alive;
- registered fd: `Rc<RegisteredFdState>` lease;
- unregistered buffer: boxed typed buffer stored in the operation;
- registered buffer: `Rc<RegisteredBufferState<_>>` lease.

The user handle may detach, cancel, or be forgotten without invalidating those resources.

## Addressing risks 2 and 3: wait and external-wake lifecycle

### Local wait registration flow

Every blocking loop should follow this pattern:

```rust
loop {
    if condition.is_ready() {
        return consume();
    }

    let registration = condition.register_waiter(cx);

    if condition.is_ready() {
        drop(registration);
        continue;
    }

    cx.park_with_registration(registration);
}
```

`park_with_registration` stores the guard in the task's current wait set before suspending. On normal wake, the guard is disarmed as part of consuming the wake. On cancellation/unwind, the guard drops and invalidates the waiter.

For multi-wait:

```rust
let registrations = waitables
    .iter()
    .filter_map(|waitable| waitable.register_waiter(cx))
    .collect::<WaitRegistrationSet>();

if timeout fires or any waitable wins {
    drop(registrations);
}
```

Dropping the set invalidates all non-winning waiters. This fixes stale registrations left by timed `wait_any`/`wait_all`.

### Cross-thread registration flow

Stackful cross-thread channel `send`/`recv` should register with a guard:

1. Try the queue fast path.
2. Lock the wait queue.
3. Recheck readiness under the wait queue lock.
4. Insert `ExternalWaitRegistration`.
5. Park with the registration guard.
6. On wake, drop/disarm the guard and retry the queue fast path.

If scope cancellation interrupts the task while parked, the guard is dropped from the task's active wait set. That cancels the external registration before the task unwinds out of the coroutine.

### Scheduler quiescence

`wait_for_scope` must not decide cancellation from `ready_len()` alone. It should first synchronize all scheduler-visible wake sources:

```rust
fn has_runnable_or_staged_work_for_scope(&self, scope: &ScopeState) -> bool {
    let mut scheduler = self.core.borrow_mut();
    scheduler.prepare_external_wake();
    scheduler.has_ready_task_in_scope(scope) || scheduler.has_completion_that_may_wake_scope(scope)
}
```

Minimum viable fix:

- call `prepare_external_wake()` before checking quiescence;
- make external ready entries generation-checked `TaskKey`s;
- if any external ready entry schedules a task in the scope, do not cancel;
- run one scheduler tick after draining external ready before deciding no runnable work remains.

Longer term, each task slot should know its owning scope id. Then the scheduler can answer "does this scope have runnable/staged work?" without cloning all scope task ids.

## Addressing risk 4: server-safe async cancellation and reclamation

### Separate user interest from kernel ownership

`IoResult::drop` should not mean "forget everything and leak the buffer." It should mean:

1. mark the user handle detached;
2. request cancellation if the operation is still submitted;
3. move the operation to a scheduler-owned detached operation list;
4. keep fd and buffer resources in `OperationState`;
5. reap the CQE later and reclaim resources deterministically.

```rust
enum UserInterest {
    Attached,
    DetachedCancelRequested,
    DetachedCompleteSilently,
}
```

For `Drop`, default to `DetachedCancelRequested`. For an explicit future API, consider:

- `detach(self)`: keep running and discard result;
- `cancel(self)`: request cancellation and return immediately;
- `cancel_and_wait(self, cx)`: request cancellation and wait for original and cancel CQEs;
- `into_reaper(self)`: produce a handle that can be polled/joined by application-level cleanup logic.

### Detached operation reaper

The scheduler owns:

```rust
detached_ops: VecDeque<Rc<OperationState>>,
```

When a detached op completes, `run_completed_io` observes that no user result is needed and drops `OperationResources`. If cancellation submission is unsupported, failed, or races with completion, the operation still remains in `detached_ops` until the original CQE arrives. This avoids buffer leaks while preserving safety.

`Runtime::block_on` already drains in-flight I/O before return. With detached ops, that drain becomes the final cleanup guarantee:

- by `block_on` return, all submitted operations are either completed/reaped or the runtime has panicked because the kernel made no forward progress;
- no buffer/fd leak remains from ordinary dropped `IoResult`s.

### Backpressure and cancellation storm behavior

Add runtime counters and optional limits:

```rust
pub struct RuntimeConfig {
    pub max_detached_ops: usize,
    pub max_in_flight_cancels: usize,
}
```

Defaults can be generous and effectively unbounded for alpha, but server users need observability:

- current in-flight ops;
- detached ops;
- cancel requests submitted;
- cancels unsupported;
- cancels completed;
- original completions after cancel.

The important behavior is deterministic ownership, not immediate reclamation. Memory is reclaimed when CQEs arrive, not when the user handle drops.

## Addressing risk 5: task and scope metadata reclamation

### Scheduler slot reuse

When a task reaches `CoroutineResult::Return(())`, call:

```rust
scheduler.finish_task(task_key);
```

`finish_task` should:

- clear `tasks[slot].task`;
- clear `queued`;
- cancel/drop any remaining wait registrations in the task wait set;
- increment the slot generation;
- push the slot onto `free_task_slots`;
- notify the owning scope that the task reached a terminal state.

The generation increment must happen before wake queues can observe the slot as reusable.

### Scope child table

Replace `ScopeStateInner`'s append-only `task_ids` and `panic_payloads` vectors with a table keyed by `TaskKey`:

```rust
struct ScopeChild {
    task: TaskKey,
    join: Weak<dyn JoinCompletion>,
    panic_payload: Rc<RefCell<Option<PanicPayload>>>,
    state: ChildState,
}

enum ChildState {
    Running,
    Completed,
    Canceled,
}
```

On child completion:

- decrement `remaining`;
- remove or tombstone the child entry;
- keep panic payload only if it contains an unobserved panic;
- keep result only through `JoinState`, not through scope state;
- if `remaining == 0`, wake scope waiters.

For unobserved panic propagation, scope state does not need to retain every child forever. It needs a single pending panic slot or a small panic queue:

```rust
pending_panic: Option<PanicPayload>
```

The child writes into scope's pending panic on panic completion. The scope no longer needs a vector of per-child `Rc<RefCell<Option<PanicPayload>>>`.

### Long-lived server scopes

For process-lifetime accept loops, encourage this shape:

- root scope owns the accept loop task;
- each connection/request uses a nested scope;
- nested scope completion reclaims all child task metadata;
- task slots are reused across connection churn.

The runtime should make the memory behavior correct even if a user keeps one long-lived scope and spawns many short-lived children, but docs should still teach nested scopes as the structured unit of connection/request lifetime.

## Interaction between these changes

The five risks should be fixed together because partial fixes leave holes:

- `TaskKey` generation makes stale local and external waiters safe, and enables scheduler slot reuse.
- `WaitRegistration` guards let scope cancellation cleanly unwind blocked tasks without leaving registrations behind.
- External wake generation checks prevent a staged ready task from being canceled or misapplied after slot reuse.
- `OperationState` ownership keeps fd/buffer resources alive even if the user handle drops or a task is canceled.
- Detached operation reaping avoids buffer leaks while relying on the same wait/cancel lifecycle.
- Scope child cleanup depends on terminal task notifications from `finish_task`.

## Implementation plan

### Phase 1: Task identity and scheduler slot reuse

1. Introduce `TaskKey { slot, generation }`.
2. Replace scheduler-facing `TaskId` in `Task`, `Waiter`, `ExternalWakeInner.ready`, stack dumps, and scope task tracking.
3. Add `free_task_slots`.
4. Implement generation validation in `schedule`, `pop_ready`, and external wake drain.
5. Add tests:
   - stale waiter cannot schedule a reused slot;
   - many spawn/join iterations reuse bounded scheduler slots;
   - stale external wake for old generation is discarded.

### Phase 2: Local wait registration guards

1. Add `WaitRegistration`, `WaitRegistrationSet`, and `TaskWaitSet`.
2. Change `Waitable::add_waiter` to `register_waiter`.
3. Update local primitives, `JoinHandle`, `Timeout`, and `IoResult`.
4. Make `wake_one` skip stale/inactive waiters.
5. Add tests:
   - `wait_any` timeout does not consume a later mutex/channel wake;
   - canceled task waiting on a local primitive leaves no stale waiter;
   - repeated timeouts do not grow waiter queues.

### Phase 3: External wait registration guards

1. Redesign `ExternalWaiter` as `ExternalWaitRegistration`.
2. Store generation-checked `TaskKey` in external ready queues.
3. Add cancel/remove support to cross-thread channel wait queues.
4. Update stackful cross-thread `send`/`recv` to park with guards.
5. Add tests:
   - scope cancellation while stackful cross-thread receiver is parked removes the external waiter;
   - a message racing with scope cancellation cannot underflow external waiter counts;
   - externally staged ready task prevents scope quiescence cancellation.

### Phase 4: Operation ownership and fd lifetime

1. Introduce `OperationState` and `OperationResources`.
2. Move fd/buffer/registered-resource ownership into operation state before SQE submission.
3. Add `IoFd` as the required unregistered async fd identity.
4. Back `IoFd` with `Rc<IoFdState>` initially, while keeping the public type compatible with a future scheduler fd table.
5. Remove borrowed async fd APIs before stabilization.
6. Add tombstone/queue-compaction benchmarks for waiter invalidation strategies.
7. Add tests:
   - `IoFd` keeps the underlying fd alive until pending async operations are reaped;
   - dropping all user-visible `IoFd` clones while an operation is pending does not close the kernel-visible fd early;
   - dropping `IoResult` before CQE reclaims buffer and fd after drain;
   - registered fd/buffer dropped while pending is reclaimed only after CQE.

### Phase 5: Detached operation reaper

1. Add scheduler-owned detached operation list.
2. Change `IoResult::drop` and `Timeout::drop` to detach and request cancel, not leak.
3. Reap detached operations on every CQE pass and in `Runtime::block_on` drain.
4. Add runtime counters for detached/cancel state.
5. Add tests:
   - timeout storm with dropped async reads returns memory to bounded baseline after CQE drain;
   - unsupported cancel path still reclaims resources when original CQE completes;
   - `block_on` returns only after detached operation resources are dropped.

### Phase 6: Scope metadata cleanup

1. Replace append-only scope vectors with task-keyed child tracking.
2. Store only pending unobserved panics in scope state.
3. Remove child metadata on terminal state.
4. Add tests:
   - one long-lived scope spawning many completed children keeps bounded child metadata;
   - unobserved panic still propagates after child metadata cleanup;
   - nested scopes reclaim metadata independently.

## API compatibility

This crate is still `0.0.1-alpha.0`, so prefer fixing API shape now rather than preserving unsafe ergonomics.

Recommended breaking changes before stabilization:

- remove borrowed async fd APIs from the hot path;
- reserve async fd APIs for `IoFd` and registered fd variants;
- change hidden `Waitable::add_waiter` internals freely;
- add explicit detach/cancel/cancel-and-wait semantics to `IoResult`;
- document that cancellation completion is eventual and resource reclamation happens at CQE reaping.

## Decisions and open questions

Decisions:

1. `IoResult::drop` requests cancellation by default. Applications that intentionally want to ignore a still-running operation should call an explicit `detach`.
2. Borrowed async fd APIs should not duplicate fds. Async operations use a cheap-clone `IoFd`/registered-fd identity instead.
3. Cancellation-safe `Waitable` registration remains private for now. Public waitable implementations need a stable contract later.

Open question:

1. How much tombstoning is acceptable in wait queues?
   - Token invalidation avoids O(N) removal on cancellation, but queues must be compacted opportunistically when stale entries accumulate.
   - Add tests and benchmarks that vary waiter churn, timeout rate, cancellation rate, queue length, and wake-one/wake-all patterns.
   - Use the results to choose compaction thresholds rather than guessing upfront.

## Success criteria

The design is complete when these properties hold:

- safe async I/O cannot target a reused unregistered fd;
- a timed-out or canceled wait cannot consume a later wake;
- cross-thread wait cancellation leaves no external waiter count, queue, or staged-ready corruption;
- scope exit only cancels tasks after draining scheduler, I/O, and external wake sources relevant to that scope;
- dropped pending async I/O is eventually reclaimed without leaking buffers/fds;
- scheduler task slots and scope child metadata are bounded by live or recently-live work, not total historical task count;
- tests cover cancellation races, timeout races, fd reuse, resource reclamation, and high-churn long-lived scopes.
