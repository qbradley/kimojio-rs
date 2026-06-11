# Feature Specification: Stack Cancellation Runtime

**Branch**: jj:@ (nkutkylq/dc4d73d4)  |  **Created**: 2026-06-11  |  **Status**: Draft
**Input Brief**: Implement the cancellation, lifetime, and reclamation design for `kimojio-stack`.

## Overview

`kimojio-stack` should provide a stackful runtime where cancellation, wait registration, asynchronous I/O lifetime, and scope cleanup behave predictably under normal server workloads and under failure paths. A server author should be able to spawn scoped stackful tasks, wait on runtime primitives, issue asynchronous I/O, and rely on the runtime to prevent stale wakes, fd lifetime hazards, and unbounded metadata growth.

The current design goal is to make cancellation explicit and deterministic without turning the local hot path into a high-overhead runtime. Dropping a pending I/O result should request cancellation and leave enough runtime-owned state for the kernel completion path to reclaim resources. Timed-out, canceled, or unwound waits should not remain capable of consuming future wakes. Completed tasks should release scheduler and scope metadata so long-lived accept loops scale with live work rather than historical work.

The implementation should preserve the thread-per-core runtime model and keep efficient local operations as the default. Where cross-thread coordination exists, the runtime should use explicit registration state so cancellation and external wake races remain safe.

## Objectives

- Ensure safe async I/O lifetime ownership so pending operations cannot use a closed or reused fd identity.
- Make dropped pending I/O cancel by default while still allowing explicit detach semantics for callers that intentionally ignore completion.
- Make wait registration cancellation-safe for local waits, multi-waits, timeouts, and unwind paths.
- Make cross-thread wait and wake handling robust against cancellation races and stale task references.
- Make scope quiescence decisions account for staged external wakes and other scheduler-visible work before canceling children.
- Reclaim completed task and scope metadata so long-lived scopes remain bounded by live or recently-live work.
- Add tests and benchmarks that prove correctness and inform wait-queue tombstone compaction thresholds.

## User Scenarios & Testing

### User Story P1 - Cancel Pending I/O Safely

Narrative: As a low-latency server author, I can drop or abandon a pending I/O result during disconnects, timeouts, or scope cancellation and rely on the runtime to cancel or reap the operation without leaking kernel-visible resources.

Independent Test: Start an operation, drop the user-visible result before completion, drive the runtime to quiescence, and verify the operation is canceled or completed and resources are reclaimed.

Acceptance Scenarios:
1. Given a pending async I/O operation, When its result handle is dropped, Then the runtime requests cancellation by default and keeps operation resources alive until completion is reaped.
2. Given a pending async I/O operation whose cancellation races with normal completion, When the runtime receives completion, Then it reclaims operation resources exactly once.
3. Given a caller intentionally wants to ignore a still-running operation, When it uses explicit detach behavior, Then the runtime may complete silently while still preserving resource lifetime safety.

### User Story P2 - Prevent Async fd Reuse Hazards

Narrative: As a runtime user, I can submit async I/O only through a stable runtime-owned fd identity so a pending operation cannot accidentally target an unrelated fd after the original object is closed or reused.

Independent Test: Submit async I/O through the runtime-owned fd identity, drop all user-visible fd handles while the operation is pending, and verify the operation still owns a valid fd identity until it is reaped.

Acceptance Scenarios:
1. Given an async I/O operation, When all user-visible fd handles are dropped before completion, Then the runtime keeps the operation's fd identity alive until the completion path releases it.
2. Given a caller has a borrowed fd-like object, When they want async I/O, Then they must first transfer ownership into a stable runtime fd identity instead of relying on a borrowed async API.
3. Given repeated async read/write submissions, When the runtime clones the fd identity for each operation, Then the clone path does not require a per-operation kernel fd duplication call.

### User Story P3 - Avoid Stale Waits and Wakes

Narrative: As a runtime user, I can combine waits, timeouts, joins, and cancellation without stale registrations consuming later wakes or rescheduling completed tasks.

Independent Test: Register multiple waits, time out or cancel one path, later wake the primitive, and verify only live waiters are scheduled.

Acceptance Scenarios:
1. Given a task waiting on multiple waitables, When one wait path wins or times out, Then all non-winning registrations are invalidated before later wakes occur.
2. Given a canceled or unwound task with active waits, When another task wakes the same primitive later, Then the canceled task is not scheduled.
3. Given a completed task slot is reused, When an old waiter or wake references the previous occupant, Then the runtime discards the stale wake.

### User Story P4 - Handle Cross-Thread Wake Races

Narrative: As a server author using cross-thread channels or external wake sources, I can cancel scopes or receive messages during races without corrupting waiter counts, losing valid wakes, or waking stale tasks.

Independent Test: Race cross-thread send/receive wakeups against scope cancellation and verify waiter accounting, staged wake handling, and task scheduling remain consistent.

Acceptance Scenarios:
1. Given a stackful task parked in a cross-thread wait queue, When its scope is canceled, Then the external wait registration is canceled or invalidated before the task is dropped.
2. Given a cross-thread wake races with cancellation, When the scheduler drains external ready work, Then only live task identities are scheduled and waiter accounting remains correct.
3. Given external ready work is staged for a scope, When the scope checks quiescence, Then the scheduler accounts for that staged work before deciding to cancel children.

### User Story P5 - Keep Long-Lived Scopes Bounded

Narrative: As a server author running a long-lived accept loop, I can spawn and complete many short-lived children without scheduler slots or scope metadata growing with total historical task count.

Independent Test: Run many spawn/join or spawn/complete cycles inside a long-lived scope and verify scheduler and scope metadata remain bounded after warmup.

Acceptance Scenarios:
1. Given many children complete normally, When their results are joined or otherwise no longer needed, Then scheduler slots become reusable.
2. Given child tasks panic and the parent does not observe every handle, When the scope exits or propagates the panic, Then panic propagation remains correct without retaining per-child metadata indefinitely.
3. Given nested request scopes complete under a long-lived parent, When each nested scope finishes, Then its child metadata is reclaimed independently.

### Edge Cases

- Cancellation and normal completion race for the same I/O operation; resources must be reclaimed once.
- A wait times out immediately before the waitable is woken; stale registration must not consume the later wake.
- A task is canceled while parked on a cross-thread wait queue; external registration cleanup must be race-safe.
- A staged external wake exists while a scope is deciding whether it has runnable work; the scope must not cancel live work prematurely.
- Task identity is reused after completion; old waiters and external wakes must not schedule the new occupant.
- Cancellation support is unavailable or fails for a submitted operation; resource reclamation must still occur when the original completion arrives.
- Wait-queue tombstones accumulate under high timeout/cancellation churn; benchmarks must identify safe compaction thresholds.

## Requirements

### Functional Requirements

- FR-001: The runtime must use generation-checked task identities so stale waiters and wakes cannot schedule completed, canceled, or reused task slots. (Stories: P3, P4, P5)
- FR-002: The runtime must make local wait registration cancellation-safe for joins, timeouts, I/O results, and multi-wait operations. (Stories: P3)
- FR-003: The runtime must make cross-thread wait registration cancellation-safe and keep external waiter accounting consistent across wake/cancel races. (Stories: P4)
- FR-004: Scope quiescence must account for staged external wakes and scheduler-visible work before canceling children. (Stories: P4)
- FR-005: Dropping a pending I/O result must request cancellation by default and keep operation resources alive until completion is reaped. (Stories: P1)
- FR-006: Async I/O must require a stable runtime-owned fd identity with cheap clone semantics and must not rely on borrowed fd lifetimes. (Stories: P2)
- FR-007: The runtime must reclaim completed task slots and scope child metadata after terminal task states while preserving unobserved panic propagation. (Stories: P5)
- FR-008: Runtime shutdown must wait for submitted or detached operations to complete or be reaped before returning from the outermost runtime call. (Stories: P1)
- FR-009: The implementation must include tests for cancellation races, stale waits, fd lifetime safety, resource reclamation, and bounded metadata growth. (Stories: P1, P2, P3, P4, P5)
- FR-010: The implementation must include benchmarks or measurement tests to evaluate wait-queue tombstone accumulation and compaction thresholds. (Stories: P3)
- FR-011: The runtime must provide explicit detach behavior for callers that intentionally want a pending operation to complete silently without default cancellation. (Stories: P1)
- FR-012: The runtime must expose detached-operation and cancellation observability so intentional detach and cancellation-storm behavior are visible. (Stories: P1)

### Key Entities

- Runtime task identity: A stable identity for a task instance that can distinguish current task occupants from stale references.
- Wait registration: A runtime-owned registration that can be invalidated or canceled when a wait path times out, wins, unwinds, or is canceled.
- External wait registration: A cross-thread-safe wait registration used by external wait queues and wake sources.
- Operation state: Runtime-owned state for a submitted I/O or timeout operation that owns resources until completion is reaped.
- Runtime fd identity: A cheap-clone, runtime-owned fd handle used by async I/O to preserve fd lifetime.
- Detach observability: Runtime-visible counters or equivalent metrics for detached operations and cancellation outcomes.
- Scope child metadata: Per-child state retained by a scope while a child is live or has unobserved terminal effects.

### Cross-Cutting / Non-Functional

- The local runtime hot path should avoid unnecessary atomics and per-operation fd duplication calls.
- Memory retained by tasks, waiters, scopes, and detached operations should be bounded by live or recently-live work after completions are reaped.
- Public APIs should make safe behavior the default; riskier behavior such as detaching a running operation should require explicit caller action.
- Existing structured-concurrency panic propagation semantics must be preserved.

## Success Criteria

- SC-001: Tests prove stale task references cannot schedule a completed or reused task identity. (FR-001)
- SC-002: Tests prove timed-out or canceled wait registrations do not consume future wakes. (FR-002)
- SC-003: Tests prove cross-thread wait cancellation and wake races do not corrupt waiter accounting or schedule stale tasks. (FR-003, FR-004)
- SC-004: Tests prove dropped pending I/O results request cancellation by default and reclaim resources after completion is reaped. (FR-005, FR-008)
- SC-005: Tests prove async I/O fd identity remains alive until pending operations are reaped, even if user-visible handles are dropped. (FR-006)
- SC-006: Tests prove long-lived spawn/complete churn keeps scheduler slots and scope child metadata bounded after warmup. (FR-007)
- SC-007: Existing panic propagation tests continue to pass, including unobserved child panic propagation. (FR-007)
- SC-008: Benchmarks or measurement tests report tombstone accumulation behavior across timeout/cancellation churn and provide data for compaction thresholds. (FR-010)
- SC-009: Repository validation for `kimojio-stack` passes after each implementation phase. (FR-009)
- SC-010: Tests prove explicit detach allows a pending operation to complete silently without default cancellation while preserving resource ownership until reaping. (FR-011)
- SC-011: Tests or diagnostics prove detached-operation and cancellation counters update for detach, cancel request, cancel completion, unsupported cancel, and original-completion-after-cancel outcomes. (FR-012)

## Assumptions

- The source design in `kimojio-stack/CANCEL-DESIGN.md` is the approved implementation direction.
- The crate is still pre-stable, so breaking API changes for async fd ownership are acceptable when they improve safety and performance.
- A cheap-clone local fd handle can start with single-thread reference counting and later move behind the same public type to a scheduler-owned fd table if benchmarks justify it.
- Cancellation completion is eventual: resources are reclaimed when the relevant completion is reaped, not necessarily when the user handle is dropped.
- Wait-queue tombstone compaction policy should be benchmark-driven rather than fixed in the specification.

## Scope

In Scope:
- Task identity generation and scheduler slot reuse.
- Cancellation-safe local wait registration.
- Cancellation-safe cross-thread wait registration and external wake draining.
- Async I/O operation ownership, runtime fd identity, cancel-by-default result dropping, and detached operation reaping.
- Scope child metadata cleanup while preserving structured panic propagation.
- Tests and benchmarks needed to validate correctness and tombstone behavior.

Out of Scope:
- Replacing the stackful coroutine backend.
- Changing the overall thread-per-core runtime model.
- Implementing a multi-threaded global scheduler.
- Stabilizing public extension points for third-party waitable implementations.
- Choosing a final tombstone compaction threshold without benchmark evidence.
- Optimizing unrelated crates or unrelated runtime subsystems.

## Dependencies

- Existing `kimojio-stack` runtime, scheduler, wait, channel, timeout, and I/O code.
- Existing cancellation design: `kimojio-stack/CANCEL-DESIGN.md`.
- Existing repository validation commands for formatting, tests, and clippy.
- Local jj workflow for logical implementation changes.

## Risks & Mitigations

- Risk: Generation-checked task identity changes may touch many runtime paths and introduce subtle scheduling regressions. Mitigation: implement as a separate phase with stale-wake and slot-reuse tests before changing higher-level wait semantics.
- Risk: Cancellation-safe wait registration may add overhead to hot paths. Mitigation: keep local registration non-atomic and use benchmarks to evaluate tombstone and compaction behavior.
- Risk: Cross-thread registration cleanup may race with external wake delivery. Mitigation: model explicit registration states and cover wake/cancel races with tests.
- Risk: Cancel-by-default I/O dropping may increase cancellation traffic under workloads that intentionally ignore completions. Mitigation: provide explicit detach behavior and counters so intentional detach is visible.
- Risk: Operation ownership changes may interact with registered resources and buffer ownership. Mitigation: test registered and unregistered fd/buffer drops while operations are pending.
- Risk: Scope metadata cleanup may break unobserved panic propagation. Mitigation: keep panic propagation tests in every phase that changes scope or task terminal state.

## References

- Source design: `kimojio-stack/CANCEL-DESIGN.md`
- Workflow context: `.paw/work/stack-cancellation-runtime/WorkflowContext.md`
