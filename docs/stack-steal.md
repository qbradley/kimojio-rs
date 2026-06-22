# Stackful Work-Stealing Runtime

`kimojio-stack-steal` is an experimental multi-worker stackful runtime. It keeps
the structured concurrency model of `kimojio-stack`, but lets explicitly
eligible work run on a worker pool with configurable stealing policies.

The stack runtime crates are currently workspace-private (`publish = false`)
because `kimojio-stack-steal` depends on new, unpublished `kimojio-stack`
runtime API exports.

The shared `kimojio-stack::runtime_api` traits and hidden companion-runtime hooks
used by this crate are alpha release surface, not a stable downstream contract.
Before publishing a runtime that depends on them, the shared API needs an
explicit version/release decision and release notes for any breaking alpha
changes.

Use local or pinned spawns for non-`Send` state. Use stealable spawns for work
that may run on another worker:

```rust,no_run
use kimojio_stack_steal::{Runtime, RuntimeConfig, StealPolicy};

let mut runtime = Runtime::with_config(RuntimeConfig {
    workers: std::num::NonZeroUsize::new(4).unwrap(),
    steal_policy: StealPolicy::steal_one(),
    ..RuntimeConfig::default()
});

let value = runtime.block_on(|cx| {
    cx.scope(|scope| {
        let handle = scope.spawn_stealable(|cx| {
            let ring = cx.create_worker_ring();
            ring.nop(cx).unwrap();
            42
        });
        handle.join(cx)
    })
});

assert_eq!(value, 42);
```

For accept loops that move already-accepted sockets into worker-pool connection
handlers, prefer `Scope::try_spawn_stealable`. It reports queue saturation
synchronously so the accept loop can stop accepting, close the just-accepted
socket, or shed work instead of leaving a connected socket unread. The returned
`JoinHandle` exposes `has_started()` and `wait_started(cx)` so a server can tell
whether a connection job is only queued or has actually begun reading.
`kimojio-stack-tls::TlsContext` is immutable/shareable after OpenSSL
configuration is complete, so wrap it in `Arc<TlsContext>` and move clones into
stealable connection tasks when TLS policy is shared across accepted sockets.

The runtime context exposes the same direct io_uring operation surface as
`kimojio-stack`, including filesystem, socket setup, send/recv, vectored I/O,
poll/epoll, splice/tee, futex, cancellation, async read/write, and registered
fixed-file/fixed-buffer operations. Direct operations run on the embedded
stackful scheduler for the current root or worker context, preserving
`kimojio-stack` buffer ownership, fixed-resource, cancellation, and waiter
semantics.

Code that needs explicit worker-local versus synchronized shared-ring costs can
still create ring handles. Worker-local rings enforce owner-worker and
runtime-instance use and cannot be created from the root `block_on` context of a
multi-worker runtime. Shared rings are cloneable and synchronized for
cross-worker use, but remain runtime-affine. The root `block_on` context is not a
worker; use `RuntimeContext::current_worker()` or
`RuntimeContext::execution_place()` when code needs to distinguish root from
worker-pool execution. Shared rings route no-op, timeout/sleep, and socket
read/write lifecycle operations through runtime-owned io_uring schedulers
instead of a ring-owned helper thread. Single-worker runtimes drive shared
operations locally; multi-worker runtimes assign each shared ring to a real owner
worker and use bounded per-ring queues, bounded submitted-operation tracking,
deduplicated pending worker commands, and worker wakeups for submission and
completion routing.
Successful shared synchronous read/write calls also reuse a tiny
`RuntimeContext`-owned payload cache after warmup: one read buffer and one write
buffer per context, exact-size only, up to 64 KiB capacity each. This removes the
large per-call `Vec` payload allocation from repeated same-size sync operations
without changing shared async operation ownership; `Arc`/queued-operation state
for cross-worker async routing remains a visible shared-ring cost.
Worker command queues also retain their configured `max_shared_ring_queue_len`
capacity across drain cycles using worker-local scratch storage. This avoids
regrowing the runtime-owned command queue after warmup without changing queue
bounds or adding a new configuration knob.

The runtime also implements the shared `kimojio-stack::runtime_api` socket I/O
contract used by the runtime-agnostic HTTP/1.1+TLS slice. Worker code routes
socket operations through worker-local rings, while root/non-worker code uses
shared rings. Runtime-neutral cancellation requests leave result handles
drainable, so HTTP/TLS timeout paths can cancel pending shared socket operations,
drain worker-owned state, and then close/reclaim sockets without hiding ownership
costs.
Pending shared-ring waits must run from a spawned stackful context that can
register an external waiter. Root `block_on` code may create shared rings and
submit work for routing, but waiting for a pending shared result from root returns
a clear no-stackful-context error instead of falling back to OS-thread sleeps.

Each worker thread owns a worker-local stackful scheduler. Stealing moves
eligible queued jobs before they start running; already-running stackful
continuations do not migrate between workers.

The worker pool uses crossbeam work-stealing deques plus a global injector.
Worker-local schedulers reuse completed guarded stacks through `kimojio-stack`
so hot local spawns avoid per-task stack `mmap`/`mprotect`/`munmap`.
Use `Runtime::metrics()` after `block_on` for completed-run scheduler and
stealing counters. Use `RuntimeContext::pool_diagnostics()` during `block_on`
when code needs a read-only snapshot of embedded stack scheduler pools and the
current context's bounded shared-sync payload cache; context-local cache values
are not aggregated into `Runtime::metrics()`.
`Runtime::metrics().backpressure()` returns a compact summary of queue depths,
enqueue-to-start ready wait, rejected submissions, and worker completion
imbalance. Use it in connection-accept harnesses when accepted sockets appear to
remain unread under full-suite load: queued work and high ready-wait indicate
worker-pool backpressure, while zero queueing points investigation back toward
the connection/protocol path.

Benchmarks for local scheduling, raw scheduler queue movement, full stealable
handoff, rings, metrics, and channels are available with:

```sh
cargo test -p kimojio-stack-steal --bench runtime_baseline -- --test
cargo bench -p kimojio-stack-steal --bench runtime_baseline
cargo bench -p kimojio-stack-steal --features tokio --bench runtime_baseline
```

Use the `scheduler/raw_*` benchmarks to measure queue mechanics without stack
allocation, task allocation, joins, or OS wakeups. Use the
`scheduler/spawn_stealable_*` benchmarks for full runtime handoff costs.
Ring benchmarks separate `ring/owned_worker_local_*` from
`ring/shared_worker_owned_*` so worker-local io_uring costs are not conflated
with shared-ring routing, retained worker command-queue capacity, and
synchronization costs.

Runtime-agnostic downstream benchmark coverage lives in the downstream crates and
uses explicit labels:

```sh
cargo test -p kimojio-stack-http --bench http_request_response -- --test
cargo test -p kimojio-stack-tls --bench tls_read_write -- --test
cargo bench -p kimojio-stack-http --bench http_request_response -- --noplot
cargo bench -p kimojio-stack-tls --bench tls_read_write -- --noplot
```

Use `stack-steal/worker-local/*` HTTP/TLS labels to measure worker-owned socket
I/O and `stack-steal/shared-root/*` labels to measure shared-ring routing from
non-worker stackful tasks. HTTP labels also separate plaintext, TLS, and active
deadline paths; TLS labels separate read versus write and stack-core waitable
async paths. Compare Criterion medians and tail percentiles only within matching
payload sizes and protocols.

## Performance Gate Status

The current implementation includes benchmark smoke coverage and local
measurements, but it does **not** yet satisfy the release performance gate from
the stack-steal specification. The required documented-machine run must report
median and p99 values for local scheduling, steal transfer/resume, and worker
scaling. Until that report exists, the release-mode median/p99 threshold work is
a failing performance investigation item, not a passed acceptance criterion.

The crate is a foundation for runtime-generic stack HTTP, gRPC, TLS, storage,
and telemetry libraries. HTTP/1.1, HTTP/2, protocol-neutral HTTP wrappers, gRPC,
storage HTTP transports/clients, and OpenTelemetry unary exporters now expose
runtime-generic compatibility paths while preserving stack-core aliases.

## Limitations and Future Work

- Live migration of already-running stackful continuations is not implemented.
- Stealable work requires `Send + 'static`; use local or pinned spawns for
  non-`Send` state.
- Queue saturation for `spawn_stealable` is currently reported when the returned
  handle is joined, by resuming a rejection panic payload. A fallible
  backpressure-oriented spawn API is future work.
- Direct `RuntimeContext` I/O has operation parity with `kimojio-stack`.
  Explicit shared-ring I/O remains intentionally narrower: no-op, timeout/sleep,
  and the socket read/write/shutdown/close lifecycle used by the HTTP+TLS
  migration.
- Shared-ring completion currently polls active shared operations in the owner
  worker loop; lower-overhead completion fanout remains an optimization item if
  benchmarks show it is needed.
- Downstream HTTP/TLS/gRPC/storage/telemetry runtime-generic APIs currently cover
  the migrated socket-based paths. Runtime-neutral traits for broader direct
  file/storage io_uring APIs remain future shared runtime capabilities.
- The crate is not publishable until the shared `kimojio-stack` runtime API is
  versioned and released first.
- Release-mode median/p99 threshold validation is currently a failing
  performance investigation item until it is run on a documented benchmark
  machine and compared against the spec thresholds.
