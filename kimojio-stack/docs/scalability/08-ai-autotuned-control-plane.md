# AI-Assisted Autotuned Control Plane

## One-paragraph thesis

`kimojio-stack` should not put AI in the scheduler hot path, but it can use telemetry, simulation, and recommendation systems to make thread-per-core configuration much smarter: the runtime remains explicit, local-first, and predictable, while an optional control plane learns tenant shapes, tests scheduling and topology alternatives offline, and emits bounded, explainable plans for worker pinning, sharding, offload lanes, fairness budgets, and opt-in stealing. The imaginative version is a self-optimizing runtime fleet with workload digital twins and scheduling copilots; the grounded version is a low-overhead telemetry contract plus a recommend-only tuner that can graduate to tightly scoped auto-apply for reversible knobs.

## Wild idea version

Imagine every production cluster has a continuously updated "runtime twin": a compact simulation model of each service, tenant class, worker topology, I/O ring, queue, shard, and offload lane. The live runtime records only fixed-budget facts, but the control plane enriches those facts offline with benchmark results, deployment history, incident timelines, kernel/platform metadata, and operator annotations. An AI planner then proposes concrete runtime manifests:

```text
Service api-gateway, 96 cores, 4 NUMA nodes
  workers: 72 pinned I/O workers, 12 crypto lanes, 8 blocking lanes, 4 spare
  tenant A: dedicated shard set on NUMA 0, no stealing, p99 budget 2 ms
  tenant B: burstable weighted pool across NUMA 1-2, idle-only stealable tasks
  tenant C: background class, offload compression, capped ready budget
  ring depth: 512 for edge workers, 2048 for storage-heavy workers
  rebalance: move shard 17 only if queue-depth debt persists for 90 s
```

The planner would behave like a flight computer rather than a replacement pilot. It could:

- infer tenant archetypes such as latency-sensitive RPC, streaming fan-out, bursty batch, storage-bound, lock-contention-heavy, or noisy-neighbor-prone;
- forecast what happens if a hot tenant receives dedicated cores, smaller ready batches, larger rings, or stricter admission limits;
- generate "what-if" explanations for operators: "This recommendation reduces tenant B p99.9 by moving offload-heavy tasks away from worker 18, at the cost of 6% lower best-effort throughput";
- search unusual layouts that humans rarely try, such as asymmetric workers, cold spare cores, NUMA-local tenant islands, separate CQE-first workers, or deliberately underfilled shards to preserve tail latency;
- learn from failed recommendations and refuse to repeat them on similar deployments.

In the wildest form, the control plane becomes a scheduling research lab attached to the fleet. It runs millions of simulated replays, synthesizes adversarial tenant mixes, and ships ranked control manifests with confidence intervals. The runtime still executes a plain manifest; the "AI" never gets to make unbounded per-task decisions.

## Practical grounded version

The practical design is an optional package around three artifacts:

1. **Telemetry contract**: per-worker and per-tenant counters, histograms, and bounded event samples that the runtime can expose with a declared overhead budget.
2. **Tuning manifest**: a versioned, diffable file that declares worker topology, tenant classes, queue budgets, ring settings, shard placement, offload lane sizing, and stealing policy.
3. **Recommendation engine**: an offline-first tool that consumes telemetry and benchmark traces, then emits a manifest diff with explanation, confidence, expected cost, and rollback criteria.

The default runtime behavior remains static and explicit. Autotuning starts in `RecommendOnly` mode. Operators can inspect, canary, and apply suggestions. A later `GuardedAutoApply` mode may change only reversible, bounded knobs such as ready budgets, offload-lane concurrency caps, admission thresholds, or steal-attempt intervals. It should not silently migrate pinned I/O ownership, change task `Send` requirements, resize stacks, alter tenant isolation tier, or enable cross-worker behavior.

The MVP does not need an LLM in the production path. It can start with rules, queueing models, and search. AI assistance is most valuable for summarizing telemetry, classifying workloads, generating hypotheses, and exploring a large configuration space offline.

## Telemetry inputs and overhead budget

Telemetry must be designed like a runtime feature, not an observability afterthought. Every input needs cardinality bounds, sampling rules, and a cost estimate.

### Required hot-path-safe inputs

- Per-worker counters:
  - scheduler ticks,
  - ready queue depth snapshots,
  - local wake count,
  - remote wake count,
  - tasks run,
  - yields,
  - parks,
  - steal attempts and successes,
  - ring submissions,
  - CQEs reaped,
  - offload submissions,
  - time spent in scheduler phases.
- Per-worker histograms:
  - wake-to-resume latency,
  - CQE-to-resume latency,
  - submit-to-CQE latency,
  - task run duration between yields,
  - remote inbox drain delay.
- Per-tenant counters, bounded by configured tenant slots:
  - admitted requests,
  - completed requests,
  - rejected/deferred requests,
  - CPU budget consumed,
  - queue debt,
  - offload bytes/items,
  - fairness throttles,
  - SLO violations.
- Static descriptors:
  - worker id,
  - core id,
  - NUMA node,
  - ring depth,
  - shard id,
  - lane type,
  - tenant class,
  - runtime version,
  - manifest version.

### Optional sampled inputs

- Bounded event samples for unusually slow wake-to-run or CQE-to-run paths.
- Short trace windows activated by explicit operator command or threshold breach.
- Tenant transition samples showing when a tenant shifts from latency-sensitive to bursty or storage-bound.
- Cross-worker wake path samples for diagnosing accidental remote amplification.

### Overhead budget

Initial budget target:

- **CPU**: less than 1% steady-state overhead with default telemetry enabled; less than 3% during bounded diagnostic windows.
- **Allocations**: zero allocation on scheduler hot paths after startup.
- **Memory**: fixed per-worker telemetry pages plus fixed per-tenant slots; no unbounded label maps.
- **Cardinality**: tenant ids must map to predeclared slots or an explicit "overflow/unknown" bucket.
- **Export cadence**: snapshots at 1 Hz by default; high-frequency traces only by explicit window.
- **Backpressure**: telemetry drops samples instead of slowing worker loops.

Cost should be configured directly:

```text
telemetry:
  mode: counters-and-histograms
  max_tenants: 4096
  per_worker_event_ring: 65536
  export_interval_ms: 1000
  max_cpu_overhead_percent: 1.0
  diagnostic_windows:
    enabled: operator-only
    max_duration_seconds: 60
```

## Offline simulation/tuning, online recommendation loop, and safety rails

### Offline simulation and tuning

Offline tuning is the safest place for imagination. It should:

1. ingest benchmark traces and production telemetry snapshots;
2. reconstruct per-worker queue pressure, I/O latency, tenant demand, and cross-worker traffic;
3. replay workloads against alternative manifests;
4. search over bounded parameters:
   - worker count,
   - core pinning,
   - NUMA placement,
   - shard count,
   - shard-to-worker assignment,
   - offload lane count,
   - ring depth,
   - ready/I/O/remote budgets,
   - stealing thresholds,
   - tenant weights and caps;
5. produce ranked recommendations with predicted p50/p99/p99.9, throughput, CPU cost, memory cost, and fairness impact.

The simulator can begin approximate. Even a coarse model is useful if it catches obvious mistakes: a shard pinned to a saturated worker, too many offload jobs competing with I/O workers, remote wake storms, or an aggressive stealing threshold that improves average throughput while hurting p99.9.

### Online recommendation loop

The online loop should be deliberately slow:

```text
runtime telemetry -> local exporter -> tuning service -> manifest diff
    -> explanation -> operator/canary -> apply -> monitor -> keep or rollback
```

Recommended control periods should be seconds to minutes, not microseconds. The loop should optimize stable topology and policy, not individual scheduling decisions.

Recommendation states:

- `Shadow`: compute advice but never surface operational actions.
- `RecommendOnly`: surface manifest diffs with rationale.
- `Canary`: apply to a small worker set, tenant subset, or process replica.
- `GuardedAutoApply`: apply only allow-listed reversible knobs.
- `Static`: freeze recommendations and run the last known-good manifest.

### Safety rails

- **No AI in the hot path**: runtime workers execute static policies.
- **Signed/versioned manifests**: every applied plan is auditable and rollbackable.
- **Constraint solver before apply**: reject plans that violate tenant reservations, NUMA constraints, core limits, memory budgets, or operator pinning.
- **Hysteresis**: require sustained evidence before changing topology.
- **Rate limits**: cap how often each knob can change.
- **Canary first**: topology changes should prove themselves on a bounded scope.
- **Automatic rollback**: revert if SLO, error rate, queue debt, CPU, memory, or remote wake amplification crosses thresholds.
- **Explainability requirement**: every recommendation must include "because", "expected benefit", "cost", "risk", and "rollback trigger".

## API/developer/operator model

### Developer model

Developers should classify work explicitly:

```rust
runtime.spawn_pinned(worker_id, tenant_id, |cx| {
    // Latency-sensitive, worker-affine I/O path.
});

runtime.spawn_offload(OffloadLane::Crypto, tenant_id, item);

runtime.spawn_stealable(TaskClass::Background, tenant_id, |cx| {
    // Requires Send and migration-safe runtime APIs.
});
```

Task metadata should be cheap and bounded:

- `tenant_id`,
- `task_class`,
- `shard_id`,
- `latency_budget`,
- `offload_lane`,
- `migration_allowed`,
- `fairness_group`.

The runtime should not infer dangerous semantics from opaque labels. If a task is pinned, it stays pinned. If it is non-`Send`, it is never recommended for stealing. If a tenant is reserved, recommendations cannot spend its cores on burstable work without an explicit policy change.

### Operator model

Operators interact with manifests and recommendations:

```text
kimojio tune capture --service api --window 10m
kimojio tune simulate --trace capture-2026-06-11 --candidates topology/*.yaml
kimojio tune recommend --service api --mode recommend-only
kimojio tune explain recommendation-42
kimojio tune canary recommendation-42 --replicas 2 --duration 15m
kimojio tune apply recommendation-42
kimojio tune rollback --to manifest-2026-06-10
```

Every output should be diffable:

```diff
- worker 18: io, shard [17, 22], ready_budget 128
+ worker 18: io, shard [22], ready_budget 64
+ worker 73: io, shard [17], ready_budget 64

reason: shard 17 accounts for 42% of tenant B queue debt on worker 18
expected: tenant B p99.9 -18%, worker 73 utilization +31%
risk: remote wake rate may increase by 4-7%
rollback: p99.9 > baseline +5% for 3 consecutive minutes
```

### Control-plane API shape

The runtime should expose a narrow control surface:

- load a manifest at startup;
- expose telemetry snapshots;
- validate a candidate manifest;
- apply allow-listed live updates;
- reject non-live-safe updates with a clear reason;
- report current manifest version and rollback target.

This avoids a magical "scheduler API" where an external process can mutate arbitrary internals.

## Interaction with runtime strategies

### Pinned cores

The control plane can recommend core maps, worker counts, and NUMA placement, but pinning remains explicit. It should never silently move a pinned task. For high-performance services, the common recommendation should be tenant- or shard-affine workers with one local scheduler and one local I/O path per worker.

Useful recommendations:

- dedicate cores to latency-sensitive tenants;
- reserve spare cores for burst absorption;
- isolate noisy offload lanes from I/O workers;
- keep tenant shards NUMA-local to their memory and I/O devices;
- reduce worker count when fewer hotter workers produce better cache locality than many underfilled workers.

### Offload lanes

Offload lanes should be first-class and visible. The tuner can size lanes for crypto, compression, blocking filesystem calls, DNS, logging, or CPU-heavy parsing. Recommendations should include queue caps and admission policy so that offload work cannot silently starve pinned I/O workers.

The important rule is that offload lanes are not "free background capacity." Their cost is cores, memory, queues, and latency. The manifest must show those costs.

### Sharding

Sharding is the most promising control point. The tuner can recommend:

- tenant-to-shard placement;
- shard splitting for hot tenants;
- shard merging for cold tenants;
- consistent-hash weight changes;
- planned rebalance windows;
- per-shard queue budgets.

Shard moves should be coarse and rare. The control plane may identify the move, but the application should decide whether state, connection ownership, and storage affinity make the move safe.

### Work stealing

Work stealing should remain opt-in and constrained. The tuner can suggest:

- whether stealable queues are worth enabling for a task class;
- idle-only thresholds;
- victim selection limits;
- steal batch size;
- cooldown after failed steals;
- tenants or task classes excluded from stealing.

It should not recommend stealing pinned I/O tasks, tasks with worker-local resources, or non-`Send` stackful tasks. A recommendation that enables stealing must quantify remote wake cost and p99.9 impact, not just average throughput.

## Fairness and multi-tenant policy

Tenant policy should be declarative:

```text
tenant_classes:
  reserved-low-latency:
    min_cores: 8
    max_remote_wake_rate: low
    stealable: false
    p99_budget_ms: 2

  burstable:
    min_share: 10%
    max_share: 60%
    borrow_idle_capacity: true
    throttle_on_debt: true

  background:
    max_share: 20%
    offload_only_when_idle: true
    preempt_for_reserved: true
```

The classifier can suggest that a tenant has changed class, but the policy change should be explicit. For example, the system can say "tenant X behaves like bursty batch and is causing queue debt for reserved tenants"; it should not silently downgrade tenant X unless the operator has enabled that exact transition.

Fairness mechanisms:

- per-tenant admission gates;
- weighted queue budgets;
- latency debt accounting;
- maximum remote wake contribution;
- per-tenant offload lane caps;
- noisy-neighbor detection;
- reserved capacity that cannot be borrowed unless policy allows it;
- overload behavior that is explicit: reject, defer, shed background work, or violate best-effort first.

The control plane should optimize for declared policy, not global throughput alone.

## Failure modes and rollback/safety

Potential failure modes:

- **Bad recommendation**: simulator model misses real cache, kernel, NUMA, or application behavior.
- **Telemetry drift**: counters are incomplete, delayed, or incorrectly attributed.
- **Feedback loops**: a change alters workload behavior, causing oscillating recommendations.
- **Tenant misclassification**: a bursty tenant is treated as latency-sensitive or vice versa.
- **Cardinality explosion**: too many tenant labels or task classes overwhelm telemetry.
- **Remote wake amplification**: a topology change increases cross-worker traffic.
- **Offload starvation**: lanes fill and backpressure leaks into pinned workers.
- **Stealing regressions**: idle-only stealing improves utilization but hurts cache locality.
- **Rollback mismatch**: stateful shard moves cannot be instantly reverted.
- **Operator overtrust**: recommendations appear authoritative without enough confidence.

Safety responses:

- default to static manifests;
- require confidence and evidence thresholds;
- keep a last-known-good manifest in every process;
- separate live-safe knobs from restart-required knobs;
- validate manifests before apply;
- canary by replica, tenant, or worker subset;
- stop changing when telemetry becomes unhealthy;
- prefer degrading to known static policy over chasing a moving optimum;
- make rollback criteria part of the recommendation, not an afterthought.

## Prototype plan and benchmark plan

### Prototype plan

1. **Manifest schema**
   - Define a static topology and policy manifest for workers, shards, tenants, lanes, budgets, and stealing.
   - Add validation rules and a clear distinction between startup-only and live-update fields.

2. **Telemetry skeleton**
   - Add fixed-size counters and histograms for worker queues, wake-to-resume, CQE-to-resume, remote wakes, and per-tenant queue debt.
   - Export snapshots without adding hot-path allocation.

3. **Trace capture and replay**
   - Capture bounded windows of aggregated runtime behavior.
   - Build a replay tool that can compare candidate manifests offline.

4. **Heuristic recommender**
   - Start without AI: flag saturated workers, hot shards, remote wake amplification, overfull offload lanes, and fairness debt.
   - Emit manifest diffs with explanations.

5. **AI-assisted analysis**
   - Use AI outside production to summarize traces, cluster tenant behavior, propose candidate manifests, and generate operator explanations.
   - Require deterministic validation before any candidate can be applied.

6. **Recommend-only integration**
   - Surface recommendations in CLI/operator tooling.
   - Do not apply automatically.

7. **Guarded auto-apply experiment**
   - Allow only reversible budget changes under strict constraints.
   - Keep topology and tenant class changes manual.

### Benchmark plan

Benchmarks should compare static expert configuration, naive defaults, heuristic recommendations, and AI-assisted recommendations.

Measure:

- throughput per core;
- p50/p95/p99/p99.9/max latency;
- wake-to-resume and CQE-to-resume latency;
- remote wake rate;
- worker utilization imbalance;
- queue debt by tenant;
- offload lane saturation;
- memory per worker and per tenant slot;
- telemetry overhead with disabled/default/diagnostic modes;
- recommendation stability over time;
- rollback time and correctness.

Workloads:

- single low-latency tenant from 1 to N cores;
- mixed reserved/burstable/background tenants;
- hot-shard imbalance;
- storage-bound I/O with per-worker rings;
- CPU-heavy offload pressure;
- remote wake storm;
- NUMA-local versus NUMA-crossing placement;
- failure injection: delayed telemetry, wrong tenant labels, core loss, offload lane saturation.

Success criteria for MVP:

- default telemetry overhead stays under the declared budget;
- recommendations are reproducible from captured inputs;
- manifest diffs are explainable and rollbackable;
- offline replay predicts directionally correct outcomes for benchmark workloads;
- no runtime behavior changes unless a manifest explicitly enables them.

## Pros / Cons

### Pros

- Preserves explicit runtime control while making configuration smarter.
- Uses AI where it is safer: offline analysis, workload classification, explanation, and candidate search.
- Turns tuning knowledge into versioned artifacts instead of tribal memory.
- Helps operators reason about hundreds of cores, tenants, lanes, and shards.
- Can improve tail latency by finding topology and fairness issues humans miss.
- Encourages first-class telemetry for scheduler latency and cross-worker costs.
- Provides a path from manual recommendations to limited auto-apply without hiding costs.

### Cons

- Adds a control-plane surface area that must be secured, tested, and documented.
- Telemetry itself can perturb the runtime if budgets are not enforced.
- Simulators can be confidently wrong.
- Operators may over-trust recommendations with weak evidence.
- Tenant classification can encode policy mistakes if not reviewed.
- Manifest design may become complex before the runtime has enough production data.
- Auto-apply, even guarded, creates new failure modes and rollback requirements.
- AI-assisted tooling may distract from simpler static tuning until the baseline is strong.

## Open questions

- What is the smallest telemetry set that predicts useful tuning actions without exceeding a 1% CPU budget?
- Should the manifest format live in `kimojio-stack`, a future multithreaded crate, or an external operations tool?
- Which fields are safe to update live, and which require process restart or shard drain?
- How should tenant ids be represented so telemetry stays bounded while operators can still debug real tenants?
- Can offline replay model stackful scheduler behavior accurately enough to predict p99.9 direction?
- What tenant classification vocabulary is useful but not overly prescriptive?
- Should AI-generated recommendations require human approval forever, or can some reversible knobs become auto-applied?
- How should recommendation confidence be calibrated and displayed?
- What is the right integration point with OpenTelemetry without importing its cardinality and allocation costs into the hot path?
- How much topology control belongs in the runtime versus the application or deployment orchestrator?
