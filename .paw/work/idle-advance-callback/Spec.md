# Feature Specification: Idle Advance Callback

**Branch**: feature/idle-advance-callback  |  **Created**: 2026-04-08  |  **Status**: Draft
**Input Brief**: Replace the queue-based and default-based idle advance APIs with a single `FnMut` callback for maximum flexibility and simpler design.

## Overview

The virtual clock's idle advance mechanism currently uses three separate APIs to control what happens when the runtime has no ready tasks: a one-shot queue (`virtual_clock_advance_idle`), a repeating default (`virtual_clock_advance_idle_default`), and a count query (`virtual_clock_pending_idle_advances`). While functional, this design fragments a single concept — "what to do when idle" — across multiple entry points with overlapping behavior. It also cannot express dynamic strategies like "advance to the next pending timer" without the caller pre-computing and queuing exact durations.

This feature replaces all three APIs with a single callback: `virtual_clock_set_idle_advance`. The callback is a `FnMut(Instant, Option<Instant>) -> Option<Duration>` that receives the current virtual time and the next pending timer deadline (if any), and returns how far to advance — or `None` to stop advancing and block in io_uring. This is strictly more powerful: fixed advances, repeating defaults, conditional logic, advance-to-next-timer, countdowns, and complete disabling are all expressible as closures. The "advance to next timer on idle" pattern — the most common use case — becomes a one-liner.

The change is purely internal to the virtual clock idle mechanism. The explicit `virtual_clock_advance` and `virtual_clock_advance_to` APIs remain unchanged. Existing tests that use idle advances will be migrated to the callback API, and new tests will demonstrate dynamic strategies that were previously impossible.

## Objectives

- Unify idle advance control into a single, composable callback
- Enable the "advance to next timer on idle" pattern without pre-computation
- Enable dynamic idle advance strategies (conditional, variable-duration, stateful) that the current API cannot express
- Reduce API surface area (three functions → one setter + one query)
- Maintain zero overhead when no callback is installed (no idle advancement)

## User Scenarios & Testing

### User Story P1 – Advance to Next Timer on Idle

Narrative: A test author sets up a virtual clock callback that automatically advances to the next pending timer whenever the runtime is idle. The author spawns tasks with various sleep durations and they all complete without manually computing or queuing advance durations.

Independent Test: Install the advance-to-next-timer callback, spawn a task sleeping 60s, and verify the sleep completes without any explicit `advance()` calls.

Acceptance Scenarios:
1. Given a virtual clock with the advance-to-next callback, When a task sleeps for 60s and no other tasks are ready, Then the sleep completes because idle advance fires to the timer's deadline
2. Given a virtual clock with the advance-to-next callback, When multiple tasks sleep for different durations, Then they complete in deadline order as successive idle points advance to each next timer
3. Given a virtual clock with the advance-to-next callback, When no timers are pending (callback receives `None`), Then the callback returns `None` and the runtime blocks normally

### User Story P2 – Fixed-Duration Repeating Advance

Narrative: A test author installs a callback that always advances by a fixed 1ms on every idle point, equivalent to the old `virtual_clock_advance_idle_default(Some(1ms))` behavior.

Independent Test: Install a fixed-1ms callback, sleep for 60s, and verify the sleep completes.

Acceptance Scenarios:
1. Given a callback returning `Some(Duration::from_millis(1))`, When a task sleeps for 60s, Then the sleep completes after sufficient idle iterations
2. Given a callback returning a fixed duration, When the callback is replaced with a `None`-returning callback, Then idle advancement stops

### User Story P3 – Conditional and Stateful Advance

Narrative: A test author installs a callback that uses mutable captured state to implement a strategy — for example, advancing a fixed number of times then stopping, or advancing by increasing durations.

Independent Test: Install a countdown callback (advance N times then return `None`), verify exactly N idle advances occur.

Acceptance Scenarios:
1. Given a stateful callback that counts invocations and returns `None` after 3 calls, When the runtime idles repeatedly, Then exactly 3 advances occur and the runtime blocks on the 4th idle
2. Given a callback that captures a `Rc<Cell<bool>>` flag, When the flag is toggled from another task, Then idle advancement starts or stops based on the flag

### User Story P4 – Clearing the Callback

Narrative: A test author installs a callback, runs part of a test with automatic advancement, then clears the callback to switch to explicit `virtual_clock_advance()` for a critical section requiring precise control.

Independent Test: Install a callback, complete a sleep, clear the callback, verify that a subsequent sleep does not auto-complete on idle.

Acceptance Scenarios:
1. Given an installed callback, When `virtual_clock_set_idle_advance(None)` is called, Then no idle advancement occurs on subsequent idle points
2. Given a cleared callback, When `virtual_clock_advance()` is called explicitly, Then timers fire normally (explicit API unaffected)

### User Story P5 – Backward-Compatible Upgrade

Narrative: A developer using the previous `virtual_clock_advance_idle` or `virtual_clock_advance_idle_default` APIs upgrades. The old functions no longer exist but equivalent behavior is achievable with simple callback closures and clear migration guidance.

Independent Test: All old idle advance test scenarios pass when rewritten with the callback API.

Acceptance Scenarios:
1. Given old code using `virtual_clock_advance_idle(dur)`, When migrated to a one-shot callback pattern, Then behavior is equivalent
2. Given old code using `virtual_clock_advance_idle_default(Some(dur))`, When migrated to a fixed-duration callback, Then behavior is equivalent
3. Given old code checking `virtual_clock_pending_idle_advances()`, When migrated to callback-based tracking, Then equivalent observability exists

### Edge Cases
- Callback receives `(now, None)` when no timers are pending: should return `None` to avoid advancing time pointlessly
- Callback returns `Some(Duration::ZERO)`: treated as no advancement (no timers fire, no infinite loop)
- Callback panics: panic propagates normally (same as any user code panic in kimojio)
- Callback is replaced while idle advances are in-flight: new callback takes effect on next idle point
- Multiple timers at the same deadline with advance-to-next: all fire on a single advance (existing timer ordering preserved)

## Requirements

### Functional Requirements
- FR-001: Provide `virtual_clock_set_idle_advance(callback)` that accepts `Option<impl FnMut(Instant, Option<Instant>) -> Option<Duration> + 'static>`, storing the callback on the virtual clock state (Stories: P1, P2, P3, P4)
- FR-002: When the runtime is idle and a callback is installed, call the callback with `(current_virtual_time, next_pending_timer_deadline)` and advance by the returned `Duration` (Stories: P1, P2)
- FR-003: When the callback returns `None` or no callback is installed, do not advance and allow the runtime to block in io_uring (Stories: P1, P3, P4)
- FR-004: When the callback returns `Some(Duration::ZERO)`, treat as no-advance to prevent infinite loops (Stories: P1)
- FR-005: Provide `virtual_clock_has_idle_advance()` query returning `bool` — whether a callback is currently installed (Stories: P5)
- FR-006: Remove `virtual_clock_advance_idle(Duration)`, `virtual_clock_advance_idle_default(Option<Duration>)`, and `virtual_clock_pending_idle_advances()` (Stories: P5)
- FR-007: Update runtime event loop idle detection to use the callback instead of the queue/default mechanism (Stories: P1, P2, P3)
- FR-008: Passing `None` to `virtual_clock_set_idle_advance` clears any installed callback (Stories: P4)
- FR-009: Update documentation (virtual-clock-guide.md, doc comments) to reflect the new API (Stories: P1–P5)
- FR-010: Migrate all existing idle advance tests to use the callback API (Stories: P5)

### Key Entities
- Idle advance callback: `Box<dyn FnMut(Instant, Option<Instant>) -> Option<Duration>` stored on `VirtualClockState`
- `virtual_clock_set_idle_advance`: the sole API for configuring idle behavior

### Cross-Cutting / Non-Functional
- Zero overhead when no callback is installed (same `can_idle_advance()` guard, just checks `Option::is_some`)
- Single-threaded: callback is `FnMut`, not `Fn + Send` (matches kimojio's threading model)
- Callback storage uses `Box<dyn FnMut(...)>` — one heap allocation per install, negligible for test workloads

## Success Criteria
- SC-001: "Advance to next timer" pattern works as a one-liner callback — spawned task with 60s sleep completes without explicit advance calls (FR-001, FR-002)
- SC-002: All existing idle advance test scenarios pass when migrated to callback API (FR-006, FR-010)
- SC-003: Fixed-duration callback (`|_, _| Some(Duration::from_millis(1))`) resolves a 60s sleep (FR-002)
- SC-004: Clearing the callback stops idle advancement (FR-003, FR-008)
- SC-005: Callback receiving `None` for next_deadline can return `None` to block (FR-002, FR-003)
- SC-006: `Duration::ZERO` return does not cause infinite loop (FR-004)
- SC-007: Stateful callback (countdown) fires exactly N times (FR-001)
- SC-008: `virtual_clock_advance_idle`, `virtual_clock_advance_idle_default`, and `virtual_clock_pending_idle_advances` no longer exist in the public API (FR-006)
- SC-009: Documentation and guide reflect the new callback API (FR-009)

## Assumptions
- Callback heap allocation (`Box<dyn FnMut>`) is acceptable — virtual clock is test infrastructure, not production hot path
- The three removed APIs have no downstream consumers outside this repository (this is an unreleased feature)
- `FnMut` (not `Fn`) is needed because common patterns use mutable captured state (counters, flags)
- The callback is called at most once per idle point — consistent with current behavior where one advance fires per idle iteration

## Scope

In Scope:
- New `virtual_clock_set_idle_advance` API with `FnMut(Instant, Option<Instant>) -> Option<Duration>` callback
- New `virtual_clock_has_idle_advance` query
- Remove `virtual_clock_advance_idle`, `virtual_clock_advance_idle_default`, `virtual_clock_pending_idle_advances`
- Remove `VecDeque<Duration>` queue and `Option<Duration>` default from `VirtualClockState`
- Add `Option<Box<dyn FnMut(Instant, Option<Instant>) -> Option<Duration>>>` to `VirtualClockState`
- Update runtime event loop idle path
- Migrate existing tests
- New tests for dynamic callback patterns
- Update virtual-clock-guide.md and doc comments

Out of Scope:
- Changes to `virtual_clock_advance()` or `virtual_clock_advance_to()` (explicit advance is orthogonal)
- Changes to `virtual_clock_enable()` / `virtual_clock_now()` / `virtual_clock_epoch()`
- Changes to `sleep()`, `sleep_until()`, `timeout_at()` (timer registration unaffected)
- Multi-threaded callback support (`Send`/`Sync`)
- Auto-advance-future design (separate design doc already exists)

## Dependencies
- Existing `VirtualClockState` idle advance infrastructure (being replaced)
- Runtime event loop idle detection in `runtime.rs`
- `virtual-clock` Cargo feature flag (unchanged)

## Risks & Mitigations
- **Risk**: Callback called while holding mutable borrow on `VirtualClockState`. **Impact**: Double-borrow panic if callback accesses clock state. **Mitigation**: Extract callback from state before invoking it, pass `now` and `next_deadline` as parameters rather than letting callback access state.
- **Risk**: Callback returning non-`None` indefinitely without firing timers causes infinite loop. **Impact**: Test hangs. **Mitigation**: `Duration::ZERO` treated as no-advance (FR-004); document that callbacks should return `None` when no timers are pending.
- **Risk**: Migration breaks test behavior. **Impact**: Test failures. **Mitigation**: Each migrated test is verified to pass; one-to-one mapping from old patterns to callback equivalents.

## References
- Virtual clock implementation: `kimojio/src/virtual_clock.rs`
- Runtime idle path: `kimojio/src/runtime.rs:416-435`
- Existing idle advance tests: `kimojio/tests/virtual_clock_integration.rs:225-330`
- Virtual clock guide: `docs/virtual-clock-guide.md`
