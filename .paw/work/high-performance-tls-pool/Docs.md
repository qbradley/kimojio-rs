# High Performance TLS Pool

## Overview

This work adds `kimojio-tls-pool`, a runtime-independent OpenSSL TLS pool crate. The crate provides a pool-owned TLS stream handle that can run operations immediately on the caller thread or route them to background executor threads, then report completion through callbacks.

The goal is to make TLS work placement measurable without tying the lowest layer to kimojio, kimojio-stack, tokio, or another runtime. The crate currently supports blocking OpenSSL streams over regular connected streams and exposes statistics that show how operations were placed.

## Architecture and Design

### High-Level Architecture

The crate is split into small modules:

- `config`: pool configuration, placement modes, size thresholds, and ready/idle executor behavior.
- `pool`: pool construction, executor threads, shutdown, executor selection, and shared pool statistics.
- `stream`: per-stream operation queueing, same-stream serialization, read/write submission, callbacks, and stream-local overlap statistics.
- `operation`: operation kind and placement types.
- `policy`: adaptive placement decisions based on operation size and executor load snapshots.
- `stats`: thread-safe pool and executor counters.
- `tls`: OpenSSL client/server handshake helpers and blocking TLS read/write integration.

Each stream serializes its own operations. Adaptive routing may move a stream between executors over time, but the stream queue allows only one active operation for that stream at once. This preserves the OpenSSL requirement that a TLS stream not be mutably accessed concurrently.

### Design Decisions

The core crate uses the Rust OpenSSL crate directly and does not reuse the existing kimojio runtime TLS wrappers. This keeps the pool usable from normal threads and leaves runtime-specific adapters for future layers.

Callbacks execute where the operation runs. Immediate operations call back on the submitting thread. Background operations call back on the selected executor thread. Callers that need callbacks delivered elsewhere can bridge them through channels or a future adapter layer.

The adaptive policy starts intentionally simple. Small operations prefer immediate execution, near-maximum operations are eligible for background execution, and medium operations can route to an executor when the chosen executor's estimated queue cost is lower than the operation cost. The statistics and benchmarks are intended to guide future policy tuning.

### Integration Points

The crate is a new Cargo workspace member and depends on `openssl` at runtime. Test and benchmark support uses existing workspace dev-dependency conventions.

No kimojio runtime modules are required to use the pool. Existing runtime-specific TLS crates remain separate.

## User Guide

### Prerequisites

Users need OpenSSL development libraries available for the existing `openssl` crate build. The core crate expects connected blocking streams that implement `Read + Write + Send + 'static`.

### Basic Usage

Create a `TlsPool` from a `PoolConfig`, perform client or server stream construction with OpenSSL connector/acceptor values, then submit reads or writes with callbacks.

Callbacks receive `Result` values. Success values are the bytes read or written. Errors surface TLS, I/O, shutdown, or stream-state failures.

### Advanced Usage

Placement modes:

- Immediate-only: operations run on the submitting thread.
- Background-only: operations always route to pool executors.
- Adaptive: operation size and executor load determine immediate versus background placement.

The pool exposes aggregate and per-executor statistics for submitted, immediate, background-routed, queued, completed, failed, ready-spin, and idle-transition counts. Streams expose local statistics including `max_active`, which verifies same-stream serialization.

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

The benchmark covers single client/server and three-pair RPC write scenarios. It compares immediate-only, background-only, and adaptive modes across 4 KiB, 8 KiB, 16 KiB, 24 KiB, and 32 KiB bodies and prints p50/p95/p99 summary lines.

### Edge Cases

- Same-stream operations are serialized even when submitted from multiple threads.
- Operation callbacks are expected exactly once on success or error.
- Operation-level I/O errors are surfaced through callback results.
- Pool shutdown rejects new background work explicitly.

## Limitations and Future Work

The current implementation uses blocking OpenSSL streams and does not include non-blocking socket readiness integration. It does not integrate with tokio, kimojio, kimojio-stack, kernel TLS, or hardware TLS offload.

Adaptive placement is intentionally simple and should be tuned using the provided statistics and benchmarks. Future work can add runtime adapters, richer load models, bounded queues, cancellation, and non-blocking readiness support.
