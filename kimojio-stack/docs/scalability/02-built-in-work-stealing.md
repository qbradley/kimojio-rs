# Built-In Stackful Work Stealing

## Thesis

Built-in work stealing can fit `kimojio-stack` only if stealing is an explicit scheduler feature, not a hidden nested executor and not transparent migration of arbitrary stackful state. The safest direction is a per-core runtime group where each core keeps its own flat scheduler and `io_uring`, while the group can rebalance owned, ready, migration-eligible work at clearly defined boundaries. Default stackful coroutines remain local and predictable; stealable work must opt in to stronger ownership and affinity rules so stackful safety, ring ownership, and tail-latency costs stay visible.

## What exactly can and cannot be stolen

| Unit | Can it be stolen? | Direction |
|------|-------------------|-----------|
| New work items / jobs | Yes. | This should be the first-class, safe stealing unit. A `Send` job descriptor is queued before a coroutine stack is allocated or before any ring resource is acquired. The destination worker allocates the stack and runs it locally. |
| Not-yet-started stackful tasks | Yes, if spawned through a stealable API. | Represent these as `StartTask` records in a group deque. They have closure/result bounds such as `F: Send + 'scope` and `T: Send + 'scope`, but no live stack yet. |
| Ready stackful continuations | Only with explicit opt-in and strict eligibility. | A continuation is eligible only while suspended, not queued locally as pinned, not inside a non-migratable scope, not holding a runtime-local waiter, and with no in-flight `io_uring` operation or fixed-resource lease. Even then, safe Rust cannot prove that arbitrary live stack locals are `Send`; safe APIs should not promise general continuation stealing. |
| Running stacks | No. | A task with its instruction pointer on a worker thread cannot be stolen. The owner must reach a yield/park boundary first. |
| Blocked tasks | Usually no. | A blocked task is owned by the primitive or I/O operation that will wake it. The wake path may requeue it onto a stealable ready queue after it becomes ready, but thieves should not pull it out of a wait queue. |
| Accepted connections | Yes, before ring-local state exists. | An acceptor may hand an `OwnedFd` to another worker as a job. Once the fd is registered in a ring, has pending I/O, or is wrapped in ring-affine state, it is pinned unless explicitly unregistered or drained. |
| File descriptors | Sometimes. | Plain process-wide owned fds can move if no pending operation references them. Fixed-file registrations are per-ring resources and should not move implicitly. |
| I/O buffers and `IoResult`s | No while pending. | Kernel-visible pointers, fixed-buffer leases, and pending result handles remain owned by the submitting worker until the CQE is reaped or cancellation completes. |
| Raw stacks | Not by copying. | A stack allocation may be moved as an owned object only while suspended and only if the task is migration-safe. The bytes must not be relocated or copied because stack frames may contain self-references and raw frame addresses. |

The key feasibility constraint is stackful migration. Unlike compiler-generated async futures, a stackful coroutine does not expose the set of locals live across a yield. A closure being `Send` does not prove that values created later and left on the stack at a migration point are `Send`. Therefore the safe, predictable v1 should steal jobs and not-yet-started tasks, while treating already-started stack migration as a later, explicitly unsafe or heavily constrained feature.

## Runtime architecture and per-core scheduler changes

Introduce a `RuntimeGroup` above the existing single-threaded `Runtime` model:

- `Runtime` remains the low-overhead single-core API with no cross-thread atomics on its hot paths.
- `RuntimeGroup` owns `N` worker threads, one worker scheduler per core, one `io_uring` per worker, and a small shared control plane.
- Each worker keeps the existing flat scheduler loop: run ready work, submit/reap ring work according to `RingEnterPolicy`, park only at the root, and never drive a nested scheduler from inside a coroutine.
- Work stealing is attempted only by the root worker loop when local ready work is exhausted or below a configured threshold. It is not performed from arbitrary channel, mutex, join, or I/O calls.

Per worker state should split local and stealable work:

- A local ready queue for pinned task IDs, preserving current cheap FIFO semantics.
- A worker-owned steal deque for migration-eligible `StartTask` records and, later, eligible continuations.
- A global injector queue for external submissions, overflow, and cross-core accepted-connection handoff.
- A stable group task ID for stealable work, separate from current per-scheduler slot IDs.
- An owner token/state machine such as `LocalReady -> Published -> InTransfer -> Owned(worker) -> Running`.

Owner operations should be cheap and predictable:

- The local worker pushes and pops its own deque without contending with other workers where possible.
- Thieves steal from the opposite end, preferably in bounded batches.
- Victim selection is randomized within a NUMA node before crossing sockets.
- Steal attempts have a configurable budget per scheduler tick.
- Remote wakeups use the existing external-wake pattern per worker: a condvar for thread sleep and an eventfd or ring poll source when the worker may be blocked in `io_uring_enter`.

The architecture should not use a global scheduler lock. A shared queue is acceptable as an explicit injector, but not as the main dispatch path for every coroutine.

## Stack ownership, memory safety, io_uring fd/resource ownership implications

Stackful safety requires a harder line than ordinary work-stealing runtimes:

- A stack may move only while its coroutine is suspended and no worker holds a borrowed scheduler frame for it.
- A stack allocation must be transferred by ownership, not copied or relocated.
- The runtime context stored on a migratable stack cannot contain `Rc<SchedulerCell>` or other thread-local scheduler references. A stealable task needs a migration-aware context that resolves the current worker through a task header or thread-local worker slot at method call time.
- Existing local coroutines can keep the current `Rc`-based representation for zero-cost single-core operation.
- Safe stealable APIs must require `Send` bounds on closures, return values, join state, panic payload paths, and any shared scope state.
- Because arbitrary live stack locals cannot be statically checked for `Send`, safe continuation migration should be disabled until there is a credible enforcement mechanism. If exposed earlier, it should be behind an `unsafe` marker trait or an experimental feature with clear invariants.

`io_uring` ownership should remain per worker:

- The worker that submits an SQE owns the operation state until the CQE is reaped.
- Pending I/O pins the task to the submitting worker unless the API explicitly detaches the operation and later reports readiness through a portable handle.
- Registered files and buffers are ring-local. Moving them requires explicit unregister/register or a separately designed portable resource pool; hidden re-registration would violate the no-hidden-cost principle.
- `IoResult`, cancellation state, detached buffers, and fixed-resource leases must carry an owner worker ID.
- Dropping or canceling an operation from another worker should enqueue a control message to the owner, not mutate the owner ring directly.
- Plain `OwnedFd` handoff is allowed before registration and before pending I/O. After handoff, the destination worker becomes the ring owner for future operations.

This implies an I/O-affinity model: CPU-only jobs can be broadly stealable, but I/O-heavy tasks tend to become sticky once they submit operations. That is acceptable if it is visible in metrics and API names.

## API/developer model

The developer model should make locality the default and stealing opt-in:

```rust
let group = RuntimeGroup::with_config(RuntimeGroupConfig {
    workers: WorkerCount::PhysicalCores,
    stealing: WorkStealing::Enabled {
        max_steal_batch: 32,
        attempts_per_idle_tick: 4,
        numa_policy: NumaPolicy::PreferLocal,
    },
    ..Default::default()
});

group.block_on(|cx| {
    cx.scope(|scope| {
        scope.spawn_local(|cx| handle_control_plane(cx));
        scope.spawn_stealable(|cx| run_cpu_or_connection_job(cx));
    })
});
```

Proposed public concepts:

- `Runtime::new()` remains single-threaded and non-stealing.
- `RuntimeGroup` or `MultiRuntime` is the explicit multi-core entry point.
- `Scope::spawn_local` is pinned to the current worker and supports the current stackful borrowing model.
- `Scope::spawn_stealable` accepts only `Send` work and initially steals before first resume.
- `cx.current_worker()`, `cx.affinity()`, and `cx.set_affinity(TaskAffinity)` expose placement.
- `TaskAffinity::Pinned`, `TaskAffinity::StealableBeforeStart`, and possibly `TaskAffinity::StealableAtExplicitYield` describe migration policy.
- `cx.migration_point()` may later publish an already-started continuation, but only for tasks that opted into the stricter safety contract.
- Listener helpers should be explicit, for example `accept_local`, `accept_balanced`, or `accept_to(worker)`, rather than silently redistributing every connection.

Convenience should not hide work movement. A task that becomes ring-affine, enters a local-only critical section, or uses a ring-local registered resource should become visibly pinned until it exits that state.

## Fairness, NUMA, cache locality, tenant isolation

The scheduler should optimize for local progress first:

- Local ready tasks run before stealing.
- Owner workers pop recently spawned work locally for cache warmth.
- Thieves take older work from the far end of a deque to reduce contention and preserve producer locality.
- Stealing from the same NUMA node is preferred; cross-node stealing happens only after local victims are empty or over configured imbalance thresholds.
- Accepted connections can be balanced by listener sharding, `SO_REUSEPORT`, or explicit fd handoff; all should be selectable because network locality and TLS state can dominate.

Fairness should be bounded and observable rather than magical:

- Per-worker ready budgets prevent one hot worker from ignoring I/O completions.
- Per-tenant or per-class lanes can limit how much work a tenant publishes and how much can be stolen.
- A global injector capacity provides backpressure for external submissions.
- Steal budgets prevent idle workers from causing cross-core cache storms.
- Metrics should expose per-worker queue depth, steal attempts, steal successes, failed steals, cross-NUMA steals, migrated bytes/stacks, and pinning reasons.

Tenant isolation should be explicit. If a service has noisy neighbors, use tenant-specific queues or worker sets rather than one fully shared global pool.

## Hidden costs and how they are surfaced/controlled

Costs introduced by built-in stealing:

- Atomic operations on steal queues.
- Cross-core cache misses when task metadata, stacks, or connection state move.
- Eventfd/condvar wakeups when a remote worker must be interrupted.
- Additional task header state for owner, affinity, and migration eligibility.
- Potential `Arc` or lock use in stealable scope/join state.
- Lost locality for TLS sessions, disk buffers, and fixed I/O resources.

Controls:

- Disabled by default for `Runtime`.
- Enabled only through `RuntimeGroupConfig`.
- Configurable steal batch size, steal attempt budget, global injector capacity, NUMA policy, and accepted-connection balancing policy.
- Separate `spawn_local` and `spawn_stealable` APIs.
- Per-task pinning reasons surfaced through diagnostics.
- Counters and histograms for steal latency, enqueue-to-start latency, remote wake count, and migration rejection reasons.
- Benchmark gates that compare single-core no-steal overhead against the current runtime.

The most important non-goal is transparent "helpful" migration from any blocking primitive. If a mutex, channel, join, or I/O wait secretly drives global stealing, the runtime would lose its flat scheduling property.

## Failure modes, cancellation, backpressure

Expected failure modes and mitigations:

- **Steal storms:** idle workers repeatedly scan empty victims. Use bounded attempts, randomized victim choice, exponential idle backoff, and worker sleep.
- **Stack ping-pong:** a task bounces between cores and loses cache locality. Use owner stickiness, minimum runtime before republish, and migration cooldown.
- **I/O pinning surprise:** a mostly CPU task submits one I/O operation and stops moving. Surface pinning in metrics and provide explicit split APIs for CPU sub-jobs.
- **Remote cancellation races:** a handle on worker A cancels a task or I/O owned by worker B. Route cancellation through the owner state machine and require acknowledgement before resource reclamation.
- **Scope exit with stolen children:** structured scope state must count children globally and wake the parent only after all local and stolen children finish or are canceled.
- **Worker panic or shutdown:** the group should fail the whole `block_on` unless a recovery mode is explicitly designed. Silent task migration after panic risks violating resource ownership.
- **Backpressure collapse:** global injectors and accept handoff queues must be bounded. When full, acceptors should stop accepting, apply `EPOLLIN`/poll backpressure, or return a visible overload error.
- **Fixed-resource exhaustion:** registered fd/buffer slots are per worker. Handoff should fail fast or fall back only if configured; hidden re-registration loops can create tail spikes.
- **Cancellation of pending I/O:** the owner ring submits cancellation and reaps both cancel and original CQEs. The task is not stealable until that lifecycle is complete.

Cancellation should preserve the current principle that dropping a handle never leaves kernel-owned pointers dangling. Cross-core cancellation makes that more important, not less.

## Prototype plan and benchmark plan

Prototype in stages:

1. **Baseline instrumentation:** add per-worker-ready and I/O lifecycle metrics without changing scheduling. Prove zero or near-zero overhead for single-core `Runtime`.
2. **RuntimeGroup skeleton:** create multiple independent workers with explicit external submission and shutdown, but no stealing. Validate structured cancellation across workers.
3. **Stealable job deque:** implement work stealing for `Send` not-yet-started jobs. Allocate coroutine stacks on the destination worker. Keep started tasks pinned.
4. **Balanced accept handoff:** add explicit accepted-fd job handoff before fd registration or I/O. Measure listener sharding versus central acceptor handoff.
5. **I/O affinity and resource accounting:** tag operations, fds, buffers, and result handles with owner worker IDs. Route remote cancel/drop through owner queues.
6. **Experimental continuation migration:** only after the above is stable, test moving suspended stacks behind an internal feature. Require no pending I/O, no local waiters, migration-aware context, and an unsafe safety contract for live stack locals.
7. **Public API hardening:** decide whether continuation migration is safe enough for public exposure. If not, ship job/pre-start stealing as the stable built-in feature.

Benchmark plan:

- Single-worker scheduler latency and allocation checks versus current `Runtime`.
- Spawn/join throughput for local versus stealable not-yet-started tasks.
- Steal latency under empty, balanced, and imbalanced queues.
- CPU throughput workloads with chunky jobs across 1, 2, 4, 8, 32, and 100+ cores.
- I/O ping-pong and disk read/write latency with stealing disabled/enabled but no steals.
- HTTP/gRPC echo and TLS handshake/load tests with local accept, balanced accept, and stolen CPU work.
- Mixed workload: low-latency I/O tasks plus chunky compression/hash/storage jobs.
- NUMA tests measuring local-first and cross-node steal policies.
- Tail latency under cancellation storms, accept overload, and worker idleness.
- Regression gates for single-digit-core deployments where stealing overhead may not pay for itself.

## Pros / Cons

Pros:

- Provides built-in scaling for bursty and imbalanced workloads without forcing every service to hand-roll worker pools.
- Preserves the existing single-core runtime path when stealing is not enabled.
- Makes connection and job handoff a runtime-supported pattern with consistent diagnostics.
- Lets services use stackful call chains while still distributing coarse CPU or connection work across many cores.
- Keeps `io_uring` ownership local and predictable.

Cons:

- Safe migration of already-started stackful continuations is not generally provable with today's stackful model.
- Multi-core stealable tasks require more `Send`, `Arc`, atomic, and owner-state machinery than current local tasks.
- I/O-heavy tasks become sticky, so stealing may help less than expected unless CPU work is split into separate jobs.
- Debugging placement, fairness, and cancellation becomes more complex.
- NUMA and tenant isolation need configuration; a single global pool can hurt tail latency.
- There is a real risk of hidden overhead creeping into the single-core path unless `Runtime` and `RuntimeGroup` remain strongly separated.

## Open questions

- Should stable v1 explicitly reject already-started stack migration and market the feature as built-in job/pre-start-task stealing?
- Can `corosensei` coroutine stacks be soundly transferred between OS threads, and what trait bounds does that require?
- Is there any practical way to enforce that live stack locals at a migration point are `Send`, or must continuation migration remain unsafe?
- Should stealable scoped tasks support `Send + 'scope` borrowing like `std::thread::scope`, or should the first API require `'static` work?
- How should `RuntimeContext` be redesigned so migratable stacks do not hold worker-local `Rc` scheduler pointers?
- Should fixed-file and fixed-buffer resources stay strictly per worker, or is an explicit portable registration pool worth the complexity?
- Which remote wake mechanism is preferred: eventfd plus poll, `MSG_RING`, io_uring futex operations where available, or a configurable mix?
- How much fairness should be built into the core scheduler versus delegated to tenant-specific lanes?
- What are the acceptable single-core overhead budgets for adding owner IDs, metrics, and external wake hooks?
- Should worker failure abort the whole runtime group, or should there be a structured recovery/cancellation story?
