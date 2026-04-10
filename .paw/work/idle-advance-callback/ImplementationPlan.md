# Idle Advance Callback Implementation Plan

## Overview

Replace the queue-based and default-based idle advance mechanism in `VirtualClockState` with a single `FnMut(Instant, Option<Instant>) -> Option<Duration>` callback. This collapses three public APIs into one setter + one query, enables dynamic strategies like "advance to next timer on idle", and simplifies both the internal state and the runtime event loop integration.

## Current State Analysis

- `VirtualClockState` has two idle advance fields: `idle_advances: VecDeque<Duration>` (one-shot queue) and `idle_advance_default: Option<Duration>` (repeating fallback) — `virtual_clock.rs:53-54`
- Eight methods operate on these fields: `queue_idle_advance`, `has_idle_advances`, `take_next_idle_advance`, `pending_idle_advances`, `set_idle_advance_default`, `idle_advance_default`, `can_idle_advance`, `take_next_idle_advance_or_default` — `virtual_clock.rs:193-252`
- Three public API functions in `operations.rs`: `virtual_clock_advance_idle` (:2156), `virtual_clock_advance_idle_default` (:2215), `virtual_clock_pending_idle_advances` (:2170)
- Runtime event loop uses `can_idle_advance()` for busy-poll override (:392-399) and `take_next_idle_advance_or_default()` for the idle execution block (:419-437)
- 21 existing tests use these APIs across three files (6 integration, 7 operations unit, 8 virtual_clock unit) — see CodeResearch.md §4
- The runtime releases the `TaskState` borrow via `into_inner()` before waking tasks — the same pattern is needed for callback invocation

## Desired End State

- `VirtualClockState` stores a single `Option<Box<dyn FnMut(Instant, Option<Instant>) -> Option<Duration>>>` instead of queue + default
- Four internal methods replace eight: `set_idle_advance_fn`, `has_idle_advance_fn`, `take_idle_advance_fn`, `restore_idle_advance_fn`
- Three public API functions replace three: `virtual_clock_set_idle_advance`, `virtual_clock_clear_idle_advance`, `virtual_clock_has_idle_advance`
- Runtime idle block: extract callback via `Option::take()`, release TaskState borrow, invoke callback, re-borrow, put callback back, then advance and wake if callback returned `Some(dur)` with non-zero duration
- All migrated tests pass; new tests demonstrate advance-to-next-timer, conditional, countdown, and clearing patterns

**Verification**:
- `cargo test --features virtual-clock` — all tests pass
- `cargo test` (no features) — all tests pass (feature-gated code not compiled)
- `cargo clippy --all-targets --all-features` — no warnings

## What We're NOT Doing

- Changing `virtual_clock_advance()` or `virtual_clock_advance_to()` (explicit advance is orthogonal)
- Changing `sleep()`, `sleep_until()`, `timeout_at()` (timer registration unaffected)
- Adding `Send`/`Sync` to the callback (kimojio is single-threaded)
- Implementing the auto-advance-future design (separate design doc)

## Phase Status
- [x] **Phase 1: Callback Infrastructure + Tests** - Replace queue/default with callback, update runtime, migrate and add tests
- [x] **Phase 2: Documentation** - Update guide, doc comments, Docs.md

## Phase Candidates
<!-- No candidates for this focused refactoring -->

---

## Phase 1: Callback Infrastructure + Tests

### Changes Required:

- **`kimojio/src/virtual_clock.rs`**: Core state refactoring
  - Replace fields `idle_advances: VecDeque<Duration>` and `idle_advance_default: Option<Duration>` with `idle_advance_fn: Option<Box<dyn FnMut(Instant, Option<Instant>) -> Option<Duration>>>` and `idle_advance_dirty: bool` (dirty flag for self-replacement detection)
  - Remove `VecDeque` import (if no longer needed)
  - Update `new()` to initialize `idle_advance_fn: None`, `idle_advance_dirty: false`
  - Remove methods: `queue_idle_advance`, `has_idle_advances`, `take_next_idle_advance`, `pending_idle_advances`, `set_idle_advance_default`, `idle_advance_default`, `take_next_idle_advance_or_default`
  - Rename `can_idle_advance` → `has_idle_advance_fn`: returns `self.idle_advance_fn.is_some()`
  - Add `set_idle_advance_fn(&mut self, f: Option<Box<dyn FnMut(Instant, Option<Instant>) -> Option<Duration>>>)`: stores callback and sets `idle_advance_dirty = true` (so runtime knows user code touched the field)
  - Add `take_idle_advance_fn(&mut self) -> Option<Box<dyn FnMut(...)>>`: `Option::take()` for the runtime to extract before releasing borrow; also resets `idle_advance_dirty = false` (baseline before callback invocation)
  - Add `restore_idle_advance_fn(&mut self, f: Box<dyn FnMut(...)>)`: puts callback back only if `!self.idle_advance_dirty` (callback didn't replace/clear itself during invocation); if dirty, drops the old callback since user code already set a new state
  - Migrate unit tests in same file: replace queue/default tests with callback equivalents (see test plan below)

- **`kimojio/src/runtime.rs`**: Event loop update
  - Update busy-poll override (:392-399): replace `c.can_idle_advance()` with `c.has_idle_advance_fn()`. Note: this keeps busy_poll active whenever a callback is installed; the "fall through" path (callback returns `None` or `Duration::ZERO`) exits the idle block without `continue`, so the next loop iteration re-evaluates busy_poll. To prevent spinning, add a `last_idle_advanced: bool` flag to VirtualClockState: set `true` when advance produces wakers, `false` when callback returns None/ZERO. Include this flag in the busy-poll condition: `c.has_idle_advance_fn() && (task_state.any_ready() || c.last_idle_advanced_or_first_call())`
  - Replace idle advance execution block (:419-437) with new two-phase pattern:
    1. Extract callback + params: `take_idle_advance_fn()` (resets dirty flag), `now()`, `next_deadline()` in inner block
    2. Release borrow: `task_state.into_inner()`
    3. Invoke callback: `cb(now, next_deadline)` → `Option<Duration>`
    4. Re-borrow: `cell.borrow_mut()`
    5. Restore callback via `restore_idle_advance_fn(cb)` — this method internally checks the dirty flag; if dirty (callback called `set_idle_advance` or `clear_idle_advance` during invocation), the old callback is dropped instead of restored
    6. If `Some(dur)` with `!dur.is_zero()`: call `clock.advance(dur)`, collect wakers, set `last_idle_advanced = true`, release borrow, wake, re-borrow, continue
    7. Else: set `last_idle_advanced = false`, fall through (no advance, runtime proceeds to io_uring)

- **`kimojio/src/operations.rs`**: Public API replacement
  - Remove `virtual_clock_advance_idle` (currently :2156-2162)
  - Remove `virtual_clock_pending_idle_advances` (currently :2170-2176)
  - Remove `virtual_clock_advance_idle_default` (currently :2215-2229)
  - Add `virtual_clock_set_idle_advance(f: impl FnMut(Instant, Option<Instant>) -> Option<Duration> + 'static)`: wraps in `Box` and delegates to `VirtualClockState::set_idle_advance_fn`. Accepts the closure directly (not `Option`) for ergonomics — clearing uses a separate call or passing a `None`-returning closure.
    Actually — accept `Option<impl FnMut(...) + 'static>` to support clearing with `None`, matching Spec FR-008. The `impl` approach doesn't work with `Option<impl>` directly, so the public signature should accept `Box<dyn FnMut(...)>` wrapped by a convenience that takes `impl`. Alternatively, provide two functions: `virtual_clock_set_idle_advance` taking `impl FnMut + 'static` and `virtual_clock_clear_idle_advance` to clear. The two-function approach is cleaner for callers. Use this approach.
  - Add `virtual_clock_clear_idle_advance()`: clears callback (sets to `None`). **Note**: Spec FR-008 describes passing `None` to a single function; the two-function split (`set` + `clear`) is the Rust-idiomatic realization of that requirement since `Option<impl Trait>` is not expressible.
  - Add `virtual_clock_has_idle_advance() -> bool`: delegates to `has_idle_advance_fn()`
  - Migrate unit tests in operations.rs test module (see test plan below)

- **`kimojio/tests/virtual_clock_integration.rs`**: Integration test migration + new tests (see test plan below)

### Test Plan:

**Migrated tests** (equivalent behavior to old tests, proving callback can do everything the old API did):

| Old Test | New Test | Callback Used |
|----------|----------|---------------|
| `idle_advance_completes_sleep` | `callback_completes_sleep` | advance-to-next: `\|now, next\| next.map(\|d\| d.saturating_duration_since(now))` |
| `idle_advance_fires_in_order` | `callback_fires_timers_in_order` | advance-to-next (same callback, fires successive timers) |
| `idle_advance_wakes_spawned_task` | `callback_wakes_spawned_task` | advance-to-next |
| `pending_idle_advances_count` | `has_idle_advance_reflects_state` | Set callback → `true`, clear → `false` |
| `idle_advance_default_resolves_sleep` | `fixed_duration_callback_resolves_sleep` | fixed: `\|_, _\| Some(Duration::from_millis(1))` |
| `idle_advance_default_cleared` | `cleared_callback_stops_advancement` | Set then clear, verify no auto-advance |

Plus equivalent migrations for the operations.rs and virtual_clock.rs unit tests:

**operations.rs unit test migrations** (7 tests):

| Old Test | New Test | Strategy |
|----------|----------|----------|
| `idle_advance_completes_sleep` | `callback_completes_sleep` | advance-to-next callback |
| `idle_advance_60s_completes_fast` | `callback_60s_completes_fast` | advance-to-next, wall-clock assertion |
| `multiple_idle_advances_fire_in_sequence` | `callback_fires_timers_in_sequence` | advance-to-next, verify clock positions |
| `idle_advance_spawned_task` | `callback_wakes_spawned_task` | advance-to-next, spawned + main |
| `idle_advance_default_completes_sleep` | `fixed_callback_completes_sleep` | fixed 1ms callback |
| `idle_advance_default_none_is_noop` | `no_callback_is_noop` | verify no advancement without callback |
| `queue_drains_before_default` | `replace_callback_changes_behavior` | install one callback, sleep, replace with different callback, sleep |

**virtual_clock.rs unit test migrations** (8 tests — these test internal state, adapt to new field):

| Old Test | New Test | Strategy |
|----------|----------|----------|
| `queue_idle_advance_enqueues` | `set_idle_advance_fn_stores` | Set callback, verify `has_idle_advance_fn()` |
| `take_next_idle_advance_applies_in_order` | Remove — queue ordering is N/A with callback |
| `take_next_idle_advance_returns_none_when_empty` | `take_idle_advance_fn_returns_none_when_empty` | No callback → take returns None |
| `idle_advance_default_none_by_default` | `idle_advance_fn_none_by_default` | No callback by default, `has_idle_advance_fn()` false |
| `set_idle_advance_default` | `set_and_clear_idle_advance_fn` | Set, verify has, clear (set None), verify not has |
| `zero_duration_default_treated_as_none` | Remove — Duration::ZERO is now a runtime-level guard, not state-level filtering |
| `take_next_uses_default_when_queue_empty` | Remove — no queue/default distinction |
| `queue_takes_priority_over_default` | Remove — no queue/default distinction |

Tests removed (4): these tested queue-specific mechanics that no longer exist. Their behavioral coverage is subsumed by the new callback tests above and the integration tests.

**New tests** (demonstrate capabilities the old API couldn't express):

| Test | What it verifies |
|------|-----------------|
| `advance_to_next_timer_one_liner` | The `\|now, next\| next.map(\|d\| d - now)` pattern completes a 60s sleep in <10ms wall time (SC-001) |
| `conditional_callback_with_flag` | Callback captures `Rc<Cell<bool>>`; when flag is false, returns `None` (no advance); when true, advances. Toggle from spawned task. (SC-007 variant) |
| `countdown_callback` | Callback with captured counter, decrements each call, returns `None` at zero. Verify exactly N advances occur. (SC-007) |
| `callback_receives_none_when_no_timers` | Install callback, don't register any timers, verify callback receives `None` for next_deadline and returns `None` (SC-005) |
| `zero_duration_return_does_not_loop` | Callback always returns `Some(Duration::ZERO)`. Verify runtime doesn't hang — falls through without advancing. (SC-006) |
| `replace_callback_mid_test` | Install one callback, sleep, install a different callback, sleep again. Verify both behaviors. |
| `callback_self_clears_during_invocation` | Callback calls `virtual_clock_clear_idle_advance()` inside itself (safe since borrow is released), then returns `Some(dur)`. Verify the advance fires AND the callback is gone afterward (not restored). |
| `callback_receives_correct_now_and_deadline` | Callback captures `Rc<Cell<(Instant, Option<Instant>)>>` to record parameters. Verify `now` matches `virtual_clock_epoch()` and `next_deadline` matches the registered sleep deadline. |

### Success Criteria:

#### Automated Verification:
- [ ] `cargo test --features virtual-clock` — all tests pass (migrated + new)
- [ ] `cargo test` — all tests pass (no feature = no virtual clock code compiled)
- [ ] `cargo clippy --all-targets --all-features` — no warnings
- [ ] `cargo fmt --check` — no formatting issues

#### Manual Verification:
- [ ] Old functions `virtual_clock_advance_idle`, `virtual_clock_advance_idle_default`, `virtual_clock_pending_idle_advances` no longer exist in `operations.rs` (SC-008)
- [ ] `VecDeque<Duration>` and `idle_advance_default: Option<Duration>` no longer exist in `VirtualClockState`
- [ ] Advance-to-next-timer test completes 60s virtual sleep in <10ms wall time

---

## Phase 2: Documentation

### Changes Required:

- **`.paw/work/idle-advance-callback/Docs.md`**: Technical reference (load `paw-docs-guidance`)
- **`docs/virtual-clock-guide.md`**: Update API reference table (lines 49-61) — remove three old entries, add `virtual_clock_set_idle_advance`, `virtual_clock_clear_idle_advance`, `virtual_clock_has_idle_advance`. Rewrite idle advance prose sections (lines 260-306) to explain callback model with examples. Update "Manual advancement only" caveats section (lines 237-258) if relevant.
- **`docs/virtual-clock-design.md`**: Update API sections that reference `virtual_clock_advance_idle` and `virtual_clock_pending_idle_advances` to use the new callback API.
- **`README.md`**: Update the virtual clock code snippet that uses `virtual_clock_advance_idle(Duration::from_secs(60))` to use the callback API.
- **`examples/retry-backoff/src/main.rs`**: Rewrite the idle advance calls (lines 72-74) to use `virtual_clock_set_idle_advance` with an appropriate callback pattern.
- **Doc comments in `operations.rs`**: Comprehensive doc comments on new public functions with usage examples showing advance-to-next-timer, fixed-duration, and conditional patterns.
- **Doc comments in `virtual_clock.rs`**: Update struct-level and method-level docs for changed internals.

### Success Criteria:
- [ ] `docs/virtual-clock-guide.md` contains no references to removed APIs
- [ ] `docs/virtual-clock-design.md` contains no references to removed APIs
- [ ] `README.md` virtual clock snippet uses callback API
- [ ] `examples/retry-backoff/` compiles and uses callback API
- [ ] New API functions have doc comments with `# Examples` sections
- [ ] Content accurately reflects implemented behavior

---

## References
- Spec: `.paw/work/idle-advance-callback/Spec.md`
- Research: `.paw/work/idle-advance-callback/CodeResearch.md`
- Issue: none
