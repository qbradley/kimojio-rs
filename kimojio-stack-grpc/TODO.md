# kimojio-stack-grpc TODO

This file tracks work still deferred after unary, client-streaming, and
server-streaming gRPC support landed. Keep the crate's current constraints:
explicit scheduling, bounded buffering, no hidden connection pools, and no
background tasks unless the API makes their cost visible.

## Bidirectional-streaming RPCs

### Goal

Support RPCs where client and server both send ordered message streams on one
HTTP/2 stream.

### Current state

- Unary, client-streaming, and server-streaming APIs are implemented.
- `RuntimeUnaryClient` and `RuntimeUnaryServer` are generic over the HTTP/2
  runtime/socket family, with stack-core aliases preserved.
- No public API currently drives simultaneous client send and server receive
  progress for one RPC.

### Pickup notes

- Define an explicit stackful driving model for simultaneous send and receive.
  Avoid hidden background drivers unless the public API exposes that task and
  buffering cost.
- Decide whether the API is half-duplex by caller convention or provides true
  independent send/receive handles.
- Specify EOF, cancellation, and terminal-status semantics when one side
  half-closes before the other.
- Preserve HTTP/2 flow-control backpressure; unread inbound DATA must not grow an
  unbounded per-stream queue.

### Suggested validation

- Ordered duplex exchange with interleaved messages.
- Peer half-close while the other side continues sending.
- Cancellation after each side has sent at least one message.
- Tonic bidirectional interop in both directions.

## Same-connection multiplexed client stream handles

### Goal

Allow multiple active server-streaming or future bidirectional RPC handles on one
gRPC client/HTTP/2 connection.

### Current state

- `RuntimeServerStreamingResponse<'a, S, M>` still borrows
  `&'a mut RuntimeUnaryClient<S>`, so one client object can expose only one
  active streaming response at a time.
- Concurrent streaming RPCs are still possible with independent client
  connections.
- Lower-level HTTP/2 can process concurrent stream IDs and route per-stream
  response events.

### Pickup notes

- Decide the explicit connection ownership model before coding: connection
  driver object, split stream handles, or scoped cooperative multiplexing.
- Preserve no-hidden-cost behavior: if a driver task is required, expose it in
  the API.
- Preserve bounded buffering and flow-control credit semantics per stream.
- Consider non-breaking aliases such as `GrpcClient`/`GrpcServer` if the
  `Unary*` names become misleading for multiplexed use.

### Suggested validation

- Two active server-streaming RPCs over one connection with out-of-order
  responses.
- One slow stream must not cause unbounded memory growth for another stream.
- Cancellation of one stream must not reset or corrupt another stream.

## Active cancellation wakeup for idle server producers

### Goal

Allow a server producer blocked on its own source to observe client disconnect or
cancellation promptly.

### Current state

- `ServerStream::next` is driven inline by `RuntimeUnaryServer::serve_one`.
- There is no hidden background reader watching for `RST_STREAM` while a
  producer is parked.
- Cancellation is observed when stream operations process a reset, wait on flow
  control, or encounter transport I/O errors.

### Pickup notes

- Add an explicit cancellation source if prompt idle cancellation is needed.
- Prefer a `RuntimeWaitable`/`Waitable`-compatible signal producers can combine
  with source readiness.
- Avoid a silent background transport reader; expose any driver/task requirement
  in the API.
- Ensure TLS and plaintext transports surface cancellation consistently.

### Suggested validation

- Producer waits on both message source and cancellation signal.
- Client cancels while producer is idle before any next message.
- Producer exits without sending a placeholder message or leaking a task.

## Per-message compression and negotiation

### Goal

Support compressed gRPC messages when both peers negotiate compatible encodings.

### Current state

- The gRPC frame compression flag must be zero.
- `decode_message` and related helpers reject non-zero compression flags.
- Metadata can carry regular gRPC headers, but no `grpc-encoding` /
  `grpc-accept-encoding` policy exists.

### Pickup notes

- Keep compression opt-in and visible in `ClientConfig`/`ServerConfig`.
- Enforce decompressed-size limits through existing `max_message_len` semantics.
- Avoid per-message heap churn; investigate reusable buffers before enabling
  compression for high-volume streams.
- Define unsupported-encoding status mapping and interop behavior.

### Suggested validation

- Unsupported compression flags remain rejected.
- Negotiated compressed unary, client-streaming, and server-streaming messages
  round trip.
- Decompressed payloads over the message limit are rejected before delivery.
- Tonic interop for accepted and rejected encodings.

## Generic streaming body coordination with HTTP

### Goal

Decide whether gRPC streaming should share a general streaming `Body`
abstraction with `kimojio-stack-http` or continue using gRPC-specific stream
paths.

### Current state

- `kimojio-stack-http::Body` remains a bounded buffered body.
- HTTP/2 response streaming and gRPC streaming bypass `Body` and write/read DATA
  frames directly.
- This keeps unary APIs simple but leaves non-gRPC HTTP streaming without a
  general body type.

### Pickup notes

- Only introduce a shared abstraction if non-gRPC HTTP users need it.
- Avoid trait-object or allocation-heavy designs on the hot path.
- Keep existing `Body` behavior stable for buffered callers.
- Specify how flow-control credit maps to caller consumption.

### Suggested validation

- Buffered-body APIs remain unchanged.
- Streaming users can apply backpressure without hidden queues.
- HTTP/2 trailers and reset behavior remain observable.

## Streaming benchmarks and allocation baselines

### Goal

Measure latency, throughput, and allocation cost for long-lived streaming RPCs.

### Current state

- Unary gRPC has benchmark/allocation coverage.
- Streaming behavior has functional, interop, cancellation, and protocol-limit
  coverage.
- No dedicated streaming benchmark or per-message allocation baseline is checked
  in CI.

### Pickup notes

- Add Criterion benchmarks for plaintext and TLS server-streaming plus
  client-streaming with small and moderate payloads.
- Measure per-message cost across multiple messages on one stream.
- Compare direct closure streams and bounded-channel `ReceiverStream`.
- Keep a smoke-mode benchmark command suitable for CI.

### Suggested validation

- `cargo bench -p kimojio-stack-grpc --bench <streaming-bench> -- --test`
- Allocation threshold test for warmed local socketpair streaming if stable.
- Document expected allocation budget in `STREAMING.md`.

## Optional stricter `ReceiverStream` termination

### Goal

Offer a channel-backed stream adapter for callers that want sender drop to be
treated as an error rather than clean EOF.

### Current state

- `ReceiverStream::new(receiver)` treats all senders being dropped as clean
  stream completion.
- Producers should send `Err(Status)` before dropping if the stream failed.

### Pickup notes

- Keep current clean-on-drop behavior for ergonomics and compatibility.
- Add a separate constructor or adapter for strict behavior rather than changing
  `ReceiverStream::new`.
- Document how producers signal clean completion versus failure.

### Suggested validation

- Clean sender drop still returns OK for the default adapter.
- Strict adapter maps sender drop to a non-OK status.
- Explicit `Err(Status)` from either adapter reaches the client unchanged.

## Documentation freshness

### Goal

Keep public docs aligned with current streaming/runtime-generic capabilities.

### Current state

- Crate-level rustdoc is current.
- Some longer-form docs/README material may still describe client-streaming or
  server-streaming as unsupported.

### Pickup notes

- Refresh `README.md`, `STREAMING.md`, and `docs/stack-http-grpc.md` so feature
  matrices match the current API.
- Add a small example for client-streaming alongside existing server-streaming
  examples.

### Suggested validation

- `cargo test --doc`
- `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps -p kimojio-stack-grpc`
