# Feature Specification: Cross-Thread Waker

**Branch**: feature/cross-thread-waker  |  **Created**: 2026-02-19  |  **Status**: Draft
**Input Brief**: Enable kimojio Waker to be safely used from any thread while maintaining zero-overhead on the local thread.

## Overview

Kimojio is a single-threaded io_uring-based async runtime. Today, its `Waker` implementation is built on `Rc<Task>`, making it `!Send` and preventing any cross-thread usage. This means external runtimes like tokio cannot wake kimojio tasks — a significant interoperability limitation for workloads that bridge kimojio's high-performance I/O with tokio's ecosystem (e.g., gRPC clients, HTTP libraries, database drivers).

The goal is to make kimojio's `Waker` satisfy the full `Send + Sync` contract required by `std::task::Waker`, enabling any thread to wake a kimojio task. The critical constraint is that the local-thread fast path must remain lock-free and atomic-free — kimojio's performance advantage comes from avoiding synchronization on the hot path, and that must be preserved.

On the local thread, waking remains as fast as today — a simple check confirms locality, and the task is scheduled directly with no synchronization. From other threads, waking uses an efficient notification mechanism that interrupts the event loop immediately, ensuring the woken task is processed on the next iteration without requiring the caller to know anything about kimojio internals.

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
1. Given a kimojio task waiting on a cross-thread signal, When a waker is invoked from another thread, Then the kimojio task is scheduled and polled on its owning thread within the next event loop iteration
2. Given a kimojio event loop that is idle and waiting for I/O, When a cross-thread wake occurs, Then the event loop unblocks and processes the woken task
3. Given a waker cloned on the local thread and sent to a foreign thread, When the foreign thread invokes the waker, Then the task is correctly identified and scheduled without data races

### User Story P2 – Local-Thread Wake Remains Zero-Overhead

Narrative: A developer uses kimojio as today — all futures and I/O completions wake tasks on the same thread. The new waker implementation detects this and takes the fast path with no synchronization overhead.

Independent Test: A kimojio benchmark comparing local-thread wake latency before and after the change shows no measurable regression.

Acceptance Scenarios:
1. Given a waker created for a local task, When it is invoked on the owning thread, Then no mutex, atomic operation, or shared reference counting is performed
2. Given the optimized wake path used with direct task state access, When the waker belongs to the local runtime, Then the task is scheduled directly without additional indirection
3. Given a local-thread wake, When tracing is enabled, Then the same trace events are emitted as the current implementation

### User Story P3 – Waker Satisfies Send + Sync

Narrative: A library author writes generic async code that requires `Waker: Send + Sync` (as guaranteed by the standard library). Kimojio wakers work in this context without any special handling.

Independent Test: A kimojio waker is stored in a thread-safe container and used from a different thread without compilation errors or runtime panics.

Acceptance Scenarios:
1. Given a kimojio waker, When it is sent to another thread, Then it compiles and runs without panic
2. Given a kimojio waker, When it is cloned from a foreign thread, Then a valid waker is produced that can also wake the task
3. Given a kimojio waker for a task that has already completed, When it is invoked from any thread, Then the wake is a no-op (no panic, no undefined behavior)

### Edge Cases

- **Task completed before cross-thread wake**: The wake channel receives a task identity for a completed/removed task. The owning thread looks it up, finds no entry, and discards the wake — no panic.
- **Multiple cross-thread wakes for same task**: Multiple threads wake the same task simultaneously. The owning thread drains all entries but duplicate scheduling for an already-ready task is silently ignored.
- **Waker outlives runtime**: A waker is held after the owning runtime shuts down. Cross-thread wake delivery fails gracefully (no panic, no UB).
- **Signal saturation**: Many cross-thread wakes accumulate before the event loop drains them. The signaling mechanism coalesces notifications; a single drain processes all pending wakes.
- **Task identity reuse**: A task completes and its identity is reused for a new task. A stale waker wakes the new task — this is acceptable (spurious wake) and consistent with the standard Waker contract.

## Requirements

### Functional Requirements

- FR-001: Wakers produced by the runtime must satisfy the `Send + Sync` contract — they must be safe to send, clone, and invoke from any thread (Stories: P1, P3)
- FR-002: The waker must use a lightweight, `Send + Sync` task identity rather than a thread-local reference (Stories: P1, P2)
- FR-003: When a wake occurs on the owning thread, the task must be scheduled with no locks, atomic operations, or synchronization primitives (Stories: P2)
- FR-004: When a wake occurs on a foreign thread, the task identity must be delivered to the owning thread via a thread-safe mechanism (Stories: P1)
- FR-005: Cross-thread wakes must interrupt a blocking event loop so the woken task is processed promptly (Stories: P1)
- FR-006: The event loop must process cross-thread wake notifications and schedule the identified tasks (Stories: P1)
- FR-007: Any thread must be able to discover the wake channel for a given waker's owning thread without the waker carrying a shared reference-counted pointer (Stories: P1)
- FR-008: An optimized wake path must remain available for callers that already hold mutable access to the runtime's task state (Stories: P2)
- FR-009: Cloning a waker from any thread must produce a valid waker without atomic reference counting (Stories: P2, P3)
- FR-010: Waking a task that has already completed or whose identity has been reused must not panic or cause undefined behavior (Stories: P1, P3)

### Key Entities

- **Waker Identity**: The data carried inside the waker — contains enough information to identify the task and its owning thread; must be `Send + Sync` and copyable without atomic operations
- **Cross-Thread Wake Channel**: Per-runtime-thread mechanism for receiving wake notifications from foreign threads
- **Thread Registry**: Global mapping from thread identifiers to wake channels, enabling cross-thread wake delivery

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

- The thread identifier used for locality checks is a cheaply comparable cached value (no syscall)
- Task identities are stable for the lifetime of a task (not reused until the task is removed)
- The number of runtime threads is small (typically 1-8), so a global registry with mutex-protected lookup is acceptable for the cross-thread path
- The target platform supports an efficient file-descriptor-based notification mechanism for cross-thread signaling

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

- Linux kernel support for file-descriptor-based cross-thread notification
- io_uring support for polling notification file descriptors

## Risks & Mitigations

- **Stale waker after runtime shutdown**: A foreign thread holds a waker after the owning thread exits. Mitigation: The registry entry is removed on shutdown; cross-thread wake detects the missing entry and silently drops the notification.
- **Performance regression on local path**: Adding a locality check could add overhead. Mitigation: The check is a simple integer comparison of a cached value; benchmark to verify no measurable regression.
- **Notification ordering**: The event loop must not miss wakes. Mitigation: Drain the channel completely after each notification; the signaling mechanism guarantees at least one wakeup per batch of notifications.
- **Task identity ABA problem**: Task identity reused for a different task. Mitigation: This produces a spurious wake, which is safe — the new task gets an extra poll and returns Pending. This is consistent with the standard Waker contract.

## References

- Issue: none
