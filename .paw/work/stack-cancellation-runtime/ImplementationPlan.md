# Stack Cancellation Runtime Implementation Plan

## Overview

Implement the cancellation, lifetime, and reclamation design in `kimojio-stack/CANCEL-DESIGN.md` so stackful tasks, waits, external wakes, async I/O, and scopes have explicit lifecycle ownership. The plan proceeds in small jj-backed phases that first make task identities generation-safe, then make waits cancellation-safe, then make cross-thread and I/O resource ownership deterministic, and finally document the as-built behavior.

The implementation is intentionally staged because scheduler task identity is a foundation for stale-wake safety, waiter cancellation depends on task identity, cross-thread external wake handling depends on both, and deterministic I/O reclamation depends on the runtime being able to cancel tasks and waits without leaving unowned kernel-visible resources.

Each phase may be split into multiple logical `jj new` changes during implementation. Create a new jj change for each coherent subset of work that can be verified independently, and do not create or checkout a git branch.

## Current State Analysis

- `TaskId` is currently a bare `usize`, and scheduler storage is indexed by `Vec<Option<Task>>`, `FixedBitSet`, and `VecDeque<TaskId>` (`kimojio-stack/src/lib.rs:271`, `kimojio-stack/src/lib.rs:3627-3630`). `reserve_task` appends a new slot, while `schedule` only checks that the slot currently contains a task (`kimojio-stack/src/lib.rs:3690-3711`).
- `run_one` removes completed tasks from the task vector and calls `scope_state.task_finished()` but does not provide generation-safe stale wake rejection (`kimojio-stack/src/lib.rs:4131-4173`).
- `ScopeStateInner` tracks `remaining`, `panic_payloads`, `task_ids`, and `waiters`, and `task_started` appends child ids while `task_finished` only decrements `remaining` (`kimojio-stack/src/lib.rs:3108-3155`).
- Local waits use `Waitable::add_waiter`, `Waiter { scheduler, task_id }`, and `Waiters { first, rest }`; `wait_all` and `wait_any` register waiters without registration guards (`kimojio-stack/src/wait.rs:16-22`, `kimojio-stack/src/lib.rs:3158-3169`, `kimojio-stack/src/lib.rs:3363-3399`, `kimojio-stack/src/wait.rs:59-142`).
- Cross-thread waits use `ExternalWaiter` with an `AtomicBool` active flag and `ExternalWakeInner { waiters, ready: VecDeque<TaskId> }`; cross-thread channel send/recv add `ExternalWaiter` values before parking (`kimojio-stack/src/lib.rs:3176-3287`, `kimojio-stack/src/lib.rs:3357-3360`, `kimojio-stack/src/channel/cross_thread.rs:392-416`, `kimojio-stack/src/channel/cross_thread.rs:480-504`).
- Async I/O results hold buffer ownership and `IoState`; `IoResult::drop` and `Timeout::drop` call `cancel_pending`, while `IoState` stores result, waiters, timeout, and registered resources (`kimojio-stack/src/lib.rs:2633-2736`, `kimojio-stack/src/lib.rs:2860-2918`, `kimojio-stack/src/lib.rs:3001-3096`).
- `Runtime::block_on` drives the scheduler, drains scheduler I/O after the main task, and asserts there are no remaining tasks or in-flight I/O (`kimojio-stack/src/lib.rs:584-610`, `kimojio-stack/src/lib.rs:4051-4058`).
- Existing verification includes Rust CI commands for clippy, tests, fmt, and all-features clippy (`.github/workflows/rust.yml:48-51`) and repository-specific instructions requiring `cargo fmt`, `cargo clippy`, `cargo clippy --all-targets --all-features`, and tests (`.copilot-instructions.md:1-20`).

## Desired End State

- Scheduler-facing task references are generation-checked and stale waiters/external wakes cannot schedule completed, canceled, or reused task slots.
- Local wait registration uses private cancellation-safe guards so timed-out, winning, canceled, or unwound wait paths invalidate non-winning registrations.
- Cross-thread wait queues and external wake draining use generation-checked identities and explicit external registration state.
- Async I/O uses a cheap-clone runtime-owned fd identity for unregistered async operations, and dropped pending results cancel by default while operation resources remain owned until completion is reaped.
- Runtime shutdown and scope exit drain/cancel/reap submitted or detached operations before returning.
- Long-lived scopes and scheduler storage are bounded by live or recently-live work after warmup.
- Tests cover cancellation races, stale waits, fd lifetime safety, resource reclamation, panic propagation, and bounded metadata growth.
- Benchmarks or measurement tests report waiter tombstone behavior under timeout/cancellation churn.

## What We're NOT Doing

- Not replacing the stackful coroutine backend or changing the thread-per-core runtime model.
- Not introducing a multi-threaded global scheduler.
- Not exposing a stable public extension API for custom `Waitable` implementations.
- Not choosing final wait-queue tombstone compaction thresholds before measurement data exists.
- Not using per-operation `dup`/`fcntl(F_DUPFD_CLOEXEC)` for async fd lifetime.
- Not optimizing unrelated crates or unrelated runtime subsystems.

## Phase Status

- [x] **Phase 1: Generation-Safe Task Identity** - Introduce generation-checked task keys and scheduler slot reuse.
- [x] **Phase 2: Cancellation-Safe Local Waits** - Add private waiter registrations and invalidate stale local waits.
- [x] **Phase 3: External Wake and Scope Quiescence** - Make cross-thread waiter cancellation and staged wake draining generation-safe.
- [x] **Phase 4: Async I/O Ownership and Runtime Fd Handles** - Add cheap-clone fd identity and operation-owned fd/resource leases.
- [x] **Phase 5: Detached Operation Reaping and Scope Metadata Cleanup** - Reap detached operations and bound long-lived scope/task metadata.
- [x] **Phase 6: Tombstone Measurement and Documentation** - Add waiter churn measurements and write as-built technical docs.

## Phase Candidates

---

## Phase 1: Generation-Safe Task Identity

### Changes Required:

- **`kimojio-stack/src/lib.rs` task identity**: Replace scheduler-facing `TaskId = usize` with a generation-checked task identity for runtime scheduling. Preserve an internal slot index where needed, but ensure local `Waiter`, scheduler ready queues, and scope child tracking can distinguish stale references from current slot occupants. Perform only the mechanical external-wake type adaptation needed for compilation; Phase 3 owns the external registration state-machine redesign.
- **`kimojio-stack/src/lib.rs` scheduler storage**: Add per-slot generation metadata and a free-slot list around the current `tasks`, `queued`, and `ready` scheduler storage described in CodeResearch (`kimojio-stack/src/lib.rs:3627-3648`).
- **`kimojio-stack/src/lib.rs` scheduler operations**: Update `reserve_task`, `schedule`, `run_one`, and ready queue pop paths so stale generations are discarded and completed slots become reusable (`kimojio-stack/src/lib.rs:3690-3711`, `kimojio-stack/src/lib.rs:4131-4173`).
- **`kimojio-stack/src/lib.rs` scope state integration**: Update `ScopeState::task_started` and `ScopeState::task_finished` call sites to use generation-safe task identity while preserving existing panic propagation (`kimojio-stack/src/lib.rs:3108-3146`).
- **Tests**: Add scheduler-focused tests in `kimojio-stack/src/lib.rs` near existing runtime tests to cover stale wake rejection, slot reuse after repeated spawn/join, and panic propagation preservation.

### Success Criteria:

#### Automated Verification:
- [ ] Targeted task-identity tests prove stale task references cannot schedule reused slots and spawn/complete churn reuses scheduler slots after warmup.
- [ ] Tests pass: `cargo test -p kimojio-stack --all-targets --all-features`
- [ ] Formatting passes: `cargo fmt --all --check`
- [ ] Lint passes: `cargo clippy --all-targets -- -D warnings`
- [ ] All-features lint passes: `cargo clippy --all-targets --all-features -- -D warnings`

#### Manual Verification:
- [ ] A stale task reference cannot schedule a reused task slot.
- [ ] Repeated spawn/complete churn reuses scheduler slots after warmup.
- [ ] Existing unobserved child panic propagation behavior remains intact.

---

## Phase 2: Cancellation-Safe Local Waits

### Changes Required:

- **`kimojio-stack/src/lib.rs` waiter model**: Introduce a private wait registration/token model connected to the current `Waiter` and `Waiters` structures (`kimojio-stack/src/lib.rs:3158-3169`, `kimojio-stack/src/lib.rs:3363-3399`).
- **`kimojio-stack/src/wait.rs` waitable protocol**: Replace the internal `Waitable::add_waiter` flow with a private registration-returning flow while keeping public wait helpers usable (`kimojio-stack/src/wait.rs:16-22`, `kimojio-stack/src/wait.rs:59-142`).
- **`kimojio-stack/src/lib.rs` park/waiter integration**: Update `RuntimeContext::park` and waiter creation so active registrations are invalidated on timeout, cancellation, or unwind (`kimojio-stack/src/lib.rs:2003-2021`).
- **`kimojio-stack/src/lib.rs` local wait producers**: Update scope waiters, join handles, I/O result waiters, timeout waiters, and other local waitable implementations to use registration guards.
- **`kimojio-stack/src/lib.rs` tombstone handling**: Add a provisional opportunistic compaction mechanism for invalidated local waiter tokens. Use Phase 2 tests to enforce bounded behavior for known churn patterns, while Phase 6 measures and tunes thresholds.
- **Tests**: Add tests for `wait_any`/`wait_all` timeout cleanup, canceled task waiter cleanup, stale local wake skipping, and repeated timeout churn not accumulating unbounded live waiters.

### Success Criteria:

#### Automated Verification:
- [x] Tests pass: `cargo test -p kimojio-stack --all-targets --all-features`
- [x] Formatting passes: `cargo fmt --all --check`
- [x] Lint passes: `cargo clippy --all-targets -- -D warnings`
- [x] All-features lint passes: `cargo clippy --all-targets --all-features -- -D warnings`

#### Manual Verification:
- [x] Timed-out and non-winning wait registrations cannot consume later wakes.
- [x] Canceled or unwound tasks do not remain live in local waiter queues.
- [x] Invalidated waiter tombstones are compacted by a provisional mechanism before known churn patterns grow without bound.
- [x] Local wait registration remains private and does not stabilize a public extension API.

---

## Phase 3: External Wake and Scope Quiescence

### Changes Required:

- **`kimojio-stack/src/lib.rs` external waiter model**: Redesign `ExternalWaiter` and `ExternalWake` state so external ready queues carry generation-checked task identity and registration state instead of bare `TaskId` (`kimojio-stack/src/lib.rs:3176-3287`, `kimojio-stack/src/lib.rs:3357-3360`). This phase owns the external wait registration state machine; Phase 1 only prepares task identity types.
- **`kimojio-stack/src/lib.rs` external ready drain**: Update `drain_external_ready` and scheduler integration so stale external wakes are discarded and live staged wakes are scheduled before scope quiescence decisions (`kimojio-stack/src/lib.rs:3844-3851`, `kimojio-stack/src/lib.rs:4060-4128`).
- **`kimojio-stack/src/channel/cross_thread.rs` stackful channel waits**: Update stackful send/recv wait registration and wait queue internals to own cancelable external registrations (`kimojio-stack/src/channel/cross_thread.rs:211-218`, `kimojio-stack/src/channel/cross_thread.rs:392-416`, `kimojio-stack/src/channel/cross_thread.rs:480-504`).
- **`kimojio-stack/src/lib.rs` scope quiescence**: Ensure the scope-wait path synchronizes externally staged ready work and I/O/timeout completions that may make scoped tasks runnable before deciding child work is quiescent (`kimojio-stack/src/lib.rs:3117-3134`, `kimojio-stack/src/lib.rs:4060-4128`).
- **Tests**: Add wake/cancel race tests for cross-thread stackful send and recv, staged external wake before scope exit, I/O or timeout completion racing with scope exit, and external waiter count consistency.

### Success Criteria:

#### Automated Verification:
- [x] Tests pass: `cargo test -p kimojio-stack --all-targets --all-features`
- [x] Formatting passes: `cargo fmt --all --check`
- [x] Lint passes: `cargo clippy --all-targets -- -D warnings`
- [x] All-features lint passes: `cargo clippy --all-targets --all-features -- -D warnings`

#### Manual Verification:
- [x] Cross-thread wake/cancel races do not underflow or leak external waiter accounting.
- [x] A staged external wake for a live scoped task is considered before scope cancellation.
- [x] I/O or timeout completions that make scoped tasks runnable are considered before scope cancellation.
- [x] Stale external ready entries do not schedule reused task slots.

---

## Phase 4: Async I/O Ownership and Runtime Fd Handles

### Changes Required:

- **`kimojio-stack/src/lib.rs` runtime fd identity**: Add a cheap-clone runtime-owned fd handle for unregistered async I/O, initially backed by local reference-counted state and shaped so it can later become a scheduler fd-table handle without changing the public type.
- **`kimojio-stack/src/lib.rs` async I/O APIs**: Update unregistered `read_async` and `write_async` paths to require the runtime-owned fd identity rather than borrowed fd lifetimes (`kimojio-stack/src/lib.rs:1393-1445`, `kimojio-stack/src/lib.rs:1608-1658`).
- **`kimojio-stack/src/lib.rs` operation resources**: Move fd and registered-resource leases into operation-owned state so kernel-visible fd resources remain live until completion is reaped (`kimojio-stack/src/lib.rs:2633-2736`, `kimojio-stack/src/lib.rs:3001-3096`, `kimojio-stack/src/lib.rs:2590-2629`). Preserve existing typed buffer ownership for consumed `IoResult`s; deterministic reclamation of dropped pending buffers belongs to the Phase 5 detached-operation reaper.
- **`kimojio-stack/src/lib.rs` timeout and registered-resource integration**: Preserve existing `Timeout`, registered fd, and registered buffer behavior while adapting to operation-owned resource state (`kimojio-stack/src/lib.rs:2860-2918`, `kimojio-stack/src/lib.rs:2590-2629`).
- **Tests**: Add tests proving runtime fd identity remains alive while async operations are pending, dropping all user-visible fd handles does not close the fd early, and registered resource drops while pending are reclaimed after CQE.

### Success Criteria:

#### Automated Verification:
- [x] Tests pass: `cargo test -p kimojio-stack --all-targets --all-features`
- [x] Formatting passes: `cargo fmt --all --check`
- [x] Lint passes: `cargo clippy --all-targets -- -D warnings`
- [x] All-features lint passes: `cargo clippy --all-targets --all-features -- -D warnings`

#### Manual Verification:
- [x] Async I/O no longer depends on borrowed fd lifetimes.
- [x] Runtime fd cloning is a local cheap-clone operation, not per-operation fd duplication.
- [x] Kernel-visible fd and registered-resource leases remain owned until operation reaping; deterministic reclamation of dropped pending buffers remains Phase 5 work.

---

## Phase 5: Detached Operation Reaping and Scope Metadata Cleanup

### Changes Required:

- **`kimojio-stack/src/lib.rs` I/O cancellation semantics**: Update `IoResult::drop` and `Timeout::drop` so dropping requests cancellation by default and detaches user interest without prematurely dropping operation resources (`kimojio-stack/src/lib.rs:2711-2735`, `kimojio-stack/src/lib.rs:2911-2918`, `kimojio-stack/src/lib.rs:3071-3092`).
- **`kimojio-stack/src/lib.rs` explicit detach semantics**: Add explicit detach behavior for callers that intentionally want a pending operation to complete silently without default cancellation, while preserving operation-owned resource lifetime until CQE reaping.
- **`kimojio-stack/src/lib.rs` detached operation reaper**: Add scheduler-owned detached operation tracking and reap detached operations from completion processing and runtime shutdown (`kimojio-stack/src/lib.rs:3001-3096`, `kimojio-stack/src/lib.rs:4051-4058`).
- **`kimojio-stack/src/lib.rs` detached/cancel observability**: Add counters or equivalent runtime diagnostics for detached operations, cancel requests, unsupported/failed cancels, completed cancels, and original completions after cancel. Use existing runtime configuration/diagnostics surfaces where they fit.
- **`kimojio-stack/src/lib.rs` shutdown/drain behavior**: Ensure `Runtime::block_on` only returns after submitted or detached operations are completed/reaped (`kimojio-stack/src/lib.rs:584-610`, `kimojio-stack/src/lib.rs:4051-4058`).
- **`kimojio-stack/src/lib.rs` scope metadata cleanup**: Replace append-only scope child metadata with bounded terminal-state tracking while preserving unobserved panic propagation (`kimojio-stack/src/lib.rs:3108-3155`, `kimojio-stack/src/lib.rs:4163-4165`).
- **Tests**: Add tests for dropped pending I/O resource reaping, explicit detach completion without cancellation, cancellation/normal-completion races, unsupported or failed cancel reclamation on original CQE, runtime shutdown draining detached operations, detached/cancel counters, long-lived scope child metadata bounds, and unobserved panic propagation after cleanup.

### Success Criteria:

#### Automated Verification:
- [x] Targeted shutdown/reaping tests verify dropped pending I/O drains through `block_on` and runtime-owned resource counters return to zero.
- [x] Targeted detach tests verify explicit detach completes silently without default cancellation and still reclaims resources on CQE.
- [x] Targeted unsupported/failed-cancel tests verify resources remain tracked and are reclaimed when the original CQE arrives. Coverage records unsupported cancels on kernels without the opcode and failed cancel completions on kernels that report them.
- [x] Counter/diagnostic tests verify detach and cancel outcome counters update.
- [x] Tests pass: `cargo test -p kimojio-stack --all-targets --all-features`
- [x] Formatting passes: `cargo fmt --all --check`
- [x] Lint passes: `cargo clippy --all-targets -- -D warnings`
- [x] All-features lint passes: `cargo clippy --all-targets --all-features -- -D warnings`

#### Manual Verification:
- [x] Dropped pending I/O resources are reclaimed after completion reaping.
- [x] Explicit detach is available for intentional complete-silently behavior.
- [x] `block_on` does not return while detached operations still own kernel-visible resources.
- [x] Scope and scheduler metadata stay bounded during long-lived spawn/complete churn.
- [x] Unobserved child panic propagation remains correct.

---

## Phase 6: Tombstone Measurement and Documentation

### Changes Required:

- **`kimojio-stack/benches/runtime_baseline.rs`**: Add waiter churn/tombstone benchmarks or measurement groups covering timeout rate, cancellation rate, queue length, wake-one, and wake-all patterns alongside existing runtime benchmark groups (`kimojio-stack/benches/runtime_baseline.rs:8-510`). Use these results to tune the provisional compaction thresholds introduced in Phase 2.
- **`kimojio-stack/src/lib.rs` tests**: Add focused measurement-style tests where appropriate to assert waiter queue behavior remains bounded for known patterns.
- **`.paw/work/stack-cancellation-runtime/Docs.md`**: Create as-built technical documentation covering task identity, wait registration lifecycle, external wake lifecycle, async fd and operation ownership, shutdown/reaping behavior, and verification results.
- **Project docs**: Update crate-level docs or `kimojio-stack/CANCEL-DESIGN.md` only if implementation materially changes public API or design decisions beyond the existing design document.

### Success Criteria:

#### Automated Verification:
- [x] Tests pass: `cargo test -p kimojio-stack --all-targets --all-features`
- [x] Formatting passes: `cargo fmt --all --check`
- [x] Lint passes: `cargo clippy --all-targets -- -D warnings`
- [x] All-features lint passes: `cargo clippy --all-targets --all-features -- -D warnings`
- [x] Benchmark compiles/runs: `cargo bench -p kimojio-stack --bench runtime_baseline -- waiters`

#### Manual Verification:
- [x] Tombstone benchmark output is sufficient to choose or revisit compaction thresholds.
- [x] `Docs.md` accurately reflects the implemented behavior.
- [x] Crate-level API documentation for `IoFd`, `read_async`, and `write_async` reflects Phase 4 public API changes.
- [x] Public-facing documentation updates, if any, are consistent with existing markdown style.

---

## References

- Issue: none
- Spec: `.paw/work/stack-cancellation-runtime/Spec.md`
- Code Research: `.paw/work/stack-cancellation-runtime/CodeResearch.md`
- Design: `kimojio-stack/CANCEL-DESIGN.md`
- Workflow Context: `.paw/work/stack-cancellation-runtime/WorkflowContext.md`
