# Multithreaded kimojio-stack design

## Question

Can `kimojio-stack` support both the current single-threaded stackful runtime and
a multithreaded work-stealing runtime in one coherent design? Is that feasible
in Rust? Should it instead be a separate runtime such as
`kimojio-stack-multithread`? How should io_uring work in such a design, and how
can the runtime avoid the tail-latency and thread-scaling pathologies commonly
seen in large async work-stealing executors?

## Short answer

Yes, a unicorn is possible, but only if it is not designed as "Tokio, but
stackful." The feasible design is a **hybrid runtime with explicit task
classes**:

1. **Pinned stackful tasks** run on one chosen worker and never migrate while
   suspended.
2. **Stealable stackful tasks** may be stolen only at safe yield/park
   boundaries, never while running.
3. **I/O ownership is sharded**, usually one io_uring per worker, with
   completion routed back to the worker that owns the operation.
4. **Cross-thread behavior is opt-in and visible**, not an ambient default.
5. **The single-thread runtime remains the primary primitive**, and the
   multithreaded runtime is built as a layer over multiple local runtimes rather
   than replacing their cost model.

The more conservative conclusion is that this should probably start as a
separate crate or feature-gated runtime, for example
`kimojio-stack-multithread`, sharing primitives where possible but not forcing
the current `kimojio-stack` API to become `Send`-infected.

## Feasibility in Rust

A multithreaded stackful runtime is feasible in Rust, but it changes the safety
and API constraints dramatically.

The current `kimojio-stack` design leans on single-threaded assumptions:

- `Rc`, `Weak`, `Cell`, and `RefCell` are used throughout the scheduler,
  waiters, joins, channels, locks, and I/O state.
- `Scope::spawn` allows non-`Send` closures and return values bounded by a
  scope lifetime.
- Synchronization primitives can be cooperative and local rather than atomic.
- A `Waiter` can requeue a task by touching the local scheduler directly.
- A coroutine stack can be resumed only by the thread currently driving that
  scheduler.

Those are not incidental implementation details. They are the reason the current
runtime is simple and fast. A work-stealing design cannot preserve them for all
tasks.

Rust can support the multithreaded version if the API distinguishes:

```rust
scope.spawn_local(|cx| { /* !Send, pinned to this worker */ });
scope.spawn_stealable(|cx| { /* Send, movable between workers at yield points */ });
runtime.spawn_pinned(worker_id, |cx| { /* Send or !Send depending on scope */ });
```

The key rule is: **migration requires `Send`; borrowing non-`'static` stack data
requires lexical scope; combining both requires careful scoped lifetimes plus
worker-bound execution.** The current scoped stack borrowing remains easy for
pinned local tasks. It becomes much harder for stealable tasks and should be
restricted to `Send` borrows and return values, if supported at all.

## Why "just add work stealing" is the wrong design

The current runtime is valuable because its costs are visible:

- one scheduler,
- one ready queue,
- one io_uring,
- guarded stacks,
- readiness waiters,
- no cross-thread synchronization in the hot path.

Naively adding work stealing risks losing exactly that value. If every wakeup
may cross threads, every queue needs atomics, every waiter needs thread-safe
state, every task must become `Send`, and every operation may contend on a
global injector queue or shared driver, then `kimojio-stack` becomes another
general-purpose executor with larger task stacks.

That is the worst of both worlds: the memory cost of stacks plus the tail
latency of global scheduling.

The design goal should instead be:

> Preserve thread-local execution as the default. Use work stealing only to
> rebalance explicit stealable tasks at safe points, and never put I/O
> completions, pinned tasks, or local wait queues on a global hot path.

## Recommended architecture

### Runtime shape

Use a top-level multithreaded runtime that owns N workers:

```text
MultiRuntime
  Worker 0: LocalScheduler + io_uring + local ready queue + local timers
  Worker 1: LocalScheduler + io_uring + local ready queue + local timers
  Worker 2: LocalScheduler + io_uring + local ready queue + local timers
  ...
  Global: steal registry, optional injector, shutdown coordination, metrics
```

Each worker is essentially a `kimojio-stack` local runtime with:

- a local coroutine table,
- a local ready queue,
- a local io_uring,
- a local I/O state pool,
- local cooperative sync waiters,
- local completion scratch storage,
- a way to receive cross-thread wake messages.

The global runtime should not be the scheduler. It should be a coordinator.

### Task classes

#### Pinned tasks

Pinned tasks are the default and are closest to today's model.

Properties:

- Run on exactly one worker.
- May use non-`Send` data if spawned in a local scope.
- May borrow from parent stack through scoped spawning.
- Use local waiters and local synchronization primitives.
- Submit I/O to the worker's ring.
- Are woken by requeueing on the same worker.

Pinned tasks are the latency-sensitive path. They should be the primary answer
for servers that can partition connections or shards by core.

#### Stealable tasks

Stealable tasks are an opt-in class.

Properties:

- Closure, stack contents, return value, and reachable state must be `Send`.
- The task may move only when suspended at a runtime boundary.
- It is never migrated while running.
- It should not hold worker-local I/O resources across migration.
- It should not wait on local-only primitives unless those primitives are
  explicitly marked cross-worker.

This means the scheduler can represent a stealable suspended task as an owned
coroutine object plus metadata. Another worker may steal it from a victim's
stealable queue and resume it later.

The difficult part is proving that a suspended stackful coroutine is safe to move
to another OS thread. That depends on the coroutine implementation, stack
allocation, TLS assumptions, and what the task may have stored on its stack.
Even if the stack memory itself is movable by pointer ownership, the values on
the stack must be `Send`, and the runtime must ensure worker-local handles are
not live across the migration point.

Because Rust cannot automatically prove "everything currently live on this
stack is `Send`," the API must make migration a property of the whole task:
`spawn_stealable` requires `F: Send` and should expose only APIs whose handles
are safe to suspend-and-migrate.

#### Affine task groups

A useful middle ground is an affine group:

- A task group is assigned to a preferred worker.
- The group can be relocated only as a group and only when all tasks are parked
  at safe points.
- Internal waiters and non-`Send` state remain local within the group.

This is more complex than pinned tasks but safer than arbitrary work stealing.
It may be useful for moving entire connection groups, actor shards, or service
partitions during load imbalance.

### Scheduling design

Each worker should have multiple queues:

```text
local_ready       fast FIFO/LIFO-local queue for pinned and current-worker tasks
stealable_ready   deque for Send tasks that may be stolen
remote_inbox      MPSC queue or eventfd-fed queue for cross-thread wakeups
io_completions    drained from the worker's io_uring
```

Worker loop:

1. Drain a bounded number of local ready tasks.
2. Reap local io_uring completions without blocking.
3. Drain a bounded number of remote wake messages.
4. If local work remains, continue locally.
5. If idle, try to steal from another worker's stealable queue.
6. If still idle and local I/O is in flight, block in this worker's io_uring.
7. If globally idle, park on an eventfd/condition mechanism.

Fairness should be budgeted, not accidental. For example:

- Run at most `ready_budget` local tasks before checking I/O.
- Run at most `lifo_budget` immediate resumes before FIFO rotation.
- Drain at most `remote_budget` remote wakeups per tick.
- Steal at most half a victim's stealable queue.
- Do not steal from workers with low queue depth.

The scheduler should prefer cache locality and low latency over maximum
theoretical load balance.

## io_uring integration

There are three plausible io_uring designs.

### Option A: one io_uring per worker

This is the recommended default.

Each worker owns its own ring. Tasks running on that worker submit to that
worker's ring. CQEs are reaped by the same worker, and completions wake local
tasks directly.

Advantages:

- No shared ring lock.
- Completion handling stays local.
- Registered files and buffers can be worker-owned.
- Tail latency is easier to reason about.
- Blocking in `io_uring_enter` affects only an idle worker.

Disadvantages:

- File and buffer registration is per ring, so fixed resources may need to be
  registered on multiple workers or pinned to one worker.
- If a task migrates after submitting I/O, completion must route back to the
  original worker or the I/O state must be transferable.
- Load distribution for I/O-heavy tasks must be explicit.

Design rule: **an I/O operation is owned by the ring that submitted it until its
CQE is processed.**

For pinned tasks, this is simple. For stealable tasks, the runtime should either:

1. forbid migration while the task has in-flight I/O owned by a worker ring, or
2. allow migration but route the completion to the task's current worker through
   a remote wake message.

The first rule is simpler and likely better for tail latency.

### Option B: one shared io_uring

This is not recommended as the primary design.

Advantages:

- Registration tables are shared.
- Any worker can submit any operation to the same ring.
- Completions are centralized.

Disadvantages:

- Shared submission and completion paths require synchronization.
- CQE handling becomes a global bottleneck.
- Wakeups frequently cross threads.
- A busy I/O driver can create head-of-line blocking.
- The design starts to resemble centralized async executors with known tail
  latency risks.

This option may be useful for a specialized driver thread model, but not for a
low-tail-latency general runtime.

### Option C: dedicated I/O workers plus compute workers

In this design, some threads own io_uring instances and other threads run
coroutines. Compute workers submit I/O requests to I/O workers, then park until
completion is routed back.

Advantages:

- Separates I/O polling from CPU work.
- Can isolate slow kernel interactions from application work.
- Useful if application workers should never enter the kernel.

Disadvantages:

- Every I/O operation becomes cross-thread messaging.
- Completion routing adds latency.
- Backpressure is harder.
- It weakens the blocking-style `cx.read` value proposition.

This is attractive for some workloads, but it should not be the default
`kimojio-stack` multithread story.

## Registered resources in a multiring design

Fixed files and fixed buffers complicate multithreading.

With one ring per worker, a `RegisteredFd` or `RegisteredBuffer` is naturally
ring-affine. That should be explicit in the type system:

```rust
RegisteredFd<'worker>
RegisteredBuffer<'worker, B>
```

or represented dynamically:

```rust
struct RegisteredFd {
    owner: WorkerId,
    slot: u32,
}
```

The runtime should reject use of a registered resource from the wrong worker
unless it provides an explicit migration or replication operation.

Possible policies:

1. **Worker-affine registration:** fastest and simplest. Registered resources
   belong to one worker.
2. **Replicated registration:** register the same fd or memory on every worker.
   Higher setup cost, easier migration.
3. **Lazy per-worker registration:** first use on a worker registers there.
   Good ergonomics, but creates unpredictable latency on first use.

For low tail latency, worker-affine or explicit replicated registration is
better than lazy registration.

## Avoiding Tokio-like tail latency and scaling issues

The goal is not to criticize Tokio specifically; Tokio optimizes for a broad
async ecosystem and general-purpose scheduling. `kimojio-stack` should optimize
for a narrower target. The relevant pathologies to avoid are:

- global queues becoming hot,
- cross-thread wake storms,
- work stealing stealing too aggressively,
- I/O driver contention,
- tasks migrating away from their data,
- cooperative tasks running too long,
- LIFO-local scheduling starving older tasks,
- timer and I/O wakeups being delayed behind large ready batches,
- blocking fallbacks stalling executor workers,
- unbounded spawn or wake amplification.

### Design principles for low tail latency

#### 1. Local first, global rarely

Most wakeups should enqueue locally. Most tasks should stay on their worker.
The global injector should be for external submissions and emergency load
balancing, not the default scheduling path.

#### 2. Work stealing only when idle

Workers should steal only when they are actually idle and have no local I/O
completion pressure. Do not steal merely because another worker has "more" work.
The cure for imbalance can be worse than imbalance if it destroys cache locality
and creates cross-thread wakeups.

#### 3. Steal batches, not single hot tasks

If stealing is needed, steal a bounded batch from the cold end of a victim's
stealable deque. Do not steal tasks that just woke locally and likely have hot
cache state.

#### 4. Preserve I/O affinity

Connections, registered buffers, and ring-owned operations should stay on one
worker. If a server accepts connections on multiple workers, distribute accepted
connections explicitly, then keep each connection's coroutine pinned.

#### 5. Budget scheduler phases

Every loop iteration should have budgets for ready tasks, I/O reaping, remote
inbox draining, and stealing. Tail latency often comes from one phase starving
another.

#### 6. Add starvation instrumentation

The runtime should measure:

- time from wake to resume,
- ready queue depth,
- remote inbox depth,
- steal attempts and successes,
- time spent in each scheduler phase,
- I/O submit-to-CQE latency,
- CQE-to-task-resume latency,
- longest run without yield,
- per-worker utilization.

Without these metrics, the runtime cannot prove that its design avoids tail
latency.

#### 7. Make blocking visible

Provide debug-mode detection for long-running tasks that do not yield. Document
that blocking syscalls inside a worker are forbidden unless routed through an
explicit blocking pool. If fallbacks to synchronous syscalls exist, classify
them as setup-only or move them to a blocking path.

#### 8. Keep cross-worker sync separate

Local `Mutex`, `Semaphore`, and channels should remain local and cheap.
Cross-worker variants should be separate types with names that make the cost
obvious:

```rust
LocalMutex<T>
LocalChannel<T>
CrossWorkerChannel<T>
CrossWorkerNotify
```

Do not silently upgrade local primitives to atomics and locks.

## API sketch

### Runtime construction

```rust
let runtime = MultiRuntime::builder()
    .workers(num_cpus::get())
    .stack_size(64 * 1024)
    .io_uring_per_worker(true)
    .stealing(StealingPolicy::IdleOnly)
    .build();

runtime.block_on_pinned(|cx| {
    // Runs on one worker. Similar to today's Runtime::block_on.
});
```

### Spawning

```rust
cx.scope(|scope| {
    scope.spawn_local(|cx| {
        // !Send allowed. Worker-pinned.
    });

    scope.spawn_pinned(worker_id, |cx| {
        // Explicit worker affinity.
    });

    scope.spawn_stealable(|cx| {
        // Requires Send task state and migration-safe APIs.
    });
});
```

### I/O

```rust
// Worker-affine, low-latency path.
let n = cx.read(&fd, &mut buf)?;

// Explicit ownership and completion path.
let read = cx.read_async(&fd, vec![0; 4096]);
cx.wait_all(&[&read], None)?;
let out = read.get(cx)?;
```

### Cross-worker submission

```rust
runtime.spawn_on(worker_id, |cx| { ... });
runtime.spawn_stealable(|cx| { ... });
```

External submissions should enter a worker inbox directly when possible, not a
single global queue.

## Should this be one runtime or a separate crate?

### Option 1: one crate, two runtime types

```rust
kimojio_stack::Runtime
kimojio_stack::MultiRuntime
```

Advantages:

- Shared documentation and primitives.
- Easier migration path.
- One crate identity.

Disadvantages:

- The single-thread API may become polluted by `Send` bounds and multithreaded
  concerns.
- Users may assume local primitives are safe cross-worker.
- Implementation complexity lands in the core crate.

This is acceptable only if the local runtime remains untouched and the
multithreaded runtime is clearly additive.

### Option 2: separate crate

```text
kimojio-stack
kimojio-stack-multithread
```

Advantages:

- Preserves the current runtime's clarity and single-threaded optimization.
- Allows different trait bounds and primitive types.
- Reduces accidental API coupling.
- Makes the experimental nature of work stealing explicit.

Disadvantages:

- More maintenance overhead.
- Some code duplication or internal shared crate pressure.
- Users must choose a runtime up front.

This is the recommended path for initial development.

### Option 3: shared internal core plus two public crates

```text
kimojio-stack-core       internal scheduler/coroutine/io_uring pieces
kimojio-stack            single-thread public API
kimojio-stack-multithread multithread public API
```

This may be the eventual architecture, but it is probably premature. The
multithreaded design should first prove its API and latency behavior before
forcing a core split.

## Proposed development path

1. **Keep `kimojio-stack` single-threaded and stable.**
   Do not add `Send` bounds or atomics to the current hot path.

2. **Prototype `kimojio-stack-multithread`.**
   Start with one worker-local runtime per thread and pinned tasks only. Add
   explicit `spawn_on(worker_id, ...)`.

3. **Add one io_uring per worker.**
   Keep I/O ownership worker-local. Measure I/O submit-to-completion and
   completion-to-resume latency.

4. **Add cross-worker wake inboxes.**
   Use them for external spawn and explicit cross-worker notifications, not for
   all local operations.

5. **Add stealable tasks only after pinned tasks are solid.**
   Require `Send`, forbid migration with in-flight worker-owned I/O initially,
   and steal only from idle workers.

6. **Add cross-worker primitives as separate types.**
   Keep local primitives local.

7. **Benchmark against explicit goals.**
   Measure p50, p99, p99.9, max latency, memory per task, throughput per core,
   cross-thread wake cost, and scaling from 1 to N workers.

## Is the unicorn possible?

Yes, but the unicorn is not "any task anywhere at any time." That version is
possible only by giving up the current runtime's simplicity and much of its
latency story.

The plausible unicorn is:

- pinned stackful tasks for the hot I/O path,
- explicit worker partitioning for predictable latency,
- one io_uring per worker,
- local-first scheduling,
- opt-in `Send` stealable tasks for background or imbalance-prone work,
- separate cross-worker primitives,
- instrumentation that proves wake-to-run and CQE-to-run latency stay bounded.

This gives users a single multithreaded runtime process where most work is
thread-isolated and stackful, but some work can be stolen when the programmer
chooses to make it migration-safe.

## Conclusion

A combined single-thread/work-stealing `kimojio-stack` is feasible in Rust, but
not as a transparent extension of the current API. The current design's power
comes from locality, scoped borrowing, non-`Send` support, local waiters, and a
single io_uring-driven scheduler. Work stealing cuts directly against those
properties unless it is explicit and constrained.

The best design is therefore:

1. Preserve the current single-thread runtime as the core latency primitive.
2. Build a separate experimental multithreaded runtime around multiple local
   workers.
3. Use one io_uring per worker.
4. Make pinned tasks the default.
5. Add stealable tasks as an opt-in `Send` task class.
6. Keep local and cross-worker synchronization types separate.
7. Treat tail-latency instrumentation as a required feature, not an afterthought.

If implemented this way, `kimojio-stack` can offer something genuinely distinct:
not a clone of async Rust's work-stealing ecosystem, but a thread-per-core
stackful runtime with a carefully bounded escape hatch for work stealing.
