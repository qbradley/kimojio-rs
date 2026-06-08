# High Performance TLS Pool

## Overview

This work adds `kimojio-tls-pool`, a runtime-independent OpenSSL TLS pool crate. The crate provides a pool-owned TLS stream handle that can run operations immediately on the caller thread or route them to background executor threads, then report completion through callbacks.

The goal is to make TLS work placement measurable without tying the lowest layer to kimojio, kimojio-stack, tokio, or another runtime. The crate currently supports blocking OpenSSL streams over regular connected streams and exposes statistics that show how operations were placed.

## Architecture and Design

### High-Level Architecture

The crate is split into small modules:

- `config`: pool configuration, placement modes, size thresholds, and ready/idle executor behavior.
- `pool`: pool construction, executor threads, shutdown, executor selection, and shared pool statistics.
- `stream`: per-stream operation queueing, same-stream serialization, read/write submission, readiness handoff, callbacks, and stream-local overlap statistics.
- `operation`: operation kind and placement types.
- `policy`: adaptive placement decisions based on operation size and executor load snapshots.
- `stats`: thread-safe pool and executor counters.
- `tls`: OpenSSL client/server handshake helpers and fd-backed TLS integration.

Each stream serializes its own operations. Adaptive routing may move a stream between executors over time, but the stream queue allows only one active operation for that stream at once. This preserves the OpenSSL requirement that a TLS stream not be mutably accessed concurrently.

Read and write operations make nonblocking TLS progress. Reads first attempt progress inline so already-buffered plaintext can complete immediately. If OpenSSL reports WANT_READ or WANT_WRITE, the pool registers the required readiness interest with a pool-owned readiness reactor and resumes TLS progress on an executor only after readiness arrives. This prevents peer/socket-blocked TLS operations from occupying executor threads while they wait for external progress.

### Design Decisions

The core crate uses the Rust OpenSSL crate directly and does not reuse the existing kimojio runtime TLS wrappers. This keeps the pool usable from normal threads and leaves runtime-specific adapters for future layers.

Callbacks execute where the operation completes. Immediate operations call back on the submitting thread when they can finish without waiting for socket readiness. Background and readiness-resumed operations call back on executor threads. Callbacks are nonblocking notifications and must not synchronously wait for follow-up work on the same stream. Callback and operation panics are contained so stream/executor cleanup still runs; panic results are surfaced as operation failures where a callback can still be invoked.

The adaptive policy starts intentionally simple. Small writes prefer immediate execution, near-maximum writes are eligible for background execution, and medium writes can route to an executor when the chosen executor's estimated queue cost is lower than the operation cost. Reads are readiness-driven rather than pure size-driven because readiness dominates read latency and blocking reads can otherwise starve the pool.

The current callback API necessarily type-erases callbacks that may execute on another thread. Read/write operations also pass through an internal job queue, which still uses boxed jobs for heterogeneous work. The `write_shared` API accepts `Arc<[u8]>` so repeated payloads can avoid per-call payload allocation/copying, and `write_batch` amortizes callback and queueing overhead across multiple shared chunks. A deeper alternative is an enum-only operation queue with fixed read/write variants and a separate typed completion channel API. That would reduce allocations on the hot path but would trade away arbitrary user callbacks, or require callbacks to be represented as a small fixed set of completion targets.

### Integration Points

The crate is a new Cargo workspace member and depends on `openssl` at runtime. Test and benchmark support uses existing workspace dev-dependency conventions.

No kimojio runtime modules are required to use the pool. Existing runtime-specific TLS crates remain separate.

## User Guide

### Prerequisites

Users need OpenSSL development libraries available for the existing `openssl` crate build. The core crate expects connected fd-backed streams that implement `Read + Write + AsFd + Send + 'static`. The pool sets the fd to nonblocking mode after handshake and assumes exclusive control over that fd's blocking-mode state while the TLS stream is alive.

### Basic Usage

Create a `TlsPool` from a `PoolConfig`, perform client or server stream construction with OpenSSL connector/acceptor values, then submit reads or writes with callbacks.

Callbacks receive `Result` values. Success values are the bytes read or written. Errors surface TLS, I/O, shutdown, or stream-state failures.

### Advanced Usage

Placement modes:

- Immediate-only: operations run on the submitting thread.
- Background-only: operations always route to pool executors.
- Adaptive: operation size and executor load determine immediate versus background placement.

The pool exposes aggregate and per-executor statistics for submitted, immediate, background-routed, queued, readiness-waited, readiness-resumed, completed, failed, ready-spin, and idle-transition counts. Streams expose local statistics including `max_active`, which verifies same-stream serialization.

The aggregate queued count includes both same-stream queueing and executor queueing. Separate stream-queued and executor-queued counters expose the source for policy analysis. Readiness waits are reported separately because they park socket-blocked operations without occupying a worker.

## API Reference

### Key Components

- `TlsPool`: creates stream handles, owns executors, reports pool statistics, and starts shutdown.
- `TlsStream`: submits read/write operations and reports stream-local statistics.
- `PoolConfig`: controls executor count, placement mode, thresholds, and ready/idle behavior.
- `PlacementMode`: selects immediate-only, background-only, or adaptive placement.
- `PoolStatsSnapshot`: point-in-time pool and executor counters.
- `StreamStatsSnapshot`: point-in-time stream-local counters.

### Configuration Options

- Executor count controls how many background executor threads the pool starts.
- Ready executor count and spin duration control how executors behave when work drains.
- Size thresholds define the small-message immediate preference and near-maximum background eligibility for adaptive mode.
- Maximum read length controls the largest buffer accepted by read operations. The default is 32 KiB.

## Testing

### How to Test

Run the crate tests:

```sh
cargo test -p kimojio-tls-pool --quiet
```

Smoke-test the RPC write benchmark:

```sh
cargo bench -p kimojio-tls-pool --bench rpc_write -- --test
```

Run the benchmark:

```sh
cargo bench -p kimojio-tls-pool --bench rpc_write
```

The benchmark covers single client/server, three-pair per-connection-pool, and three-pair shared-pool RPC write scenarios. It compares immediate-only, background-only, and adaptive modes across 4 KiB, 8 KiB, 16 KiB, 24 KiB, and 32 KiB bodies and prints five-sample median/high smoke summaries while Criterion reports statistically sampled timings. It also includes fixed-workload RPC/TLS write scaling groups, a one-stream pool encrypt group, saturated pool encrypt scaling, and corrected direct-OpenSSL baselines. The benchmark smoke command is wired into CI on the default Rust toolchain.

The fixed-workload groups are useful for latency/overhead analysis but do not saturate additional workers. The saturated group gives each executor one independent TLS stream and a fixed amount of 32 KiB full-write work, which removes the earlier measurement artifact where each worker received too little work and direct baselines counted a single partial `ssl_write` as a full 32 KiB write. On the current test host, saturated pool throughput scales from about 4.9 GiB/s with one executor to about 9.6 GiB/s with two, 18.7 GiB/s with four, and 29.3 GiB/s with eight. Corrected direct-OpenSSL baselines show the same shape for this batched crypto path; fixed-workload and per-operation groups remain useful for measuring scheduler and callback overhead. The benchmark groups are intentionally retained to validate future work on lower-level OpenSSL/memory-BIO/record-queue designs if higher eight-core scaling is required.

### Edge Cases

- Same-stream operations are serialized even when submitted from multiple threads.
- Operation callbacks are expected exactly once on success or error.
- Operation-level I/O errors are surfaced through callback results.
- Pending reads do not consume executor threads while waiting for socket readability.
- Pool shutdown rejects new background work explicitly and accepted queued work completes or receives an explicit shutdown result.
- Panicking operations or callbacks do not leave stream activity counters wedged or executor load inflated.

## Limitations and Future Work

The current implementation uses OpenSSL over nonblocking fds and a pool-owned readiness reactor for read readiness. It does not integrate with tokio, kimojio, kimojio-stack, kernel TLS, or hardware TLS offload.

Adaptive placement is intentionally simple and should be tuned using the provided statistics and benchmarks. Future work can add runtime adapters, richer load models, bounded queues, cancellation, multi-fd readiness backends, and lower-allocation completion APIs.
