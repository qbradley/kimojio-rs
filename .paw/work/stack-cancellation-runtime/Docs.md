# Stack Cancellation Runtime

## Overview

The stack cancellation runtime work makes `kimojio-stack` cancellation-safe across scheduler tasks, local waits, cross-thread wakes, scope exit, and io_uring operation ownership. It closes the main lifetime gaps identified in `kimojio-stack/CANCEL-DESIGN.md`: stale task handles cannot wake reused task slots, timed-out waiters cannot steal later wakes, external wake queues are generation-safe, and pending I/O resources remain owned until their CQEs are reaped.

The implementation keeps the runtime's flat scheduler model intact. A stackful coroutine that blocks parks through its `RuntimeContext`; the root scheduler drives ready tasks, external wake signals, and io_uring completions without nested scheduler execution inside a coroutine.

## Architecture and Design

### High-Level Architecture

The cancellation model is split across four cooperating layers:

1. Scheduler task identity uses generation-checked `TaskKey` values and reusable task slots.
2. Local waits use scheduler-owned wait tokens leased through `WaitRegistration`.
3. Cross-thread waits use explicit external registration state so wake/cancel races can be claimed atomically.
4. Async I/O uses operation state as the owner of kernel-visible resources until completion reaping.

`Runtime::block_on` still owns a single scheduler for the current OS thread. Scopes spawn stackful coroutines into that scheduler, and every scope waits for its children before returning. If a scope reaches quiescence with unfinished children and no runnable work, it interrupts those children with internal cancellation and then continues draining completions until all scoped work finishes.

### Design Decisions

**Generation-checked task identity.** Ready queues, local waiters, external waiters, and scope child tracking use `TaskKey` instead of bare slots. Reusing a scheduler slot increments its generation, so stale wake entries are discarded instead of scheduling an unrelated future task.

**Wait registration guards.** Local wait registration creates a token that is invalidated when the registration guard drops. Timed-out waits, non-winning `wait_any` arms, canceled tasks, and unwind paths therefore leave tombstones that cannot consume future wakeups. Waiter queues compact opportunistically to keep known churn patterns bounded.

**External wake claim-before-wake.** Cross-thread stackful wait queues claim a waiter under the queue lock before staging the external wake. This prevents a cancellation race from consuming a wake that should have gone to another waiter.

**Cheap-clone async fd ownership.** Unregistered async reads and writes now require `IoFd`. It consumes an `OwnedFd` once and clones locally without `dup` or `fcntl`, allowing operation state to keep the descriptor alive cheaply until the CQE is reaped.

**Detached operation reaper.** Dropping or canceling an `IoResult`/`Timeout` detaches user interest. Dropped handles request cancellation by default; explicit `detach` skips that cancellation request. In both cases, scheduler-owned detached operation state retains buffers, fd handles, registered-resource leases, and cancel state until both original and cancel CQEs are resolved.

**Bounded successful scope metadata.** Completed successful child task IDs and panic-payload slots are removed from `ScopeState`. Panic payload slots are retained only for children that actually panic so unobserved child panic propagation remains correct.

### Integration Points

- `RuntimeContext::read_async` and `RuntimeContext::write_async` require `&IoFd`.
- `RuntimeContext::register_fd` and `RuntimeContext::register_buffer` keep their fixed-resource lease behavior; async operation state now also handles dropped-result reaping.
- `RuntimeContext::io_counters` exposes detach/cancel diagnostics for tests and operational introspection.
- `Waitable`, `wait_any`, `wait_all`, `select`, `join`, channels, `Notify`, `Mutex`, `Semaphore`, `RwLock`, and `Barrier` all participate in cancellation-safe wait registration.
- `kimojio-stack-http` plaintext transport and `kimojio-stack-tls` streams adapted their async internals to store `IoFd` where needed.

## User Guide

### Prerequisites

This implementation is for Linux/x86-64 io_uring-backed `kimojio-stack` runtimes. Code using unregistered async I/O should transfer file descriptor ownership into `IoFd` once before starting async operations.

### Basic Usage

Use blocking-from-coroutine APIs (`read`, `write`, `sleep`, `close`, and related methods) when the current stackful coroutine should wait for a single operation. The runtime parks only that coroutine and continues running other scoped work.

For waitable async I/O, construct an `IoFd` and pass it to `read_async` or `write_async`. Complete the handle with `get`/`try_get`, wait on it through `wait_any`/`wait_all`, or call `cancel`/`detach` if the result is no longer needed.

### Advanced Usage

Use `detach` when an operation should continue silently and the result can be discarded without requesting kernel cancellation. Use `cancel` or dropping the handle when the preferred behavior is to request cancellation and then let the runtime reap the eventual CQE.

`RuntimeContext::io_counters` reports current detached operation count and aggregate cancellation outcomes. These counters are snapshots within one `block_on` execution and are useful for tests, diagnostics, and future runtime observability.

Registered fd and buffer users can drop registered handles while async I/O is pending. The slot is retired immediately from the user perspective, but the actual unregister/reuse happens only after the in-flight operation completes.

## API Reference

### Key Components

- `TaskKey`: internal generation-checked scheduler identity.
- `WaitRegistration`: internal guard for cancellation-safe local wait registration.
- `ExternalWaitRegistration`: internal cross-thread wait registration with atomic active/ready/canceled state.
- `IoFd`: public cheap-clone fd identity for unregistered async I/O.
- `IoResult::detach`: detach and discard a pending async I/O result without requesting cancellation.
- `Timeout::detach`: detach and discard a pending timeout without requesting cancellation.
- `IoCounters`: snapshot of detached and canceled I/O lifecycle counters.

### Configuration Options

No new runtime configuration is required. Existing runtime configuration still controls stack size, ring entries, fixed-file/fixed-buffer slot counts, ring-enter policy, and busy polling.

## Testing

### How to Test

Run the main validation suite:

```bash
cargo test -p kimojio-stack --all-targets --all-features
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Run waiter churn and tombstone measurements with the runtime benchmark harness:

```bash
cargo bench -p kimojio-stack --bench runtime_baseline -- waiters
```

The Phase 6 verification pass completed with the following results:

- `cargo test -p kimojio-stack --all-targets --all-features`: passed
- `cargo fmt --all --check`: passed
- `cargo clippy -- -D warnings`: passed
- `cargo clippy --all-targets -- -D warnings`: passed
- `cargo clippy --all-targets --all-features -- -D warnings`: passed
- `cargo test`: passed
- `cargo bench -p kimojio-stack --bench runtime_baseline -- waiters`: passed. On the validation host, timeout churn, queue length 8, queue length 64, wake-one after timeout churn, and wake-all after timeout churn measured around 13.1-13.6 us/iteration; the explicit wake-before-timeout cancellation path measured around 1.29 us/iteration.

### Edge Cases

- Timed-out local waits leave inactive tokens and cannot steal a later wake.
- Stale local and external wakes cannot schedule a reused task slot.
- Cross-thread wake/cancel races claim stackful waiters before staging wakes.
- Scope exit drains staged external wakes and completed I/O before canceling quiescent children.
- Dropped pending I/O and timeouts are reaped before `Runtime::block_on` returns.
- Explicitly forgotten `IoResult` values still leak by definition; callers should use `get`, `try_get`, `cancel`, or `detach` when reclamation matters.

## Limitations and Future Work

Waiter tombstone compaction is provisional and measurement-driven. The new benchmark group provides data for future threshold tuning, but no final production policy has been chosen.

`IoFd` is currently backed by `Rc<IoFdState>`, which matches the local thread-per-core runtime. If clone overhead or cache locality becomes measurable, the public type can be backed by a scheduler-owned fd table without changing callers.

Detached/cancel counters are basic snapshots rather than a complete production metrics API. Future diagnostics may expose richer per-runtime state or integrate with tracing.
