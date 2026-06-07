# Feature Specification: Stack OpenTelemetry

**Branch**: stackful  |  **Created**: 2026-06-07  |  **Status**: Draft
**Input Brief**: Add a low-level OpenTelemetry-compatible capability for emitting logs and metrics from stackful applications.

## Overview

Applications built on the stackful Kimojio runtime need a direct way to emit operational telemetry without adopting a heavyweight runtime integration or hidden background work. This feature provides a low-level telemetry surface that allows users to connect to an OpenTelemetry-compatible destination and send logs and metrics while preserving explicit control over scheduling, allocation, and latency.

The initial user journey is intentionally narrow: a user configures a destination, opens a connection, emits log records or metric data, and receives an explicit success or failure result. The feature should favor predictable costs over convenience abstractions, so users can decide when to allocate, batch, connect, flush, retry, or drop data.

The crate must be compatible with OpenTelemetry collector-style destinations for logs and metrics and include an integration path that proves real interoperability. Higher-level SDK concepts, automatic instrumentation, traces, resource discovery, and hidden worker tasks are intentionally deferred until the low-level transport and encoding path is proven.

## Objectives

- Enable stackful applications to send log records to an OpenTelemetry-compatible destination with explicit connection and error handling.
- Enable stackful applications to send metric data to an OpenTelemetry-compatible destination with explicit connection and error handling.
- Preserve Kimojio runtime principles by making allocation, batching, retries, and I/O costs visible to the caller.
- Prove interoperability against a real OpenTelemetry-compatible receiver in integration testing.
- Provide a public surface focused on connection and export operations that can later support higher-level convenience layers without redesigning the core transport.

## User Scenarios & Testing

### User Story P1 – Emit Logs

Narrative: A stackful service operator wants to send structured log records from a Kimojio stackful application to an OpenTelemetry-compatible destination and receive explicit feedback when export succeeds or fails.

Independent Test: Start a compatible telemetry receiver, connect from a stackful task, emit at least one log record, and verify the receiver observes that record.

Acceptance Scenarios:
1. Given a reachable compatible destination, When a user emits a log record with timestamp, severity, body, and attributes, Then the destination receives an equivalent log record.
2. Given the destination rejects or closes the connection, When a user emits a log record, Then the call returns an explicit error without silently dropping the record.
3. Given multiple log records are emitted in one operation, When export succeeds, Then the destination receives every record in the requested batch.

### User Story P1 – Emit Metrics

Narrative: A stackful service operator wants to publish metric observations from a Kimojio stackful application to an OpenTelemetry-compatible destination while controlling when data is created and sent.

Independent Test: Start a compatible telemetry receiver, connect from a stackful task, emit at least one monotonic sum and one gauge data point, and verify the receiver observes both metrics.

Acceptance Scenarios:
1. Given a reachable compatible destination, When a user emits a monotonic sum or gauge with name, numeric value, timestamp, and attributes, Then the destination receives equivalent metric data.
2. Given unsupported metric input, When a user attempts to emit it, Then the API reports a validation error before sending malformed telemetry.
3. Given multiple metric data points are emitted in one operation, When export succeeds, Then the destination receives every requested data point.

### User Story P2 – Control Runtime Costs

Narrative: A latency-sensitive stackful service author wants telemetry emission to be explicit and predictable, with no hidden background threads, hidden runtime, or unexpected buffering behavior.

Independent Test: Inspect and exercise the API to verify export only occurs when the caller invokes it and that failures are returned synchronously to the stackful caller.

Acceptance Scenarios:
1. Given a configured telemetry client, When the user does not call an export method, Then no telemetry I/O is performed by the crate.
2. Given a user provides a pre-built batch, When export is invoked, Then the operation performs only the work required for that export and returns completion status to the caller.
3. Given export is blocked on I/O, When other stackful tasks are runnable, Then the runtime can continue scheduling unrelated stackful work.

### User Story P2 – Interoperate with Existing Stackful Networking

Narrative: A stackful application author wants telemetry emission to reuse the stackful networking model already used for HTTP and gRPC so telemetry does not introduce a different execution model.

Independent Test: Use the telemetry crate from a stackful task in the same style as other stackful networking clients and verify the task can emit telemetry and continue running.

Acceptance Scenarios:
1. Given an existing stackful runtime, When a user creates a telemetry client and emits data, Then the API works from stackful tasks without requiring an async runtime.
2. Given the destination requires secure transport, When the user provides appropriate transport configuration, Then the client can emit telemetry over that transport.

### Edge Cases

- Destination unavailable: connection or export returns an explicit error.
- Destination closes mid-export: export returns an explicit error and does not claim success.
- Empty log or metric batch: handled deterministically as either a successful no-op or a validation error, documented by the API.
- Oversized telemetry payload: rejected or surfaced as an explicit transport/protocol error.
- Invalid metric shape or unsupported metric type: rejected before malformed data is sent.
- Receiver returns partial success or rejection details: exposed to the caller rather than hidden.
- Integration-test receiver is unavailable in the local environment: the normal test suite remains deterministic, while receiver-backed tests are explicitly opt-in or environment-gated.

## Requirements

### Functional Requirements

- FR-001: Provide a low-level client capability to connect to an OpenTelemetry-compatible logs receiver and export log records. (Stories: P1 Emit Logs)
- FR-002: Provide a low-level client capability to connect to an OpenTelemetry-compatible metrics receiver and export cumulative monotonic sum and gauge metric data. (Stories: P1 Emit Metrics)
- FR-003: Return explicit success, rejection, and failure information for telemetry export attempts. (Stories: P1 Emit Logs, P1 Emit Metrics)
- FR-004: Allow callers to construct and submit telemetry batches explicitly, without hidden automatic export behavior. (Stories: P2 Control Runtime Costs)
- FR-005: Avoid requiring an async runtime, background worker, or implicit scheduling mechanism outside the stackful runtime. (Stories: P2 Control Runtime Costs, P2 Interoperate with Existing Stackful Networking)
- FR-006: Support secure and insecure destination connections where compatible with existing stackful transport capabilities. (Stories: P2 Interoperate with Existing Stackful Networking)
- FR-007: Include interoperability testing against a real OpenTelemetry-compatible destination for logs and metrics. (Stories: P1 Emit Logs, P1 Emit Metrics)
- FR-008: Document the supported telemetry signal scope, supported metric shapes, export behavior, and known limitations. (Stories: P1 Emit Logs, P1 Emit Metrics, P2 Control Runtime Costs)

### Key Entities

- Telemetry Destination: The configured endpoint that receives exported telemetry.
- Telemetry Resource: Caller-supplied identifying attributes for the entity emitting telemetry.
- Instrumentation Scope: Caller-supplied name/version metadata grouping emitted telemetry.
- Log Record: A timestamped structured event with severity, body, and attributes.
- Metric Data: Named numeric cumulative monotonic sums or gauges with timestamps and attributes.
- Export Result: The outcome of an export attempt, including success, protocol rejection, or transport failure.
- Telemetry Batch: A caller-owned collection of log records or metric data points submitted in one export operation.

### Cross-Cutting / Non-Functional

- Export behavior must be explicit: the crate must not start hidden workers or perform hidden periodic flushes.
- Error handling must be visible: failed exports must not be silently converted into success.
- Runtime compatibility must preserve cooperative stackful scheduling.
- Export behavior must make per-export work visible to callers by documenting any required buffering, encoding, copying, or allocation behavior.

## Success Criteria

- SC-001: A stackful application can export at least one log record to a compatible receiver and the receiver can verify the record contents. (FR-001, FR-007)
- SC-002: A stackful application can export at least one cumulative monotonic sum and one gauge to a compatible receiver and the receiver can verify each metric name, value, timestamp, and attributes. (FR-002, FR-007)
- SC-003: Failed connection, closed connection, and receiver rejection cases return explicit errors in tests. (FR-003)
- SC-004: The crate exposes no hidden background export behavior; all export work is initiated by caller actions. (FR-004, FR-005)
- SC-005: Telemetry emission can run from stackful tasks without requiring an async runtime. (FR-005)
- SC-006: Documentation identifies supported signals, supported metric shapes, connection behavior, and limitations. (FR-008)
- SC-007: Integration tests against a real compatible receiver can be run locally without pushing code or relying on external hosted services. (FR-007)

## Assumptions

- The initial compatibility target is the OpenTelemetry Protocol logs and metrics export surface.
- Traces are out of scope for the initial crate.
- The initial crate prioritizes client-side export; receiving telemetry is out of scope.
- Higher-level OpenTelemetry SDK conveniences such as automatic instrumentation, global providers, sampling, resource auto-detection, processors, and background periodic export are out of scope.
- Integration testing may use a local compatible receiver when the environment provides one; otherwise receiver-backed tests should be explicitly gated.
- Initial metric support is limited to numeric cumulative monotonic sums and gauges; unsupported metric shapes fail explicitly and are documented.

## Scope

In Scope:
- A new stackful OpenTelemetry export capability.
- Low-level client-side log export.
- Low-level client-side metric export.
- Explicit connection, export, and finish/close semantics with error handling.
- Tests proving protocol behavior and receiver interoperability.
- Documentation for low-level usage and limitations.

Out of Scope:
- Trace export.
- Server-side telemetry ingestion.
- Automatic instrumentation.
- Global telemetry provider setup.
- Background workers, hidden retries, or automatic periodic batching.
- Full OpenTelemetry SDK replacement.
- Hosted-service-specific exporters beyond compatibility with standard receivers.
- Streaming or long-lived bidirectional telemetry export.

## Dependencies

- Existing stackful runtime and networking capabilities.
- Existing stackful HTTP/gRPC support.
- OpenTelemetry-compatible receiver for integration testing.

## Risks & Mitigations

- Protocol complexity: Logs and metrics schemas may be broad. Mitigation: start with a clearly documented supported subset and explicit rejection for unsupported input.
- Hidden allocation risk: Encoding telemetry can allocate unexpectedly, increasing latency variance. Mitigation: require documentation of per-export buffering, encoding, copying, and allocation behavior.
- Integration-test fragility: Receiver-backed tests may fail in environments without a local compatible receiver. Mitigation: keep regular tests deterministic and gate receiver-backed tests explicitly.
- Dependency/runtime mismatch: Some telemetry ecosystem components may assume async runtimes, which would conflict with explicit stackful scheduling. Mitigation: require runtime behavior to remain visible to the caller and avoid hidden runtime dependencies.
- Latency regressions: Telemetry export may introduce unpredictable blocking. Mitigation: all blocking must be cooperative stackful I/O with explicit caller control over when export occurs.

## References

- Initial user brief: request from 2026-06-07 for low-level OpenTelemetry-compatible logs and metrics export from stackful applications, with no hidden costs.
