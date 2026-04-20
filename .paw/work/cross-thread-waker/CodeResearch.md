# Code Research: Cross-Thread Waker

---
work_id: cross-thread-waker
stage: code-research
commit: 35865d7d87d2d2f5bc7cf03d9a5c10a7a9fec88f
branch: feature/cross-thread-waker
repo: https://github.com/bercknash/kimojio-rs
---

## 1. Current Waker Implementation

**File: `kimojio/src/task_ref.rs`**

The waker system is built on `Rc<Task>` and uses a custom `RawWakerVTable`. It is `!Send` and `!Sync` because `Rc` is not thread-safe.

### Static VTABLE (lines 21–49)

A `static VTABLE: std::task::RawWakerVTable` with four function pointers:

- **clone** (lines 22–27): Calls `clone_waker_task(task)` to increment `Rc` refcount, returns a new `RawWaker` with the cloned `Rc` pointer and the same `&VTABLE`.
- **wake** (lines 28–36): Calls `consume_waker_task(task)` (transfers ownership from the raw pointer back into an `Rc`), then calls `TaskState::get()` to borrow the thread-local `TaskState`, then calls `task_state.schedule_io(task)` to mark the task ready. This is the consume-and-wake path — wake takes ownership so drop won't be called.
- **wake_by_ref** (lines 37–44): Calls `clone_waker_task(task)` (increments refcount), then `TaskState::get()` followed by `task_state.schedule_io(task)`. Does not consume the waker.
- **drop** (lines 45–48): Calls `consume_waker_task(task)` to reconstruct the `Rc` and let it drop normally, decrementing the refcount.

### Helper Functions

- **`clone_waker_task(task: *const ())`** (lines 51–61): Casts the raw pointer to `*const Task`, calls `Rc::increment_strong_count(task)` then `Rc::from_raw(task)` to produce a new `Rc<Task>` while keeping the original pointer's reference alive. **SAFETY**: On exit, total refcount is one greater than before entry.
- **`consume_waker_task(task: *const ())`** (lines 63–68): Casts raw pointer to `*const Task`, calls `Rc::from_raw(task)` to take ownership without incrementing the refcount. **SAFETY**: The waker will no longer own its reference.
- **`create_waker(task: Rc<Task>)`** (lines 70–78): Takes an `Rc<Task>`, calls `Rc::into_raw` to get a raw pointer, constructs a `RawWaker` with that pointer and `&VTABLE`, then calls `unsafe { Waker::from_raw(raw) }`.

### `wake_task` Optimization (lines 12–19)

```rust
pub fn wake_task(task_state: &mut TaskState, waker: &std::task::Waker) {
    if waker.vtable() == &VTABLE {
        let task = clone_waker_task(waker.data());
        task_state.schedule_io(task);
    } else {
        waker.wake_by_ref()
    }
}
```

This is an optimization: when the caller already holds a `&mut TaskState` reference (via `TaskStateCellRef`), it avoids the re-entrant `TaskState::get()` call inside the VTABLE's `wake`/`wake_by_ref`. It checks the vtable pointer identity to confirm it's a kimojio waker, then directly clones the `Rc<Task>` and schedules it. If it's a foreign waker, falls back to `wake_by_ref()`.

### Key Problem for Cross-Thread Waker

Both `wake` and `wake_by_ref` in the VTABLE call `TaskState::get()` (line 34, line 42), which accesses the **caller's thread-local** `TASK_STATE`. If called from a foreign thread, this accesses the wrong `TaskState` — or an uninitialized one — causing incorrect behavior or panics. Additionally, `Rc` is `!Send`, so the raw pointer can't safely be sent across threads.

## 2. TaskState and Thread-Local Storage

**File: `kimojio/src/task.rs`**

### Thread-Local Declaration (lines 524–526)

```rust
thread_local! {
    static TASK_STATE: TaskStateCell = TaskStateCell::new(TaskState::default());
}
```

Each thread gets its own `TaskState` wrapped in a `TaskStateCell`. The `TaskState::default()` calls `TaskState::new()`.

### `TaskState::get()` (lines 563–570)

```rust
pub fn get() -> TaskStateCellRef<'static> {
    let task_state = TASK_STATE.with(|task_state| task_state as *const TaskStateCell);
    let task_state = unsafe { &*task_state };
    task_state.borrow_mut()
}
```

Returns a `TaskStateCellRef<'static>` — a mutable borrow guard on the thread-local `TaskState`. The `'static` lifetime is justified by the comment that tasks can't run after their thread is gone.

### TaskState Struct Fields (lines 457–522)

| Field | Type | Purpose |
|-------|------|---------|
| `task_count` | `usize` | Number of live tasks; event loop exits when 0 (line 460) |
| `last_ready_cpu` | `bool` | Tracks if last polled task was CPU-priority (line 465) |
| `thread_id` | `ThreadId` | Thread identity, set at construction (line 467) |
| `ring` | `Ring` | The io_uring instance for this thread (line 470) |
| `ring_poll` | `Ring` | io_uring instance with IOPOLL enabled (line 473) |
| `trace_buffer` | `TraceBuffer` | Tracing infrastructure (line 475) |
| `stats` | `URingStats` | Performance statistics (line 478) |
| `enter_stats` / `enter_stats_wait` | `EnterStats` | Timing stats for enter calls (lines 481–484) |
| `current_task` | `Option<Rc<Task>>` | The currently running task (line 487) |
| `fill_ready_task_list_io` | `VecDeque<Rc<Task>>` | Newly ready tasks awaiting cohort transfer (line 492) |
| `ready_task_list_io` | `VecDeque<Rc<Task>>` | Current I/O cohort of ready tasks (line 496) |
| `ready_task_list_cpu` | `VecDeque<Rc<Task>>` | CPU-priority ready tasks (line 499) |
| `completion_pool` | `VecDeque<Rc<Completion>>` | Recycled completion objects (line 501) |
| `tasks` | `HandleTable<Rc<Task>>` | All live tasks by index (line 504) |
| `completed_tasks` | `VecDeque<Rc<Task>>` | Tasks awaiting drop (line 506) |
| `next_tag` | `u32` | Trace tag counter (line 510) |
| `probe` | `rustix_uring::Probe` | Kernel capability probe (line 513) |
| `keep_running` | `bool` | Event loop continuation flag (line 518) |

### `thread_id` Initialization (line 536)

```rust
thread_id: std::thread::current().id(),
```

Set once during `TaskState::new()` to the constructing thread's ID. This is the value that a cross-thread waker would compare against to determine locality.

### `get_current_thread_id()` (lines 601–603)

Returns `self.thread_id` — a simple field access, no syscall.

### `schedule_io(task)` (lines 712–732)

Public entry point for scheduling a task. Checks `task.get_state()`:
- `Complete` → ignored (task already done)
- `Running` or `Suspend` → sets state to `Ready`, calls `schedule_io_internal(task)`
- `Ready` or `Aborted` → ignored (already scheduled or canceled)
- `Unknown` → panic

### `schedule_io_internal(task)` — The Assert (lines 681–709)

```rust
fn schedule_io_internal(&mut self, task: Rc<Task>) {
    assert_eq!(
        self as *const TaskState, task.task_state,
        "It is incorrect to wake a uringruntime task from another thread"
    );
    // ... tracing, priority check, push to ready list
}
```

**Critical**: Line 682–685 asserts that the current `TaskState` pointer (`self`) matches the task's stored `task_state` pointer. This is the explicit guard against cross-thread waking — it will panic if a task is scheduled on the wrong thread's TaskState. This assert must be preserved for local wakes and bypassed/replaced for cross-thread wakes.

After the assert, it gets a trace tag, records a `TaskScheduled` event, checks if the current task is high_priority (line 701–704), and pushes to either `ready_task_list_io` (high priority) or `fill_ready_task_list_io` (normal).

### `schedule_new()` (lines 655–679)

Creates a new task via `HandleTable::insert_fn`, which calls `Task::new(future, task_index, task_state_ptr, ...)`. The task_state pointer is `self as *const TaskState`. Increments `task_count`, calls `schedule_io_internal`.

### `complete_task()` (lines 766–780)

Static method taking `&TaskStateCell` (not `&mut TaskState`). Sets task state to `Complete`, calls `task.joining_tasks.wake_all()` (which invokes wakers — must happen before borrowing `TaskState`), then borrows `TaskState`, removes the task from `HandleTable`, pushes to `completed_tasks`, decrements `task_count`.

## 3. Task Struct

**File: `kimojio/src/task.rs`** (lines 146–180)

### Fields

| Field | Type | Purpose |
|-------|------|---------|
| `activity_id` | `Cell<uuid::Uuid>` | Tracing activity ID (line 148) |
| `tenant_id` | `Cell<uuid::Uuid>` | Tracing tenant ID (line 151) |
| `active_state` | `MutInPlaceCell<Pin<Box<dyn FutureOrResult>>>` | The future being polled (line 153) |
| `task_index` | `u16` | Index into `HandleTable`, also used as waker identity (line 156) |
| `high_priority` | `Cell<bool>` | Priority inheritance flag (line 160) |
| `ready` | `Cell<TaskReadyState>` | Current lifecycle state (line 168) |
| `tag` | `Cell<u32>` | Trace correlation tag (line 170) |
| `joining_tasks` | `WaitList` | Tasks waiting for this task to complete (line 174) |
| `io_scope_completions` | `MutInPlaceCell<Option<IoScopeCompletions>>` | Tracked I/O for io_scope (line 176) |
| `task_state` | `*const TaskState` | Raw pointer to owning `TaskState` (line 179) |

### Key Observations for Cross-Thread Waker

- `task_index` (u16) is the identity of a task within its thread's `HandleTable`. Combined with a thread identifier, it uniquely identifies a task globally.
- `task_state: *const TaskState` is a raw pointer to the owning thread's `TaskState`. This is used by `schedule_io_internal`'s assert. A cross-thread waker cannot use this pointer — it must use a thread-safe lookup mechanism instead.
- The `Task` is `Rc`-based and `!Send`. The cross-thread waker must NOT carry an `Rc<Task>` — it needs a lightweight identity (task_index + thread_id) instead.

### `Task::new()` (lines 238–262)

Takes `task_index: u16` and `task_state: *const TaskState`, stores them as fields. Initial state is `TaskReadyState::Ready`.

## 4. HandleTable and Index

**File: `kimojio/src/handle_table.rs`**

### Index Type (lines 43–51)

```rust
#[derive(Copy, Clone, Debug, Eq, Hash, Ord, PartialOrd, PartialEq)]
pub struct Index(NonZeroUsize);
```

A `NonZeroUsize` wrapper. `Copy + Clone`, making it cheap to pass around. `get()` returns the underlying `usize`.

### Slot Enum (lines 28–33)

```rust
pub enum Slot<T> {
    Free { next_free: Option<Index> },
    Used { value: T },
}
```

Free slots form a linked list for O(1) reuse.

### HandleTable Operations

- **`insert_fn()`** (lines 87–114): Reuses free slot or appends. Calls the provided closure with the `Index` so the value can reference its own index. Returns the `Index`.
- **`get(index)`** (lines 117–124): Returns `Option<&T>` — `Some` if slot is `Used`, `None` if `Free` or out of bounds.
- **`remove(index)`** (lines 127–144): Returns `Option<T>`. Replaces `Used` with `Free`, adds to free list.

### Relevance to Cross-Thread Waker

Task indices are `NonZeroUsize` values (starting at 1). After `remove()`, an index can be reused for a new task. A stale waker holding an old index will find the new task or `None` — both are safe (spurious wake or no-op). The HandleTable itself is `!Sync` (contains `Vec`) and must only be accessed from the owning thread.

## 5. Runtime Event Loop

**File: `kimojio/src/runtime.rs`**

### `Runtime` Struct (lines 297–301)

```rust
pub struct Runtime {
    busy_poll: BusyPoll,
    server_pipe: OwnedFd,
    client_pipe: OwnedFd,
}
```

### `Runtime::new()` (lines 304–318)

Takes `thread_index: u8` and `Configuration`. Gets `TaskState::get()`, sets trace buffer thread index, creates a `bipipe()` for the runtime server communication channel. Returns `Runtime` with both pipe ends.

### `Runtime::get_handle()` (lines 320–322)

Returns `RuntimeHandle::new(self.client_pipe.try_clone())` — clones the client-side fd for cross-thread use.

### `block_on()` Event Loop (lines 328–411)

1. Asserts future size ≤ `MAX_TASK_STACK_SIZE` (line 335)
2. Gets `TaskState::get()` (line 337)
3. Schedules the main future as a task via `task_state.schedule_new()` (line 345)
4. Schedules a runtime server task via `schedule_runtime_server()` (line 356)
5. **Main loop** (lines 376–396):
   ```
   while task_count > 0 && keep_running:
       submit_and_complete_io_all(task_state, busy_poll)
       prepare_cohort()
       while get_ready() returns a task:
           poll_task(task, task_state)
   ```
6. Cleanup: takes old state, drops references carefully (lines 406–408)

### `poll_task()` (lines 19–76)

Key waker flow:
1. Sets `task_state.current_task = Some(task.clone())` (line 31)
2. **Drops `task_state` via `into_inner()`** (line 37) — releases the `TaskStateCellRef` borrow so that poll can re-borrow
3. Creates waker: `let waker = create_waker(task.clone())` (line 39)
4. Polls the task (line 41)
5. **Re-borrows** `TaskState` via `task_state_ref.borrow_mut()` (line 58) or `TaskState::complete_task()` (line 56)
6. Clears `current_task` (line 61)

This drop/reborrow pattern is critical — it allows the polled future to call `TaskState::get()` internally without triggering a recursive borrow panic.

### `process_completions()` (lines 87–195)

Processes CQEs from io_uring:
1. Reconstitutes `Rc<Completion>` from user_data pointer (line 133)
2. Extracts the waker from `CompletionState::Submitted` via `use_mut` (lines 140–178)
3. Transitions state to `CompletionState::Completed` (line 167)
4. Returns the completion to pool (line 180)
5. **Key pattern** (lines 184–186):
   ```rust
   let task_state_ref = task_state.into_inner();
   waker.wake();
   task_state = task_state_ref.borrow_mut();
   ```
   Drops `TaskStateCellRef` before calling `waker.wake()`, then reborrows. This is because `wake()` internally calls `TaskState::get()`.

### `submit_and_complete_io()` (lines 233–295)

Submits SQEs and waits for CQEs. Decides whether to block (`want = 1`) or poll (`want = 0`) based on whether there are ready tasks. Calls `process_completions()`.

### `submit_and_complete_io_all()` (lines 197–224)

Calls `submit_and_complete_io` twice (once for regular ring, once for iopoll ring). Also handles dropping completed tasks with the same careful borrow pattern.

### Relevance to Cross-Thread Waker

The event loop blocks in `ring.submit_and_wait(want)` when `want = 1` (line 268). A cross-thread wake must interrupt this blocking call. The eventfd approach would submit a poll SQE on the eventfd to the io_uring, so when a cross-thread wake writes to the eventfd, the CQE wakes the loop. Additionally, after processing CQEs, the loop should drain the cross-thread wake channel and schedule the identified tasks.

## 6. CompletionState and Waker Storage

**File: `kimojio/src/lib.rs`** (lines 104–127)

```rust
enum CompletionState {
    Idle { entry: Option<SQE>, timespec: bool },
    Submitted { waker: Waker, activity_id: Uuid, tag: u32, canceled: bool },
    Completed { result: Result<u32, Errno>, ... },
    Terminated,
}
```

### Waker Lifecycle

1. **Created**: When `RingFuture::poll()` transitions `Idle → Submitted` (ring_future.rs:224–229), it stores `cx.waker().clone()` in the `Submitted` variant.
2. **Updated**: On re-poll while `Submitted`, the waker is updated via `waker.clone_from(cx.waker())` (ring_future.rs:241).
3. **Extracted**: In `process_completions()` (runtime.rs:150), the waker is cloned out of `Submitted` before transitioning to `Completed`.
4. **Invoked**: After dropping `TaskStateCellRef`, `waker.wake()` is called (runtime.rs:185).

### `Completion` Struct (lines 129–137)

```rust
pub(crate) struct Completion {
    state: MutInPlaceCell<CompletionState>,
    owned_resources: CompletionResources,
    timespec: rustix_uring::types::Timespec,
    tag: u32,
    task_index: u16,
    iopoll: bool,
}
```

The `state` field uses `MutInPlaceCell<CompletionState>` for interior mutability. Wakers stored here are `std::task::Waker` — currently `!Send` due to being backed by `Rc<Task>`.

## 7. AsyncEvent Waker Usage

**File: `kimojio/src/async_event.rs`**

### WaitData Struct (lines 236–244)

```rust
pub struct WaitData {
    pub activity_id: Uuid,
    pub waker: MutInPlaceCell<Option<Waker>>,
    pub task_id: u16,
    pub canceled: Cell<bool>,
    pub tag: u32,
    pub link: LinkedListLink,
}
```

The `waker` field is `MutInPlaceCell<Option<Waker>>`, set via `set_waker()` (lines 249–253) which clones `cx.waker()`.

### WaitList (lines 288–360)

An intrusive linked list of `Rc<WaitData>` using `intrusive-collections`.

### `wake_one()` (lines 306–333)

Critical scoping pattern:
```rust
fn wake_one(&self) -> bool {
    if let Some(task) = self.wait_data.use_mut(|wait_data| wait_data.pop_front()) {
        {
            // Scope task_state to within this block because the waker.wake()
            // call will invoke the RawWakerVTable APIs that internally will
            // call TaskState::get() which is illegal.
            let task_state = TaskState::get();
            task_state.write_event(...);
        }
        task.waker.use_mut(|waker| {
            if let Some(waker) = waker {
                waker.wake_by_ref()
            }
        });
        true
    } else { false }
}
```

**Key**: The `TaskState` borrow is scoped to a block before `waker.wake_by_ref()` is called. This is because `wake_by_ref` internally calls `TaskState::get()` (via the VTABLE). If `TaskState` were still borrowed, it would panic.

### `wake_all()` (lines 335–337)

Simply loops calling `wake_one()`.

## 8. All Callers of `wake_task`

The `wake_task()` function (task_ref.rs:12–19) is called from three locations:

### Call Site 1: `task.rs:223` — `Task::cancel_io_scope_completions`

```rust
// task.rs lines 219–227
for wait in io_scope_completions.waits.drain(..) {
    wait.canceled.set(true);
    wait.waker.use_mut(|waker| {
        if let Some(waker) = waker {
            wake_task(&mut task_state, waker)
        }
    });
}
```

Called during I/O scope cancellation. The caller already holds a `TaskStateCellRef` (`task_state`), so using `wake_task` avoids re-entrant borrow.

### Call Site 2: `operations.rs:1494` — `io_scope_cancel_and_wait_internal`

```rust
// operations.rs lines 1490–1497
for wait in gathered_completions.waits.drain(..) {
    wait.canceled.set(true);
    wait.waker.use_mut(|waker| {
        if let Some(waker) = waker {
            wake_task(&mut task_state, waker)
        }
    });
}
```

Same pattern as Call Site 1, during I/O scope teardown in operations.

### Call Site 3: `task_ref.rs:12` — The function definition itself

The `wake_task` function is the optimization. It uses vtable pointer comparison (`waker.vtable() == &VTABLE`) to detect kimojio wakers and directly schedule them without going through the VTABLE (which would try to `TaskState::get()`).

**Impact on cross-thread waker**: When the new waker has a different VTABLE, `wake_task` will fall through to `waker.wake_by_ref()`. This is correct as long as the new waker's `wake_by_ref` handles cross-thread properly. However, if `wake_task` is called with the new local-path VTABLE, it should still work as an optimization.

## 9. TaskStateCell

**File: `kimojio/src/task_state_cell.rs`**

### Purpose

A specialized `RefCell` for `TaskState` with zero-overhead in release builds. Enforces single-borrow invariant only in debug builds (line 14: "Turning on the enforcement in release builds causes a 7% regression").

### `TaskStateCell` (lines 20–24)

```rust
pub struct TaskStateCell {
    cell: std::cell::UnsafeCell<TaskState>,
    #[cfg(debug_assertions)]
    borrowed: std::cell::Cell<bool>,
}
```

### `borrow_mut()` (lines 35–49)

Returns `TaskStateCellRef`. In debug builds, panics if already borrowed (line 39: `"TaskStateCell borrowed recursively"`). Sets borrowed flag.

### `TaskStateCellRef` (lines 52–60)

```rust
pub struct TaskStateCellRef<'a> {
    value: &'a TaskStateCell,
    value_ref: &'a mut TaskState,
}
```

Implements `Deref<Target=TaskState>` and `DerefMut`. The `into_inner()` method (line 58) drops the mutable borrow and returns `&'a TaskStateCell`, enabling the reborrow pattern used extensively in the runtime.

### Drop (lines 77–82)

In debug builds, clears the borrowed flag on drop.

### Relevance to Cross-Thread Waker

The entire borrow/drop/reborrow pattern around `waker.wake()` calls exists because the VTABLE's wake function calls `TaskState::get()`, which calls `borrow_mut()`. If the local-thread waker can avoid `TaskState::get()` in the VTABLE (by directly scheduling), this pattern becomes unnecessary for local wakes. However, backward compatibility requires preserving the pattern for foreign wakers that might still call `TaskState::get()`.

## 10. MutInPlaceCell

**File: `kimojio/src/mut_in_place_cell.rs`**

### Design (lines 57–63)

```rust
pub struct MutInPlaceCell<T> {
    cell: std::cell::UnsafeCell<T>,
    recursion_check: recursion_check::RecursionCheck,
    _marker: PhantomData<*const T>,  // makes !Send
}
```

Zero-overhead interior mutability via closure-based access (`use_mut`). Not Sync, but explicitly `Send` if `T: Send` (line 68: `unsafe impl<T: Send> Send for MutInPlaceCell<T> {}`).

### `use_mut()` (lines 97–113)

Takes `FnOnce(&mut T) -> U`, panics on recursive calls in debug. The closure cannot return a reference (compile error), preventing reference escapes.

### Recursion Check

- Debug builds (lines 116–145): Uses `Cell<bool>` to detect and panic on recursive `use_mut` calls.
- Release builds (lines 147–157): No-op — zero overhead.

### Relevance to Cross-Thread Waker

`MutInPlaceCell` is used for:
- `Completion.state` (lib.rs:130) — stores wakers
- `WaitData.waker` (async_event.rs:239) — stores wakers
- `Task.active_state` (task.rs:153) — stores the future
- `Task.io_scope_completions` (task.rs:176)

All of these are `!Sync` (by design) and must only be accessed from the owning thread. The cross-thread waker must NOT access these directly — it must send a message to the owning thread instead.

## 11. Pipe Infrastructure

**File: `kimojio/src/pipe.rs`**

### `bipipe()` (lines 28–36)

```rust
pub fn bipipe() -> (OwnedFd, OwnedFd) {
    socketpair(AddressFamily::UNIX, SocketType::STREAM, SocketFlags::empty(), None).unwrap()
}
```

Creates a bidirectional Unix domain socket pair. Used for cross-thread RuntimeHandle communication.

### `pipe()` (lines 41–50)

Creates a unidirectional pipe via `libc::pipe2`.

### Usage Pattern

The RuntimeHandle system uses `bipipe()` to create communication channels between threads. Each Runtime creates a pipe pair in `Runtime::new()` (runtime.rs:312). The server end stays on the runtime thread; the client end is cloned for cross-thread callers.

## 12. RuntimeHandle

**File: `kimojio/src/runtime_handle.rs`**

### `RuntimeHandle` Struct (lines 43–45)

```rust
pub struct RuntimeHandle {
    client_pipe: MessagePipe<RuntimeServerRequestEnvelope, ()>,
}
```

Wraps a `MessagePipe` to the runtime server. Cloneable (line 42: `#[derive(Clone)]`).

### `open_channel()` (lines 64–77)

Creates a new `bipipe()`, sends an `OpenRequest` to the runtime server over the existing pipe. The server spawns a task that listens on the new pipe.

### `open_channel_sync()` (lines 81–93)

Same as `open_channel` but uses synchronous pipe I/O — safe to call from any thread.

### `close_sync()` (lines 95–98)

Sends a `Shutdown` message synchronously.

### Runtime Server (lines 269–288)

```rust
async fn runtime_server(server_pipe: OwnedFd) -> Result<(), Errno> {
    let server = MessagePipe::<(), RuntimeServerRequestEnvelope>::new(server_pipe);
    loop {
        match *server.recv_message().await? {
            RuntimeServerRequestEnvelope::Shutdown => return Ok(()),
            RuntimeServerRequestEnvelope::Open(mut request) => {
                operations::spawn_task(async move { request.init.handle(request.fd).await });
            }
        };
    }
}
```

Listens for open/shutdown requests. The `recv_message()` call internally submits a read SQE to io_uring and suspends the task until data arrives.

### Relevance to Cross-Thread Waker

The RuntimeHandle/pipe pattern demonstrates that cross-thread communication already works via pipe I/O. However, this is a high-level RPC mechanism — too heavy for individual wake notifications. The cross-thread waker needs a lighter mechanism (eventfd + MPSC channel) that doesn't require per-wake serialization through the message pipe.

## 13. eventfd Availability

### rustix Crate

The project depends on `rustix = { version = "1", features = [...] }` (Cargo.toml:37–47). The current features are: `io_uring`, `mount`, `fs`, `mm`, `process`, `rand`, `system`, `time`, `thread`.

**`rustix::event::eventfd` is available** in rustix v1.1.2 but requires the `"event"` feature, which is **NOT currently enabled**. The `event` module is gated behind `#[cfg(feature = "event")]` in rustix's lib.rs.

**Action required**: Add `"event"` to the rustix features list in workspace `Cargo.toml` to use `rustix::event::eventfd()`.

### Signature

```rust
pub fn eventfd(initval: u32, flags: EventfdFlags) -> Result<OwnedFd>
```

Returns an `OwnedFd` that can be used with io_uring for cross-thread notification. Writing a u64 counter value to the fd signals the event; reading drains the counter.

## 14. Documentation System

### README (README.md)

Standard Rust project README with crate description, prerequisites (Linux 5.15+), installation, hello world example. Links to docs.rs for API documentation.

### No mkdocs/docsify

No documentation site infrastructure found. No `docs/` directory, no `mkdocs.yml`, no `docsify` configuration.

### API Documentation

Standard `cargo doc` — the crate has doc comments throughout. `cargo doc --document-private-items` works.

## 15. Verification Commands

### From CI (`.github/workflows/rust.yml`)

| Command | Purpose |
|---------|---------|
| `cargo clippy --all-targets -- -D warnings` | Lint (treat warnings as errors) |
| `cargo test --all-targets --features ssl2` | Run all tests |
| `cargo test --doc` | Run doc tests |
| `cargo fmt --all -- --check` | Format check |
| `cargo clippy --all-targets --all-features -- -D warnings` | Lint with all features |

### Rust Toolchain

Tested with Rust 1.90.0 and 1.92.0 (CI matrix). Local toolchain specified in `rust-toolchain.toml`.

### Minimum Supported Rust Version

Edition 2024, requires at least Rust 1.90.0.

### Benchmarks (from kimojio/Cargo.toml)

- `iouring` bench
- `mut_in_place_cell_bench`
- `async_bench`
- `async_stream_bench`

## Cross-Cutting Observations

### The Borrow/Drop/Reborrow Pattern

Throughout the codebase, wherever `waker.wake()` or `waker.wake_by_ref()` is called, the code carefully drops any active `TaskStateCellRef` first. This is because the current VTABLE's wake functions call `TaskState::get()` → `borrow_mut()`. Locations:

1. **runtime.rs:184–186** (process_completions): `task_state.into_inner()` → `waker.wake()` → reborrow
2. **runtime.rs:37–58** (poll_task): `task_state.into_inner()` → poll → reborrow
3. **async_event.rs:313–322** (WaitList::wake_one): scope `TaskState::get()` before `waker.wake_by_ref()`
4. **task.rs:771–773** (complete_task): `task.joining_tasks.wake_all()` called before `cell.borrow_mut()`

### Thread Safety Boundaries

Everything inside `TaskState` is `!Send` and `!Sync`:
- `Rc<Task>` references
- `HandleTable<Rc<Task>>`
- `VecDeque<Rc<Task>>` ready lists
- `MutInPlaceCell<T>` (explicitly `!Sync`)
- `Ring` (io_uring instance)

The cross-thread waker must NOT touch any of these. It can only send a lightweight message (task index) to the owning thread.

### Task Identity for Cross-Thread Waker

The waker needs to carry:
1. **`task_index: u16`** — identifies the task in the HandleTable (from `Task.task_index`, task.rs:156)
2. **Thread identifier** — identifies which thread's TaskState owns the task. Options:
   - `std::thread::ThreadId` (from `TaskState.thread_id`, task.rs:467) — `Send + Sync + Copy + Eq`
   - A custom u64 index into a global registry

Both values are small, `Copy`, `Send`, `Sync`, and require no heap allocation or reference counting. This makes the waker data `Send + Sync` without any `Arc` or atomic operations.

### Existing Cross-Thread Patterns

The `RuntimeHandle` / `MessagePipe` system already does cross-thread communication via pipe I/O (reading/writing serialized pointers over Unix sockets). The `RuntimeClientPipeSync` (runtime_handle.rs:136–153) wraps a `std::sync::Mutex<IdMessagePipe>` for sync access from any thread. The cross-thread waker can follow a similar pattern but with a lighter mechanism.
