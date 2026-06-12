# Stackful Work-Stealing Runtime

`kimojio-stack-steal` is an experimental multi-worker stackful runtime. It keeps
the structured concurrency model of `kimojio-stack`, but lets explicitly
eligible work run on a worker pool with configurable stealing policies.

The crate is currently workspace-private (`publish = false`) because it depends
on new, unpublished `kimojio-stack` runtime API exports.

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
methods. Worker-local rings enforce owner-worker and runtime-instance use, while
shared rings are cloneable and synchronized for cross-worker use. Shared rings
are currently a proof-layer implementation: each shared ring owns one helper OS
thread, has a bounded request queue, and should not be treated as the final
high-performance shared io_uring architecture.

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

The crate is a foundation for future runtime-generic stack HTTP, gRPC, TLS,
storage, and telemetry libraries. Those downstream crates have not been migrated
to the stealing runtime yet.

## Limitations and Future Work

- Live migration of already-running stackful continuations is not implemented.
- Stealable work requires `Send + 'static`; use local or pinned spawns for
  non-`Send` state.
- Shared-ring I/O is a safe proof layer with a helper thread and bounded queue,
  not the final high-performance shared io_uring design.
- Downstream stack HTTP, gRPC, TLS, storage, and telemetry crates are not
  runtime-generic yet.
- The crate is not publishable until the shared `kimojio-stack` runtime API is
  versioned and released first.
- Release-mode median and p99 threshold investigation remains deferred until it
  can be run on a documented benchmark machine.
