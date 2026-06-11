# Complementary Throughput Runtime

`kimojio-stack` should stay a local, predictable, low-overhead runtime for
latency-sensitive stackful I/O, while chunky, irregular, throughput-dominated
work should move to an explicit companion runtime that can use work stealing,
global utilization, and heavier synchronization without changing the core
scheduler cost model. The design direction is a separate throughput runtime with
explicit offload, bounded admission, cooperative cancellation, and stackful
result delivery, so applications can compare it directly with built-in work
stealing and simple threads before accepting its costs.

## Boundary between kimojio-stack and the throughput runtime

The boundary should be visible in both types and control flow:

- `kimojio-stack::Runtime` remains single-threaded, scope-based, and optimized
  for low-latency I/O and fine-grained cooperative scheduling.
- The throughput runtime is a separate runtime type, likely a separate crate such
  as `kimojio-stack-throughput` or `kimojio-throughput`, whose public API
  requires thread-safe job state.
- Stackful coroutines do not migrate into the throughput runtime. They submit
  jobs, park cooperatively, and later join results.
- Throughput workers do not drive stackful schedulers, resume stackful
  coroutines, or call `RuntimeContext` APIs directly.
- Local stackful synchronization primitives remain local. Cross-runtime
  interaction uses explicit thread-safe endpoints, completion handles, and
  external wake integration.

This makes the comparison clear:

| Model | Best fit | Hidden cost risk |
|-------|----------|------------------|
| Current `kimojio-stack` | Low-latency I/O, explicit partitioning, non-`Send` scoped work | Manual load distribution |
| Built-in work stealing inside `kimojio-stack` | One-runtime ergonomics | `Send` pollution, atomics on local paths, cross-thread wake storms |
| Simple threads or fixed pools | Rare coarse tasks, simple ownership | Weak cancellation/backpressure, poor irregular load balance |
| Complementary throughput runtime | Frequent chunky irregular tasks needing utilization | Explicit offload, `Send` bounds, queueing and result-buffer costs |

The companion runtime should be used only at deliberate boundaries: CPU-heavy
transforms, blocking or throughput-oriented disk work, TLS setup or bulk crypto,
compression, indexing, parsing, batch RPC fanout, and other tasks whose latency
budget is much larger than a stackful scheduler tick.

## API/developer model for offloading and joining results

The baseline API should make costs explicit and support both one-shot and batch submissions:

```rust
let throughput = ThroughputRuntime::builder()
    .workers(WorkerCount::AvailableCores { reserve_for_stack: 4 })
    .queue_capacity(QueueCapacity {
        global_jobs: 4096,
        per_tenant_jobs: 512,
        result_slots: 4096,
    })
    .stealing(StealingPolicy::IdleOnly)
    .build()?;

runtime.block_on(|cx| {
    cx.scope(|scope| {
        let cancel = CancelToken::new();

        let digest = throughput.spawn(
            JobOptions::new(JobClass::Cpu)
                .tenant("ingest")
                .deadline(cx.deadline())
                .cancel_token(cancel.clone()),
            move |job_cx| {
                job_cx.check_cancelled()?;
                compute_digest(input)
            },
        )?;

        let ready = cx.wait_any(&[digest.waitable()], timeout)?;
        if ready.timed_out() {
            digest.cancel();
        }

        let result = digest.join(cx)?;
        scope.spawn(move |_| consume(result));
    })
});
```

Recommended handle model:

```rust
pub struct ThroughputRuntime;
pub struct JobHandle<T>;
pub struct JobWaitable<'a>;

impl ThroughputRuntime {
    pub fn try_spawn<T, F>(&self, opts: JobOptions, f: F) -> Result<JobHandle<T>, TrySpawnError<F>>
    where
        T: Send + 'static,
        F: FnOnce(JobContext) -> T + Send + 'static;

    pub fn spawn<T, F>(&self, opts: JobOptions, f: F) -> Result<JobHandle<T>, SpawnError>
    where
        T: Send + 'static,
        F: FnOnce(JobContext) -> T + Send + 'static;

    pub fn spawn_batch<T, I, F>(&self, opts: BatchOptions, inputs: I, f: F) -> Result<BatchHandle<T>, SpawnError>
    where
        T: Send + 'static,
        I: IntoIterator + Send + 'static,
        I::Item: Send + 'static,
        F: Fn(JobContext, I::Item) -> T + Send + Sync + 'static;
}

impl<T> JobHandle<T> {
    pub fn waitable(&self) -> JobWaitable<'_>;
    pub fn try_join(&self) -> Result<Option<T>, JoinError>;
    pub fn join(self, cx: &RuntimeContext<'_>) -> Result<T, JoinError>;
    pub fn cancel(&self);
    pub fn detach(self);
}
```

The first prototype should require `F: Send + 'static` and `T: Send + 'static`.
Scoped borrowed jobs can be a later feature only if the API can prove that every
borrowed job is completed, cancelled, and reclaimed before the stackful scope
exits. A possible later shape is:

```rust
scope.offload_scoped(&throughput, opts, |job_cx| {
    // May borrow from the parent scope, but cannot detach.
});
```

That API should be treated like `std::thread::scope`: ergonomic, but stricter and
harder to implement than the `'static` offload path.

## Work stealing algorithm and worker topology

The runtime should optimize for throughput under irregular load, not invisible
latency magic. Suggested topology:

```text
ThroughputRuntime
  NUMA shard 0
    worker 0: local deque + result buffer + metrics
    worker 1: local deque + result buffer + metrics
    shard injector queue
  NUMA shard 1
    worker N: local deque + result buffer + metrics
    shard injector queue
  global coordinator: admission, shutdown, tenant limits, metrics
```

Worker count should be configurable rather than always `num_cpus()`. Services
must be able to reserve cores for stackful I/O workers, OS housekeeping, and
kernel io_uring helper activity. On a 6-core VM this may mean one or two
throughput workers; on a 256-core VM it may mean several NUMA-local shards.

Scheduling algorithm:

1. Submissions choose a shard by explicit affinity, tenant hash, source stackful
   runtime, or least-loaded shard.
2. A worker pushes locally generated follow-up chunks onto the bottom of its
   deque and pops from the bottom for cache locality.
3. External submissions enter a bounded shard injector, not a single hot global
   queue.
4. Idle workers first drain their local deque, then their shard injector, then
   steal from the top of another worker's deque in the same NUMA shard.
5. Cross-shard stealing is a last resort after a longer idle threshold.
6. Steals take a bounded batch, usually half the victim's cold tasks up to a
   configured limit.
7. Workers periodically check cancellation, result backlog, and fairness budgets.

Jobs should be coarse enough to amortize the offload boundary. A practical
initial rule is that a job should run for at least tens of microseconds, and
preferably hundreds, unless it is part of a batched submission where the runtime
can coalesce admission and result delivery.

## Backpressure, cancellation, and result delivery into stackful scopes

Backpressure is part of the API, not an implementation detail:

- `try_spawn` fails immediately when tenant, shard, global, memory, or result
  limits are exhausted.
- `spawn` may block only in the caller's environment. From stackful code it
  should park the coroutine through a `Waitable`; from OS-thread code it may wait
  on a condition variable.
- `OffloadPermit` can reserve queue capacity before expensive input preparation.
- Batch APIs should support bounded in-flight chunks so a batch of one million
  items does not enqueue one million closures.
- Result queues are bounded separately from job queues; a slow stackful consumer
  must apply backpressure rather than accumulating completed results forever.

Cancellation is cooperative:

- `JobHandle::cancel` sets a cancellation token and removes the job from queues
  if it has not started.
- Running jobs observe cancellation through `JobContext::is_cancelled`,
  `check_cancelled`, or chunk boundaries.
- The runtime does not kill worker threads or unwind arbitrary running code.
- Dropping an unjoined handle should request cancellation and detach result
  delivery according to explicit policy; scope exit should cancel and drain any
  handles owned by that scope.
- Completed-but-unobserved results remain bounded by result capacity and then
  exert backpressure on workers.

Result delivery into stackful scopes should reuse the existing cross-thread wake
pattern: each handle owns completion state, and completion pushes an external
wake into the submitting stackful runtime. `JobWaitable` becomes ready when the
result, panic, or cancellation state is available. `join(cx)` parks only the
current stackful coroutine; it must not block the OS thread that drives
`kimojio-stack`.

Panic policy should also be explicit. A job panic can be captured as
`JoinError::Panic(payload)` and re-raised by `join`, matching stackful join
semantics, or converted to an error in fallible APIs. Silent worker death is not
acceptable.

## TLS/disk/CPU-heavy workload fit

Good fits:

- TLS certificate verification, key generation, expensive handshakes, bulk crypto
  batches, and compression around TLS streams.
- CPU-heavy parsing, JSON/protobuf transcoding, checksums, hashing, indexing,
  image or document transforms, and other compute stages after I/O has delivered
  buffers.
- Disk-adjacent work such as compression, encryption, checksums, directory tree
  walking, cold-cache metadata scans, and blocking filesystem calls that do not
  fit the low-latency io_uring path.
- Irregular batch work where some items take 100x longer than others and static
  per-core partitioning leaves cores idle.

Poor fits:

- Per-packet or per-request hot-path work whose offload and wakeup cost is
  comparable to the work itself.
- Work that needs `!Send` state, stackful local locks, stackful channels, or
  worker-local `io_uring` resources.
- Latency-sensitive socket I/O that should stay pinned to a stackful runtime.
- Very rare administrative tasks where a simple scoped thread is clearer.

The runtime should make the TLS/disk/CPU distinction explicit through
`JobClass`. Classes can set default queue limits, worker reservations, and
instrumentation:

```rust
pub enum JobClass {
    Cpu,
    BlockingDisk,
    TlsCrypto,
    Compression,
    BackgroundMaintenance,
}
```

`BlockingDisk` may need a lower worker cap than pure CPU work so blocking syscalls
do not consume every throughput worker. TLS and compression often benefit from
batch APIs that amortize submission and improve cache locality.

## Fairness, tenant isolation, NUMA/cache locality

Fairness should be budgeted:

- Per-tenant queue and in-flight limits prevent one tenant from filling every
  worker deque.
- Weighted deficit scheduling lets high-priority tenants receive more service
  without starving low-priority tenants.
- Long jobs should expose chunk boundaries or cooperative yield points so
  cancellation and fairness can take effect.
- Workers should track longest uninterrupted job time and emit diagnostics for
  jobs that exceed configured budgets.

Tenant isolation should include memory, queue slots, result slots, and CPU time.
A tenant that stops joining results should backpressure its own submissions
before affecting unrelated tenants. Admission errors should identify the limiting
resource.

NUMA and cache locality matter more as services scale toward hundreds of cores:

- Prefer NUMA-local submission based on the stackful runtime's current worker,
  tenant placement, or data ownership.
- Allocate large job buffers before submission only when ownership and NUMA
  placement are intentional; otherwise let the throughput worker allocate local
  scratch.
- Steal within a NUMA shard before stealing across sockets.
- Avoid stealing jobs marked `Affinity::Strict`.
- Keep hot follow-up work on the worker that produced it unless load imbalance is
  severe.

The design should expose enough metrics to prove these policies: local pops,
same-shard steals, cross-shard steals, remote memory ratio when available, LLC
misses in benchmarks, per-tenant service time, and queue wait time.

## Costs and hidden-cost avoidance

The companion runtime has real costs:

- Jobs and results cross a thread-safe boundary.
- Closures and results must be `Send`, usually `'static`.
- Submission, completion, cancellation, and metrics use atomics or locks.
- Worker threads consume native stacks and scheduling resources.
- Result delivery may wake a stackful runtime from another thread.
- Queueing improves utilization but can increase latency variance.

The key is to keep those costs opt-in:

- Do not add atomics, global queues, `Send` bounds, or cross-worker waiters to
  the `kimojio-stack` hot path.
- Do not silently route stackful tasks to the throughput runtime.
- Name APIs after the cost: `offload`, `throughput`, `blocking_disk`, `batch`.
- Keep local and cross-runtime primitives separate.
- Bound every queue and make saturation observable.
- Prefer explicit worker and tenant configuration over magic auto-scaling.
- Require applications to choose what happens on handle drop: cancel, detach, or
  join at scope exit.

This avoids the main failure mode of built-in work stealing: making every local
operation pay for the possibility that some unrelated task might migrate.

## Prototype plan and benchmark plan

Prototype phases:

1. **Fixed-pool baseline.** Build a separate runtime with fixed worker threads,
   bounded submission, `JobHandle<T>`, `join(cx)`, panic capture, and external
   stackful wake integration. No stealing yet.
2. **Backpressure and cancellation.** Add `try_spawn`, stackful waiting for
   permits, result-capacity limits, cooperative cancellation tokens, and
   deterministic drop policy.
3. **Per-worker deques.** Replace the central queue with worker deques and shard
   injectors while preserving the fixed-pool API.
4. **Idle-only work stealing.** Add same-shard stealing, bounded steal batches,
   and metrics for steal attempts, success, and stolen job age.
5. **Tenant and class policy.** Add per-tenant limits, weighted scheduling,
   `JobClass`, and class-specific worker caps.
6. **NUMA topology.** Add optional worker pinning, NUMA shards, affinity hints,
   and cross-shard stealing thresholds.
7. **Scoped borrowed jobs, if justified.** Only after the `'static` API is stable,
   evaluate whether a non-detachable scoped offload API is worth the safety and
   implementation complexity.

Benchmark comparisons:

- Inline stackful execution on one or more manually partitioned runtimes.
- `std::thread::scope` or one thread per chunky task.
- A simple fixed thread pool with one central queue and no stealing.
- The throughput runtime with stealing disabled.
- The throughput runtime with idle-only stealing enabled.
- If implemented, an experimental built-in `kimojio-stack` work-stealing runtime.

Benchmark workloads:

- Uniform CPU chunks to measure scheduler overhead.
- Irregular CPU chunks with heavy-tailed durations to measure utilization.
- TLS handshake and bulk crypto batches.
- Compression/checksum pipelines fed by stackful I/O.
- Blocking disk or metadata scans isolated in `JobClass::BlockingDisk`.
- Mixed workload: latency-sensitive stackful echo/RPC service plus background
  throughput jobs.

Metrics:

- Throughput per core and total throughput from 1, 2, 4, 8, 16, 32, 64, 128, and
  256 workers where hardware permits.
- Stackful wake-to-resume latency while offload load is present.
- Offload submit-to-start, run time, complete-to-stackful-wake, and join latency.
- p50, p99, p99.9, and max latency for both stackful service requests and
  throughput jobs.
- Queue depth, result backlog, cancellation latency, steal rate, failed steals,
  cross-shard steals, worker utilization, memory per job, and allocations per
  submission.
- Sensitivity to chunk size, tenant count, result consumer speed, NUMA placement,
  and reserved stackful core count.

Success means the throughput runtime wins over simple threads and a fixed pool
for irregular chunky work while leaving the stackful latency path measurably
unchanged when no offload is used and acceptably protected when offload is under
load.

## Pros / Cons

Pros:

- Preserves the current `kimojio-stack` value proposition for predictable
  low-latency paths.
- Gives applications a first-class place for chunky work that benefits from
  global utilization.
- Makes `Send`, queueing, cancellation, and result delivery costs explicit.
- Provides a fair benchmark target against built-in work stealing and simple
  threads.
- Scales from small VMs to large single-VM deployments through configurable
  worker counts, shards, and tenant limits.
- Enables richer scheduling policy without infecting local stackful primitives.

Cons:

- Adds another runtime for developers to configure and understand.
- Requires explicit data movement and `Send` ownership at the offload boundary.
- Can increase latency if used for work that is too fine-grained.
- Needs careful shutdown, cancellation, panic, and drop semantics.
- Introduces metrics and operational complexity.
- Scoped borrowed jobs are hard to support safely on long-lived worker threads.

## Open questions

- Should the crate be named around `kimojio-stack` interop
  (`kimojio-stack-throughput`) or around a general pool (`kimojio-throughput`)?
- Is the first production API strictly `'static`, or is scoped offload important
  enough to design from day one?
- Should `join(cx)` consume the handle, or should handles support shared
  observation with a separate `take_result` operation?
- What is the default handle-drop policy: cancel, detach, or scope-owned drain?
- How should job panics map to stackful scope panic propagation?
- Which metrics are mandatory in the first prototype, and which can wait?
- Should `BlockingDisk` use the same worker pool with class caps or a physically
  separate blocking pool?
- How much NUMA control should be stable API versus builder-only advanced
  configuration?
- What chunk-size guidance is specific enough to be useful without becoming
  hardware-dependent?
- At what workload size does this runtime beat a simple fixed thread pool, and
  is that threshold acceptable for the target services?
