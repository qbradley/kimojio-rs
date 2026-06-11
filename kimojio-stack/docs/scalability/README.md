# Scalability Design Synthesis

## Executive summary

The strongest path is **not** to turn `kimojio-stack` into a transparent multithreaded executor. The recommended architecture is a hybrid:

1. keep the core runtime local, per-worker, and cheap;
2. scale with a **pinned fleet** of local runtimes and explicit routing at ownership boundaries;
3. add **named offload lanes** backed by a companion throughput/parallel runtime for chunky `Send` work;
4. use **actor/resource sharding** as a higher-level fairness and placement model;
5. use telemetry and manifests to tune the fleet, but keep AI/autotuning out of the hot path.

This gives the system a practical answer to overloaded cores without hiding costs: route new movable work, offload coarse CPU/TLS/disk stages, split hot tenants or resources, and only steal not-yet-started `Send` jobs inside explicit throughput lanes. Do not migrate arbitrary live stackful coroutines, do not move in-flight `io_uring` state, and do not let convenience APIs conceal queueing, wakeups, copies, or tenant debt.

## Comparison matrix

Legend: `++` strong fit, `+` good fit, `0` mixed, `-` weak/risky, `--` avoid as default. Implementation risk and operational complexity are rated by burden, so `High` is worse.

| Direction | Latency | Throughput | Predictability | Implementation risk | Tenant fairness | NUMA/cache locality | TLS/disk fit | Operational complexity |
|---|---|---|---|---|---|---|---|---|
| 01 Pinned-core routing | `++` local hot path; handoff only at declared boundaries | `+` strong when work is routable; weak for one indivisible hot connection/resource | `++` best alignment with kimojio principles | Medium | `+` admission and routing can enforce budgets | `++` hierarchical routing can stay NUMA-local | `++` fd/TLS/ring ownership remains natural | Medium-high: policies, counters, route tuning |
| 02 Built-in work stealing | `0/-` can reduce idle time but adds remote wakes/cache misses; dangerous for tails if hidden | `+` for irregular `Send` jobs before stack allocation | `-` unless strictly opt-in and pre-start only | High: stackful migration and ring ownership are hard | `0` utilization is not tenant fairness | `0/-` steals fight locality unless heavily constrained | `-` pending I/O and registered resources pin work | High: state machines, diagnostics, surprising pinning |
| 03 Companion throughput runtime | `+` protects stackful hot path; offload adds visible latency | `++` best practical answer for chunky irregular work | `++` explicit `Send` boundary, bounded queues | Medium | `+` per-tenant/job-class limits are natural | `+` shard/NUMA-aware pools possible | `+` good for TLS crypto, compression, blocking-ish disk stages; not direct socket ownership | Medium-high: another runtime to size and observe |
| 04 Explicit offload lanes | `++` isolates CPU/TLS/disk/background pressure from reactors | `+` good if lanes are sized and not fragmented | `++` queueing and resource class are visible | Medium | `++` per-tenant rings, priorities, quotas | `+` lane placement can be NUMA-local | `++` strongest vocabulary for TLS, disk, CPU, background separation | High: many knobs; wrong lanes/cost hints hurt |
| 05 Two-tier I/O/compute cores | `+/0` protects reactors, but every offload hop hurts tiny requests | `++` strong for large TLS/disk/compute-heavy services | `+` if rings and ownership are explicit | High | `+` credits per tier can isolate tenants | `+/--` excellent if NUMA-local; awful if all-to-all/cross-socket | `++` very strong for TLS/disk pipelines | Very high: sequencing, return rings, credit loops, topology |
| 06 Actor-sharded runtime | `+` local actor turns are cheap; remote messages cost | `+` good when shard keys split heat; bottlenecks on indivisible shards | `+` explicit placement epochs/mailboxes | Medium-high | `++` best multi-tenant model | `+` placement can follow tenant/resource memory | `+` resource-affine actors fit TLS/disk, but migration is constrained | High: actors, supervisors, placement, migration protocol |
| 07 Structured parallelism/job graphs | `+` good when grain is coarse and scoped; bad if used for tiny work | `++` excellent for fork-join, batch, DAG workloads | `+` lexical joins and budgets preserve clarity | Medium | `0/+` scope budgets help, but not a full tenant scheduler | `0/+` needs affinity/grain tuning | `+` good for buffer crypto, compression, disk-processing DAGs; I/O ownership must stay explicit | Medium-high: graph memory, cancellation, nested parallelism |
| 08 AI/autotuned control plane | `0/+` indirect improvement through better manifests | `0/+` indirect utilization gains | `+` in recommend-only mode; `--` if autonomous hot-path decisions | High for auto-apply; low/medium for telemetry + offline recommendations | `+` can surface debt and propose budgets | `+` can find topology issues | `0/+` sizes lanes and shards; does not own resources | Very high if overbuilt; useful only with strict safety rails |

## Cross-cutting observations and conflicts

- **Every credible proposal preserves a local fast path.** The disagreement is where to put the scaling layer: above the runtime (pinned fleet/actors), beside it (throughput/parallel lanes), or inside it (work stealing). The least risky answer is above and beside, not inside the current hot path.
- **Stackful migration is the hard wall.** A closure being `Send` does not prove that live stack locals at a suspension point are `Send`. Safe v1 APIs should not promise migration of already-started stackful continuations.
- **`io_uring`, TLS, and disk resources are ownership-sensitive.** Pending SQEs, fixed buffers, registered files, TLS session state, and socket write ordering want a clear owner. Moving them implicitly violates predictability and creates tail spikes.
- **Utilization and locality conflict.** Work stealing improves idle-core utilization, but can burn cache locality and create remote wakes. Routing and sharding should prefer local/NUMA-local placement and only cross boundaries when the benefit is visible.
- **Fairness belongs at admission and dequeue boundaries.** A global work-conserving scheduler is not enough for multi-tenancy. Use per-tenant credits, bounded queues, cost units, and explicit rejection/defer policies.
- **Lane separation protects critical paths but can fragment capacity.** TLS, disk, CPU, and background lanes should exist, but the first implementation should keep the lane vocabulary small and prove that sizing knobs are understandable.
- **Two-tier I/O/compute is powerful but not a universal default.** It is attractive for huge TLS/disk/compute-heavy services; it adds extra hops and sequencing complexity that can hurt smaller or tiny-request workloads.
- **Actors solve tenant/resource placement better than raw tasks.** Actor sharding gives fairness, supervision, and controlled rebalancing, but it requires application-visible shard keys. It should be a framework layer, not an invisible scheduler trick.
- **Autotuning is only acceptable as a manifest/control-plane tool.** It can suggest core maps, budgets, shard moves, and lane sizes. It must not make per-task scheduling decisions or silently alter task affinity.
- **Long-lived indivisible work remains hard.** A single hot tenant, connection, stream, lock, or disk shard can still bottleneck on one worker unless the application exposes protocol/resource split points.

## Strongest ideas to combine

1. **Pinned fleet substrate** from proposal 01: one local runtime, scheduler, and I/O ring per worker; no global ready queue.
2. **Explicit ownership envelopes**: `AcceptedFd`, `RequestEnvelope`, `ResponseSink`, `BulkJob`, `CryptoJob`, `DiskJob`, and stale-safe completion descriptors.
3. **Bounded handoff and completion rings** from proposals 01, 04, and 05: fixed descriptors, owned buffers, hard capacity, batched wakes, and no fallback unbounded queues.
4. **Companion throughput runtime** from proposal 03: explicit offload, `Send` closures/results, queue/result backpressure, cooperative cancellation, panic capture, and stackful `Waitable` integration.
5. **Named lanes** from proposal 04: CPU, TLS/crypto, disk/blocking, background, and tenant-specific logical queues where justified.
6. **Actor/resource sharding** from proposal 06: tenant/resource mailboxes, placement epochs, local fairness budgets, and controlled moves only at mailbox/I/O-safe boundaries.
7. **Structured parallelism** from proposal 07: fork-join, chunked map/reduce, and later DAGs for work with known grain and dependencies.
8. **NUMA-aware hierarchy** across all designs: choose NUMA node, lane/shard, then worker; avoid per-request scans of hundreds of cores.
9. **Telemetry + manifest contract** from proposal 08: fixed-budget metrics, versioned topology/policy, recommend-only tuning, canary/rollback for live-safe knobs.
10. **Opt-in pre-start stealing only** from proposal 02: useful inside throughput/parallel lanes for `Send` jobs, not for arbitrary local stackful coroutines.

## Designs to avoid or defer

Avoid as default:

- transparent migration of live stackful coroutines;
- hidden work stealing from channel, mutex, join, or I/O wait paths;
- a single global ready queue or global MPMC queue on hot paths;
- unbounded queues after a bounded queue reports full;
- automatic TLS/disk offload for operations below a measured grain threshold;
- compute lanes writing sockets directly or mutating reactor-owned fd state;
- lazy fixed-file/fixed-buffer registration on latency-critical paths;
- all-to-all reactor/compute ring matrices before sparse NUMA-local rings are proven insufficient;
- AI or heuristic controllers that mutate scheduler policy in the hot path;
- auto-changing tenant isolation class or enabling stealing without explicit policy.

Defer until the base is measured:

- safe public migration of already-started stackful continuations;
- actor state migration for actors with live child scopes, local waiters, or in-flight worker-owned I/O;
- full job graph APIs before fork-join/chunked APIs are stable;
- guarded auto-apply of tuning recommendations;
- automatic hot-tenant splitting beyond producing explicit recommendations;
- full two-tier reactor/compute topology for every service class.

## Recommended path forward with phased architecture

### MVP / near-term prototype

Build the smallest architecture that proves explicit scaling without touching the single-core hot path:

1. **Metrics first.** Add fixed-budget per-worker and per-tenant counters/histograms needed for routing decisions: ready depth, wake-to-resume, CQE-to-resume, handoff depth, remote wakes, queue age, rejected work, and tenant debt. Target zero hot-path allocation and a declared CPU overhead budget.
2. **Pinned fleet skeleton.** Start `N` independent local `Runtime` workers, each pinned optionally, each with one primary `io_uring`, a local ready queue, timers, a bounded handoff inbox, and local metrics. `Runtime::new()` remains the existing single-threaded path.
3. **Accept/request routing.** Support `SO_REUSEPORT` or raw-fd handoff before TLS; route by static policy, tenant hash, and power-of-two choices within a NUMA/lane group. Keep established connection I/O on the owner worker.
4. **Admission control.** Enforce hard limits for connections, stack bytes, body bytes, handoff slots, in-flight I/O ops/bytes, and per-tenant queue debt. Fail early with visible overload results.
5. **Minimal offload lane.** Add CPU and background lanes backed by a fixed worker pool, bounded submission/result queues, `try_submit`, cooperative cancellation, panic capture, and a `Waitable`/join bridge back to stackful code. Require `Send + 'static` for v1.
6. **Explicit completions.** Results return to the owning worker through completion descriptors; stale generations are dropped safely; no offload worker writes sockets or touches worker-local I/O state.
7. **Benchmark gates.** Prove that no-offload single-core and per-worker local operation overhead remains effectively unchanged.

### Medium-term evolution

1. Add TLS/crypto and disk lanes with separate budgets, cost hints, and tenant limits.
2. Replace fixed central lane queues with per-worker deques and NUMA shard injectors; enable **idle-only pre-start stealing** for `Send` jobs inside throughput lanes.
3. Add `ParallelScope` with `join`, `join2`, chunked map/reduce, bounded queues, cooperative cancellation, and integration with stackful waiting.
4. Introduce actor/resource sharding as a framework layer: bounded mailboxes, route-cache epochs, per-actor turn budgets, tenant credits, and pinned resource declarations.
5. Add controlled actor moves only at mailbox boundaries when state is `Send` or snapshot/rebuild is supplied and no worker-owned I/O is in flight.
6. Add NUMA-aware placement for workers, lane pools, buffer slabs, and route decisions.
7. Build operator-facing manifests for worker sets, lanes, queue limits, tenant classes, and explicit stealing policy.

### Long-term research bets

1. Full two-tier reactor/compute deployments for very large TLS/disk/compute-heavy services, using sparse NUMA-local rings and connection generation/sequence validation.
2. Job graphs for disk/TLS/batch pipelines once fork-join APIs and graph-memory reclamation are understood.
3. Offline recommend-only tuner that consumes telemetry and emits manifest diffs with expected benefit, cost, confidence, canary plan, and rollback trigger.
4. Guarded auto-apply only for reversible knobs such as queue caps, ready budgets, and offload concurrency; topology, tenant class, stealing, and resource ownership changes remain manual or canaried.
5. Experimental continuation migration only behind internal/unsafe gates, and only if stackful `Send` safety, context relocation, and I/O pinning invariants can be proven. It is not needed for the recommended v1.

## Concrete API/architecture sketch for the recommended hybrid path

Architecture:

```text
PinnedFleet
  Control plane: manifest, admission tables, route policy, metrics snapshots

  Worker i:
    Local Runtime
    local ready queue + timers
    worker-owned io_uring
    accepted connections / TLS / socket write queues
    bounded handoff inbox
    completion ring from lanes
    local metrics page

  Offload lanes:
    cpu lane: NUMA-sharded ThroughputRuntime workers + bounded queues
    tls lane: optional crypto workers with per-connection/lane affinity
    disk lane: bounded blocking/disk workers or disk-owned rings
    background lane: low-priority bounded workers
```

Builder shape:

```rust
let fleet = PinnedFleet::builder()
    .workers(WorkerCount::physical_cores())
    .pin_threads(true)
    .numa(NumaPolicy::PreferLocal)
    .listen(addr, ListenPolicy::ReusePort { backlog: 8192 })
    .admission(Admission::new()
        .max_handoff_slots_per_worker(4096)
        .max_stack_bytes_per_worker(256 << 20)
        .max_body_bytes_per_request(1 << 20)
        .tenant_limits(TenantLimits::weighted()))
    .lane("cpu", OffloadLaneConfig::cpu()
        .workers(WorkerCount::reserve_for_io(4))
        .queue_capacity(8192)
        .stealing(StealingPolicy::IdleOnlyPreStart))
    .lane("background", OffloadLaneConfig::background()
        .queue_capacity(1024)
        .overload(Overload::Shed))
    .build()?;
```

Request routing:

```rust
fleet.serve(|cx, conn| {
    let tenant = conn.tenant();

    conn.route_requests(|req| {
        if req.path() == "/healthz" {
            RouteTarget::Local.cost(Cost::latency_request())
        } else if req.body_len() > 64 * 1024 || req.estimated_cpu_us() > 100 {
            RouteTarget::lane("cpu")
                .tenant(tenant)
                .cost(Cost::cpu_micros(req.estimated_cpu_us()).bytes(req.body_len()))
                .on_full(HandoffFull::Reject)
        } else {
            RouteTarget::tenant_hash("latency", tenant)
                .cost(Cost::latency_request())
        }
    })
});
```

Explicit handoff/offload:

```rust
let receipt = cx.handoff()
    .to(RouteTarget::lane("cpu"))
    .tenant(tenant)
    .cost(Cost::cpu_micros(500).bytes(body.len()))
    .on_full(HandoffFull::Reject)
    .send(RequestEnvelope::owned(req, body, response_sink))?;

let digest = cx.offload("cpu", JobOptions::new()
    .tenant(tenant)
    .class(JobClass::Cpu)
    .deadline(cx.deadline())
    .cost(Cost::cpu_micros(250)), move |job_cx| {
        job_cx.check_cancelled()?;
        compute_digest(buffer)
    })?;

let result = digest.join(cx)?; // parks only this stackful coroutine
```

Actor sharding as a framework layer:

```rust
fleet.register_actor::<TenantActor>()
    .key_by(|msg| ShardKey::TenantBucket {
        tenant: msg.tenant(),
        bucket: msg.bucket(),
    })
    .mailbox_capacity(1024)
    .placement(ActorPlacement::PinnedOrMovableAtMailboxBoundary)
    .turn_budget(ActorTurnBudget::messages_or_micros(64, 50))
    .resource_policy(ResourcePolicy::worker_affine_io());
```

Rules encoded by the API:

- local stackful work stays local unless explicitly put in an owned envelope;
- work crossing workers is `Send` and budgeted;
- queues are bounded and report typed overload;
- lanes never write directly to sockets or mutate owner-worker `io_uring` state;
- stealing is lane-local and pre-start unless an experimental API says otherwise;
- every cross-worker path has metrics for enqueue wait, run time, completion latency, wake count, bytes, tenant, and rejection reason.

## Benchmark and experiment plan

### Baseline and overhead gates

- Compare current single-runtime performance to `Runtime` inside `PinnedFleet` with routing/offload disabled.
- Measure scheduler tick cost, wake-to-resume, CQE-to-resume, allocations after warmup, memory per worker, and telemetry CPU overhead.
- Gate: local hot-path overhead must be negligible or explicitly accepted before adding features above it.

### Routing and handoff

- Accept distribution: single listener, `SO_REUSEPORT`, weighted reuseport, raw-fd handoff.
- Request routing: local, tenant hash, power-of-two choices, NUMA-local lane selection.
- Workloads: plaintext/TLS echo, HTTP/1.1 keep-alive, HTTP/2/gRPC unary, streaming with credit.
- Metrics: handoff latency, remote wakes, inbox depth, queue age, route-cache hit/miss, stale generation drops, p99/p99.9 latency, per-tenant admitted/rejected counts.

### Offload lanes and throughput runtime

- Compare inline execution, simple fixed pool, companion runtime without stealing, and idle-only lane-local stealing.
- Workloads: TLS handshakes, TLS record crypto, compression, parsing, hashing, checksum, disk metadata/cache-miss simulation, mixed latency + bulk tenants.
- Sweep chunk sizes to find the minimum offload grain where throughput wins without p99.9 regression.
- Measure submit-to-start, run time, complete-to-wake, join latency, result backlog, failed submissions, worker utilization, cache misses where available, and allocation rate.

### Queue buildup and backpressure

- Saturate each queue independently: handoff inbox, lane ingress, per-tenant mailbox, result ring, return ring, disk bytes-in-flight.
- Verify memory plateaus at configured bounds and overload is visible as `Full`, `TenantLimited`, `DeadlineMiss`, or `ShuttingDown`, not hidden heap growth.
- Measure queue depth by item count, bytes/work units, age, oldest item, rejection rate, producer stall time, and downstream recovery time.
- Test slow consumers: completed results not joined, connection write backpressure, disk completion delay, and return-ring saturation.

### Tombstoning, stale entries, and cancellation cleanup

Evaluate all places that may leave tombstones or stale descriptors:

- canceled queued offload jobs marked instead of removed;
- stale connection completions after `ConnectionGeneration` changes;
- actor placement redirect stubs after shard moves;
- mailbox entries invalidated by tenant drain or deadline expiration;
- graph nodes canceled while dependencies or I/O completions are still outstanding.

Metrics and tests:

- tombstone/stale count per queue and per tenant;
- skip/scan cost per dequeue and p99 dequeue latency as tombstone density rises;
- retained bytes and time-to-reclaim after cancellation storms;
- compaction thresholds and compaction pause time;
- correctness under repeated cancel/retry/move cycles: no duplicate replies, no lost live messages, no dangling kernel-visible buffers;
- impact on queue age and admission decisions while tombstones occupy capacity.

### Fairness and multi-tenancy

- One noisy tenant plus many quiet tenants; reserved/burstable/background classes; hot-shard imbalance; tenant that stops joining results.
- Measure per-tenant throughput, p99/p99.9, queue debt, rejected/deferred work, borrowed credits, fairness throttles, remote wake contribution, and noisy-neighbor containment.
- Verify low-latency tenants stay bounded when best-effort tenants saturate CPU/TLS/disk lanes.

### NUMA and scale

- Sweep 1, 2, 4, 8, 16, 32, 64, 128, and 256 workers where hardware permits.
- Compare local-only, same-NUMA stealing/handoff, cross-NUMA overflow, and deliberately bad placement.
- Track local vs remote handoffs, remote memory/cache indicators where available, p99.9 latency, throughput/core, and utilization imbalance.

### Actor and shard experiments

- Local vs remote actor message round trip.
- Route-cache epoch miss and `Moved` handling.
- Mailbox turn-budget fairness.
- Controlled actor move with no in-flight I/O, with queued messages, with redirect stubs, and with aborted moves.
- Hot-tenant split/merge convergence and tail-latency impact.

### Control-loop overhead and autotuning

- Measure telemetry disabled/default/diagnostic overhead, export cadence cost, cardinality overflow behavior, and allocation behavior.
- Replay captured traces through heuristic recommendations; compare predicted direction to actual benchmark runs.
- Test recommendation stability under delayed telemetry, wrong labels, remote wake storms, core loss, and lane saturation.
- For any live update experiment, measure validation time, apply time, affected workers, rollback time, and SLO impact.
- Gate: no behavior changes unless a manifest explicitly enables them; no hot-path dependency on the recommender.

## Risks and open questions

- What exact overhead budget is acceptable for fleet metrics on the single-core path?
- Which crate boundary is right: core `kimojio-stack`, `kimojio-stack-fleet`, `kimojio-stack-throughput`, or an application-server framework?
- What is the first stable lane vocabulary? CPU/background only, or TLS/disk from day one?
- What grain-size guidance prevents users from offloading tiny work and hurting tails?
- Should v1 offload require `'static`, or is scoped offload important enough to justify early complexity?
- How should fixed files/buffers be shared or replicated across worker-owned rings without hidden registration costs?
- Which fairness algorithm is simple enough to trust: token buckets, weighted deficit round robin, or a hybrid?
- How much actor supervision and mailbox delivery semantics belong in the runtime versus a framework?
- Can HTTP/2/gRPC stream routing be made useful without excessive protocol machinery?
- How should worker/core pinning interact with NIC RSS, IRQ affinity, cgroups, and container schedulers?
- When does a two-tier topology beat local TLS/disk on real hardware, and where is the crossover for tiny requests?
- What metrics should be stable API versus debug-only, especially for per-tenant labels?
- How do we prevent tuning manifests from becoming a second, overly complex scheduler?
- Is there any sound, ergonomic path to safe continuation migration, or should it remain permanently out of scope?

## Final recommendation

Fund and prototype the **explicit hybrid**: `PinnedFleet` + bounded handoff + admission control + named offload lanes backed by a companion throughput/parallel runtime. Keep `Runtime` local and cheap. Add actor sharding for multi-tenant placement after the basic handoff/lane model is measured. Use work stealing only for not-yet-started `Send` jobs inside explicit throughput lanes. Treat two-tier reactor/compute and AI-assisted tuning as later, opt-in tools for large deployments.

This is the most faithful path to kimojio principles: it uses all cores by moving work at declared ownership boundaries, not by hiding a global scheduler under a low-latency stackful runtime.
