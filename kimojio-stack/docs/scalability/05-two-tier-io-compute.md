# Two-Tier I/O and Compute Cores

`kimojio-stack` should explore a two-tier runtime for very large single-VM deployments: keep the latency-sensitive socket reactor path on dedicated, mostly local cores, and move TLS, disk, compression, parsing, and chunky application compute to separate cores through bounded, explicit handoff queues. The thesis is not to make every operation cross-thread by default; it is to preserve the current per-core reactor strengths for connection ownership, readiness, and io_uring completion while giving expensive or bursty work a predictable place to run, with bounded memory, visible backpressure, tenant-aware fairness, and response routing back to the exact reactor that owns the connection.

## Reactor/core topology

Recommended topology:

```text
NIC/RSS queue 0 -> Reactor R0 -> bounded rings -> Compute/TLS/Disk lanes C*
NIC/RSS queue 1 -> Reactor R1 -> bounded rings -> Compute/TLS/Disk lanes C*
...

Compute/TLS/Disk lane Cj -> return ring -> owning Reactor Ri -> socket write
```

- A **reactor core** owns accepted sockets, socket fd registration, io_uring submission/completion for network reads and writes, connection lifecycle, per-connection write queues, and the final decision to close or half-close a connection.
- A **compute core** owns CPU-heavy or latency-variable work: TLS record processing and handshakes, application parsing, compression, decompression, request handling, cache lookup that may be chunky, and response construction.
- A **disk core/lane** owns filesystem operations that can block or have long-tail kernel latency: open/stat/read/write/fsync/sync_file_range, cache-miss reads, and large file transfer preparation.
- Each connection has stable metadata:
  - `ConnectionId`
  - `ConnectionGeneration`
  - `OwnerReactorId`
  - `TenantId` or fairness class
  - optional `TlsLaneId`, `ParserLaneId`, and `DiskLaneId`
- The **handoff boundary** is an owned work descriptor plus owned buffer handles. Reactors do not hand out mutable references to reactor-local state, fd tables, or scheduler state. Compute lanes return actions, not direct socket operations.
- Responses always flow back through a return descriptor carrying `OwnerReactorId`, `ConnectionId`, `ConnectionGeneration`, `Sequence`, buffers, and action flags. The owner reactor validates generation and ordering, then enqueues or submits the actual socket write.

The first implementation should prefer one reactor per NIC/RSS queue or per selected CPU, with compute lanes in the same NUMA node. Full all-to-all routing across hundreds of cores should be avoided unless benchmarked; use per-NUMA groups and sparse rings between active reactor/compute pairs.

## Handoff queue/ring design

The hot path should use bounded rings with explicit ownership transfer:

```text
Ingress:
  Reactor Ri receives encrypted/plain bytes into buffer B
  Ri enqueues Work {
      owner: Ri,
      conn: (ConnectionId, Generation),
      seq,
      tenant,
      kind: TlsDecrypt | Parse | App | DiskRead | Compress,
      buffer: B,
      credits,
      deadline
  } to a selected lane Cj

Return:
  Cj enqueues Completion {
      owner: Ri,
      conn: (ConnectionId, Generation),
      seq,
      action: Write | ContinueRead | Close | Error | NeedDisk | NeedTlsEncrypt,
      buffers,
      status
  } to Ri's return ring

Apply:
  Ri drains return ring, drops stale generations, preserves per-connection order,
  updates credits, and submits send/write/shutdown through its own ring.
```

Design preferences:

- Use **SPSC rings** where possible: one reactor producer to one compute consumer, and one compute producer back to one reactor consumer.
- Where MPSC is unavoidable, shard by reactor or tenant rather than using one global queue.
- Keep descriptors fixed-size, cache-line padded, and cheap to copy. Put payload bytes in owned buffers, registered buffer handles, or slab tokens.
- Batch ring drains, but cap batch size so one hot tenant cannot monopolize a reactor or compute lane.
- Wake the receiving core only on empty-to-nonempty transitions, or after a bounded batching delay. Prefer eventfd or io_uring wake integration over per-message wakeups.
- Capacity is part of the API contract. A full ring means "the downstream tier is saturated"; it must trigger backpressure rather than heap allocation or hidden queue growth.
- Maintain per-connection sequence numbers. Compute may run ahead for independent chunks, but the reactor is the serialization point for writes, close, and cancellation.

At hundreds of cores, a full `reactors x compute_lanes x 2` ring matrix can become a hidden memory and cache-management cost. Start with per-NUMA lane groups, create rings lazily for active pairs, and expose limits for maximum active pairings.

## API/developer model

The developer model should make cross-tier cost explicit without making normal connection code awkward:

- Reactor-local APIs remain for accepting, reading, writing, timers, and connection lifecycle. They must stay nonblocking with respect to CPU-heavy work.
- Cross-tier APIs require owned, `Send` work items:
  - `cx.offload_tls(conn, bytes)`
  - `cx.offload_parse(conn, plaintext)`
  - `cx.offload_compute(class, request)`
  - `cx.offload_disk(op)`
- The return type should behave like a stackful waitable on the owning reactor, but internally it represents a descriptor expected on a return ring.
- Non-`Send` state remains reactor-local. Anything crossing into compute/TLS/disk lanes must be owned or explicitly shareable.
- Connection handlers should be able to choose policies:
  - "inline on reactor only for tiny known-bounded work"
  - "always offload TLS"
  - "offload after N bytes or N cycles"
  - "pin this tenant to these compute lanes"
- The reactor should expose a clear "cannot enqueue" result so handlers can stop reading, shed, retry later, or close gracefully.

The important rule is: **compute code never writes directly to a socket**. It returns `ConnectionAction` values to the owner reactor. This preserves fd affinity, avoids cross-thread fd state locks, and gives one place to enforce ordering, cancellation, and backpressure.

## TLS/disk placement strategy

TLS and disk should be treated as separate placement problems.

### TLS

- TLS handshake, certificate verification, key schedule work, record encryption/decryption, and bulk crypto should usually run off the reactor.
- Each TLS connection should be pinned to a stable TLS lane so TLS session state is not protected by a lock and record ordering is natural.
- The reactor owns ciphertext socket I/O. The TLS lane transforms ciphertext buffers into plaintext buffers and plaintext response buffers into ciphertext write buffers.
- For small, already-warm TLS records, a policy may allow bounded inline decrypt/encrypt on the reactor, but only with a strict cycle or byte budget.
- If kTLS or NIC TLS offload is used, the reactor may handle more of the data path, but handshake/control-plane work should still be isolated from the reactor.
- Prefer co-locating TLS and parsing on the same compute lane for a connection to avoid an extra hop: ciphertext `R -> C`, plaintext parse/app on `C`, encrypted response `C -> R`.

### Disk

- Reactors should not perform blocking filesystem fallbacks or long-tail disk operations.
- Disk lanes should own file io_uring instances, blocking fallback pools, fixed-file registration policy, and page-cache-miss behavior.
- Disk completion should return buffer handles to either a compute lane for compression/TLS or directly to the owner reactor if bytes are ready to send.
- Small metadata/cache-hit operations may be allowed on compute lanes only if their latency distribution is measured and bounded.
- Large static responses should use explicit staging:
  1. reactor receives request;
  2. compute parses and decides file/range;
  3. disk lane reads or prepares send buffers;
  4. optional compression/TLS lane transforms;
  5. owner reactor writes to the socket.

## Backpressure, fairness, tenant isolation

Backpressure must be credit-based and visible at every boundary:

- Each connection has ingress credits for bytes/descriptors outstanding beyond the reactor.
- Each tenant has aggregate credits per tier: reactor ingress, TLS, app compute, disk, compression, and return buffers.
- Ring capacity and buffer-pool capacity are hard limits. Exhaustion stops reads or disables read interest for affected connections instead of allocating unbounded memory.
- Credits are returned only when the owner reactor applies a completion or drops a stale completion and reclaims buffers.

Fairness should be budgeted:

- Reactors drain return rings with weighted deficit round robin across tenants or fairness classes.
- Compute lanes select work by tenant budget, not raw queue order alone.
- Disk lanes enforce per-tenant in-flight byte and operation limits.
- Accept loops should consider tenant admission limits so a connection storm cannot consume all descriptors and buffers.
- A noisy tenant with expensive TLS or disk misses should fill only its own credits, not every reactor's global resources.

For latency-sensitive tenants, reserve minimum credits and compute shares. For best-effort tenants, allow opportunistic use of idle capacity but revoke it immediately when reserved work appears.

## NUMA/cache locality implications

The two-tier model only helps if locality is deliberate:

- Align NIC RSS queues, IRQ affinity, reactor cores, and socket memory pools on the same NUMA node.
- Prefer compute/TLS/disk lanes on the same NUMA node as the owning reactor.
- Keep connection-to-lane mapping stable to preserve TLS state, parser state, branch predictor history, and hot tenant data.
- Use per-NUMA buffer slabs and return buffers to their home slab after the owner reactor is done.
- Avoid cross-socket handoff except as an explicit overload mode with separate metrics.
- Keep descriptors small enough to fit in cache lines; avoid false sharing on producer/consumer indices.
- Batch enough to amortize cache-line transfer, but keep upper bounds to avoid tail-latency spikes.

On a single VM with hundreds of cores, NUMA topology can dominate queue cost. The design should report local versus remote handoffs separately, because aggregate throughput can look good while p99.9 latency is driven by cross-socket returns.

## Costs and hidden-cost avoidance

Expected costs:

- one or more extra cross-core handoffs per request;
- remote cache-line movement for ring indices and descriptors;
- wakeup/eventfd traffic;
- buffer ownership tracking;
- per-tier scheduling budgets;
- queue memory for active reactor/lane pairs;
- possible extra latency for tiny requests that would have completed inline.

Hidden costs to avoid:

- unbounded "temporary" queues after a bounded ring reports full;
- global MPMC queues for all reactors and compute lanes;
- TLS state protected by a shared lock;
- fd writes from compute lanes requiring shared connection locks;
- per-message heap allocation or `Arc` churn on hot paths;
- waking a sleeping core for every descriptor instead of batched notifications;
- lazy fixed-buffer or fixed-file registration on the first user request;
- work stealing that moves connection state away from its buffers and reactor;
- retry loops that burn a full core under downstream saturation.

The runtime should expose queue occupancy, enqueue failures, handoff latency, wakeup counts, buffer-pool pressure, and per-tenant credit state as first-class metrics. If these are not observable, the model will hide overload rather than control it.

## Failure modes and cancellation

Key failure modes:

- **Compute lane overload:** ingress rings fill; reactors stop posting reads for affected connections or shed low-priority tenants.
- **Return ring overload:** compute lanes cannot return completions; they must stop accepting more work from affected owners and propagate pressure upstream.
- **Connection closes while work is in flight:** reactor increments the connection generation and sends cancellation tokens. Late completions with stale generation are dropped and their buffers reclaimed.
- **Out-of-order compute completion:** reactor uses per-connection sequence numbers and applies only the next expected action, buffering or canceling later results according to policy.
- **Disk I/O cannot be canceled immediately:** disk lane marks the operation canceled, detaches user-visible state, and reclaims buffers when the kernel completion arrives.
- **TLS error or protocol violation:** TLS/compute lane returns a close/error action; the owner reactor performs alert write, shutdown, or hard close according to connection policy.
- **Compute panic or lane failure:** affected jobs return error completions if possible; otherwise owner reactors time out outstanding work and close or retry connections on another lane.
- **Reactor failure:** because the reactor owns fds, all connections on that reactor are closed unless a future design supports explicit fd migration.

Cancellation is owned by the reactor because it owns connection liveness. Compute lanes may observe cancellation and stop early, but only the reactor decides whether a result is still applicable to the socket.

## Prototype plan and benchmark plan

### Prototype plan

1. Build a minimal two-tier echo prototype with one reactor thread and one compute thread using bounded owned descriptors and a return ring.
2. Add `ConnectionId`, `ConnectionGeneration`, sequence numbers, and stale-completion dropping.
3. Add credit-based read backpressure: when compute ingress is full, stop posting reads for that connection; resume when credits return.
4. Expand to multiple reactors and compute lanes within one NUMA node with stable connection-to-lane hashing.
5. Add a TLS-cost simulator, then integrate real TLS record processing.
6. Add a disk lane with fixed-size buffer pools and benchmark cache-hit and cache-miss behavior separately.
7. Add per-tenant credits and weighted drain budgets.
8. Add metrics for queue occupancy, handoff latency, wakeups, allocation counts, stale completions, and cancellation latency.
9. Only after the model is stable, test sparse cross-NUMA overflow routing.

### Benchmark plan

Compare single-tier reactor execution against two-tier execution for:

- plain echo: 64 B, 1 KiB, 16 KiB, and 1 MiB payloads;
- TLS echo with handshake-heavy and steady-state record-heavy mixes;
- parse-heavy small requests;
- compression-heavy responses;
- disk-backed static responses with warm page cache and forced cache-miss scenarios;
- mixed tenants where one tenant is CPU-heavy or disk-heavy and another is latency-sensitive.

Measure:

- throughput;
- p50, p95, p99, and p99.9 end-to-end latency;
- reactor loop delay and maximum time between io_uring reaps;
- handoff enqueue-to-start and enqueue-to-return latency;
- queue occupancy and full-ring events;
- wakeups per request;
- bytes copied per request;
- allocations after warmup;
- CPU utilization per tier;
- local versus remote NUMA traffic;
- fairness under tenant overload.

Scale tests should cover at least `1R/1C`, `1R/3C`, `4R/4C`, one full NUMA node, and progressively larger single-VM shapes. The design is successful only if reactors keep low loop delay under TLS/disk/compute pressure and latency-sensitive tenants remain bounded when best-effort tenants saturate compute lanes.

## Pros / Cons

Pros:

- Keeps socket ownership and io_uring completion local to reactor cores.
- Prevents TLS, disk, compression, and parsing bursts from starving network readiness.
- Makes backpressure explicit through bounded rings and credits.
- Enables tenant-aware scheduling at each tier.
- Scales better than a global executor queue when cores reach large single-VM counts.
- Preserves a path for low-allocation buffer ownership and fixed-resource I/O.

Cons:

- Adds at least one cross-core handoff and often two.
- Can hurt tiny request latency if offload policy is too aggressive.
- Requires careful connection sequencing, cancellation, and generation tracking.
- Increases operational complexity: more queues, metrics, and tuning knobs.
- Risks hidden memory growth if ring topology is not sparse and bounded.
- TLS and parser placement can add extra hops if not co-located.
- Cross-NUMA traffic can erase the expected gains.

## Open questions

- What is the smallest work size, in cycles or bytes, that should be offloaded rather than run inline on the reactor?
- Should TLS and parsing be a single per-connection lane by default, or separate lanes with an explicit pipeline?
- How many reactors should a single compute lane serve before return-ring scanning becomes expensive?
- Is a sparse SPSC ring matrix enough, or is a sharded MPSC design better at hundreds of cores?
- Which wake mechanism has the best tail latency for reactor-to-compute and compute-to-reactor notification on target kernels?
- How should fixed buffers be registered when disk and network rings are owned by different cores?
- Can kTLS or NIC offload make reactor-side TLS viable for some deployments?
- What API should expose tenant credits without leaking too much scheduler internals?
- Should overload close connections, stop reading, return application-level errors, or use tenant-specific policy?
- Is reactor fd migration worth supporting for maintenance and failure recovery, or should connection ownership remain strictly non-migratable?
