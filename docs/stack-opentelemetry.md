# Stack OpenTelemetry

`kimojio-stack-opentelemetry` provides low-level OpenTelemetry Protocol export
for stackful applications. It is designed for explicit scheduling and
predictable costs: callers build telemetry batches, call export methods
directly, and close clients explicitly.

The initial scope supports:

| Signal | Supported |
|--------|-----------|
| Logs | timestamp, observed timestamp, severity number/text, body, attributes, resource, instrumentation scope |
| Metrics | numeric cumulative monotonic sums and gauges with timestamps, values, attributes, resource, instrumentation scope |

Traces, automatic instrumentation, global providers, background workers,
automatic retry, and periodic batching are intentionally out of scope.

## Transport model

Logs and metrics use unary OTLP gRPC over the existing stackful HTTP/2 transport.
Create clients from an `h2::ClientConnection`, a `StackTransport`, or a connected
plaintext socket. With the `tls` feature enabled, TLS helper constructors are
also available.

Plaintext builds avoid the TLS/OpenSSL dependency surface by default.

The OpenTelemetry runtime migration boundary is inherited from gRPC: export
clients own unary gRPC clients, which own HTTP/2 stack connections.
Runtime-agnostic export should follow generic HTTP/2 and gRPC wrappers when
those exist, without adding a separate telemetry runtime trait. Optional
runtime-operation instrumentation can attach to explicit shared-contract events
such as capability checks, socket submission/completion, wait registration,
cancellation, close, and adapter error mapping; disabled instrumentation must not
add mandatory dynamic dispatch, allocation, background work, or helper threads.

## Export behavior

Export is caller controlled:

1. Build `Resource` and `InstrumentationScope` metadata.
2. Build a `LogBatch` or `MetricBatch`.
3. Create `LogsClient` or `MetricsClient`.
4. Call `export(cx, batch)`.
5. Call `finish(cx)` to close the underlying gRPC connection.

`export` returns partial-success information reported by the receiver:

- `LogsExportResult::rejected_log_records()`
- `MetricsExportResult::rejected_data_points()`
- `error_message()` on both result types

Empty batches return locally without sending telemetry. Oversized encoded
requests fail before export with `ErrorKind::SizeLimit`. Unsupported metric
shapes such as histograms and summaries return `ErrorKind::Unsupported`.
Monotonic sums are cumulative; callers should provide cumulative totals rather
than delta-only interval values.

## Receiver interop tests

Default tests do not require Docker. Real OpenTelemetry Collector tests are
opt-in:

```sh
KIMOJIO_STACK_OTEL_RECEIVER_TESTS=1 cargo test -p kimojio-stack-opentelemetry --test receiver_interop -- --nocapture
KIMOJIO_STACK_OTEL_RECEIVER_TESTS=1 cargo test -p kimojio-stack-opentelemetry --all-features --test receiver_interop -- --nocapture
```

The tests generate a temporary collector config with an OTLP gRPC receiver and
file exporter, then read the file-exporter output to verify logs, metrics,
resource attributes, and scope metadata. If Docker is unavailable, the tests
print an explicit skip reason and pass.
