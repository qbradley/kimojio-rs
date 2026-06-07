# Stack OpenTelemetry

## Overview

`kimojio-stack-opentelemetry` adds low-level OpenTelemetry Protocol export foundations for stackful applications. It supports caller-owned log and metric batches, explicit export calls, explicit finish/close behavior, and inspectable export errors without installing a global provider or starting background workers.

The implemented scope covers logs plus numeric cumulative monotonic sums and gauges. Traces, automatic instrumentation, global providers, background periodic export, and server-side telemetry ingestion remain out of scope.

## Architecture and Design

### High-Level Architecture

The crate sits above `kimojio-stack-grpc` and uses unary gRPC over the existing stackful HTTP/2 transport. Callers either provide an HTTP/2 client connection directly or use explicit plaintext/TLS helpers that construct the stackful transport boundary.

Telemetry construction is separate from export:

1. Build caller-owned `Resource` and `InstrumentationScope` metadata.
2. Build a `LogBatch` or `MetricBatch`.
3. Create `LogsClient` or `MetricsClient`.
4. Call `export(cx, batch)`.
5. Call `finish(cx)` to close the underlying gRPC connection.

### Design Decisions

- **Handwritten supported-subset protocol types**: The crate defines prost-compatible OTLP structures for the supported logs and metrics subset. Tests decode emitted bytes with upstream `opentelemetry-proto` message types so handwritten tags remain wire-compatible.
- **No hidden export behavior**: Clients only perform I/O when `export` or `finish` is called. Tests assert there is no transport write before explicit export.
- **Explicit partial success**: Logs and metrics export results expose OTLP rejected counts and receiver messages.
- **Feature-gated TLS**: Plaintext builds avoid TLS/OpenSSL dependencies. Enabling the `tls` feature exposes TLS helper constructors and enables the `kimojio-stack-http` TLS path.
- **Receiver interop is opt-in**: Docker-backed OpenTelemetry Collector tests are gated by `KIMOJIO_STACK_OTEL_RECEIVER_TESTS=1`, so normal test runs do not require Docker.

### Integration Points

- `LogsClient` and `MetricsClient` wrap the shared unary export helper.
- `ExportClientConfig` configures gRPC message limits and encoded request size limits.
- `Error` categorizes validation, size-limit, gRPC, and unsupported-feature failures.
- Receiver-backed tests use an independent OpenTelemetry Collector container with an OTLP gRPC receiver and file exporter.

## User Guide

### Prerequisites

- A `kimojio-stack` runtime context.
- A connected stackful transport to an OTLP gRPC endpoint.
- For TLS helpers, enable the crate's `tls` feature and provide an established stack TLS stream or use HTTP TLS helpers.

### Basic Usage

Create a low-level logs client from an HTTP/2 connection, build a batch, export it, and finish the connection:

```rust,no_run
use kimojio_stack_opentelemetry::{
    AnyValue, ExportClientConfig, InstrumentationScope, KeyValue, LogBatch,
    LogRecord, LogsClient, Resource, SeverityNumber,
};

# fn run(cx: &kimojio_stack::RuntimeContext<'_>, http: kimojio_stack_http::h2::ClientConnection) -> Result<(), kimojio_stack_opentelemetry::Error> {
let mut client = LogsClient::new(http, ExportClientConfig::default());
let batch = LogBatch::new(
    Resource::new().with_attribute(KeyValue::new(
        "service.name",
        AnyValue::String("example".into()),
    )),
    InstrumentationScope::new("example-scope"),
)
.with_record(LogRecord::new(
    1,
    SeverityNumber::Info,
    AnyValue::String("hello".into()),
));

let result = client.export(cx, batch)?;
assert_eq!(result.rejected_log_records(), 0);
client.finish(cx)?;
# Ok(())
# }
```

Metrics export supports numeric cumulative monotonic sums and gauges:

```rust,no_run
use kimojio_stack_opentelemetry::{
    ExportClientConfig, GaugeDataPoint, InstrumentationScope, MetricBatch,
    MetricsClient, MonotonicSumDataPoint, NumberValue, Resource,
};

# fn run(cx: &kimojio_stack::RuntimeContext<'_>, http: kimojio_stack_http::h2::ClientConnection) -> Result<(), kimojio_stack_opentelemetry::Error> {
let mut client = MetricsClient::new(http, ExportClientConfig::default());
let batch = MetricBatch::new(Resource::new(), InstrumentationScope::new("example"))
    .with_monotonic_sum(MonotonicSumDataPoint::new(
        "requests",
        1,
        2,
        NumberValue::I64(7),
    ))
    .with_gauge(GaugeDataPoint::new("queue_depth", 2, NumberValue::F64(3.0)));

let result = client.export(cx, batch)?;
assert_eq!(result.rejected_data_points(), 0);
client.finish(cx)?;
# Ok(())
# }
```

### Advanced Usage

Use `LogsClient::from_transport` or `MetricsClient::from_transport` when the caller owns a `StackTransport`. Use `LogsClient::new` or `MetricsClient::new` when the caller already owns an HTTP/2 client connection and wants full control over transport setup.

Empty batches are encoded as deterministic no-op OTLP requests. Oversized encoded requests return `ErrorKind::SizeLimit` before export. Unsupported metric shapes such as histograms and summaries are represented by `MetricShape::validate_supported`, which returns `ErrorKind::Unsupported`.

## API Reference

### Key Components

- `Resource`: caller-supplied telemetry producer attributes.
- `InstrumentationScope`: caller-supplied scope name/version/attributes.
- `KeyValue` and `AnyValue`: supported telemetry attributes.
- `LogBatch` and `LogRecord`: supported log export inputs.
- `MetricBatch`, `MonotonicSumDataPoint`, `GaugeDataPoint`, and `NumberValue`: supported metric export inputs. Monotonic sums are encoded with cumulative aggregation temporality, so callers should provide cumulative totals rather than delta-only interval values.
- `LogsClient` and `MetricsClient`: low-level unary export clients.
- `LogsExportResult` and `MetricsExportResult`: receiver partial-success information.
- `Error` and `ErrorKind`: inspectable validation, size-limit, gRPC, and unsupported-feature failures.

### Configuration Options

- `ExportClientConfig::max_message_len`: gRPC message-size limit.
- `ExportClientConfig::export_limits.max_encoded_len`: encoded OTLP request-size limit before gRPC framing.
- `tls` feature: enables TLS helper constructors and TLS/OpenSSL dependencies.

## Per-Export Cost and Data Movement

Export is synchronous from the caller's stackful task and has no hidden work after
`export` returns. Each export:

1. Converts the caller-owned batch into supported-subset OTLP prost structures.
2. Computes the encoded message length and checks `ExportLimits`.
3. Encodes the OTLP request into a `Vec<u8>` through prost.
4. Passes the prost message to the unary gRPC helper, which applies the gRPC
   frame and writes through the existing HTTP/2 stackful transport.
5. Decodes the unary OTLP response and returns partial-success fields.

The current implementation therefore performs per-export encoding allocation for
the request bytes and normal gRPC/HTTP2 buffering underneath. It does not retain
batches after export, does not start background buffering, and does not retry or
flush later. `finish` closes the underlying gRPC connection; it does not perform
additional telemetry export beyond normal transport shutdown.

Allocation behavior is covered by warmed local-loop observation tests for logs
and metrics. These tests record current hot-path allocation budgets rather than
claiming zero allocation, because prost request encoding and gRPC framing still
own encoded buffers in this low-level implementation.

## Testing

### How to Test

Normal validation does not require Docker:

```sh
cargo test -p kimojio-stack-opentelemetry
cargo test -p kimojio-stack-opentelemetry --all-features
cargo clippy -p kimojio-stack-opentelemetry --all-targets -- -D warnings
cargo clippy -p kimojio-stack-opentelemetry --all-targets --all-features -- -D warnings
```

Receiver-backed interop tests are opt-in:

```sh
KIMOJIO_STACK_OTEL_RECEIVER_TESTS=1 cargo test -p kimojio-stack-opentelemetry --test receiver_interop -- --nocapture
KIMOJIO_STACK_OTEL_RECEIVER_TESTS=1 cargo test -p kimojio-stack-opentelemetry --all-features --test receiver_interop -- --nocapture
```

When Docker is unavailable, receiver-backed tests print an explicit skip reason and return success.

### Final Validation Results

The final validation after Society-of-Thought review fixes completed successfully with:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --features ssl2
cargo test --all-targets --features virtual-clock
cargo test --doc
```

Receiver-backed tests also compiled and ran in default/all-features mode with
the environment gate unset, and the Docker-unavailable opt-in path returned an
explicit skip message in this environment.

### Edge Cases

- Empty log and metric batches produce deterministic no-op export requests.
- Encoded requests over `ExportLimits::max_encoded_len` return a size-limit error before export.
- Unsupported metric shapes return an unsupported-feature error.
- Monotonic sum points with `start_time_unix_nano > time_unix_nano` return a validation error before export.
- Connection-unavailable and mid-export-close cases return explicit errors.
- Export blocked on I/O still allows unrelated stackful work to continue.

## Limitations and Future Work

- Traces are not implemented.
- Histograms, exponential histograms, summaries, exemplars, dropped counts, schema URLs, and advanced OTLP fields are outside the initial supported subset.
- No automatic batching, retry, global provider, or background worker is provided.
- Receiver-backed tests require Docker and the configured OpenTelemetry Collector image to execute the real receiver path.
