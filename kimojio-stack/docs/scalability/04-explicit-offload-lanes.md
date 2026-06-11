# Explicit Offload Lanes

kimojio-stack should scale chunky or policy-sensitive work through explicit offload lanes rather than transparent task migration: the I/O hot path stays predictable, developers choose where expensive work runs, operators can see queueing and saturation, and backpressure is applied at submission points before hidden CPU, TLS, or disk costs turn into latency spikes.

## Lane types and why separate

- **TLS lanes:** encryption, decryption, handshakes, certificate parsing, and key update work can become CPU-bound while still sitting on a latency-sensitive path. Keeping TLS separate from general CPU work prevents bulk compute from starving crypto progress and makes handshake storms visible.
- **Disk lanes:** filesystem metadata, buffered reads/writes, fsync, log compaction, cache fill, and spill work have different blocking behavior from pure CPU tasks. Dedicated disk lanes isolate kernel or device stalls from CPU queues and allow per-device or per-volume policy.
- **CPU lanes:** compression, parsing, serialization, validation, hashing, routing-table rebuilds, and batch transforms should use all cores when requested, but only after explicit submission. CPU lanes expose the cost of chunky work instead of letting it silently stretch reactor turns.
- **Background lanes:** maintenance, metrics aggregation, expiry scans, compaction planning, prefetch, and other non-request-critical jobs need bounded capacity and lower priority so they yield to foreground work.
- **Tenant-specific lanes:** noisy tenants or premium tenants may need separate quotas, rings, or worker sets. Tenant lanes make fairness and isolation enforceable without requiring every call site to implement bespoke accounting.

Separate lanes let each class use the right queue depth, admission policy, worker count, priority rules, cancellation behavior, telemetry labels, and overload response. A single global pool is simpler, but it hides which resource is saturated and encourages accidental contention between unrelated costs.

## API/developer model

The API should make offload decisions visible at the call site:

```text
tls_lane.submit(scope, priority, cost_hint, work) -> OffloadHandle<Result>
disk_lane.submit(scope, tenant, budget, op) -> OffloadHandle<Result>
cpu_lane.map(scope, items, cost_hint, policy) -> OffloadBatch<Result>
```

Key model points:

- Submission is explicit; no runtime automatically migrates work because it looks expensive.
- The caller chooses a named lane or lane class and supplies a scope for cancellation.
- APIs return handles/futures that must be awaited, polled, joined, or deliberately detached.
- Cost hints are cheap scalar values such as bytes, records, estimated cycles, or operation class.
- Admission can fail synchronously with `Full`, `TenantLimited`, `ShuttingDown`, or `Cancelled`.
- Fire-and-forget is restricted to background lanes and still requires a scope, telemetry label, and overload policy.
- Hot-path APIs should offer non-allocating or pre-reserved submission variants where practical.

The developer should be able to read a request path and see every point where work can queue, block, be cancelled, or run on another core.

## Queue/ring mechanics and backpressure

Each lane should own bounded submission rings rather than unbounded work queues. The default shape is:

- per-worker local rings for cache locality and low contention;
- a lane-level ingress ring for external submissions;
- optional per-tenant rings for fairness and quota accounting;
- work stealing only within an explicit lane class, never across unrelated resource classes by default.

Backpressure should be enforced before enqueue:

- **Hard capacity:** if the ring is full, submission fails or follows a configured overload action.
- **Byte/work budgets:** queues account for declared cost, not just item count.
- **Caller-runs policy:** only enabled explicitly, and only where running inline cannot violate latency bounds.
- **Deadline-aware admission:** reject work that cannot plausibly complete before its deadline.
- **Batch coalescing:** allowed for disk/background lanes when it reduces device or maintenance overhead.

The queue mechanics should prefer fixed-size nodes, slab-backed descriptors, or intrusive task records owned by the caller/runtime to avoid heap allocation on hot submissions. Wakeups should be batched where possible, but each batch boundary must remain visible in telemetry.

## Cancellation, scope integration, and result delivery

Every offloaded item belongs to a scope. Cancelling the scope should:

- reject new submissions;
- remove not-yet-started items from queues when cheap;
- mark queued items as cancelled when removal would be expensive;
- deliver cancellation to running work through cooperative checks;
- drop or route late results according to the handle policy.

Result delivery should avoid forcing worker threads to wake reactors for every small completion. Preferred options are:

- completion rings associated with the submitting reactor or scope;
- batched wakeups after N completions or a short time budget;
- explicit join handles for request-critical work;
- detached completion sinks for background work.

Panic, fatal error, and worker shutdown behavior must be lane-specific and observable. Request-scoped offloads should fail the request or operation; background offloads should report through supervision and telemetry without corrupting shared state.

## Fairness, tenant limits, priority classes

Fairness should be built into admission and dequeue, not bolted on after a queue is already full:

- Per-tenant inflight limits by item count and declared work units.
- Weighted fair dequeue among tenant rings for shared lanes.
- Priority classes such as `critical`, `foreground`, `best_effort`, and `maintenance`.
- Aging or starvation guards so low priority work eventually makes progress under sustained load.
- Separate emergency capacity for cancellation cleanup, TLS handshakes, or control-plane work.
- Configurable isolation for tenants that need hard caps instead of weighted sharing.

Priority must not become hidden migration. A high-priority CPU item may jump ahead within the CPU lane, but it should not silently consume TLS or disk lane capacity. Cross-lane escalation should require policy configuration and be reported.

## Costs and hidden-cost visibility

The design goal is not to make expensive work disappear; it is to make expensive work schedulable, bounded, and visible. Every lane should expose:

- enqueue attempts, accepted work, rejected work, and rejection reason;
- queue depth by item count and declared work units;
- wait time, run time, completion latency, and deadline misses;
- worker utilization, steal counts, park/unpark counts, and batch sizes;
- per-tenant usage, throttling, and priority mix;
- inline fallbacks and caller-runs events;
- allocation count or descriptor-pool misses on submission paths.

APIs and traces should preserve cause and origin: a request waiting on TLS encryption should look different from a request waiting on disk fsync or CPU compression. Dashboards should show saturation by lane so operators can change worker counts, queue depths, or tenant limits without guessing.

## Operational controls and telemetry

Operators should be able to configure lanes independently:

- worker count, CPU affinity, and optional NUMA placement;
- ring depth and work-unit budget;
- per-tenant limits and priority weights;
- overload action: reject, shed background work, caller-runs, coalesce, or degrade feature;
- deadline defaults and maximum queue wait;
- lane enable/disable and drain behavior during shutdown;
- telemetry sampling and high-cardinality tenant label policy.

Runtime controls should support safe adjustment of queue limits, weights, and background capacity without restart. Worker-count changes may be more constrained, but should be observable and coordinated with drain/rebalance operations. Telemetry should include stable lane names and policy versions so benchmark and production data can be compared.

## Prototype plan and benchmark plan

Prototype plan:

1. Define a small `OffloadLane` abstraction with explicit `submit`, `try_submit`, scoped cancellation, and typed rejection.
2. Implement bounded rings with fixed descriptors and a completion ring for result delivery.
3. Start with CPU and background lanes because they are easiest to exercise without device-specific behavior.
4. Add TLS and disk lane adapters that wrap representative crypto and filesystem operations.
5. Add per-tenant accounting and weighted dequeue once basic lane behavior is measurable.
6. Add operator-facing configuration and metrics only after core overhead is understood.

Benchmark plan:

- Measure submission overhead on the reactor hot path with empty, half-full, and saturated rings.
- Compare inline execution, explicit offload, and rejected overload behavior for small and chunky tasks.
- Run TLS-heavy scenarios: handshake bursts, large record encryption, and mixed request traffic.
- Run disk-heavy scenarios: fsync bursts, metadata storms, spill/cache-fill reads, and compaction writes.
- Run CPU-heavy scenarios: compression, parsing, hashing, and batched transforms across all cores.
- Test mixed-lane interference to confirm CPU work does not starve TLS or disk work.
- Test tenant fairness with one noisy tenant, many small tenants, and priority inversion cases.
- Record p50/p95/p99/p999 latency, queue wait, rejection rate, throughput, CPU utilization, and allocation rate.

## Pros / Cons

Pros:

- Keeps scheduling choices explicit and reviewable.
- Protects I/O and reactor hot paths from accidental chunky work.
- Makes TLS, disk, CPU, and background saturation independently visible.
- Provides natural places for backpressure, cancellation, and tenant fairness.
- Allows chunky work to use all cores without pretending it is free.
- Gives operators practical knobs for overload and isolation.

Cons:

- More API surface than a transparent global executor.
- Developers must choose the correct lane and handle rejection paths.
- Incorrect cost hints can reduce fairness or overload accuracy.
- Too many lane variants can fragment capacity and complicate tuning.
- Cross-lane dependencies can deadlock if scopes, queue limits, and blocking joins are misused.
- Benchmarks and telemetry are required to avoid over-engineering the abstraction.

## Open questions

- What is the minimum lane API that supports Rust ergonomics without hiding allocation or wakeup cost?
- Should TLS record encryption be offloaded by default policy, or only at explicit call sites selected by protocol code?
- How should disk lanes map to devices, filesystems, or logical storage classes?
- What cost-hint units are stable enough for admission while still cheap for callers to provide?
- Should tenant-specific lanes be physical worker pools, logical queues over shared workers, or both?
- Which priority classes are essential for the first prototype?
- How should blocking joins be prevented or detected when a worker submits dependent work to the same saturated lane?
- What telemetry cardinality limits are needed for per-tenant metrics in large deployments?
- Can completion batching meet p99 latency goals without excessive wakeups?
- Which overload behaviors should be safe defaults for request-critical versus maintenance work?
