# Stackful Object Gateway

The Stackful Object Gateway is a canonical example application for the stackful
runtime crates. It exposes a bounded object API over gRPC, an HTTP admin
health/status endpoint, hermetic storage and telemetry fixtures, conformance
tests, workload reporting, and comparison implementations for Tokio and Go.

Default commands are hermetic and do not require cloud credentials, Docker, a
collector, or a Go toolchain unless an explicit Go or live-service gate is used.

## Implementations

| Implementation | Runtime mode | Service/admin stack |
|----------------|--------------|---------------------|
| `stack` | single stackful scheduler | `kimojio-stack-grpc` and stack HTTP |
| `stack-steal` | stackful work stealing | runtime-generic stack gRPC/HTTP paths |
| `tokio` | multithread Tokio | Tonic and Axum |
| `go` | goroutine per request | grpc-go and net/http |

All implementations use the same visible object contract: put, get, delete,
list, copy, and health/status. The comparison implementations use native
ecosystem libraries while preserving the shared conformance behavior.

`put` is a gRPC client-streaming upload of bounded object chunks, while `get` is
a server-streaming download. The other object operations and health/status calls
are unary. Stackful modes use the first-party client-streaming support in
`kimojio-stack-grpc`; Tokio and Go use their native gRPC libraries. Stackful TCP
hosts serve multiple gRPC requests on each HTTP/2 connection until the client
closes the connection.

PUT chunk offsets are validated as contiguous byte offsets. The `finish` bit is
optional at end-of-stream, but once a client sends it no later chunks are
accepted. `ListRequest.max_results=0` means unlimited for the hermetic fixture;
non-zero values cap the response object count.

## Running hosts

Stackful host self-check:

```sh
cargo run -q -p examples --bin object-gateway-stack-host -- \
  --runtime stack --grpc-addr 127.0.0.1:0 --admin-addr 127.0.0.1:0 \
  --shutdown-after-ready
```

Work-stealing stackful host self-check:

```sh
cargo run -q -p examples --bin object-gateway-stack-host -- \
  --runtime steal --workers 2 --grpc-addr 127.0.0.1:0 --admin-addr 127.0.0.1:0 \
  --shutdown-after-ready
```

The stackful host defaults to 20 KiB guarded coroutine stacks in release builds
for both `stack` and `steal` modes. Debug builds default to 40 KiB to absorb
larger unoptimized frames. Use `--stack-size-bytes` to raise or lower the
application budget and `--print-stack-usage` with `--shutdown-after-ready` to
emit page-granular runtime high-water diagnostics.

Tokio comparison host self-check:

```sh
cargo run -q -p examples --bin object-gateway-tokio-host -- \
  --grpc-addr 127.0.0.1:0 --admin-addr 127.0.0.1:0 --workers 2 \
  --shutdown-after-ready
```

Go comparison host self-check:

```sh
cd examples/go/object_gateway
go run . --grpc-addr 127.0.0.1:0 --admin-addr 127.0.0.1:0 --shutdown-after-ready
```

## Conformance

Run the default Rust/Tokio conformance matrix:

```sh
cargo run -q -p examples --bin object-gateway-conformance
```

Run targeted conformance against an already-running service:

```sh
cargo run -q -p examples --bin object-gateway-conformance -- \
  --target-grpc-addr 127.0.0.1:9100 \
  --target-admin-addr 127.0.0.1:9101 \
  --target-label stack
```

Run opt-in Go conformance:

```sh
KIMOJIO_OBJECT_GATEWAY_GO=1 \
  cargo test -p examples object_gateway_go_conformance --all-targets
```

The opt-in Go gate launches the Go process and runs the protocol-visible TCP
matrix, including cancellation, deadline, and concurrency scenarios. Go-native
unit tests cover hermetic storage failures and telemetry hooks that are not
driven through public admin endpoints.

## Workloads

Run all default Rust implementation workloads:

```sh
cargo run -q -p examples --bin object-gateway-workload -- \
  --implementation all --workload all
```

Run one workload for one implementation:

```sh
cargo run -q -p examples --bin object-gateway-workload -- \
  --implementation stack-steal --workload cancellation-heavy
```

Run a gated Go workload:

```sh
KIMOJIO_OBJECT_GATEWAY_GO=1 \
  cargo run -q -p examples --bin object-gateway-workload -- \
  --implementation go --workload small-object
```

Workload output is stable key-value text with implementation, runtime mode,
active storage client, backend mode, active telemetry sink mode, measurement
mode, workload, request count, success count, error count, canceled count,
throughput, latency sample count/confidence, p50, p95, and p99. Current Rust
workloads use `measurement_mode=cold-start-per-operation`: each logical
operation goes through the simple launch helper path rather than a long-lived
connection pool. The numbers are comparative and diagnostic; they are not an
absolute pass/fail performance threshold.

Available workload profiles:

| Workload | Purpose |
|----------|---------|
| `small-object` | sequential small put/get operations |
| `streaming-large-object` | chunked put/get with configured memory bounds |
| `concurrent` | independent concurrent puts |
| `cancellation-heavy` | canceled streaming gets after successful writes |
| `error-heavy` | missing object, size-limit, and missing copy-source errors |

Streaming workloads report and enforce the configured chunk and memory limits
with incremental length/checksum validation instead of retaining extra full-body
copies for comparison.
Operation telemetry metrics use sums/gauges in the hermetic sink. Workload
latency percentiles are computed by the harness from measured operation
durations, not by OpenTelemetry histograms. Short smoke profiles mark
`tail_latency_confidence=low-sample-diagnostic`; increase iterations in a
dedicated benchmark before treating p95/p99 as statistically meaningful.

## Optional live gates

Live storage and live OpenTelemetry runs are explicit opt-ins. The checked-in
gate helpers validate configuration and produce deterministic skip reasons;
they do not silently convert hermetic workload runs into live service calls.

| Gate | Required variables |
|------|--------------------|
| Live Azure-compatible storage | `KIMOJIO_OBJECT_GATEWAY_LIVE_STORAGE=1`, `KIMOJIO_OBJECT_GATEWAY_STORAGE_ENDPOINT`, `KIMOJIO_OBJECT_GATEWAY_STORAGE_CONTAINER` |
| Live OpenTelemetry receiver | `KIMOJIO_OBJECT_GATEWAY_LIVE_OTEL=1`, `KIMOJIO_OBJECT_GATEWAY_OTEL_ENDPOINT` |

Gate-check commands:

```sh
cargo test -p examples object_gateway_workload_live_storage_gate --all-targets
cargo test -p examples object_gateway_workload_live_telemetry_gate --all-targets
```

## Validation

Useful smoke commands:

```sh
cargo test -p examples object_gateway_conformance --all-targets
cargo test -p examples object_gateway_tokio --all-targets
cargo test -p examples object_gateway_workload --all-targets
cargo test -p examples --bench object_gateway -- --test
```

Go-specific validation:

```sh
cd examples/go/object_gateway
go test ./...
go build ./...
go generate ./...
```

`go generate` requires `protoc`, writable temporary storage (`TMPDIR` or
`/tmp`), and network/module-cache access for pinned protobuf generator plugins.
