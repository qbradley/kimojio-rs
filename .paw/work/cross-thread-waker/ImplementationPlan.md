# Cross-Thread Waker Implementation Plan

## Overview

Replace kimojio's `Rc<Task>`-based waker with a lightweight, `Send + Sync` index-based waker that packs a `thread_index: u8` and `task_index: u16` directly into the `RawWaker` data pointer — no heap allocation, no reference counting. On the local thread, a thread-local comparison confirms locality and the task is scheduled directly via `HandleTable` lookup with zero synchronization. On foreign threads, the waker posts the task index into a per-thread wake queue (protected by the global registry mutex) and writes to an `eventfd` to interrupt the event loop.

## Current State Analysis

The waker system in `task_ref.rs` wraps an `Rc<Task>` in a `RawWakerVTable`. It is `!Send` because `Rc` is `!Send`. The vtable's `wake`/`wake_by_ref` functions call `TaskState::get()` (thread-local access) then `schedule_io()`, which asserts same-thread origin via pointer comparison. Every call site in the codebase carefully scopes `TaskStateCellRef` borrows to avoid reentrancy panics when invoking wakers.

Key existing infrastructure:
- `TaskState.thread_id: ThreadId` — cached at construction, cheap to compare (`task.rs:467`)
- `TaskState.tasks: HandleTable<Rc<Task>>` — O(1) lookup by `Index(NonZeroUsize)` (`task.rs:504`)
- `Task.task_index: u16` — the task's identity within the HandleTable (`task.rs:156`)
- `TraceBuffer.thread_idx: u8` — zero-based thread index passed to `Runtime::new()` (`task.rs:300`, `runtime.rs:304`)
- `schedule_io_internal` assert at `task.rs:682-685` — explicitly guards against cross-thread wake
- `wake_task()` at `task_ref.rs:12-19` — optimization for when caller already holds `&mut TaskState`
- `rustix` dependency already present, needs `"event"` feature for `eventfd`

## Desired End State

- `std::task::Waker` produced by kimojio is `Send + Sync` per the standard library contract
- Local-thread wake path: unpack packed data → thread-local comparison → `HandleTable` lookup → `schedule_io`. No mutex, no atomic, no `Arc`.
- Cross-thread wake path: unpack packed data → lock global registry → push task index to per-thread queue → write `eventfd` → owning thread drains queue on next event loop iteration
- `wake_task()` optimization preserved — uses vtable pointer comparison + thread-index locality check + direct index lookup
- All existing tests pass without modification

## What We're NOT Doing

- Making `Task` or any of its fields thread-safe (it stays `Rc`-based, single-threaded)
- Multi-threaded task stealing or migration between runtime threads
- Changing `HandleTable` data structure
- Adding integration layers for specific external runtimes (tokio, async-std)
- Adding generation counters to HandleTable indices (ABA produces harmless spurious wake per spec)
- Changing `MutInPlaceCell`, `TaskStateCell`, or the borrow/drop/reborrow patterns

## Phase Status
- [ ] **Phase 1: Index-Based Waker (Local Thread)** - Replace Rc<Task>-based waker with packed task_index+thread_index identity, local path only
- [ ] **Phase 2: Cross-Thread Wake Infrastructure** - Add global registry, eventfd, wake queue, cross-thread path, and tests
- [ ] **Phase 3: Documentation** - Docs.md and project documentation updates

## Phase Candidates
<!-- No candidates at this time -->

---

## Phase 1: Index-Based Waker (Local Thread)

### Objective

Replace the `Rc<Task>`-based waker with an index-based waker that packs `thread_index: u8` and `task_index: u16` into the `RawWaker` data pointer. This phase implements only the local-thread wake path — cross-thread wakes will panic (preserving the existing assert behavior). The resulting waker is `Send + Sync` at the type level even though cross-thread invocation is not yet functional.

### Changes Required

- **`kimojio/src/task_ref.rs`**: Complete rewrite of the waker module.
  - Remove `clone_waker_task()` and `consume_waker_task()` helper functions (no more `Rc` in wakers)
  - Add `pack_waker_data(thread_index: u8, task_index: u16) -> *const ()` and corresponding `unpack_thread_index(*const ()) -> u8`, `unpack_task_index(*const ()) -> u16` helpers
  - Add thread-local `KIMOJIO_THREAD_INDEX: Cell<u8>` (sentinel `u8::MAX` = not a kimojio thread) and `set_kimojio_thread_index(u8)` setter. Include `debug_assert!(thread_index < u8::MAX)` in the setter to guard against sentinel collision with valid thread indices.
  - Replace `VTABLE` with new `RawWakerVTable`:
    - `clone`: return `RawWaker::new(data, &VTABLE)` — data is `Copy`, no allocation
    - `wake`: unpack data, check thread locality via `KIMOJIO_THREAD_INDEX`, if local → `TaskState::get()` + `HandleTable` lookup + `schedule_io`; if foreign → panic for now (Phase 2 adds cross-thread path)
    - `wake_by_ref`: same logic as `wake` (no ownership distinction needed since data is `Copy`)
    - `drop`: no-op (nothing to free)
  - Update `create_waker` signature: takes `task_index: u16` instead of `Rc<Task>`. Reads `KIMOJIO_THREAD_INDEX` thread-local for the thread_index component.
  - Update `wake_task`: change vtable comparison to new `VTABLE`, also verify unpacked thread_index matches `KIMOJIO_THREAD_INDEX` (both checks required — vtable confirms it's a kimojio waker, thread_index confirms it belongs to this thread), unpack task_index from waker data, lookup in `task_state.tasks`, call `schedule_io`. If thread_index mismatches, fall back to `waker.wake_by_ref()` which takes the cross-thread path.

- **`kimojio/src/task.rs`**: Adjust `schedule_io_internal` and task lookup
  - Add a public method on `TaskState` to look up a task by u16 index: `get_task_by_index(task_index: u16) -> Option<Rc<Task>>` — wraps the `NonZeroUsize` construction and `HandleTable::get` + clone
  - The assert in `schedule_io_internal` (`task.rs:682-685`) remains unchanged — it still validates that the task belongs to this thread's TaskState

- **`kimojio/src/runtime.rs`**: Set thread-local on startup
  - In `Runtime::new()`: call `set_kimojio_thread_index(thread_index)` to initialize the thread-local. This must happen here (not in `block_on`) because `thread_index` is a parameter of `Runtime::new` and must be set before any `create_waker` invocation.
  - Update `poll_task()`: change `create_waker(task.clone())` → `create_waker(task.task_index)` (the thread_index is read from the thread-local inside `create_waker`)

- **Tests**: All existing tests should pass — the local-thread path behavior is identical. Add a compile-time assertion that `std::task::Waker` produced by `create_waker` is `Send + Sync`.

### Success Criteria

#### Automated Verification:
- [ ] `cargo test --all-targets` — all existing tests pass
- [ ] `cargo test --doc` — doc tests pass
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` — no warnings
- [ ] `cargo fmt --all -- --check` — formatting clean
- [ ] Compile-time check: `create_waker` result stored in `Box<dyn Send + Sync>` compiles

#### Manual Verification:
- [ ] Local wake behavior unchanged — tasks wake and schedule exactly as before
- [ ] `wake_task()` optimization still triggers for kimojio wakers (vtable + thread_index comparison)
- [ ] No locks, atomics, or allocations in the clone/wake/drop paths for local wakes
- [ ] Local wake emits the same `TaskScheduled` trace event as the current implementation

---

## Phase 2: Cross-Thread Wake Infrastructure

### Objective

Add the global thread registry, per-thread eventfd + wake queue, the cross-thread wake path in the VTABLE, a cross-thread wake processor task in the event loop, and tests demonstrating end-to-end cross-thread waking.

### Changes Required

- **`Cargo.toml` (workspace)**: Add `"event"` to rustix features list to enable `rustix::event::eventfd`

- **`kimojio/src/cross_thread_wake.rs`** (new module): Global wake registry and per-thread wake infrastructure
  - `WakeRegistryEntry`: holds `wake_queue: Vec<u16>` and `eventfd_fd: RawFd` for one runtime thread. Safety invariant: the RawFd is valid for the lifetime of the entry; `unregister_thread` must be called before the corresponding `OwnedFd` is dropped; after unregistration, no new writes can reach the fd since the entry is removed under the mutex.
  - `static WAKE_REGISTRY: Mutex<HashMap<u8, WakeRegistryEntry>>` — global registry mapping `thread_index → entry`.
  - `register_thread(thread_index: u8, eventfd_fd: RawFd)` — inserts entry, called during `Runtime::new`. Fails/panics if thread_index already exists to prevent registry corruption.
  - `unregister_thread(thread_index: u8)` — removes entry, called during runtime shutdown
  - `cross_thread_wake(thread_index: u8, task_index: u16)` — locks registry, pushes task_index into the target thread's `wake_queue`, writes `1u64` to the eventfd. If thread_index not found (runtime shut down), silently drops the wake.
  - `drain_wake_queue(thread_index: u8) -> Vec<u16>` — locks registry, swaps `wake_queue` with empty `Vec` (minimizes lock hold time), returns the pending indices

- **`kimojio/src/lib.rs`**: Add `mod cross_thread_wake;` declaration

- **`kimojio/src/task_ref.rs`**: Enable cross-thread path in VTABLE
  - In `wake` and `wake_by_ref`: the foreign-thread branch (thread_index mismatch) calls `cross_thread_wake::cross_thread_wake(thread_index, task_index)` instead of panicking

- **`kimojio/src/runtime.rs`**: Eventfd lifecycle and wake processor
  - In `Runtime::new()` (authoritative initialization location):
    1. Call `set_kimojio_thread_index(thread_index)` FIRST to initialize TLS.
    2. Create `eventfd(0, EventfdFlags::empty())`.
    3. Call `cross_thread_wake::register_thread(thread_index, eventfd_fd)`.
    4. Store `OwnedFd` in `Runtime`.
  - Spawn a cross-thread wake processor task alongside the runtime server task. This async task loops: read 8 bytes from the eventfd (via io_uring read SQE), drain the wake queue, schedule each task via `TaskState::get()` + `get_task_by_index` + `schedule_io`. Skip indices that don't resolve (completed/removed tasks). The eventfd read must be re-issued after each completion so the event loop is always interruptible.
  - On shutdown (including `Runtime` `Drop` for panic safety): call `cross_thread_wake::unregister_thread(thread_index)`, then close the eventfd (via `OwnedFd` drop). Ensure `unregister_thread` happens while fd is valid.

- **`kimojio/src/runtime.rs` (tests)** or new test file: Cross-thread wake tests
  - **Test: basic cross-thread wake** — a kimojio task sends its waker to `std::thread::spawn`, the thread calls `wake()`, the kimojio task resumes and completes
  - **Test: cross-thread wake unblocks sleeping event loop** — a kimojio task suspends, a std::thread sleeps briefly then wakes it, verify the task resumes promptly
  - **Test: waker clone on foreign thread** — clone a waker on a std::thread and wake with the clone
  - **Test: wake completed task from foreign thread** — send waker to std::thread, complete the task locally, then the std::thread calls wake — no panic
  - **Test: waker outlives runtime** — invoke waker after runtime drops/shutdown, verify no panic and wake is ignored
  - **Test: multiple concurrent cross-thread wakes** — multiple std::threads wake the same task simultaneously — task is scheduled once and completes normally
  - **Test: waker Send + Sync at runtime** — store waker in `Arc<Mutex<Option<Waker>>>` and use from multiple threads

### Success Criteria

#### Automated Verification:
- [ ] `cargo test --all-targets` — all existing + new tests pass
- [ ] `cargo test --doc` — doc tests pass
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` — no warnings
- [ ] `cargo fmt --all -- --check` — formatting clean

#### Manual Verification:
- [ ] Cross-thread wake from std::thread completes the kimojio task
- [ ] Sleeping event loop unblocks when cross-thread wake occurs
- [ ] No panics or UB when waking completed tasks or stale wakers
- [ ] eventfd is closed and registry entry removed on runtime shutdown

---

## Phase 3: Documentation

### Changes Required

- **`.paw/work/cross-thread-waker/Docs.md`**: Technical reference (load `paw-docs-guidance`)
  - Architecture: packed waker data, local vs cross-thread paths, global registry
  - Usage: how to create a waker, send it to another thread, expected behavior
  - Safety: thread-local semantics, VTABLE contract, HandleTable index lifecycle
  - Verification approach

- **Project docs**: Update README.md if cross-thread waking is a user-visible feature worth advertising. Update any API doc comments on public waker-related functions.

### Success Criteria

- [ ] Docs.md complete and accurate
- [ ] Doc comments updated on changed public APIs
- [ ] `cargo doc` builds without warnings

---

## References

- Spec: `.paw/work/cross-thread-waker/Spec.md`
- Research: `.paw/work/cross-thread-waker/CodeResearch.md`
- Issue: none
