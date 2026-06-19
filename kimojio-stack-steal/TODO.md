# kimojio-stack-steal TODO

This file tracks remaining work for the work-stealing stackful runtime and its
runtime-neutral integration points. Completed runtime-agnostic migration items
have been removed; keep new items focused on gaps that still affect
`kimojio-stack-steal` as a companion runtime for high-scale services.

## Recommended pickup order

1. Finish HTTP/TLS deadline and drain integration work that still has active
   backlog items.
2. Profile and optimize stack-steal object-gateway streaming PUT.
3. Add broader runtime observability and stress validation.
4. Document release/compatibility expectations for the alpha runtime-neutral
   API surface.

## HTTP/TLS deadline and close-path hardening

### Goal

Normalize deadline, cancellation, drain, poisoning, and close behavior for
stack-steal plaintext/TLS HTTP transports.

### Current state

- Runtime-neutral socket read/write cancellation and drain behavior exists.
- TLS async handles are generic enough for HTTP deadline paths.
- Active backlog still includes Phase 9 work for HTTP TLS deadlines and
  stack-steal timeout close/drain coverage.

### Pickup notes

- Start in `kimojio-stack-http/src/transport.rs`,
  `kimojio-stack-tls/src/lib.rs`, and stack-steal runtime API tests.
- Ensure read and write deadlines return `Errno::TIME` consistently.
- Ensure pending TLS operations are drained and streams are poisoned before
  close/reclaim.
- Keep root/shared-ring and worker-local paths behaviorally aligned.

### Suggested validation

- HTTP TLS read and write timeout tests for stack-core and stack-steal.
- Close-after-timeout tests for worker-local and shared/root paths.
- Stress many stalled TLS connections with short deadlines and verify bounded
  worker availability.

## Stack-steal streaming PUT performance

### Goal

Reduce stack-steal overhead on object-gateway streaming PUT workloads.

### Current state

- Profiling/optimization backlog is active for stack-steal streaming PUT.
- Stackful object-gateway hosts and comparison implementations exist.
- Previous work reduced HTTP/1 stack buffers and tuned coroutine stack sizes, but
  stack-steal streaming PUT still needs focused profiling.

### Pickup notes

- Use perf/server-client profiles from object-gateway streaming PUT runs.
- Compare stack, stack-steal, Tokio, and Go implementations with identical
  payload/chunk shapes.
- Look for shared-ring routing, queue contention, copy/buffer churn, TLS/HTTP2
  framing overhead, and scheduler wake patterns.
- Apply targeted optimizations only after profiles identify hot paths.

### Suggested validation

- Repeated release benchmarks for streaming PUT locally and on the remote VM.
- Before/after p50/p95/p99 latency, throughput, CPU, allocation counts, and
  scheduler/ring metrics.
- Regression smoke test for the optimized path.

## Runtime observability and diagnostics

### Goal

Make stack-steal behavior observable enough to tune production services.

### Current state

- `Runtime::metrics` reports completed-run scheduler/stealing counters.
- `RuntimeContext::pool_diagnostics` exposes local embedded scheduler pools and
  shared sync buffer cache state.
- There is no full live view of queue depths, wake latency, per-worker ring
  activity, cancellation/drain counts, or stack high-water aggregation.

### Pickup notes

- Add opt-in metrics for per-worker ready queues, global queue depth, steals,
  failed steals, wake latency, io_uring submit/completion counts, shared-ring
  queue length, cancellation/drain outcomes, stack cache reuse, and stack
  high-water marks.
- Keep disabled instrumentation allocation-free and free of background work.
- Decide how metrics integrate with `kimojio-stack-opentelemetry`.

### Suggested validation

- Unit tests for counter increments on local, stealable, shared-ring, timeout,
  cancellation, and close paths.
- Object-gateway run that emits enough metrics to explain throughput and tail
  latency.

## Shared-ring and root-context cost model

### Goal

Keep shared-ring use explicit and cheap enough for runtime-neutral adapters.

### Current state

- Socket runtime adapters cache a context-local socket ring.
- Shared-ring command queues retain configured capacity.
- Root `sleep_async` / `sleep_for` still create a ring for the selected mode
  through `create_ring`; this may be acceptable for cold paths but should be
  measured for deadline-heavy root/shared usage.

### Pickup notes

- Measure root/shared timer-heavy and socket-heavy workloads before changing
  APIs.
- Consider caching a root timer/shared ring if measurement shows repeated
  construction on hot paths.
- Preserve runtime-affinity checks and no-hidden-helper-thread behavior.

### Suggested validation

- Allocation tests proving repeated root/shared socket operations do not allocate
  per call after warmup.
- Timer-heavy benchmark for root/shared deadline paths.
- Wrong-runtime and wrong-worker tests continue to produce structured errors.

## Runtime-neutral API release discipline

### Goal

Document the alpha runtime-neutral API surface and release/no-publish decision.

### Current state

- `runtime_api` rustdoc says the surface is alpha.
- Public traits now cover stackful wait, scoped spawn, socket I/O, timers,
  runtime-neutral waitables, and structured runtime I/O errors.
- No consolidated release note describes what is stable enough to depend on and
  what may break.

### Pickup notes

- Add a repository release note, changelog entry, or explicit no-publish note.
- Include `RuntimeCapability::SocketIo`, runtime-neutral wait/timer handles,
  generic TLS/HTTP/gRPC/storage/OpenTelemetry surfaces, stack-core aliases, and
  current deferred areas.
- Clarify semver expectations for alpha crates.

### Suggested validation

- Package/publish dry-run or explicit no-publish verification.
- `cargo test --doc`
- `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps -p kimojio-stack -p kimojio-stack-steal`

## Stress, fault-injection, and soak tests

### Goal

Prove stack-steal remains reliable under high concurrency, cancellation, and
failure pressure.

### Current state

- Focused tests cover many runtime and shared-ring paths.
- Broader stress/soak validation is not yet a standard gate.

### Pickup notes

- Add tests with many workers, many stackful tasks, frequent steals, many timers,
  repeated cancellation/close cycles, and mixed local/shared ring operations.
- Inject queue-full, wrong-worker, wrong-runtime, fd-in-use, closed-ring,
  unsupported-opcode, timeout, and cancellation outcomes.
- Keep long-running soak tests opt-in if they are too slow for default CI.

### Suggested validation

- Default stress smoke test suitable for CI.
- Extended soak command documented for local/VM runs.
- Metrics emitted during soak runs to aid diagnosis.

## Stack sizing and memory footprint for stack-steal services

### Goal

Make stack-steal stack budgets and memory footprint predictable for large
connection/task counts.

### Current state

- Object-gateway release defaults and `kimojio-stack-check` workflows exist.
- Stack-steal memory behavior was less affected by recent stack-size trimming
  than stack-core in idle connection probes.

### Pickup notes

- Add stack-check gates for representative stack-steal service entry points.
- Measure stack cache behavior, VmSize/RSS, and high-water marks at large task
  counts.
- Tie recommended stack sizes to release/debug builds and documented workloads.

### Suggested validation

- `kimojio-stack-check` entries for stack-steal object-gateway admin/gRPC paths.
- Idle and active connection memory probes for stack vs stack-steal.
- Documentation updates in `docs/stack-steal.md` and object-gateway docs.
