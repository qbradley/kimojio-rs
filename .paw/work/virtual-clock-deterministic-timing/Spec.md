# Feature Specification: Virtual Clock Deterministic Timing

**Branch**: feature/virtual-clock-deterministic-timing  |  **Created**: 2026-03-14  |  **Status**: Draft
**Input Brief**: Enable deterministic virtual time in kimojio so upstream consumers can write fast, reliable timeout tests.

## Overview

Kimojio is a thread-per-core io_uring async runtime where all timer operations — sleeps, deadlines, and I/O timeouts — flow through the kernel via io_uring timeout SQEs. This means any test that validates timeout behavior must wait real wall-clock time, making test suites slow and introducing flakiness from system load variability.

Upstream consumers (e.g., iomgr-based services) have test suites where timeout validation dominates execution time. A quorum failure test that checks a 60-second timeout takes 60 real seconds today. Multiplied across hundreds of timeout-sensitive tests, this creates multi-minute test cycles that slow development velocity and produce intermittent CI failures.

This feature introduces a Clock abstraction at the kimojio runtime level that virtualizes `sleep()`, `sleep_until()`, `timeout_at()`, and deadline-based I/O operations. When a VirtualClock is injected at runtime construction, timer operations register in a user-space timer wheel instead of submitting kernel timeout SQEs. Test code advances virtual time explicitly via `clock.advance()`, causing registered timers to fire instantly and deterministically. The key architectural insight is that kimojio already converts all deadlines to relative durations at SQE submission time — the interception point is precisely where that conversion happens.

The design preserves full backward compatibility: existing kimojio users see no API changes, no new generic parameters on public types, and no behavior differences. The virtual clock infrastructure is gated behind an opt-in Cargo feature flag, keeping production binaries unaffected.

## Objectives

- Enable upstream consumers to write timeout tests that execute in microseconds instead of seconds
- Eliminate test flakiness caused by wall-clock timing variability
- Provide deterministic timer ordering when advancing past multiple deadlines
- Maintain zero overhead on the production hot path (no runtime cost when using real time)
- Preserve complete backward compatibility — existing code compiles and runs without changes
- Offer composable infrastructure that benefits all kimojio users, not just a single upstream consumer

## User Scenarios & Testing

### User Story P1 – Deterministic Sleep in Tests

Narrative: A test author creates a kimojio runtime with a virtual clock, spawns a task that sleeps for 10 seconds, advances virtual time by 10 seconds, and observes the sleep completing instantly without waiting real wall-clock time.

Independent Test: Spawn a task with `operations::sleep(Duration::from_secs(10))`, call `clock.advance(Duration::from_secs(10))`, and verify the task completes within 1ms of wall time.

Acceptance Scenarios:
1. Given a runtime with VirtualClock, When a task calls `sleep(5s)` and virtual time is advanced by 5s, Then the sleep future resolves successfully
2. Given a runtime with VirtualClock, When a task calls `sleep(10s)` and virtual time is advanced by only 3s, Then the sleep future remains pending
3. Given a runtime with VirtualClock, When multiple tasks sleep for different durations and virtual time advances past all of them, Then tasks wake in deadline order (shortest first)
4. Given a runtime with SystemClock (default), When a task calls `sleep(1ms)`, Then the sleep uses real io_uring timeout as before (backward compatible)

### User Story P2 – Virtual Deadline I/O Timeout Computation

Narrative: A test author uses deadline-based I/O operations and the virtual clock's `now()` is used for deadline-to-duration conversion, ensuring timeout calculations are deterministic and consistent with advanced virtual time.

Independent Test: Set virtual time to T, call a `*_with_deadline` function with deadline T+5s, verify the computed remaining duration is 5s regardless of wall-clock elapsed time.

Acceptance Scenarios:
1. Given a VirtualClock at time T, When `write_with_deadline(fd, buf, Some(T + 5s))` is called, Then the remaining timeout is computed as 5s using virtual `now()`
2. Given a VirtualClock at time T+6s and a deadline of T+5s, When `read_with_deadline(fd, buf, Some(T + 5s))` is called, Then it returns a timeout error immediately (deadline already passed)

### User Story P3 – Sleep Until a Specific Instant

Narrative: A test author uses `sleep_until(deadline)` to suspend a task until a specific virtual instant, enabling precise control over when tasks resume.

Independent Test: Call `sleep_until(clock.now() + 7s)`, advance virtual time to that instant, verify the future resolves.

Acceptance Scenarios:
1. Given a VirtualClock, When `sleep_until(deadline)` is called and time is advanced to the deadline, Then the future completes
2. Given a VirtualClock where `now()` is already past the deadline, When `sleep_until(past_deadline)` is called, Then the future completes immediately

### User Story P4 – Timeout Wrapper for Arbitrary Futures

Narrative: A test author wraps an arbitrary async operation with `timeout_at(deadline, future)` so that if virtual time advances past the deadline before the inner future completes, the wrapper returns a timeout error — enabling virtual-time-aware cancellation of slow operations.

Independent Test: Wrap a future that never completes with `timeout_at(clock.now() + 5s, never())`, advance virtual time by 5s, verify timeout error is returned.

Acceptance Scenarios:
1. Given a VirtualClock, When `timeout_at(deadline, fast_future)` is called and the inner future completes before the deadline, Then the result is the inner future's output
2. Given a VirtualClock, When `timeout_at(deadline, slow_future)` is called and virtual time advances past the deadline, Then the result is a timeout error
3. Given a SystemClock, When `timeout_at` is called, Then it uses real io_uring-based timeout behavior

### User Story P5 – Timer Cancellation on Drop

Narrative: When a virtual sleep future is dropped (e.g., because another branch of a `select!` completed first), its timer registration is cleaned up from the virtual timer wheel, preventing stale wakeups.

Independent Test: Create a virtual sleep future, register it, then drop it before advancing time. Verify the timer wheel has no remaining timers.

Acceptance Scenarios:
1. Given a registered virtual timer, When the VirtualSleepFuture is dropped, Then the timer is removed from the virtual clock's timer wheel
2. Given two futures in a select, When one completes and the other (a virtual sleep) is dropped, Then no spurious wakeups occur when virtual time later advances past the dropped timer's deadline

### User Story P6 – Zero-Change Upgrade for Existing Users

Narrative: An existing kimojio user upgrades to the new version. Their code, which uses `Runtime::new()`, `operations::sleep()`, and the `#[kimojio::main]`/`#[kimojio::test]` macros, compiles and runs identically without any modifications.

Independent Test: Compile existing kimojio example code against the new version without changes and verify identical behavior.

Acceptance Scenarios:
1. Given existing code using `Runtime::new(thread_index, config)`, When compiled against the new version, Then it compiles without modification and uses SystemClock
2. Given existing code using `operations::sleep(duration)`, When compiled against the new version, Then sleep behavior is unchanged (real io_uring timeouts)
3. Given existing code using `#[kimojio::main]` and `#[kimojio::test]`, When compiled against the new version, Then macros work identically

### Edge Cases
- Advancing virtual time by zero duration: no timers fire, no error
- Advancing virtual time past multiple deadlines: timers fire in deadline order
- Calling `advance()` with no registered timers: returns 0, no error
- VirtualClock `now()` called before any advance: returns the initial instant
- Registering a timer with a deadline already in the past: fires on the next `advance()` call (not during registration, to avoid re-entrancy). Note: `VirtualSleepFuture::poll()` checks `clock.now() >= deadline` before registering, so a past-deadline sleep resolves immediately at poll time without needing `advance()`
- Dropping all references to a VirtualSleepFuture without polling: no timer registered (timer registered on first poll)
- Concurrent `advance()` calls: not applicable — kimojio is single-threaded

## Requirements

### Functional Requirements
- FR-001: Provide a `Clock` trait with `now()`, `register_timer()`, `cancel_timer()`, and `is_real()` methods (Stories: P1, P2, P6)
- FR-002: Provide a `SystemClock` implementation that delegates to `std::time::Instant::now()` and io_uring for timers (Stories: P6)
- FR-003: Provide a `VirtualClock` implementation with a priority-based timer wheel, `advance()`, `advance_to()`, and `next_deadline()` methods (Stories: P1, P3, P5)
- FR-004: Provide `Runtime::new_with_clock()` constructor accepting a `Clock` implementation, and `Runtime::new_virtual()` convenience constructor that returns both a runtime and a `VirtualClock` handle (Stories: P1)
- FR-005: Preserve `Runtime::new()` as a backward-compatible constructor using `SystemClock` with no generic parameters visible (Stories: P6)
- FR-006: Virtualize `operations::sleep()` — when virtual clock is active, register in timer wheel instead of submitting io_uring SQE (Stories: P1)
- FR-007: Provide `operations::sleep_until(deadline)` that sleeps until a specific instant using the runtime's clock (Stories: P3)
- FR-008: Provide `operations::timeout_at(deadline, future)` that races a deadline against an arbitrary future using the runtime's clock (Stories: P4)
- FR-009: Replace `Instant::now()` with `clock.now()` in all deadline-based I/O functions and synchronization primitives within kimojio that compute remaining timeouts (operations.rs deadline functions, async_event.rs, async_lock.rs) (Stories: P2)
- FR-010: Implement `Drop` for `VirtualSleepFuture` that cancels the registered timer (Stories: P5)
- FR-011: Gate `VirtualClock` and test infrastructure behind a `virtual-clock` Cargo feature flag; `sleep_until` and `timeout_at` are always available as generally useful async primitives (Stories: P1, P3, P4, P6)
- FR-012: Store the clock reference in thread-local `TaskState` so `operations::sleep()` can access it without parameter changes (Stories: P1, P6)
- FR-013: Provide documentation including a usage guide, design document, and example code demonstrating virtual time usage (Stories: P1, P3, P4)
- FR-014: Provide `Configuration::set_clock()` builder method for injecting a clock into the runtime (Stories: P1)

### Key Entities
- `Clock`: Trait defining the time provider interface
- `SystemClock`: Production clock using real system time and io_uring timers
- `VirtualClock`: Test clock with user-space timer wheel and explicit time advancement
- `TimerId`: Opaque identifier for registered virtual timers
- `VirtualSleepFuture`: Future type for virtual sleep operations
- `SleepFuture`: Enum dispatching between real (io_uring) and virtual sleep paths

### Cross-Cutting / Non-Functional
- Zero measurable runtime overhead when using SystemClock (verified by benchmark)
- VirtualClock is single-threaded, matching kimojio's threading model
- Feature flag `virtual-clock` controls compilation of VirtualClock and related types
- All new public API items have doc comments with examples

## Success Criteria
- SC-001: A test using VirtualClock with a 60-second virtual sleep completes in under 10ms wall time (FR-006, FR-003)
- SC-002: All existing kimojio tests pass without modification when the `virtual-clock` feature is not enabled (FR-002, FR-005)
- SC-003: All existing kimojio tests pass without modification when the `virtual-clock` feature is enabled (FR-002, FR-005)
- SC-004: When multiple virtual timers fire simultaneously, wakeup order matches deadline order (FR-003)
- SC-005: Dropping a virtual sleep future removes its timer from the wheel (no stale wakeups) (FR-010)
- SC-006: `operations::sleep()` signature is unchanged from the caller's perspective (FR-006, FR-012)
- SC-007: `timeout_at` correctly returns timeout error when virtual time exceeds deadline (FR-008)
- SC-008: Deadline-based I/O operations use virtual `now()` for remaining-time computation (FR-009)
- SC-009: Test suite includes at least one test per acceptance scenario across all stories (all FRs)
- SC-010: Documentation includes a usage guide and detailed design document (FR-013)

## Assumptions
- kimojio remains single-threaded per runtime instance (no `Send`/`Sync` required for VirtualClock)
- The timer wheel with O(n) cancellation is sufficient (tests rarely have >100 concurrent timers)
- Timer wakeup is in deadline order, but task execution completion order depends on runtime scheduling
- TSC-based profiling (`Timer::ticks()`) is NOT part of the virtualization surface — it's for performance metrics

## Scope

In Scope:
- `Clock` trait and `SystemClock` implementation
- `VirtualClock` with `advance()`, `advance_to()`, `next_deadline()`
- `Runtime::new_with_clock()` constructor
- Backward-compatible `Runtime::new()` (no visible generics)
- `operations::sleep()` virtualization via thread-local clock
- `operations::sleep_until()` new API
- `operations::timeout_at()` new API
- `Instant::now()` → `clock.now()` in kimojio's deadline I/O functions
- `VirtualSleepFuture` with Drop-based timer cancellation
- `virtual-clock` Cargo feature flag
- Thread-local clock storage in TaskState
- Comprehensive test suite
- Usage guide and design documentation
- Example code demonstrating virtual time usage

Out of Scope:
- iomgr-level `Instant::now()` replacements (upstream consumer responsibility)
- Multi-threaded or `Send`/`Sync` VirtualClock
- Deterministic RNG for jitter (upstream consumer responsibility)
- Real I/O timeout virtualization (network latency remains real, as designed)
- Changes to `#[kimojio::main]` or `#[kimojio::test]` macros
- TSC/Timer profiling virtualization
- Event loop modification to skip io_uring when only virtual timers pending (optimization deferred)

## Dependencies
- Existing kimojio `TaskState` thread-local infrastructure
- Existing `operations::sleep()` and deadline I/O implementations
- `pin_project_lite` crate (already a dependency)
- `std::collections::BinaryHeap` (standard library)

## Risks & Mitigations
- **Risk**: Adding a generic parameter to Runtime could leak into public API. **Impact**: Breaking change for existing users. **Mitigation**: Ensure `Runtime::new()` signature is unchanged and no generic parameters are visible to existing callers.
- **Risk**: Thread-local clock storage could conflict with existing TaskState borrow patterns. **Impact**: Runtime panic from double-borrow. **Mitigation**: Follow existing TaskState access patterns; clock reference stored alongside existing fields.
- **Risk**: `is_real()` branch in hot path could cause overhead. **Impact**: Production performance regression. **Mitigation**: Branch is highly predictable; verify with benchmarks.
- **Risk**: Virtual timer ordering differs from io_uring timer ordering. **Impact**: Tests pass but production behavior differs. **Mitigation**: Document that wakeup order is by deadline, matching io_uring's natural ordering.

## References
- Design document: /tmp/design.md
- Codebase: kimojio/src/runtime.rs, kimojio/src/operations.rs, kimojio/src/task.rs
