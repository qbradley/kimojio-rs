# Feature Specification: gRPC Server Streaming

**Branch**: feature/grpc-streaming  |  **Created**: 2026-06-08  |  **Status**: Draft
**Input Brief**: Add server-side streaming RPC support to kimojio-stack-grpc while preserving low-latency, low-allocation behavior.

## Overview

Callers need to issue one request and then consume an ordered stream of response messages from a single RPC. The stream may be short, may run for a long time, and may end either cleanly or with a status error that the caller can observe. The feature should make this usage natural for both client and server code without requiring callers to manage protocol details.

Servers need to produce response streams asynchronously, including streams backed by internal tasks or channels, while preserving ordering and backpressure. If the receiver slows down or disconnects, the sender must not build unbounded buffers or continue hidden work indefinitely. Disconnect detection is guaranteed when explicit stream interaction points process a reset, wait on flow control, or encounter transport I/O errors; this work does not add a hidden background reader solely to wake idle producers.

The implementation must fit kimojio's performance expectations: no unnecessary memory allocations, low latency, explicit backpressure, and no hidden costs. Streaming should be a first-class capability for unary-request, streaming-response RPCs, but broader streaming modes remain outside this work.

## Objectives

- Enable unary-request to streaming-response RPCs where each response message is delivered in order.
- Allow clients to asynchronously consume response streams one item at a time and distinguish message, stream error, and end-of-stream outcomes.
- Allow servers to asynchronously produce response streams from task/channel-backed sources.
- Preserve flow control and cancellation behavior so slow or disconnected peers do not cause unbounded work or buffering.
- Propagate initial metadata and terminal stream status consistently with unary RPC behavior.
- Support multiple concurrent server-streaming RPCs through independent client connections without cross-stream ordering or state interference.

## User Scenarios & Testing

### User Story P1 - Consume a streaming RPC from a client

Narrative: A caller sends a single request and reads response messages as they become available until the stream completes, fails, or is cancelled.

Independent Test: Start a streaming RPC that emits multiple messages and verify the client observes each message in order, then observes the final stream outcome.

Acceptance Scenarios:
1. Given a streaming RPC emits three successful messages, When the client reads the stream, Then the client receives the three messages in emission order before end-of-stream.
2. Given a streaming RPC terminates with a non-success status after one message, When the client reads the stream, Then the client receives the message and then receives the terminal status error.
3. Given a streaming RPC completes without messages, When the client reads the stream, Then the client observes end-of-stream without a message or error.

### User Story P2 - Produce a streaming RPC from a server

Narrative: A server handler receives one request and returns an asynchronous response source that can produce messages over time.

Independent Test: Implement a server handler that returns a task/channel-backed stream and verify clients receive the produced messages in order.

Acceptance Scenarios:
1. Given a server handler returns a response source that produces multiple successful messages, When a client calls the RPC, Then all messages are delivered in the same order they were produced.
2. Given the response source yields a status error, When a client reads the stream, Then the client observes that status as the stream terminal error.
3. Given the response source remains open while no message is ready, When the RPC is active, Then the stream remains pending without busy-waiting or synthesizing placeholder messages.

### User Story P3 - Preserve backpressure and cancellation

Narrative: A long-lived or high-volume stream should naturally slow down when the peer cannot receive quickly and should stop promptly when the peer disconnects or cancels.

Independent Test: Run a long-lived streaming RPC with a slow or disconnected client and verify the server does not accumulate unbounded queued responses and observes cancellation.

Acceptance Scenarios:
1. Given a client reads slower than the server can produce, When the stream is active, Then response production is bounded by transport flow control or explicit send readiness rather than unbounded buffering.
2. Given a client disconnects during an active stream, When the server attempts to continue producing, Then the stream is cancelled or errors and server-side work can stop.
3. Given multiple streaming RPCs run concurrently on independent client connections, When one stream is slow or cancelled, Then other streams continue independently.

### User Story P4 - Preserve metadata and status semantics

Narrative: Callers need stream setup metadata and final status to behave predictably so existing request context and error handling patterns remain usable.

Independent Test: Start a streaming RPC with initial metadata and terminal success/error statuses and verify the client observes them at the appropriate stream lifecycle points.

Acceptance Scenarios:
1. Given a streaming RPC sends initial metadata, When the client starts reading the stream, Then the metadata is available to the caller at or before the first response message is returned.
2. Given a streaming RPC finishes successfully, When the client drains the stream, Then the client observes a clean end-of-stream.
3. Given a streaming RPC finishes with a status error, When the client drains the stream, Then the terminal status is propagated exactly once.

### Edge Cases

- The server returns a stream that immediately completes successfully.
- The server returns a stream that immediately fails before any response message.
- The client cancels before the first response message is available.
- The client cancels after receiving some messages but before terminal status.
- The server attempts to produce after the transport has reported disconnect or cancellation.
- Initial metadata is present for a stream that yields no messages.
- Multiple concurrent streams interleave scheduling but must preserve per-stream ordering.

## Requirements

### Functional Requirements

- FR-001: Support unary-request, streaming-response RPCs as a public client and server capability. (Stories: P1, P2)
- FR-002: Preserve per-RPC response message ordering from server production to client consumption. (Stories: P1, P2, P3)
- FR-003: Provide asynchronous client stream consumption that distinguishes successful message, terminal stream error, and clean end-of-stream. (Stories: P1, P4)
- FR-004: Provide asynchronous server stream production that can be backed by task/channel-style producers and can yield either messages or status errors. (Stories: P2, P3)
- FR-005: Preserve transport backpressure so response streams do not use unbounded internal buffering when receivers slow down. (Stories: P3)
- FR-006: Surface client disconnect or cancellation to server-side stream production at explicit stream interaction points so associated work can stop without hidden background polling. (Stories: P3)
- FR-007: Propagate initial metadata for streaming RPCs. (Stories: P4)
- FR-008: Propagate terminal gRPC status exactly once for both successful and failed streams. (Stories: P1, P2, P4)
- FR-009: Support multiple concurrent server-streaming RPCs over independent client connections without cross-stream state interference. (Stories: P3)
- FR-010: Maintain kimojio performance expectations by avoiding unnecessary allocations, hidden buffering, busy-waiting, and implicit background work beyond what the caller explicitly creates. (Stories: P1, P2, P3, P4)

### Key Entities

- Streaming RPC: A request/response operation with one request message and zero or more ordered response messages followed by one terminal outcome.
- Client Response Stream: The client-visible asynchronous stream of response items and final outcome.
- Server Response Source: The server-visible asynchronous producer of response messages or status errors.
- Terminal Status: The final success or error outcome of a stream.
- Initial Metadata: Metadata associated with stream setup before response message delivery completes.

### Cross-Cutting / Non-Functional

- Streaming response delivery must be bounded by explicit stream state and transport flow control, not hidden unbounded queues.
- Idle streams must suspend until progress is possible rather than busy-waiting.
- The public API should make costs visible: caller-created tasks/channels may be used, but the streaming layer should not add surprising background work.
- Existing unary RPC behavior should remain unchanged.

## Success Criteria

- SC-001: A unary-request, streaming-response RPC can deliver at least three response messages in order and then complete successfully. (FR-001, FR-002, FR-003, FR-004, FR-008)
- SC-002: A streaming RPC that terminates with a status error surfaces that exact terminal status to the client after any prior messages. (FR-003, FR-004, FR-008)
- SC-003: A long-lived stream can remain open while pending for future messages without busy-waiting or requiring all responses to be precomputed. (FR-003, FR-004, FR-010)
- SC-004: Slow client consumption does not cause unbounded response buffering in the streaming layer. (FR-005, FR-010)
- SC-005: Server-side stream production can detect client cancellation or disconnect when a stream operation processes a reset, waits on flow control, or encounters transport I/O errors and stop associated work. (FR-006)
- SC-006: Initial metadata is observable for streaming RPCs. (FR-007)
- SC-007: At least two concurrent server-streaming RPCs on independent client connections can run simultaneously while preserving independent per-stream ordering and terminal outcomes. (FR-002, FR-009)
- SC-008: Existing unary RPC tests continue to pass unchanged. (FR-010)

## Assumptions

- Server-side streaming means exactly one request message followed by zero or more response messages and one terminal status.
- Client streaming and bidirectional streaming are intentionally out of scope for this work.
- Existing transport flow-control primitives are sufficient to enforce backpressure once streaming response bodies are connected to them.
- Stream producers may use caller-owned tasks or channels; the streaming layer should not create hidden long-lived tasks unless an existing transport path already requires that behavior.
- Public API naming and exact types will follow existing kimojio-stack-grpc conventions discovered during code research.
- Concurrent streaming in this workflow is defined across independent client connections; general-purpose same-connection multiplexed gRPC stream handles are out of scope.
- Active cancellation wakeup for an idle producer that is not interacting with the stream is out of scope; producers observe cancellation when their own cancellation source fires, when a stream operation processes a reset or waits for flow control, or when transport I/O reports an error.

## Scope

In Scope:
- Client API support for unary-request, streaming-response RPCs.
- Server API support for asynchronous response stream production.
- Ordered response message delivery.
- Long-lived response streams.
- Backpressure and cancellation/disconnect handling.
- Initial metadata and terminal status propagation.
- Concurrent server-streaming RPC support.
- Client cancellation APIs that can signal a still-open stream without requiring the full response stream to drain.
- Tests covering the required streaming behavior and preserving unary behavior.

Out of Scope:
- Client-streaming RPCs.
- Bidirectional-streaming RPCs.
- Custom per-message compression APIs.
- Advanced streaming exchange protocols beyond standard server-side streaming semantics.
- General-purpose same-connection multiplexed gRPC stream handles.
- A general streaming-body replacement for the existing buffered body abstraction.
- Hidden background readers or tasks whose only purpose is to wake idle stream producers on disconnect.
- Changes to unrelated HTTP, storage, or non-gRPC APIs unless required to wire streaming support correctly.

## Dependencies

- Existing kimojio-stack-grpc client/server abstractions.
- Existing kimojio-stack-http HTTP/2 request, response, body, and flow-control behavior.
- Existing kimojio-stack scheduler and wait/cancellation primitives.
- Existing protobuf encode/decode and gRPC status/metadata handling.

## Risks & Mitigations

- Risk: Streaming implementation accidentally buffers all responses before sending. Impact: high latency and memory growth for long-lived streams. Mitigation: require incremental response production tests and review for bounded buffering.
- Risk: Cancellation is not surfaced promptly to producers. Impact: leaked work after disconnect. Mitigation: include disconnect/cancellation scenarios and use existing transport cancellation signals where available.
- Risk: Terminal status is lost or delivered more than once. Impact: incorrect client error handling. Mitigation: model stream completion as one terminal outcome and test success, empty, and error cases.
- Risk: Streaming changes regress unary RPC behavior. Impact: existing users break. Mitigation: keep unary paths intact and run existing unary tests.
- Risk: API design introduces hidden allocations or background tasks. Impact: violates kimojio performance expectations. Mitigation: prefer explicit caller-owned streams/tasks and review allocation/backpressure behavior during implementation.

## References

- Source brief: private feature guidance provided at workflow start.
