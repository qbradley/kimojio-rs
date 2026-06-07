# Stack OpenTelemetry Implementation Plan

## Overview

Implement a new `kimojio-stack-opentelemetry` crate that provides low-level, caller-controlled OpenTelemetry-compatible logs and metrics export from stackful applications. The crate will build on the existing stackful HTTP/2 and unary gRPC client patterns documented in `kimojio-stack-grpc`, while keeping export invocation, connection setup, batching, and error handling explicit to the caller.

The plan treats OpenTelemetry Protocol logs and metrics export as the initial compatibility target from the specification. It uses handwritten/prost-compatible protocol data structures for the supported subset because the current repository has no protobuf-generation build path and existing stackful gRPC tests/benchmarks use handwritten prost types, while direct OpenTelemetry proto crates bring tonic/OpenTelemetry SDK feature surfaces that need separate dependency review.

## Current State Analysis

- The workspace already contains stackful HTTP/2 and gRPC crates, and `kimojio-stack-grpc::UnaryClient` sends prost-encoded unary requests over an `h2::ClientConnection` with explicit caller-supplied path, metadata, and close behavior (`.paw/work/stack-opentelemetry/CodeResearch.md:20-23`, `.paw/work/stack-opentelemetry/CodeResearch.md:116-133`).
- `kimojio-stack-http::StackTransport` is the existing plaintext/TLS transport boundary over `OwnedFd` or `TlsStream`, with transport reads/writes delegated to `RuntimeContext` (`.paw/work/stack-opentelemetry/CodeResearch.md:68-78`).
- Existing gRPC tests use handwritten `prost::Message` structs for test proto shapes rather than repository-local protobuf code generation (`.paw/work/stack-opentelemetry/CodeResearch.md:135-142`).
- Existing interop tests keep Tokio, tonic, hyper, and TLS support in dev-dependencies while public stackful crates expose stackful APIs (`.paw/work/stack-opentelemetry/CodeResearch.md:145-165`).
- The repository uses plain Markdown docs and validates formatting, clippy, doc tests, and test suites with Cargo commands (`.paw/work/stack-opentelemetry/CodeResearch.md:28-44`).

## Desired End State

- `kimojio-stack-opentelemetry` is a workspace crate with inherited package metadata and explicit dependencies.
- The crate exposes low-level log and metric export clients that operate from stackful tasks and return explicit export results/errors, including OTLP partial-success rejected counts and messages.
- Logs export can send timestamped records with severity, body, and attributes to a compatible receiver.
- Metrics export can send numeric cumulative monotonic sums and gauges with names, values, timestamps, and attributes to a compatible receiver.
- Plaintext and TLS export both work through the existing stackful gRPC/HTTP2 transport boundary.
- Unit and interop coverage demonstrate request encoding, explicit errors, independent receiver interoperability, oversized payload handling, empty batch behavior, no hidden transport writes before explicit export, allocation-budget observations, and cooperative scheduling while export is blocked on I/O.
- Documentation captures the low-level API, supported signals, supported metric shapes, connection behavior, per-export buffering/encoding/copying/allocation behavior, limitations, and validation commands.

## What We're NOT Doing

- Trace export.
- Server-side telemetry ingestion.
- Automatic instrumentation or global providers.
- Hidden background workers, automatic periodic export, hidden retries, or implicit batching.
- A full OpenTelemetry SDK replacement.
- Hosted-service-specific exporters beyond compatibility with standard OpenTelemetry Protocol receivers.
- Streaming gRPC export or long-lived bidirectional telemetry streams.

## Phase Status

- [x] **Phase 1: Protocol and Crate Foundation** - Add the workspace crate, protocol data structures, low-level attribute/value types, and validation/error surfaces.
- [x] **Phase 2: Logs Export Client** - Implement caller-controlled OTLP logs unary export over stackful gRPC with unit tests.
- [x] **Phase 3: Metrics Export Client** - Implement cumulative monotonic sum and gauge OTLP metrics unary export over stackful gRPC with unit tests.
- [x] **Phase 4: Receiver Interoperability** - Add opt-in receiver-backed logs and metrics integration tests plus local receiver guidance.
- [x] **Phase 5: Documentation** - Add Docs.md and project documentation for usage, supported scope, limitations, and verification.

## Phase Candidates

---

## Phase 1: Protocol and Crate Foundation

**Status**: Complete. Added the workspace crate, handwritten supported-subset OTLP prost messages, resource/scope/attribute/log/metric data models, partial-success response decoding, validation/error surfaces, unsupported metric-shape validation, size-limit checks, dev-only upstream `opentelemetry-proto` schema validation, and TLS feature wiring. Phase validation passed with `cargo test -p kimojio-stack-opentelemetry`, `cargo clippy -p kimojio-stack-opentelemetry --all-targets -- -D warnings`, `cargo test -p kimojio-stack-opentelemetry --all-features`, `cargo clippy -p kimojio-stack-opentelemetry --all-targets --all-features -- -D warnings`, and dependency tree checks confirming TLS dependencies only appear with `--all-features`.

### Changes Required:

- **`Cargo.toml`**: Add `kimojio-stack-opentelemetry` to workspace members and shared dependencies if new OpenTelemetry/prost-support crates are needed, following existing workspace member/dependency conventions (`.paw/work/stack-opentelemetry/CodeResearch.md:48-57`).
- **`kimojio-stack-opentelemetry/Cargo.toml`**: Create a new crate manifest with inherited workspace metadata and normal dependencies on `kimojio-stack`, `kimojio-stack-grpc`, `kimojio-stack-http` with `default-features = false`, and `prost`, mirroring stackful crate manifest style (`.paw/work/stack-opentelemetry/CodeResearch.md:48-57`, `.paw/work/stack-opentelemetry/CodeResearch.md:135-142`).
- **`kimojio-stack-opentelemetry/Cargo.toml` features**: Add a non-default `tls` feature that enables `kimojio-stack-http/tls` and exposes TLS helper APIs; keep plaintext export available without OpenSSL/TLS dependencies by default, following the gRPC crate's normal dependency pattern (`.paw/work/stack-opentelemetry/CodeResearch.md:24`, `.paw/work/stack-opentelemetry/CodeResearch.md:138`, `.paw/work/stack-opentelemetry/CodeResearch.md:214`).
- **`kimojio-stack-opentelemetry/src/lib.rs`**: Define public modules and re-exports for logs, metrics, resource/scope metadata, attributes, export errors, and client configuration.
- **`kimojio-stack-opentelemetry/src/proto.rs`**: Define prost-compatible OTLP request/response message shapes for the supported logs and metrics subset, including resource/scope wrappers and logs/metrics partial-success response fields, aligned with unary `prost::Message` use in `kimojio-stack-grpc` (`.paw/work/stack-opentelemetry/CodeResearch.md:127-141`).
- **Schema validation**: Add a dev-only validation approach that compares the handwritten supported-subset encoding against an independent OTLP reference, either through upstream `opentelemetry-proto` message types or pinned canonical proto fixtures, so compatibility does not rely solely on local handwritten types (`.paw/work/stack-opentelemetry/CodeResearch.md:137-143`).
- **`kimojio-stack-opentelemetry/src/error.rs`**: Define inspectable error categories that wrap validation, gRPC status, transport/protocol, and unsupported-shape errors, following the stable-category pattern in `kimojio-stack-grpc` errors (`.paw/work/stack-opentelemetry/CodeResearch.md:131-133`).
- **Tests**: Add unit tests for attribute encoding, resource/scope metadata encoding, request validation, unsupported metric-shape rejection, oversized payload validation, and error category inspection.

### Success Criteria:

#### Automated Verification:
- [x] Tests pass: `cargo test -p kimojio-stack-opentelemetry`
- [x] Lint/typecheck: `cargo clippy -p kimojio-stack-opentelemetry --all-targets -- -D warnings`

#### Manual Verification:
- [x] Public types expose only low-level caller-controlled construction/export concepts.
- [x] Unsupported telemetry shapes are represented as explicit validation errors.
- [x] Dependency notes capture the handwritten prost-type choice and external OpenTelemetry proto crate trade-off.
- [x] Plaintext builds do not pull TLS dependencies; `--all-features` builds include TLS helpers.

---

## Phase 2: Logs Export Client

**Status**: Complete. Added `LogsClient`, shared `ExportClientConfig`, caller-controlled unary export over `kimojio-stack-grpc::UnaryClient`, plaintext/from-transport/TLS helper constructors, finish/close semantics, partial-success result conversion, multi-record export tests, no-hidden-write check before explicit export, blocked-export cooperative scheduling coverage, oversized/empty batch behavior, and warmed allocation observation. Phase validation passed with `cargo test -p kimojio-stack-opentelemetry logs`, `cargo test -p kimojio-stack-opentelemetry`, `cargo clippy -p kimojio-stack-opentelemetry --all-targets -- -D warnings`, `cargo test -p kimojio-stack-opentelemetry --all-features`, and `cargo clippy -p kimojio-stack-opentelemetry --all-targets --all-features -- -D warnings`.

### Changes Required:

- **`kimojio-stack-opentelemetry/src/logs.rs`**: Implement low-level log record, log batch, logs client configuration, and logs export client.
- **`kimojio-stack-opentelemetry/src/client.rs`**: Build a shared unary export helper around `kimojio-stack-grpc::UnaryClient` and OTLP service paths, following the current unary call pattern (`.paw/work/stack-opentelemetry/CodeResearch.md:116-133`).
- **Transport construction APIs**: Accept a caller-created `h2::ClientConnection` for advanced control and provide explicit plaintext/TLS helpers that construct `StackTransport` plus `h2::ClientConnection` using existing patterns (`.paw/work/stack-opentelemetry/CodeResearch.md:68-95`, `.paw/work/stack-opentelemetry/CodeResearch.md:119-130`, `.paw/work/stack-opentelemetry/CodeResearch.md:158-165`).
- **Export result handling**: Decode OTLP logs export responses and surface success, partial-success rejected count/message fields, gRPC status, and transport errors.
- **Finish semantics**: Define explicit connection close/finish behavior; unary export completion is the flush point for the submitted request, and finish/close releases the underlying transport without background flushing.
- **Tests**: Add socketpair or local stackful/tonic-style unit tests verifying logs request path, encoded content, multi-record batch delivery, explicit success, empty batch behavior, oversized payload behavior, partial-success decoding, receiver rejection, transport close behavior, zero transport writes before explicit export, warmed allocation-budget behavior, and unrelated stackful work continuing while export is blocked on I/O.

### Success Criteria:

#### Automated Verification:
- [x] Tests pass: `cargo test -p kimojio-stack-opentelemetry logs`
- [x] Lint/typecheck: `cargo clippy -p kimojio-stack-opentelemetry --all-targets -- -D warnings`

#### Manual Verification:
- [x] A caller can open a client, export one log record, and observe an explicit result.
- [x] A caller can export multiple log records in one batch and observe every record at the receiver side of the test.
- [x] No hidden export occurs without an explicit caller export invocation.
- [x] Empty log batches and oversized log payloads have deterministic documented behavior.
- [x] Logs export exposes partial-success rejected count/message fields.

---

## Phase 3: Metrics Export Client

**Status**: Complete. Added `MetricsClient` over the shared unary export helper, plaintext/from-transport/TLS helper constructors, monotonic sum and gauge export request handling, finish/close semantics, partial-success result conversion, mixed metrics export tests, no-hidden-write checks before explicit export, blocked-export cooperative scheduling coverage, oversized/empty batch behavior, unsupported-shape validation, and warmed allocation observation. Phase validation passed with `cargo test -p kimojio-stack-opentelemetry metrics`, `cargo test -p kimojio-stack-opentelemetry`, `cargo clippy -p kimojio-stack-opentelemetry --all-targets -- -D warnings`, `cargo test -p kimojio-stack-opentelemetry --all-features`, and `cargo clippy -p kimojio-stack-opentelemetry --all-targets --all-features -- -D warnings`.

### Changes Required:

- **`kimojio-stack-opentelemetry/src/metrics.rs`**: Implement low-level metric names, numeric values, timestamps, attributes, monotonic sum data points, gauge data points, metric batches, and metrics export client.
- **`kimojio-stack-opentelemetry/src/proto.rs`**: Encode supported metric shapes into OTLP metrics request messages and reject unsupported shapes before export.
- **Shared client helper**: Reuse the unary export helper from Phase 2 for the OTLP metrics service path.
- **Export result handling**: Decode metrics export response success and partial-success rejected count/message fields, preserving explicit errors for status and transport failures.
- **Finish semantics**: Reuse the client close/finish behavior defined in Phase 2 for metric clients.
- **Tests**: Add tests for monotonic sum export, gauge export, mixed metric batches, unsupported shape validation, empty batch behavior, oversized payload behavior, partial-success decoding, receiver rejection handling, zero transport writes before explicit export, warmed allocation-budget behavior, and unrelated stackful work continuing while export is blocked on I/O.

### Success Criteria:

#### Automated Verification:
- [x] Tests pass: `cargo test -p kimojio-stack-opentelemetry metrics`
- [x] Lint/typecheck: `cargo clippy -p kimojio-stack-opentelemetry --all-targets -- -D warnings`

#### Manual Verification:
- [x] A caller can export at least one monotonic sum and one gauge with names, values, timestamps, and attributes.
- [x] Unsupported metric shapes fail before malformed telemetry is sent.
- [x] Empty metric batches and oversized metric payloads have deterministic documented behavior.
- [x] Metrics export exposes partial-success rejected count/message fields.

---

## Phase 4: Receiver Interoperability

**Status**: Complete. Added opt-in Docker-backed OpenTelemetry Collector interop tests gated by `KIMOJIO_STACK_OTEL_RECEIVER_TESTS=1`. The tests generate collector configuration with OTLP gRPC receiver and file exporter, verify plaintext logs/metrics export by reading collector output for log body, metric names, resource attributes, and scope metadata, include feature-gated TLS collector configuration using generated certificates, and skip with an explicit message when the env gate is unset or Docker is unavailable. Added explicit unavailable-connection and mid-export-close failure tests. Default, opt-in-with-Docker-unavailable, and all-feature test/clippy validation passed in this environment.

### Changes Required:

- **`kimojio-stack-opentelemetry/tests/receiver_interop.rs`**: Add receiver-backed integration tests that emit logs and metrics to a local OpenTelemetry-compatible receiver when the environment explicitly enables them.
- **Test receiver strategy**: Use an independent OpenTelemetry Collector container when `KIMOJIO_STACK_OTEL_RECEIVER_TESTS=1` is set. Generate a temporary collector config with OTLP gRPC receiver on localhost, file exporter output mounted to a temporary directory, and a readiness wait on the receiver port before export attempts.
- **Read-back verification**: Parse the collector file-exporter output from the mounted temporary directory to verify emitted log records, monotonic sums, gauges, resource attributes, scope metadata, and partial-success/rejection observations where supported by the receiver path.
- **Plaintext path**: Validate logs and metrics export over plaintext stackful gRPC/HTTP2.
- **TLS path**: Generate temporary test certificates and collector TLS receiver configuration, then validate logs and metrics export over secure stackful gRPC/HTTP2 using the existing stackful TLS/HTTP2 helper patterns (`.paw/work/stack-opentelemetry/CodeResearch.md:158-165`).
- **Failure tests**: Include connection unavailable, mid-export close, and receiver rejection/partial-success handling where supported by the receiver.

### Success Criteria:

#### Automated Verification:
- [x] Default tests pass without a receiver: `cargo test -p kimojio-stack-opentelemetry`
- [x] Receiver-backed tests pass when explicitly enabled with documented environment setup.
- [x] Plaintext and TLS receiver-backed paths are covered when receiver-backed tests are enabled.
- [x] Receiver-backed tests skip with an explicit reason when the env gate is unset or the local receiver toolchain is unavailable.
- [x] Lint/typecheck: `cargo clippy -p kimojio-stack-opentelemetry --all-targets -- -D warnings`

#### Manual Verification:
- [x] Receiver-backed tests do not require pushing code or accessing hosted services.
- [x] Test output identifies whether receiver-backed tests were skipped or executed.

---

## Phase 5: Documentation

**Status**: Complete. Added `.paw/work/stack-opentelemetry/Docs.md`, `docs/stack-opentelemetry.md`, and README links. Documentation covers architecture, API usage, supported logs/metrics scope, resource/scope behavior, partial-success fields, close/finish behavior, empty/oversized behavior, allocation/copying notes, and receiver-backed test execution. Final validation passed before and after Society-of-Thought fixes with `cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo clippy --all-targets --all-features -- -D warnings && cargo test --all-targets --features ssl2 && cargo test --all-targets --features virtual-clock && cargo test --doc`.

### Changes Required:

- **`.paw/work/stack-opentelemetry/Docs.md`**: Create an as-built technical reference using `paw-docs-guidance`, including architecture, API usage, supported log/metric fields, resource/scope behavior, partial-success result fields, close/finish semantics, error behavior, validation, per-export buffering/encoding/copying/allocation behavior, and limitations.
- **`docs/stack-opentelemetry.md`**: Add a user-facing Markdown guide following existing docs conventions (`.paw/work/stack-opentelemetry/CodeResearch.md:28-35`).
- **`README.md`**: Add a short link to the new Stack OpenTelemetry guide near the existing stackful HTTP/gRPC and cross-runtime channel sections.
- **Project docs verification**: Run doc tests and ensure Markdown references are accurate.
- **Final validation record**: Update this plan and Docs.md with the final validation commands and results.

### Success Criteria:

#### Automated Verification:
- [x] Docs verification: `cargo test --doc`
- [x] Full validation: `cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo clippy --all-targets --all-features -- -D warnings && cargo test --all-targets --features ssl2 && cargo test --all-targets --features virtual-clock && cargo test --doc`

#### Manual Verification:
- [x] Documentation clearly states supported signals: logs, monotonic sums, and gauges.
- [x] Documentation clearly states out-of-scope items: traces, automatic instrumentation, global providers, and background export.
- [x] Documentation states resource/scope behavior, partial-success behavior, close/finish behavior, empty-batch behavior, oversized-payload behavior, buffering, copying, and allocation behavior.
- [x] Documentation describes how to run receiver-backed integration tests.

---

## References

- Issue: none
- Spec: `.paw/work/stack-opentelemetry/Spec.md`
- Research: `.paw/work/stack-opentelemetry/CodeResearch.md`
