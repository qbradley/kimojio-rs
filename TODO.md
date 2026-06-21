# Kimojio stackful ecosystem TODO

## Runtime-neutral integration

- Finish runtime-neutral wait, socket, and TLS surfaces so `kimojio-stack-http`,
  `kimojio-stack-grpc`, `kimojio-stack-tls`, `kimojio-stack-storage`,
  `kimojio-stack-opentelemetry`, and `kimojio-stack-sqlite` work cleanly across
  `kimojio-stack` and `kimojio-stack-steal`.
- Make sure TLS can support registered buffers and fds.
- Normalize HTTP/TLS deadline, cancellation, drain, and close behavior across
  stack-core and stack-steal.

## Performance and scale validation

- Build repeatable release benchmark matrices for stack, stack-steal, Tokio, and
  Go across tiny payloads, 8 KiB payloads, streaming uploads/downloads, TLS,
  HTTP, gRPC, object-gateway, storage, telemetry, and SQLite workloads.
- Track throughput, p50/p95/p99 latency, CPU, RSS/virtual memory, allocation
  counts, stack high-water marks, io_uring submit/completion counts, and
  scheduler wake latency.
- Finish profiling and optimizing stack-steal streaming PUT paths until they are
  competitive with the Go baseline.
- Add performance regression gates for selected hot paths.

## Runtime observability

- Expose first-class runtime metrics for ready queues, parked tasks, wake delay,
  scheduler ticks, io_uring submissions/completions, cancellation/drain counts,
  stack cache reuse, registered resource use, and stack high-water marks.
- Decide how runtime-internal metrics integrate with
  `kimojio-stack-opentelemetry` without adding hidden allocation, background
  workers, or dynamic dispatch to disabled instrumentation paths.

## Higher-level service foundations

- Add optional helpers for common service patterns without hiding costs: listener
  loops, accept loops, graceful shutdown, connection lifecycle, backpressure,
  deadline propagation, retry policy, and pooling.
- Document and validate TLS/ALPN setup for HTTP/2 and gRPC services.
- Provide end-to-end examples from echo server to HTTP, gRPC, object gateway,
  storage, telemetry, and SQLite.
- Promote stackful router/tower follow-ups after the first compatibility surface
  settles: proc macros and derived extractors, production session/cache/discovery
  stores, additional compression codecs or feature gates, broader benchmark
  environments, and true background buffer/spawn-ready/hedge implementations
  backed by safe owned stackful task handles.
- Extend stackful router/tower stateful middleware beyond the first partial
  compatibility surface with session mutation commit semantics, signed/encrypted
  cookie options, cache validator/revalidation hooks, and full `Vary`-aware cache
  key policy.
- Extend router/tower framework integration with nested state/fallback scopes,
  generic serving adapters for middleware-wrapped routers, retry/hedge
  idempotency helpers, and runtime-capability-bound middleware variants.

## SQLite hardening

- Reduce fixed overhead for tiny SQLite workloads where callback/runtime cost
  dominates.
- Add stable file-identity keys for SQLite lock/SHM state so symlink and
  hard-link aliases cannot bypass coordination.
- Add OS-visible WAL shared-memory locks or clearly gate unsupported
  cross-process WAL use.
- Expand crash/recovery, cross-process, alias-path, and long-running mixed
  read/write validation.

## Safety and fault-injection validation

- Run a systematic unsafe audit for stack switching, TLS BIO integration,
  SQLite VFS callbacks, registered resources, scheduler wait/cancel paths, and
  cross-runtime adapters.
- Add fault-injection tests for cancellation races, timeout storms, close while
  I/O is pending, panic unwinding, dropped handles, partial reads/writes,
  ENOSPC/EIO, and unsupported kernel opcode fallback paths.
- Add stress and soak tests that run with many connections, many stackful tasks,
  many timers, and repeated cancellation/close cycles.

## Stack sizing and stack checking

- Turn `kimojio-stack-check` usage into a standard workflow for every stackful
  service example.
- Document how to emit stack metadata, preserve `.stack_sizes`, choose release
  and debug stack budgets, and interpret unknown edges/cycles.
- Add CI gates for stack budgets on representative stackful entry points.
- Explore bounded-recursion annotations or tooling for stack-size analysis where
  static call graphs are insufficient.

## Memory, allocator, and buffer pools

- Finish allocator and buffer-pool strategy across runtime, HTTP, gRPC, TLS,
  storage, telemetry, SQLite, and object-gateway paths.
- Add allocation regression tests for hot paths and common request/response
  workloads.
- Standardize reusable buffers for connection I/O, body streaming, TLS BIO
  transfer, gRPC framing, and SQLite scratch paths.

## Documentation and onboarding

- Build a cohesive "Kimojio Stack Book" that explains the green-thread mental
  model, runtime rules, blocking hazards, stack sizing, cancellation/deadline
  patterns, and when the stackful approach is worth its costs.
- Add feature matrices and known-limitations pages for each stackful crate.
- Keep crate-level rustdoc examples current and validated with doctests.
- Add operational recipes for deploying, profiling, and tuning stackful services
  on Linux.

## Release and compatibility discipline

- Define supported Linux kernel versions, required io_uring features, SQPOLL
  expectations, fallback behavior, and tested distro/kernel matrix.
- Clarify API stability and semver expectations for the alpha stackful crates.
- Add CI jobs for formatting, clippy, doctests, rustdoc warnings, stack-check,
  stress tests, and selected performance smoke tests.
