# Structured Parallelism and Job Graphs

`kimojio-stack` should treat parallelism as an explicit structured tool, not as hidden scheduler magic: local stackful scopes remain the default model for nested concurrency, while chunky throughput-dominated work can opt into fork-join or job-graph APIs that expose where work may run, how much work is queued, when it joins, and what cancellation costs are incurred. The design direction is Rayon-like in spirit—scoped fork-join, work splitting, joins, and bounded worker pools—but adapted to stackful execution by making migration points, `Send` requirements, stack costs, I/O ownership, and cancellation scopes visible.

## API/developer model

The public model should add a parallel layer beside normal stackful scopes rather
than making every coroutine implicitly parallel. The useful units are:

- **Local scopes**: today's `RuntimeContext::scope` and `Scope::spawn`, allowing
  non-`Send` stack-borrowing work pinned to one runtime thread.
- **Parallel scopes**: lexical regions that may run `Send` child jobs on a
  bounded worker set and must join before the scope returns.
- **Job graphs**: explicit DAGs for chunked workloads where dependencies,
  resource budgets, and result aggregation are known before or during execution.
- **Handles/futures**: typed handles for joining results, observing readiness, and
  requesting cancellation without detaching work from its owner scope.

Sketch:

```rust
cx.parallel_scope(|ps| {
    let left = ps.spawn(|pcx| compress_block(pcx, left_half));
    let right = ps.spawn(|pcx| compress_block(pcx, right_half));

    merge(left.join(pcx)?, right.join(pcx)?)
})
```

For dynamic work:

```rust
cx.job_graph(|g| {
    let read = g.spawn_io("read extents", read_extents);
    let decode = g.map_chunks_after(read, "decode", decode_extent);
    let hash = g.reduce_after(decode, "hash", hash_chunk, combine_hashes);

    hash.join()
})
```

Key rules:

- Parallel jobs are lexically scoped. A job cannot outlive the `parallel_scope` or
  graph that created it.
- Jobs that may run on another worker require `Send` closures, inputs, outputs,
  panic payloads, and cancellation state.
- A parallel job may contain nested local `kimojio-stack` scopes, but those nested
  scopes are pinned to the worker currently running the job.
- A local stackful coroutine may call a parallel scope, but joining the parallel
  work parks only that coroutine, not the whole runtime thread.
- A `ParallelHandle<T>` is readiness-aware and joinable. Dropping it requests
  cancellation according to the parent scope policy rather than silently
  backgrounding work.
- A job graph handle represents graph completion, not an ambient future executor.
  It should not imply that arbitrary Rust `Future`s are being scheduled.

Cancellation should follow structured-concurrency rules:

1. explicit cancellation of a parent scope requests cancellation for all live
   descendants;
2. child failure can either fail-fast-cancel siblings or be collected, depending
   on the chosen policy;
3. scope exit waits for all children to reach terminal states;
4. detached/background parallel work is a separate explicit API, not the default.

## Execution engine options

### Option A: inside the stack runtime

The existing runtime grows a parallel worker pool and exposes parallel APIs from
`RuntimeContext`.

Pros:

- single crate and a simple "one runtime owns everything" story;
- direct integration with `Waitable`, cancellation, tracing, and stackful joins;
- easier nested local/parallel scopes from user code.

Cons:

- risks making the local scheduler `Send`/atomic-heavy;
- increases API pressure on a crate whose value is a small local cost model;
- blurs I/O ownership if parallel workers also want io_uring access.

This should not be the first implementation unless the internal seams are already
split enough that local runtime hot paths remain local.

### Option B: side runtime

Create a sibling crate or module, for example `kimojio-stack-parallel`, that owns
a bounded pool for CPU/chunky work and integrates through explicit handles.

Pros:

- preserves the current local runtime and non-`Send` scope model;
- makes opt-in cost visible at the dependency/API boundary;
- allows Rayon-like worker-pool experiments without destabilizing I/O APIs;
- can support both `kimojio-stack` callers and non-runtime callers.

Cons:

- one more runtime-like component to configure;
- cross-runtime join/cancel integration must be carefully designed;
- nested local scopes inside parallel workers need clear capability boundaries.

This is the recommended prototype path.

### Option C: hybrid

Use a side parallel engine first, then allow a future multithreaded stack runtime
to host or share the same job-graph scheduler. Local stackful tasks remain pinned;
parallel jobs are a separate task class that can be run by worker-local runtimes
or by the side pool.

Pros:

- matches the long-term "explicit task classes" direction;
- supports thread-per-core runtimes plus a separate bulky-work lane;
- avoids committing early to one scheduler topology.

Cons:

- requires stable boundaries for cancellation, handles, metrics, and resource
  budgeting;
- may duplicate queues until the design settles.

## Scheduling and work splitting strategy

The scheduler should optimize for explicit chunky work, not tiny hidden tasks.
Useful policies:

- **Fork-join first**: support `join`, `scope.spawn`, chunked `map`, and
  reductions before general graph scheduling.
- **Work units have grain sizes**: APIs should require or infer a minimum chunk
  size. If a workload is below grain, run it inline.
- **Owner-first execution**: the caller runs one branch locally and makes the
  other branch stealable, preserving cache locality like Rayon.
- **Deque-based stealing**: workers push/pop from local deques; idle workers steal
  larger batches or the older half of a victim deque.
- **No migration while running**: a stackful job only moves before it starts or at
  explicit yield/park boundaries where the API has guaranteed `Send` state.
- **Graph-ready queues**: job graphs place only dependency-ready nodes on worker
  queues; blocked nodes remain in graph state.
- **Budgeted nesting**: nested parallel scopes should reuse the current pool and
  execute depth-first to avoid thread explosion.

For divide-and-conquer APIs, expose splitters:

```rust
ps.for_each_chunks(input, ChunkSize::bytes(256 * 1024), |chunk| {
    process(chunk)
});
```

For graph APIs, make dependencies explicit:

```text
read extent 0 ─┬─> decode 0 ─┬─> checksum 0 ─┐
read extent 1 ─┤              ├─> checksum 1 ─┼─> reduce
read extent 2 ─┴─> decode 2 ─┴─> checksum 2 ─┘
```

The runtime should report when work is mostly inline, mostly queued, or mostly
stolen so users can see whether a graph is actually parallel.

## Backpressure and fairness

Parallel APIs need bounded admission. Otherwise a local stackful runtime can
produce more work than the parallel pool, disks, TLS contexts, or memory can
absorb.

Recommended controls:

- `ParallelOptions { workers, max_queued_jobs, max_inflight_bytes, stack_size,
  fail_policy }`;
- per-scope job budgets inherited by child scopes;
- graph-level ready-node caps;
- byte/token budgets for chunked input;
- cooperative cancellation checks in long-running jobs;
- backpressure from `spawn`/`submit` when queues are full, either by running
  inline, parking the caller, or returning `WouldBlock`.

Fairness should be budget-based:

- local runtime I/O and ready tasks must not be starved by joins on parallel work;
- parallel workers should periodically check cancellation and global shutdown;
- graph nodes should not monopolize a worker if they exceed declared CPU budgets;
- nested scopes should prefer helping their own descendants before stealing
  unrelated work;
- low-latency pinned work should have a separate lane from bulky parallel jobs.

The default should be conservative: bounded queues, fixed worker count, explicit
inline fallback, and metrics that show queue depth and wait time.

## TLS/disk/chunky workload fit

Structured parallelism fits workloads where each unit is much larger than a
coroutine wakeup:

- TLS record encryption/decryption over buffers;
- compression, decompression, checksums, parsing, and serialization;
- batched file hashing or indexing;
- disk extent reads followed by CPU-heavy decode;
- fan-out/fan-in request processing where subrequests are bounded and join
  before response completion.

The design should keep I/O ownership explicit. A local `kimojio-stack` runtime
can submit io_uring operations and then hand completed buffers to a parallel
scope for CPU work. Conversely, a job graph can have an explicit I/O node type,
but that node should state which runtime or worker owns the fd/ring. Hidden
"parallel job performs arbitrary I/O anywhere" would undermine the thread-local
cost model.

For TLS specifically:

- keep connection state pinned to its owning local runtime;
- parallelize buffer-sized crypto only when records or batches are large enough;
- return encrypted/decrypted buffers through structured handles;
- avoid moving non-`Send` session state into parallel jobs;
- expose memory copies and buffer ownership in the API.

For disk workloads:

- model read-ahead, decode, and write-back as separate graph nodes;
- bound in-flight bytes, not just job count;
- preserve kernel resource lifetimes through operation-owned buffers/fds;
- prefer sequential extent batching over fine-grained per-block jobs.

## Costs and hidden-cost avoidance

The API should make these costs visible:

- worker-pool creation and thread count;
- per-job stack allocation or stack reuse;
- `Send + 'scope` constraints for movable work;
- queueing and stealing overhead;
- memory retained by graph nodes, results, and panic payloads;
- input/output buffer ownership and copies;
- cancellation latency for running jobs;
- I/O ownership and cross-thread wake paths;
- NUMA/cache effects when chunks move across workers.

Avoid:

- silently parallelizing ordinary `Scope::spawn`;
- hidden global queues on every wakeup;
- unbounded recursive spawning;
- automatic migration of local coroutines;
- ambient async `Future` execution inside stackful APIs;
- implicit disk/TLS work stealing without resource budgets.

Good defaults should be unsurprising:

- small workloads run inline;
- only explicit parallel scopes use parallel workers;
- nested parallelism reuses the same pool;
- cancellation is requested immediately but completed cooperatively;
- metrics can explain whether time was spent running, queued, stolen, blocked on
  dependencies, or canceling.

## Failure modes and cancellation semantics

Failure modes to design for:

- a child job panics while siblings are running;
- a job graph node returns an error after downstream work was queued;
- a parent local coroutine is canceled while waiting on parallel work;
- a parallel job ignores cancellation for too long;
- a worker thread panics or exits;
- graph memory grows because completed node state is retained;
- nested scopes create too many queued jobs;
- an I/O-owning node is canceled after kernel submission but before completion.

Recommended semantics:

- `ParallelScope` has a `FailurePolicy`: `FailFast`, `CollectAll`, or
  `BestEffortCancel`.
- `FailFast` requests sibling cancellation when the first child fails, then joins
  all children before returning that error.
- `CollectAll` lets already-started children finish and returns a structured
  error set.
- Dropping a `ParallelHandle` inside a live scope requests cancellation but does
  not detach runtime-owned state.
- Scope drop/cancellation waits until every child is `Completed`, `Canceled`, or
  `Panicked`.
- Running CPU jobs observe cancellation only at explicit checks or runtime
  yield/park calls; APIs should document that cancellation is cooperative.
- Graph nodes with I/O operations must keep fd and buffer resources alive until
  kernel completion is reaped, matching the current operation-state direction.

The important invariant is that structured parallelism never leaves orphaned
jobs behind a returned scope.

## Prototype plan and benchmark plan

### Prototype plan

1. Define a small `ParallelScope` API in a side crate/module with:
   - fixed-size worker pool;
   - scoped `spawn`;
   - `join`;
   - `join2`;
   - chunked `for_each`/`map_reduce`;
   - cooperative cancellation token;
   - configurable stack size and queue bounds.
2. Integrate with `kimojio-stack` through a `Waitable`/handle bridge so a local
   coroutine can wait for a parallel scope without blocking the OS thread.
3. Preserve nested local scopes by giving each parallel worker a local stackful
   execution capability or by allowing stackful jobs to run only inside explicit
   worker-local contexts.
4. Add graph state after fork-join works: node ids, dependency counts, ready
   queues, bounded in-flight bytes, result storage, and failure policy.
5. Add metrics: spawned, inline, stolen, completed, canceled, queue wait,
   execution time, cancellation latency, max graph memory, and in-flight bytes.
6. Revisit whether the engine should move inside `kimojio-stack` after the API
   and measurements are stable.

### Benchmark plan

Start with microbenchmarks:

- spawn/join one job;
- `join2` with balanced and unbalanced branches;
- recursive divide-and-conquer at varying grain sizes;
- map/reduce over buffers from 4 KiB to 16 MiB;
- cancellation of queued and running jobs;
- nested parallel scopes.

Then benchmark representative chunky workloads:

- TLS-like encrypt/decrypt buffers;
- gzip/zstd-like compression if dependencies are acceptable, or synthetic CPU
  transforms otherwise;
- checksum/hash over file extents;
- io_uring read -> CPU transform -> write graph;
- request fan-out/fan-in with bounded subjobs.

Compare against:

- inline single-thread execution;
- manual OS-thread sharding;
- Rayon for pure CPU workloads;
- Tokio `spawn_blocking` where relevant.

Measure throughput, p50/p99/p99.9 latency, queue wait, steal count, memory/RSS,
worker utilization, cancellation latency, and impact on local runtime I/O tail
latency while parallel work is active.

## Pros / Cons

Pros:

- gives users a first-class path to core utilization without making all
  stackful work multithreaded;
- preserves structured/nested concurrency and lexical lifetimes;
- keeps non-`Send` local scopes valuable and cheap;
- fits Rust's scoped parallelism model and familiar Rayon patterns;
- makes chunk size, budgets, cancellation, and worker count explicit;
- supports graph-shaped disk/TLS/batch workloads better than ad hoc spawning.

Cons:

- introduces another scheduling surface and more configuration;
- requires careful `Send` and stack-safety boundaries for movable jobs;
- cancellation remains cooperative for running CPU jobs;
- job graphs can retain substantial memory if result lifetimes are unclear;
- integration with io_uring ownership is subtle;
- benchmarks are required before choosing defaults;
- too much API surface could distract from hardening the local runtime.

## Open questions

- Should the first implementation be a sibling crate, an optional module, or an
  internal engine behind unstable APIs?
- Are parallel jobs stackful coroutines, ordinary closures on worker threads, or
  both?
- Can a safe API support moving suspended stackful jobs between workers, or
  should migration be limited to not-yet-started closures?
- What is the minimum useful bridge between `ParallelHandle<T>` and `Waitable`?
- Should `ParallelScope::spawn` run inline under pressure, park the caller, or
  return an admission error?
- How should graph result storage be reclaimed as soon as downstream nodes finish?
- What default grain-size guidance should be documented for TLS, disk, and CPU
  transforms?
- Should graph I/O nodes be allowed in the first version, or should v1 require
  I/O outside the graph and CPU work inside it?
- How should worker affinity and NUMA placement be represented without making the
  common API heavy?
- What metrics should be stable public API versus debug-only instrumentation?
