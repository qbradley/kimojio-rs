# Pinned-Core Routing Framework

The proposed direction is to keep the `kimojio-stack` runtime as a set of small, sovereign, per-core stackful runtimes and solve scalability in the application framework above them: route accepts, connections, requests, disk jobs, and bulk work at explicit ownership boundaries instead of migrating live coroutines or hiding a work-stealing executor under the API. A service scales by choosing where new work enters, admitting only work with declared resource costs, and handing off only owned, movable envelopes; once a coroutine, socket/TLS state, registered buffer, or io_uring operation belongs to a core, the default is that it stays there until the next explicit boundary.

## Problems it solves and does not solve

This solves the central tension between pinned low-latency execution and multi-core load balance. It gives an application server a way to:

- spread new connections without a global accept lock;
- shed work before allocating stacks, body buffers, or TLS state;
- route requests by tenant, method, shard, cost class, or current per-core pressure;
- isolate latency-sensitive traffic from chunky throughput-dominated work;
- keep the hot path local: local ready queues, local waiters, local io_uring completions, local TLS state, and local connection state;
- make every cross-core cost visible as a bounded handoff, not a surprise wakeup;
- scale from one runtime on one core to hundreds of runtime instances on one VM with the same mental model.

It does not solve arbitrary application bottlenecks. A single global lock, a non-yielding CPU loop, a tenant with unbounded demand, a kernel/NIC receive queue bottleneck, or a shared database shard can still dominate. It also does not promise live migration of a suspended stackful coroutine, moving in-flight io_uring operations, or transparently balancing already-established HTTP/1.1 connections. Long-lived connections and streams need protocol-level boundaries such as request completion, HTTP/2 stream completion, flow-control pauses, graceful drain, GOAWAY, reconnect, or application-defined relocation points.

## Runtime/application architecture

The framework should be a fleet of local runtimes, not a multithreaded executor:

```text
PinnedFleet
  Control plane:
    core topology, tenant table, route policies, counters, health, reconfiguration

  Worker 0 on CPU 0:
    LocalRuntime + io_uring + timers + accept socket(s) + handoff inbox + metrics
  Worker 1 on CPU 1:
    LocalRuntime + io_uring + timers + accept socket(s) + handoff inbox + metrics
  ...
```

Each worker is still recognizably `kimojio-stack`: one cooperative scheduler, one primary ready queue, one ring, worker-affine registered resources, and no ambient cross-thread synchronization on local waits. The control plane samples counters and changes routing weights, but it does not run tasks and is not a scheduler.

The key abstraction is an explicit ownership boundary. Candidate boundary types:

- `AcceptedFd`: a raw accepted socket before TLS, protocol parsing, or app state.
- `ConnectionOwner`: a worker that owns socket I/O, TLS state, read buffers, write queues, and protocol connection state.
- `RequestEnvelope`: parsed method/path/metadata plus either a bounded in-memory body or a streaming body token with explicit credit.
- `BulkJob`: owned buffers and a response sink, suitable for throughput work.
- `DiskJob`: file/offset/buffer ownership plus completion target.
- `ResponseEnvelope`: an owned response or ordered response-fragment stream back to the connection owner.

The preferred architecture is three planes:

1. **Ingress routing.** Listener sockets and accept loops choose the initial connection owner.
2. **Request routing.** A connection owner may keep cheap requests local or hand off request envelopes to another worker/lane.
3. **Resource routing.** Disk, crypto, compression, indexing, and other expensive operations may use explicit lanes with bounded queues and declared budgets.

Workers can be grouped into lanes: `Latency`, `Bulk`, `Disk`, `Handshake`, or tenant-specific shards. A lane is just a routing target set; it should not imply hidden thread pools or automatic migration.

## API sketch and developer model

The developer model should ask the service author to make three decisions: what can move, what it costs, and what happens when capacity is unavailable.

```rust
let fleet = PinnedFleet::builder()
    .cores(CoreSet::online())
    .pin_threads(true)
    .rings(IoRings::PerWorker)
    .listen(addr, ListenPolicy::ReusePort {
        backlog: 8192,
        rebalance: Rebalance::WeightedAccept,
    })
    .tenant(TenantPolicy::from_header("x-tenant-id"))
    .lane("latency", LanePolicy::low_latency().cores("0-31"))
    .lane("bulk", LanePolicy::throughput().cores("32-63"))
    .admission(|a| {
        a.max_connections_per_tenant(20_000)
            .max_stack_bytes_per_core(256 << 20)
            .max_request_body_bytes(1 << 20)
            .max_handoff_queue(4096)
    })
    .build()?;
```

Connection handling should make affinity explicit:

```rust
fleet.serve(|cx, conn| {
    let tenant = conn.tenant();

    conn.route_requests(|req| {
        match req.route_key() {
            RouteKey::Path("/healthz") => RouteTarget::Local,
            RouteKey::Path("/bulk") => RouteTarget::lane("bulk")
                .cost(Cost::cpu_micros(500).bytes(req.body_len())),
            _ => RouteTarget::tenant_hash("latency", tenant)
                .cost(Cost::latency_request()),
        }
    });
});
```

Handoff should be a first-class operation, not an implicit spawn:

```rust
let receipt = cx.handoff()
    .to(target)
    .class(WorkClass::Bulk)
    .cost(Cost::new().stack(64 << 10).body_bytes(body.len()))
    .on_full(HandoffFull::Reject(StatusCode::SERVICE_UNAVAILABLE))
    .send(RequestEnvelope::owned(req, body))?;
```

The default path remains easy: accept, TLS, parse, serve locally. The scalable path is still straightforward, but it forces the author to choose a boundary and a backpressure policy. Convenience helpers can exist, but they should desugar to visible route/admit/handoff steps.

## Scheduling/load-balancing mechanics

Load balancing should happen before work becomes pinned, or at request boundaries after a movable envelope has been created.

### Accept routing

Start with one listener per worker using `SO_REUSEPORT`. On small systems, Linux's default hash distribution may be enough. On larger systems, add an optional `SO_ATTACH_REUSEPORT_EBPF` program or userspace weight updates so overloaded workers receive fewer new connections. A worker that accepts a connection but should not own it may hand off the raw fd before TLS or protocol state is created.

Routing policy should use a bounded, cheap load vector:

```text
score = ready_depth_weight
      + wake_latency_weight
      + inflight_io_weight
      + connection_weight
      + tenant_debt_weight
      + lane_specific_pressure
```

Use power-of-two choices within a lane or tenant shard instead of scanning every core. Apply hysteresis so connections do not bounce due to noisy metrics. At hundreds of cores, route hierarchically: pick NUMA node, lane, then worker.

### Request routing

HTTP/1.1 keep-alive is mostly connection-affine: parse the next request on the connection owner, then optionally move an owned request body to a target worker. HTTP/2 and gRPC can be more interesting: the connection owner keeps socket/TLS/HPACK/flow-control state, while individual streams can be represented as request envelopes with explicit body credit and response sinks. That allows stream-level balancing without moving the connection's I/O state.

Long-running streams should declare whether they are local-only, lane-routed, or relocatable only at message boundaries. If a stream's target is overloaded, the connection owner can stop extending receive credit, send reset/resource-exhausted, or drain and issue GOAWAY.

### Worker loop

Each worker should remain local-first:

1. reap bounded local CQEs;
2. run bounded local ready tasks;
3. drain bounded handoff inbox batches;
4. update local load counters;
5. if idle, block in the worker's own io_uring or park mechanism.

There is no global ready queue. Idle workers become more attractive to routers; they do not steal live pinned work. An optional shared "routable backlog" may exist only for envelopes that have not yet been assigned a worker, and it must be bounded, metered, and outside the local scheduler hot path.

## TLS, disk, and I/O handling

Network I/O is owned by the connection worker. The worker owns the socket fd, receive buffers, write queue, TLS state, and in-flight io_uring operations. Moving a connection is allowed only when the runtime can prove there are no in-flight operations and the moved type explicitly supports it; the practical default is to move only raw fds before TLS, or drain and reconnect later.

TLS should therefore be handled in three modes:

- **Local TLS:** default and lowest latency. Handshake, decrypt, application I/O, encrypt, and write all happen on the connection owner.
- **Routed-before-TLS:** accept routing moves `AcceptedFd` to the chosen owner before creating TLS state. This is the primary balancing mechanism for TLS servers.
- **Explicit crypto/bulk lane:** large record processing, certificate validation, or expensive handshake-adjacent work may be sent as owned `CryptoJob`s, but this should be opt-in because it adds copies, ordering constraints, and cross-core wakeups.

Disk I/O should use worker-owned rings by default. Registered files and buffers are worker-affine unless the application explicitly replicates registrations across a lane. For data-local services, route disk jobs by file shard, extent, object partition, or tenant storage shard. For request-local services, use the connection/request worker's ring and account disk bytes against that worker and tenant.

Disk backpressure needs separate tokens for operation count and bytes in flight. A slow disk lane must not silently fill request stacks or response buffers. Completion returns through an explicit response sink or waiter, and the cost is counted as a handoff when it crosses workers.

## Fairness and multi-tenancy story

Fairness belongs at admission and routing boundaries, not as hidden preemption inside the stackful scheduler. Each tenant should have counters for:

- accepted connections;
- active requests/streams;
- stack bytes reserved;
- request/response buffer bytes;
- handoff queue slots;
- io_uring operations and disk bytes in flight;
- CPU/yield debt or service time estimate;
- recent errors and shed decisions.

Use weighted deficit round robin or token buckets at route points. A tenant with many connections on many cores should share one global debt ledger with per-core cached tokens, so it cannot bypass limits by spreading across workers. Low-latency lanes should have strict queue-depth and wake-latency guards; bulk lanes can trade latency for throughput by accepting deeper queues and larger batches.

When admission fails, fail early and visibly: close before TLS, return HTTP 503 with `Retry-After`, send gRPC `RESOURCE_EXHAUSTED`, refuse stream credit, or shed the lowest-priority bulk queue. Do not allocate a stack and then discover there is no memory, body capacity, or disk budget.

## Costs, overheads, and hidden-cost avoidance

The framework should make these costs observable in APIs, logs, and metrics:

- cross-core handoff count and latency;
- bytes copied or transferred by ownership;
- queue capacity and enqueue failures;
- eventfd or remote wake count;
- per-worker wake-to-resume latency;
- CQE-to-resume latency;
- per-tenant accepted/rejected work;
- lane-specific queue depths and service time.

Hidden-cost avoidance rules:

1. No live coroutine migration.
2. No implicit upgrade from local primitives to atomics or locks.
3. No unbounded framework queues.
4. No lazy fixed-buffer or fixed-file registration on latency-critical paths.
5. No background readers that buffer unlimited request bodies.
6. No automatic TLS or disk offload.
7. No global scheduler in the control plane.
8. No per-request scan of hundreds of cores.

Batch remote notifications where possible: one eventfd signal per inbox batch, not one per request. Keep metrics writes local and sampling-based. Let route decisions use approximate counters; exact global accounting is needed only at tenant admission boundaries.

## Failure modes and backpressure

Expected overload behavior should be part of the contract:

- **Target inbox full:** try an alternate target if policy allows; otherwise reject at the boundary.
- **Accept imbalance:** reduce that worker's reuseport weight or immediately hand off raw fds before TLS.
- **TLS handshake flood:** limit handshakes per tenant/source/lane before allocating large TLS state.
- **HTTP/2 stream overload:** stop extending flow-control credit, reset low-priority streams, or send GOAWAY.
- **Bulk lane saturation:** reject or queue only within a declared bounded capacity; latency lanes remain protected.
- **Disk stall:** exhaust disk tokens, stop admitting dependent requests, and propagate explicit overload errors.
- **Metrics staleness:** prefer sticky routing and hysteresis over rapid weight swings.
- **Worker panic/failure:** close owned connections and fail owned handoffs; only unowned accepts or not-yet-started envelopes may be retried elsewhere.

Backpressure should travel opposite the ownership graph. If a routed request cannot produce responses fast enough, its response sink fills, which stalls the request worker. If the connection worker cannot write, it stops pulling response fragments. If the request body is streaming, body credit prevents the connection worker from reading unbounded data from the socket.

## Prototype plan and benchmark plan

Prototype in layers:

1. **Pinned fleet skeleton.** Start N `kimojio-stack` runtimes pinned to cores, each with a local ring, local metrics, and a bounded handoff inbox.
2. **Accept router.** Add `SO_REUSEPORT` listeners and route raw accepted fds to selected workers before TLS. Implement static, round-robin, tenant-hash, and power-of-two choices policies.
3. **Admission control.** Add per-core and per-tenant limits for connections, stacks, request body bytes, handoff slots, and in-flight I/O.
4. **HTTP request envelopes.** Keep connection I/O local but route parsed, bounded HTTP/1.1 requests and unary gRPC requests to worker lanes. Return responses through explicit sinks.
5. **HTTP/2 stream routing.** Represent streams as credit-based envelopes while preserving connection-owner flow control and TLS ownership.
6. **TLS and disk lanes.** Add opt-in handshake limits, bulk crypto jobs, disk shard routing, and fixed-resource registration policies.
7. **Instrumentation.** Expose per-worker and per-tenant counters needed to prove there are no hidden queues or cross-core storms.

Benchmark against single-runtime and naive multi-runtime baselines:

- plaintext and TLS echo at p50/p99/p999 latency;
- HTTP/1.1 keep-alive with small and large bodies;
- HTTP/2/gRPC unary and many concurrent streams over few connections;
- mixed latency and bulk workloads where bulk must not hurt latency p999;
- tenant fairness with one noisy tenant and many small tenants;
- TLS handshake flood and large-record throughput;
- disk read/write mix with slow-device injection;
- accept imbalance with skewed client source ports;
- scale sweep from 1, 2, 4, 16, 64, to 128+ cores.

Primary metrics: throughput, p99/p999 end-to-end latency, wake-to-resume latency, handoff latency, handoffs per request, queue drops, allocation count, stack bytes, remote wake count, io_uring submit-to-CQE latency, and per-tenant admitted/rejected counts.

## Pros / Cons

Pros:

- preserves the current runtime's local, predictable cost model;
- gives applications real load-balancing tools without hiding migration;
- admits and sheds before expensive allocations;
- fits TLS, disk, and io_uring affinity naturally;
- supports both ultra-low-latency and throughput lanes;
- scales conceptually to hundreds of cores through hierarchical routing;
- makes multi-tenancy an explicit framework concern.

Cons:

- asks more of the application/framework author than automatic work stealing;
- long-lived connection imbalance remains hard;
- HTTP/2 stream routing adds protocol complexity;
- cross-core handoffs still cost cache misses, atomics, and wakes;
- route policies can become a tuning surface;
- some workloads may want real migratable compute tasks, which this intentionally avoids;
- reuseport/eBPF and CPU/NIC affinity behavior can vary by kernel and deployment.

## Open questions

- Should the first implementation live inside `kimojio-stack`, a companion `kimojio-stack-fleet` crate, or an application-server crate?
- What is the minimal handoff type set: raw fd, request envelope, response sink, disk job, and bulk job, or fewer?
- Should TLS state ever be movable after handshake, or should the framework make "before TLS only" the hard rule?
- How much HTTP/2 stream routing is worth implementing before generated gRPC service support exists?
- What route policy should be the safe default: tenant hash, power-of-two choices, or static core partitioning?
- How should NUMA topology, NIC RSS queues, and IRQ affinity feed into route decisions?
- Can reuseport eBPF weights be updated cheaply and portably enough for production, or should userspace raw-fd handoff be the primary mechanism?
- What counters are required for admission to be exact enough across hundreds of cores without reintroducing a global hot path?
- Should fixed files and buffers be worker-affine by default with explicit replication, or should there be lane-level registration groups?
- What debug tooling best reveals accidental hidden costs: handoff traces, per-request cost receipts, or scheduler phase histograms?
