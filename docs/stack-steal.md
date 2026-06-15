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

The runtime exposes I/O through explicit ring handles instead of implicit context
methods. Worker-local rings enforce owner-worker and runtime-instance use and
cannot be created from the root `block_on` context of a multi-worker runtime.
Shared rings are cloneable and synchronized for cross-worker use, but remain
runtime-affine. The root `block_on` context is not a worker; use
`RuntimeContext::current_worker()` or `RuntimeContext::execution_place()` when
code needs to distinguish root from worker-pool execution. Shared rings route
no-op and timeout/sleep operations through runtime-owned io_uring schedulers
instead of a ring-owned helper thread. Single-worker runtimes drive shared
operations locally; multi-worker runtimes assign each shared ring to a real
owner worker and use bounded per-ring queues, bounded submitted-operation
tracking, deduplicated pending worker commands, and worker wakeups for
submission and completion routing.

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

Benchmarks for local scheduling, raw scheduler queue movement, full stealable
handoff, rings, metrics, and channels are available with:

```sh
cargo bench -p kimojio-stack-steal --bench runtime_baseline
cargo bench -p kimojio-stack-steal --features tokio --bench runtime_baseline
```

Use the `scheduler/raw_*` benchmarks to measure queue mechanics without stack
allocation, task allocation, joins, or OS wakeups. Use the
`scheduler/spawn_stealable_*` benchmarks for full runtime handoff costs.
Ring benchmarks separate `ring/owned_worker_local_*` from
`ring/shared_worker_owned_*` so worker-local io_uring costs are not conflated
with shared-ring routing and synchronization costs.

## Performance Gate Status

The current implementation includes benchmark smoke coverage and local
measurements, but it does **not** yet satisfy the release performance gate from
the stack-steal specification. The required documented-machine run must report
median and p99 values for local scheduling, steal transfer/resume, and worker
scaling. Until that report exists, the release-mode median/p99 threshold work is
a failing performance investigation item, not a passed acceptance criterion.

The crate is a foundation for runtime-generic stack HTTP, gRPC, TLS, storage,
and telemetry libraries. The first migrated downstream slice is HTTP/1.1 over
plaintext/TLS; HTTP/2, gRPC, storage, and telemetry are still compatibility and
readiness targets.

## Limitations and Future Work

- Live migration of already-running stackful continuations is not implemented.
- Stealable work requires `Send + 'static`; use local or pinned spawns for
  non-`Send` state.
- Queue saturation for `spawn_stealable` is currently reported when the returned
  handle is joined, by resuming a rejection panic payload. A fallible
  backpressure-oriented spawn API is future work.
- Shared-ring I/O currently supports no-op, timeout/sleep, and the socket
  read/write/shutdown/close lifecycle used by the HTTP+TLS migration. Broader
  accept/connect, send/recv, registered-resource, file, and storage operations
  are future work.
- Shared-ring completion currently polls active shared operations in the owner
  worker loop; lower-overhead completion fanout remains an optimization item if
  benchmarks show it is needed.
- Downstream HTTP/1.1+TLS has a runtime-generic slice with runtime-neutral async
  wait and active TLS deadlines. HTTP/2, gRPC, storage, and telemetry are not
  fully runtime-generic yet.
- The crate is not publishable until the shared `kimojio-stack` runtime API is
  versioned and released first.
- Release-mode median/p99 threshold validation is currently a failing
  performance investigation item until it is run on a documented benchmark
  machine and compared against the spec thresholds.
