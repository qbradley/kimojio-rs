# Actor-Sharded Runtime

## Thesis

The strongest scalability direction for `kimojio-stack` is not transparent stack
migration, but an actor-sharded runtime: partition tenant or resource work into
worker-affine actors with bounded mailboxes, route requests by an explicit
placement table, and rebalance whole shards only at controlled mailbox and I/O
safe points. This keeps the current runtime's low-overhead local execution model
intact while allowing multi-tenant services to use many cores with predictable
costs, visible cross-worker traffic, and per-tenant isolation.

## Tenant/resource sharding model

The unit of placement should be a **shard actor**, not an individual coroutine.
A shard actor owns:

- a stable shard key;
- a bounded ingress mailbox;
- tenant/resource-local state;
- child stackful work created under an actor-owned scope;
- resource leases such as file descriptors, registered buffers, connection
  state, or cache entries;
- metrics for queue depth, run time, I/O pressure, and cancellation.

Shard keys should be explicit and application-visible:

```rust
enum ShardKey {
    Tenant(TenantId),
    TenantBucket { tenant: TenantId, bucket: u16 },
    Resource(ResourceId),
    Composite { tenant: TenantId, resource: ResourceId },
    System(SystemShard),
}
```

The default model is **tenant first, resource refined**:

1. Small tenants are packed into shared tenant buckets so thousands of quiet
   tenants do not create thousands of always-live workers.
2. Medium tenants get one or more tenant shards, usually placed by rendezvous
   hashing plus load-aware overrides.
3. Hot tenants can split by resource, connection group, key range, or operation
   class.
4. Resource-affine work, such as one TLS connection or one disk object family,
   may override tenant placement when moving it would be more expensive than a
   remote tenant message.

This is closer to NIC RSS, database partitioning, or Erlang-style process
placement than to a general work-stealing executor. The runtime should make the
shard boundary the place where locality, fairness, and migration rules are
declared.

## Placement and rebalancing mechanics

Use a small global **placement controller** as a coordinator, not as a scheduler.
Workers continue to run local schedulers, local ready queues, and preferably one
io_uring each. The controller owns a versioned placement table:

```text
ShardKey -> Placement { worker, epoch, policy, cost_class }
```

Routers cache placement handles for hot keys, but every handle carries an epoch.
Messages sent with a stale epoch are redirected, rejected with a retryable
`Moved` error, or forwarded by the old worker for a bounded grace period. The
normal local fast path should be: route cache hit, same worker, push to local
mailbox, no global lock.

Initial placement can use rendezvous hashing or power-of-two choices with
tenant weights. Rebalancing should be override-based rather than rebuilding a
global hash ring, so one hot shard can move without perturbing unrelated shards.

A controlled move should follow a protocol:

1. **Observe:** collect queue depth, run-budget usage, wake-to-run latency,
   in-flight I/O, memory, mailbox drops, and per-tenant budget debt.
2. **Select:** choose a shard whose predicted benefit exceeds a hysteresis
   threshold and whose cost class permits migration.
3. **Prepare target:** allocate mailbox, actor slot, metrics, resource tables,
   and optional warm caches on the destination worker.
4. **Freeze ingress:** bump the placement epoch and stop accepting new local
   messages except drain/forward messages.
5. **Quiesce:** let the actor reach a mailbox boundary; do not move live
   stackful coroutines.
6. **Drain or transfer:** move queued messages and actor state if the actor is
   `Send` and migration-safe, or call an application-provided snapshot/rebuild
   hook.
7. **Commit:** publish the new epoch and wake the destination worker.
8. **Retire source:** keep a short redirect stub, then remove it.

Migration must be rate-limited. A busy service should prefer a slightly uneven
layout over continuous reshuffling. Suggested controls:

- minimum shard residency time;
- max concurrent moves per worker;
- max bytes/messages moved per second;
- no migration while worker-local registered I/O is in flight;
- no migration for actors marked pinned or non-`Send`;
- backoff when a move is aborted.

Imaginative variants worth prototyping later:

- **Shadow read actors:** read-mostly tenant state can have worker-local cached
  readers with one writer shard.
- **Split-on-heat actors:** a tenant shard can temporarily create sub-shards for
  specific hot resources, then merge after cooling.
- **Budget lending:** idle tenants lend credits to hot tenants, but only within
  a capped debt window.
- **Mailbox spill lanes:** a hot actor can route low-priority messages to a
  colder lane or worker-owned batch processor without moving the actor itself.

The grounded recommendation is to start with pinned shard actors and explicit
move-at-mailbox-boundary rebalancing. Do not move suspended stacks.

## API/developer model

The developer should opt into sharding by defining the key, actor state, mailbox
type, and placement policy:

```rust
let runtime = ShardedRuntime::builder()
    .workers(worker_count)
    .placement(PlacementPolicy::TenantBuckets { buckets_per_tenant: 1 })
    .rebalance(RebalancePolicy::controlled())
    .build();

runtime.register_actor::<TenantActor>()
    .key_by(|msg| ShardKey::Tenant(msg.tenant_id()))
    .mailbox_capacity(1024)
    .placement(ActorPlacement::MovableAtMailboxBoundary)
    .cost_class(CostClass::LatencySensitive);
```

Actor handling should receive an actor context rather than a bare runtime
context:

```rust
trait Actor {
    type Message;

    fn handle(&mut self, cx: &mut ActorContext<'_>, msg: Self::Message);
}
```

`ActorContext` exposes the current worker, shard key, cancellation token,
mailbox budget, and local `RuntimeContext`. It should make costs visible:

- `cx.send_local(...)` requires same-worker affinity and fails otherwise.
- `cx.send(...)` may cross workers and returns a backpressure-aware result.
- `cx.call(...).with_deadline(...)` records request/reply cost and cancellation.
- `cx.spawn_child(...)` creates scoped local stackful work tied to the actor.
- `cx.spawn_chunky(...)` runs explicit CPU chunks with yield/cancel budgets.
- `cx.pin_resource(...)` records that a resource prevents migration.

The API should avoid pretending remote work is local. A route handle can cache a
worker and epoch, but remote sends, cross-worker calls, and migration hooks
should be named as such.

## Interaction with kimojio-stack scopes and channels

`kimojio-stack` scopes are a good fit for actor lifetime. Each actor owns a
long-lived parent scope, and every child coroutine spawned for that actor is
joined, canceled, or detached according to the actor's policy before the actor is
stopped or migrated. Scoped borrowing remains local: child coroutines may borrow
actor-owned state only while the actor is pinned to the current worker and the
scope guarantees quiescence.

Rebalancing must not move a scope containing live borrowed stack state. The safe
boundary is:

- no actor handler currently running;
- no child coroutine borrowing actor-local state;
- no local-only wait registration owned by the actor;
- no worker-owned I/O operation that must complete on the source worker.

Local `kimojio-stack` channels should remain local and cheap. Inter-actor
communication should choose among:

1. local bounded channels for same-worker actor internals;
2. actor mailboxes for placement-aware shard messages;
3. `channel::cross_thread` endpoints for explicit OS-thread, runtime, or
   Tokio-compatible interop.

The runtime should not silently upgrade local channels, locks, or waiters to
cross-worker primitives. If a message crosses workers, it should travel through
a bounded mailbox or explicit cross-thread channel so queueing, wakeup, and
memory costs remain observable.

## Handling TLS, disk, and chunky work

TLS connections are resource-affine. A TLS actor should own the connection fd,
TLS state, read/write buffers, and handshake progress. Initial accept can assign
connections by tenant affinity, source hash, or current worker pressure. Once an
fd or buffer is registered with a worker's io_uring, the connection should be
pinned until the registration is retired or the connection is drained. Moving a
live TLS session by copying protocol state across workers is possible in theory
but should not be the default path.

Disk work is also affinity-sensitive. Fixed-file and fixed-buffer registrations
are naturally ring-affine in a one-ring-per-worker design. A disk-backed tenant
actor should therefore own explicit fd leases and in-flight operation counts.
Migration is allowed only when those counts reach zero, or when the application
uses a rebuild hook that reopens/re-registers resources on the destination.
Avoid lazy first-use registration on a hot path; it hides latency spikes inside
ordinary message handling.

Chunky CPU work should not monopolize a worker. Examples include compression,
JSON/protobuf transforms, checksumming, crypto bursts, and large scans. Use one
of three explicit modes:

- **cooperative chunks:** the actor processes bounded slices and yields between
  slices;
- **stealable chunks:** `Send` work items with no worker-local handles may run
  on a compute queue and report back by message;
- **blocking pool:** unavoidable blocking or CPU-heavy operations run outside
  worker schedulers with bounded concurrency and cancellation/reporting hooks.

The actor remains the owner of ordering and tenant budget. Chunk processors are
helpers, not hidden places where tenant isolation disappears.

## Fairness, isolation, hot-tenant mitigation

Fairness should be budgeted at the shard level. Each worker should schedule
actors through a local fair queue rather than draining one mailbox indefinitely.
Useful controls:

- max messages or time slice per actor turn;
- per-tenant credits replenished over time;
- per-tenant limits for mailbox bytes, in-flight I/O, spawned children, and
  chunky work;
- priority lanes for control/cancellation over bulk data;
- aging so quiet tenants are not stuck behind hot tenants;
- overload policies: reject, shed low priority, retry later, or split shard.

Isolation should be hierarchical:

```text
worker budget
  tenant budget
    shard mailbox budget
      operation budget
```

A hot tenant can be mitigated without hurting others by:

1. splitting it into resource or key-range shards;
2. moving one sub-shard to a cooler worker;
3. reducing its borrowed credits;
4. pushing chunky work into explicit compute lanes;
5. applying admission control at its mailbox;
6. isolating it on dedicated workers if it remains hot.

The scheduler can be work-conserving inside these guardrails: idle capacity may
be borrowed, but the debt must be visible and bounded.

## Costs and hidden-cost avoidance

This design is attractive only if it preserves `kimojio-stack`'s explicit cost
model. Costs to expose:

- local mailbox enqueue/dequeue;
- remote message send and external wake;
- route-cache miss;
- placement-table epoch miss;
- actor migration prepare/drain/commit;
- bytes and messages transferred during migration;
- time blocked by in-flight I/O before migration;
- mailbox memory reserved per shard;
- per-tenant budget debt.

Costs to avoid hiding:

- no transparent stack migration;
- no global queue on every message;
- no ambient `Send` requirement for local actor internals;
- no automatic conversion of local primitives into atomic cross-worker
  primitives;
- no lazy resource registration in latency-sensitive handlers;
- no unbounded mailboxes;
- no rebalance loop that moves work faster than metrics can stabilize.

A good implementation should make the local path nearly identical to today's
single-threaded runtime plus a mailbox pop, and make every cross-worker path
visible in type names, metrics, or both.

## Failure modes and cancellation

Actor failure should be handled by a supervisor policy attached to the shard:

- restart with empty state;
- restart from snapshot;
- fail the tenant/resource and dead-letter pending messages;
- escalate to worker/runtime shutdown.

Panics should cancel the actor scope, reclaim child task state, and complete or
cancel pending calls with a structured error. A panic must not leave a placement
entry pointing at an actor that no longer exists.

Rebalancing failure should be ordinary, not exceptional. Prepare can fail due to
memory pressure; quiesce can time out; a route epoch can race; target workers can
become overloaded. The move protocol should support abort before commit and a
short redirect/rollback window after commit. Messages should carry request IDs
when callers need deduplication across retry after `Moved` or timeout.

Cancellation should be hierarchical:

1. cancel individual requests by call handle or deadline;
2. cancel chunky child work through an actor token;
3. cancel actor scope on stop, migration timeout, or supervisor restart;
4. cancel tenant placement during drain/shutdown;
5. cancel worker runtime during process shutdown.

I/O cancellation follows the existing runtime direction: dropping interest
requests cancellation, but operation state owns kernel-visible resources until
the CQE is reaped. Migration must wait for that terminal state unless the
resource was explicitly designed for transfer.

## Prototype plan and benchmark plan

Prototype in phases:

1. **Metrics first:** add a harness around N independent local runtimes and
   collect per-worker queue depth, wake-to-run latency, mailbox depth, and
   cross-worker wake counts.
2. **Pinned actor router:** implement versioned placement and bounded mailboxes
   with no migration. Route by tenant/resource key.
3. **Local fairness:** schedule actors with per-shard message/time budgets and
   per-tenant credit accounting.
4. **Controlled migration:** support `Send` actor state moved only at mailbox
   boundaries with no in-flight worker-owned I/O.
5. **Resource policies:** add explicit pinned TLS/disk actors and migration
   refusal reasons.
6. **Hot-tenant splitting:** allow a tenant actor to create resource/key-range
   sub-shards and update routing.
7. **Chunky work lane:** add cooperative and stealable chunk APIs with visible
   budgets.

Benchmark from 1 core through hundreds of cores where available:

- local actor message round trip;
- remote actor message round trip;
- route-cache hit/miss cost;
- cross-worker wake latency;
- p50/p99/p99.9 wake-to-handler latency under mixed tenant load;
- fairness under one hot tenant plus many quiet tenants;
- hot-tenant split and move convergence time;
- migration pause time and message loss/duplication behavior;
- memory per actor, mailbox, and pending call;
- TLS echo with tenant-affine connections;
- disk read/write mix with registered resources;
- chunky CPU workload with and without chunk lanes;
- cancellation storm during rebalancing;
- comparison with one single-thread runtime per core and with naive
  round-robin placement.

Success should be defined by tail latency and predictability, not only
throughput. A rebalance that improves average throughput but creates large p99.9
spikes is a failed design.

## Pros / Cons

Pros:

- Preserves local scheduler and scoped-stack simplicity.
- Scales multi-tenant services across cores without transparent stack
  migration.
- Makes fairness and isolation first-class.
- Keeps TLS, disk, and registered I/O affinity explicit.
- Allows hot tenants to split or move while quiet tenants stay stable.
- Gives developers visible cost boundaries for local, remote, and migration
  paths.

Cons:

- Requires developers to choose good shard keys and placement policies.
- Rebalancing is less automatic than work stealing.
- Non-`Send` or heavily resource-affine actors may be pinned forever.
- Hot tenants with one indivisible resource can still bottleneck on one worker.
- More runtime concepts: actors, mailboxes, placement epochs, supervisors, and
  budgets.
- Cross-shard request/reply flows are more complex than shared-state calls.
- Poor instrumentation would make placement bugs hard to diagnose.

## Open questions

- Should this live in `kimojio-stack`, a `kimojio-stack-sharded` crate, or a
  higher-level service framework?
- What is the minimum actor API that composes with today's `RuntimeContext` and
  `Scope` without imposing `Send` on local code?
- Should migratable actor state require `Send`, an application snapshot hook, or
  both?
- Which delivery semantics should actor mailboxes promise: at-most-once,
  at-least-once with request IDs, or configurable?
- How should placement state be observed and debugged in production?
- What default fairness algorithm is simple enough to trust but strong enough
  for hostile tenant skew?
- Can TLS sessions be transferred safely enough to be worth supporting, or
  should live connections always be pinned?
- Should disk-heavy tenants be placed by tenant, by file/resource, or by storage
  device/NUMA locality?
- What acceptance thresholds define a successful rebalance at p99.9 latency?
- How much hot-tenant splitting can be automatic before it becomes another
  hidden-cost scheduler?
