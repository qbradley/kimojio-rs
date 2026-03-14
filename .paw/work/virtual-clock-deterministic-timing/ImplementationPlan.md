# Virtual Clock Deterministic Timing Implementation Plan

## Overview

We're adding deterministic virtual time to kimojio so upstream consumers can write fast, reliable timeout tests. The implementation introduces a `Clock` trait abstraction with `SystemClock` (production) and `VirtualClock` (testing) implementations, gated behind a `virtual-clock` Cargo feature flag. When the feature is off, the codebase is byte-for-byte identical to today. When on, `operations::sleep()` and all deadline-based functions dispatch through the clock, allowing tests to advance virtual time explicitly and instantly.

## Current State Analysis

- `Runtime` is a non-generic struct with 3 fields (`busy_poll`, `server_pipe`, `client_pipe`) — `runtime.rs:297-301`
- `TaskState` is a thread-local struct accessed via `TaskState::get()` — `task.rs:457-570`
- `operations::sleep()` directly creates an io_uring `Timeout` SQE — `operations.rs:1066-1079`
- `SleepFuture` wraps `UnitFuture<'a>` via `pin_project_lite` — `operations.rs:1082-1114`
- 6 production `Instant::now()` sites in deadline functions need virtualization — see CodeResearch.md §13
- `Configuration` uses a builder pattern (`set_busy_poll()`, `set_trace_buffer_manager()`) — `configuration.rs:13-73`
- Feature flags already exist (`tls`, `fault_injection`, etc.) — `Cargo.toml:34-51`
- No docs/ directory; documentation is inline `///` comments and README.md

## Desired End State

- `Clock` trait with `now()`, `register_timer()`, `cancel_timer()`, `is_real()` — public API for extensibility
- `VirtualClock` with priority-based timer wheel, `advance()`, `advance_to()`, `next_deadline()` — for test authors
- `operations::sleep()`, `sleep_until()`, `timeout_at()` dispatch through clock transparently
- All 6 production `Instant::now()` sites use `clock.now()` when virtual clock feature is active
- `Runtime::new()` signature unchanged — zero breaking changes
- `virtual-clock` feature flag gates all new types and behavior
- Comprehensive tests covering all Spec acceptance scenarios
- Documentation: usage guide, design document, example, updated README

**Verification**:
- `cargo test` — all existing tests pass (feature off)
- `cargo test --features virtual-clock` — all existing + new tests pass (feature on)
- `cargo clippy --all-targets --all-features` — no warnings
- Virtual sleep test completes 60s virtual timeout in <10ms wall time

## What We're NOT Doing

- iomgr-level `Instant::now()` replacements (upstream consumer responsibility)
- Multi-threaded `Send`/`Sync` VirtualClock (kimojio is single-threaded)
- TSC/Timer profiling virtualization (`timer.rs` is for metrics, not logic)
- Event loop optimization to skip io_uring when only virtual timers pending (deferred)
- Changes to `#[kimojio::main]` or `#[kimojio::test]` macros
- Modifying test-only `Instant::now()` calls (sites #7-16 in CodeResearch.md)

## Phase Status
- [ ] **Phase 1: Clock Infrastructure** - Clock trait, VirtualClock timer wheel, Configuration/TaskState integration, feature flag
- [ ] **Phase 2: Sleep & Timeout Virtualization** - Virtual sleep dispatch, sleep_until, timeout_at, VirtualSleepFuture with Drop cancellation
- [ ] **Phase 3: Deadline I/O Virtualization** - Replace 6 Instant::now() sites with clock-aware dispatch
- [ ] **Phase 4: Documentation** - Usage guide, design doc, example, README update, Docs.md

## Phase Candidates
<!-- None — all phases are fully defined -->

---

## Phase 1: Clock Infrastructure

### Changes Required:

- **`kimojio/Cargo.toml`**: Add `virtual-clock = []` feature flag (no additional dependencies — uses `std::collections::BinaryHeap`, `Rc`, `RefCell`)

- **`kimojio/src/clock.rs`** (new file): Core clock module, feature-gated with `#[cfg(feature = "virtual-clock")]`
  - `Clock` trait: `now(&self) -> Instant`, `register_timer(&self, deadline: Instant, waker: Waker) -> TimerId`, `cancel_timer(&self, id: TimerId)`, `fn is_real(&self) -> bool { true }` (default impl)
  - `TimerId(u64)`: opaque timer identifier
  - `SystemClock` struct implementing `Clock` — `now()` delegates to `Instant::now()`, timer methods are no-ops (real timers go through io_uring), `is_real()` returns `true`
  - `VirtualClock` struct wrapping `Rc<RefCell<VirtualClockState>>` — implements `Clone` for shared ownership between runtime internals and test code
  - `VirtualClockState`: `current: Instant`, `timers: BinaryHeap<VirtualTimer>` (min-heap by deadline), `next_timer_id: u64`
  - `VirtualTimer`: `id: TimerId`, `deadline: Instant`, `waker: Waker` — implements `Ord` with reversed comparison for min-heap behavior
  - `VirtualClock::new(start: Instant) -> Self` — constructor
  - `VirtualClock::advance(&self, duration: Duration) -> usize` — advances time, pops expired timers, wakes tasks, returns count
  - `VirtualClock::advance_to(&self, target: Instant) -> usize` — advances to specific instant
  - `VirtualClock::next_deadline(&self) -> Option<Instant>` — peek next timer (for test stepping)
  - `VirtualClock::now(&self) -> Instant` — convenience accessor (same as `Clock::now()`)
  - `Clock` impl for `VirtualClock`: `now()` returns `state.current`, `register_timer()` pushes to heap, `cancel_timer()` removes by ID (O(n) linear scan — acceptable for test workloads), `is_real()` returns `false`

- **`kimojio/src/configuration.rs`**: Add feature-gated clock field
  - Add `#[cfg(feature = "virtual-clock")] pub(crate) clock: Option<Rc<dyn Clock>>` field to `Configuration`
  - Add `#[cfg(feature = "virtual-clock")] set_clock(self, clock: impl Clock) -> Self` builder method — wraps in `Rc` and stores
  - Update `Default` impl to include `clock: None` when feature enabled
  - Since `Configuration` uses destructuring in `Runtime::new()`, the destructuring pattern needs feature-gated field

- **`kimojio/src/task.rs`**: Add feature-gated clock field to `TaskState`
  - Add `#[cfg(feature = "virtual-clock")] pub(crate) clock: Option<Rc<dyn Clock>>` to `TaskState` struct (after existing fields)
  - Initialize to `None` in `Default` impl

- **`kimojio/src/runtime.rs`**: Wire clock from Configuration → TaskState
  - In `Runtime::new()`: extract clock from Configuration (feature-gated), store in `task_state.clock`
  - Add `#[cfg(feature = "virtual-clock")] pub fn new_with_clock(thread_index: u8, configuration: Configuration, clock: impl Clock) -> Self` — satisfies FR-004, sets clock via Configuration internally
  - Add `#[cfg(feature = "virtual-clock")] pub fn new_virtual(thread_index: u8, configuration: Configuration) -> (Self, VirtualClock)` — convenience constructor that creates a VirtualClock, injects it via `new_with_clock`, returns both

- **`kimojio/src/lib.rs`**: Module declaration and re-exports
  - Add `#[cfg(feature = "virtual-clock")] pub mod clock;`
  - Add `#[cfg(feature = "virtual-clock")] pub use clock::{Clock, SystemClock, VirtualClock, TimerId};`

- **`kimojio/src/clock.rs` tests**: Comprehensive unit tests for VirtualClock
  - Timer registration and advance fires timers in deadline order
  - `advance()` with no timers returns 0
  - `advance()` by zero fires no timers
  - `cancel_timer()` prevents firing
  - `next_deadline()` returns earliest deadline
  - `advance_to()` works correctly
  - Multiple timers at same deadline all fire
  - Timer registration with past deadline fires on next advance

### Success Criteria:

#### Automated Verification:
- [ ] `cargo test --features virtual-clock` — clock unit tests pass
- [ ] `cargo test` — all existing tests pass (feature off, no code changes visible)
- [ ] `cargo clippy --all-targets --all-features` — no warnings
- [ ] `cargo fmt --check` — no formatting issues

#### Manual Verification:
- [ ] `Configuration::new()` unchanged for existing users
- [ ] `Runtime::new()` signature unchanged
- [ ] `Runtime::new_virtual()` creates a runtime with VirtualClock handle

---

## Phase 2: Sleep & Timeout Virtualization

### Changes Required:

- **`kimojio/src/clock.rs`**: Add `VirtualSleepFuture` type (feature-gated)
  - Struct: `deadline: Instant`, `clock: Rc<dyn Clock>`, `timer_id: Option<TimerId>`, `completed: bool`
  - `Future` impl: check `clock.now() >= deadline` → Ready; otherwise register timer on first poll, return Pending
  - `Drop` impl: cancel timer via `clock.cancel_timer(id)` if registered (FR-010)
  - No pinning needed (all fields are `Unpin`)

- **`kimojio/src/operations.rs`**: Modify `sleep()` and add new functions
  - **`sleep()`**: Add feature-gated dispatch at the top — get `TaskState`, check if virtual clock exists, branch:
    - Virtual path: compute `deadline = clock.now() + duration`, create `SleepFuture::Virtual`
    - Real path: existing io_uring code (unchanged)
    - Critical: release TaskState borrow before creating `UnitFuture::with_polled()` (it re-borrows internally)
  - **`SleepFuture`**: Restructure from struct to `pin_project_lite` enum with `Real { fut: UnitFuture<'a> }` and `Virtual { fut: VirtualSleepFuture }` variants (the Virtual variant feature-gated)
    - `Future` impl: dispatch to inner variant
    - `FusedFuture` impl: dispatch to inner variant
    - `cancel()` method: dispatch to inner variant (virtual: just drop/cancel timer)
    - When feature is off, the enum has only `Real` variant — compiler optimizes to identical code
  - **`sleep_until(deadline: Instant)` (new)**: Feature-gated public function. Similar to `sleep()` but takes absolute `Instant`. Virtual path: register timer directly at deadline. Real path: compute `deadline - Instant::now()` and delegate to existing `sleep()`.
  - **`timeout_at(deadline: Instant, future: impl Future)` (new)**: Feature-gated public function. Returns `Result<T, TimeoutError>`. Implementation: use `futures::future::select()` (not `futures::select!` — avoids requiring `FusedFuture` on the inner future) between the inner future and `sleep_until(deadline)`. When virtual clock advances past deadline, sleep completes and returns timeout error.
  - Add `pub(crate) fn clock_now() -> Instant` helper **in `clock.rs`** (not operations.rs): feature-gated function that checks TaskState for virtual clock, returns `clock.now()` or `Instant::now()`. Placed in `clock.rs` to avoid coupling `async_event`/`async_lock` to `operations`. Used by Phase 2 and Phase 3 across all modules.

- **Tests** (in `kimojio/src/operations.rs` test module or `clock.rs` test module):
  - **Test harness pattern**: Virtual clock tests cannot use `#[crate::test]` (which creates a standard runtime). Instead, create a helper like `fn run_virtual_test(test: impl FnOnce(VirtualClock) -> F)` that constructs `Runtime::new_virtual()`, calls `block_on()`, and provides the clock handle to the test closure. Or use `Runtime::new_virtual()` + `block_on()` directly in each test.
  - P1 scenarios: virtual sleep completes on advance, stays pending without advance, multiple sleeps wake in order, real sleep unaffected
  - P3 scenarios: `sleep_until` completes at deadline, past deadline completes immediately
  - P4 scenarios: `timeout_at` returns inner result on fast future, returns timeout error on advance
  - P5 scenarios: dropped VirtualSleepFuture removes timer, no spurious wakeups
  - Integration test: `wait_with_deadline` on AsyncEvent with virtual clock — both `Instant::now()` and inner `sleep()` get virtualized, verify the loop in `async_event.rs:146-176` works correctly with virtual time

### Success Criteria:

#### Automated Verification:
- [ ] `cargo test --features virtual-clock` — all new sleep/timeout tests pass
- [ ] `cargo test` — all existing tests still pass (feature off)
- [ ] `cargo clippy --all-targets --all-features` — no warnings
- [ ] Virtual 60s sleep test completes in <10ms wall time

#### Manual Verification:
- [ ] `operations::sleep()` function signature unchanged
- [ ] `SleepFuture` type name unchanged (enum internals are private)
- [ ] `cancel()` method works for both real and virtual paths

---

## Phase 3: Deadline I/O Virtualization

### Changes Required:

- **`kimojio/src/operations.rs`**: Replace 3 `Instant::now()` calls in deadline functions with `clock_now()` helper from Phase 2
  - `writev_with_deadline` at line 567: `deadline.checked_duration_since(Instant::now())` → `deadline.checked_duration_since(clock_now())`
  - `write_with_deadline` at line 630: same pattern
  - `read_with_deadline` at line 845: same pattern

- **`kimojio/src/async_event.rs`**: Replace 2 `Instant::now()` calls
  - `wait_with_deadline` at line 150: `deadline.checked_duration_since(Instant::now())` → `deadline.checked_duration_since(clock_now())`
  - `wait_with_timeout` at line 187: `Instant::now() + timeout` → `clock_now() + timeout`
  - Import `clock_now` from operations (or clock module)

- **`kimojio/src/async_lock.rs`**: Replace 1 `Instant::now()` call
  - `lock_with_timeout` at line 84: `Instant::now() + timeout` → `clock_now() + timeout`
  - Import `clock_now`

- **Tests**: 
  - P2 scenarios: `write_with_deadline` computes correct remaining time from virtual clock, past virtual deadline returns timeout immediately
  - Integration test: create virtual runtime, advance time, verify `wait_with_deadline` on AsyncEvent respects virtual time
  - Integration test: `lock_with_timeout` with virtual clock computes deadline from virtual now

### Success Criteria:

#### Automated Verification:
- [ ] `cargo test --features virtual-clock` — all deadline virtualization tests pass
- [ ] `cargo test` — all existing tests still pass (feature off)
- [ ] `cargo clippy --all-targets --all-features` — no warnings

#### Manual Verification:
- [ ] All 6 `Instant::now()` production sites verified replaced (grep for remaining calls shows only test/timer code)
- [ ] Deadline I/O still works correctly with real time (no regressions)

---

## Phase 4: Documentation

### Changes Required:

- **`.paw/work/virtual-clock-deterministic-timing/Docs.md`**: Technical reference (load `paw-docs-guidance`)
  - Implementation details, architecture decisions, file-level changes
  - Verification approach and test coverage summary

- **`docs/virtual-clock-guide.md`** (new): User-facing guide for virtual clock usage
  - Getting started: enabling `virtual-clock` feature, creating virtual runtime
  - API reference: `VirtualClock::advance()`, `advance_to()`, `next_deadline()`
  - Common patterns: deterministic sleep tests, deadline testing, timeout testing, stepping through retries
  - Caveats: real I/O deadlines use wall time, timer wakeup order vs execution order
  - Migration: zero changes for existing code

- **`docs/virtual-clock-design.md`** (new): Detailed design document
  - Architecture: Clock trait, VirtualClock timer wheel, TaskState integration
  - Design decisions: why feature-gated, why Rc<RefCell<>>, why BinaryHeap
  - Virtual vs real timer path diagrams
  - Performance characteristics: zero overhead when using SystemClock
  - Known limitations and future work

- **`examples/virtual_time/`** (new): Example demonstrating virtual time
  - `Cargo.toml`: depends on `kimojio` with `virtual-clock` feature
  - `src/main.rs`: demonstrates creating virtual runtime, spawning sleeps, advancing time, assertions
  - Add to workspace members in root `Cargo.toml`

- **`README.md`**: Add "Virtual Clock for Testing" section
  - Brief description and link to guide
  - Quick example snippet

### Success Criteria:

#### Automated Verification:
- [ ] `cargo build --example` (if applicable) or `cargo check -p virtual_time` — example compiles
- [ ] `cargo clippy --all-targets --all-features` — no warnings
- [ ] `cargo doc --features virtual-clock --no-deps` — docs build without warnings

#### Manual Verification:
- [ ] Guide covers all user scenarios from Spec
- [ ] Design doc accurately reflects implemented architecture
- [ ] Example runs successfully and demonstrates key patterns
- [ ] README section is concise and links to guide

---

## References
- Issue: none
- Spec: `.paw/work/virtual-clock-deterministic-timing/Spec.md`
- Research: `.paw/work/virtual-clock-deterministic-timing/CodeResearch.md`
- Design input: `/tmp/design.md`
