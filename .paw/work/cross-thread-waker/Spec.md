# Feature Specification: Cross-Thread Waker

**Branch**: feature/cross-thread-waker  |  **Created**: 2026-02-19  |  **Status**: Draft
**Input Brief**: Enable kimojio Waker to be safely used from any thread while maintaining zero-overhead on the local thread.

## Overview

Kimojio is a single-threaded io_uring-based async runtime. Today, its `Waker` implementation is built on `Rc<Task>`, making it `!Send` and preventing any cross-thread usage. This means external runtimes like tokio cannot wake kimojio tasks — a significant interoperability limitation for workloads that bridge kimojio's high-performance I/O with tokio's ecosystem (e.g., gRPC clients, HTTP libraries, database drivers).

The goal is to make kimojio's `Waker` satisfy the full `Send + Sync` contract required by `std::task::Waker`, enabling any thread to wake a kimojio task. The critical constraint is that the local-thread fast path must remain lock-free and atomic-free — kimojio's performance advantage comes from avoiding synchronization on the hot path, and that must be preserved.

The design centers on a lightweight waker identity: instead of holding an `Rc<Task>`, the waker carries a task index (from the existing `HandleTable`) and a thread ID. On the local thread, a cheap thread-ID comparison confirms locality, and the waker operates exactly as today — direct `schedule_io` into the ready queue with no synchronization. On a foreign thread, the waker posts the task index into a thread-safe wake channel and signals the owning thread via an `eventfd` registered with io_uring, causing it to drain the channel on its next event loop iteration.

## Objectives

- Enable kimojio tasks to be woken from any thread, satisfying the `std::task::Waker` `Send + Sync` contract
- Preserve zero-overhead local-thread waking — no locks, no atomics, no `Arc` reference counting on the fast path
- Provide a signaling mechanism so cross-thread wakes interrupt a sleeping event loop
- Maintain backward compatibility with existing kimojio task scheduling and the `wake_task()` optimization

## User Scenarios & Testing

### User Story P1 – Cross-Thread Wake from External Runtime

Narrative: A developer runs a kimojio runtime on one thread and a tokio runtime on another. A kimojio task spawns work on the tokio runtime, passing a waker. When the tokio work completes, it calls `waker.wake()` from the tokio thread, and the kimojio task resumes on its owning thread.

Independent Test: A kimojio task sends its waker to a std::thread, which calls `wake()`, causing the kimojio task to be polled and complete.

Acceptance Scenarios:
1. Given a kimojio task waiting on a cross-thread signal, When `waker.wake()` is called from another thread, Then the kimojio task is scheduled and polled on its owning thread within the next event loop iteration
2. Given a kimojio event loop blocked in `submit_and_wait(1)`, When a cross-thread wake occurs, Then the event loop unblocks and processes the woken task
3. Given a waker cloned on the local thread and sent to a foreign thread, When the foreign thread calls `wake()`, Then the task is correctly identified and scheduled without data races

### User Story P2 – Local-Thread Wake Remains Zero-Overhead

Narrative: A developer uses kimojio as today — all futures and I/O completions wake tasks on the same thread. The new waker implementation detects this and takes the fast path with no synchronization overhead.

Independent Test: A kimojio benchmark comparing local-thread wake latency before and after the change shows no measurable regression.

Acceptance Scenarios:
1. Given a waker created for a local task, When `wake()` is called on the owning thread, Then no mutex, atomic operation, or `Arc` reference count is performed
2. Given the `wake_task()` helper called with `&mut TaskState`, When the waker belongs to the local runtime, Then the task is scheduled directly via `schedule_io` without thread-local access
3. Given a local-thread wake, When tracing is enabled, Then the same trace events are emitted as the current implementation

### User Story P3 – Waker Satisfies Send + Sync

Narrative: A library author writes generic async code that requires `Waker: Send + Sync` (as guaranteed by the standard library). Kimojio wakers work in this context without any special handling.

Independent Test: A kimojio waker is stored in a `Box<dyn Send + Sync>` and used from a different thread without compilation errors or runtime panics.

Acceptance Scenarios:
1. Given a kimojio waker, When it is sent to another thread via `std::thread::spawn`, Then it compiles and runs without panic
2. Given a kimojio waker, When it is cloned from a foreign thread, Then a valid waker is produced that can also wake the task
3. Given a kimojio waker for a task that has already completed, When `wake()` is called from any thread, Then the wake is a no-op (no panic, no undefined behavior)

### Edge Cases

- **Task completed before cross-thread wake**: The wake channel receives a task index for a completed/removed task. The owning thread looks it up in the `HandleTable`, finds no entry, and discards the wake — no panic.
- **Multiple cross-thread wakes for same task**: Multiple threads wake the same task simultaneously. The owning thread drains all entries but `schedule_io` already handles duplicate scheduling (task already in Ready state is ignored).
- **Waker outlives runtime**: A waker is held after the owning runtime shuts down. Cross-thread wake writes to the eventfd or channel fail gracefully (no panic, no UB).
- **Eventfd saturation**: Many cross-thread wakes accumulate before the event loop drains them. The eventfd counter accumulates; a single read resets it, and the channel is drained fully.
- **HandleTable index reuse**: A task completes and its index is reused for a new task. A stale waker wakes the new task — this is acceptable (spurious wake) and consistent with how `Waker` contracts work in Rust.

## Requirements

### Functional Requirements

- FR-001: The `RawWakerVTable` implementation must produce wakers that are `Send + Sync` — the data pointer and vtable operations must be safe to invoke from any thread (Stories: P1, P3)
- FR-002: The waker must carry a task identity consisting of a `HandleTable` `Index` value and a thread identifier, rather than an `Rc<Task>` (Stories: P1, P2)
- FR-003: On `wake()` or `wake_by_ref()`, if the calling thread is the owning thread, the task must be scheduled via the existing `TaskState::schedule_io` path with no synchronization primitives (Stories: P2)
- FR-004: On `wake()` or `wake_by_ref()`, if the calling thread is NOT the owning thread, the task index must be sent to the owning thread via a thread-safe channel and the owning thread must be signaled (Stories: P1)
- FR-005: A per-runtime-thread `eventfd` must be registered with io_uring so that cross-thread wakes interrupt a blocking `submit_and_wait` call (Stories: P1)
- FR-006: The event loop must drain the cross-thread wake channel when the eventfd completes, looking up tasks by index in the `HandleTable` and calling `schedule_io` for each (Stories: P1)
- FR-007: A global registry must map thread identifiers to per-thread wake channels, allowing any thread to find the correct channel for a given waker (Stories: P1)
- FR-008: The `wake_task()` helper function must continue to work as an optimization for callers that already hold `&mut TaskState` (Stories: P2)
- FR-009: `clone` on a waker from any thread must produce a valid waker without requiring atomic reference counting (Stories: P2, P3)
- FR-010: Waking a task that has already completed or whose index has been reused must not panic or cause undefined behavior (Stories: P1, P3)

### Key Entities

- **WakerData**: The data carried inside the `RawWaker` — contains task index and thread identifier; must be `Send + Sync`
- **CrossThreadWakeChannel**: Per-runtime-thread channel (sender is `Send`, receiver is local) for receiving task indices from foreign threads
- **Thread Registry**: Global static mapping thread identifiers to wake channel senders and eventfd file descriptors

### Cross-Cutting / Non-Functional

- Local-thread wake path must not perform any locked or atomic memory operations
- Cross-thread wake must be safe under concurrent access from multiple foreign threads simultaneously
- The eventfd and channel mechanism must not leak file descriptors when the runtime shuts down

## Success Criteria

- SC-001: A kimojio waker can be sent to and invoked from a foreign std::thread, resulting in the originating task being polled (FR-001, FR-004, FR-005, FR-006)
- SC-002: Local-thread wakes produce identical scheduling behavior to the current implementation with no additional synchronization (FR-003, FR-008)
- SC-003: All existing kimojio tests pass without modification (FR-003, FR-008, FR-010)
- SC-004: A waker can be cloned and dropped from any thread without panic or memory unsafety (FR-001, FR-009)
- SC-005: Cross-thread wakes unblock a sleeping event loop within one event loop iteration (FR-005, FR-006)
- SC-006: Waking a completed or reused-index task is silently ignored (FR-010)

## Assumptions

- The `thread_id` stored in `TaskState` (via `std::thread::current().id()`) is a sufficiently cheap comparison for the local-thread check (it's a read of a cached value, no syscall)
- HandleTable indices are stable for the lifetime of a task (they are not reused until the task is removed)
- The number of kimojio runtime threads is small (typically 1-8), so a global registry with mutex-protected lookup is acceptable for the cross-thread path
- `eventfd` is available on all target Linux kernels (it has been available since Linux 2.6.22)

## Scope

In Scope:
- New waker vtable and data structure using task index + thread ID
- Cross-thread wake channel and eventfd integration
- Global thread registry for wake channel lookup
- Event loop changes to drain cross-thread wakes
- Maintaining the `wake_task()` optimization

Out of Scope:
- Making `Task` itself thread-safe (it remains `Rc`-based and single-threaded)
- Multi-threaded task stealing or migration between runtime threads
- Changes to the `HandleTable` data structure
- Integration with specific external runtimes (tokio, async-std) — this provides the mechanism; integration is a separate concern

## Dependencies

- Linux `eventfd` syscall (via `rustix` or `libc`)
- io_uring support for polling eventfd (standard `IORING_OP_READ` on eventfd)

## Risks & Mitigations

- **Stale waker after runtime shutdown**: A foreign thread holds a waker after the kimojio thread exits. Mitigation: The registry entry is removed on shutdown; cross-thread wake detects missing entry and silently drops the wake.
- **Performance regression on local path**: Adding a thread-ID check could add overhead. Mitigation: Thread ID comparison is a simple integer compare of a cached value; benchmark to verify.
- **Eventfd read/write ordering**: The event loop must not miss wakes. Mitigation: Drain the channel completely after each eventfd read; the eventfd counter guarantees at least one read wakes the loop per batch of writes.
- **HandleTable index ABA problem**: Task index reused for a different task. Mitigation: This produces a spurious wake, which is safe — the new task gets an extra poll and returns `Pending`. This is consistent with the `Waker` contract.

## References

- Issue: none
- Source files: `kimojio/src/task_ref.rs`, `kimojio/src/task.rs`, `kimojio/src/runtime.rs`, `kimojio/src/async_event.rs`
