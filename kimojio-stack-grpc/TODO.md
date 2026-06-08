# kimojio-stack-grpc Future Work

This file tracks work intentionally deferred from the initial unary and
server-streaming gRPC support. The current implementation is optimized for
explicit scheduling, bounded buffering, and low hidden cost; future work should
preserve those constraints.

## Client-streaming RPCs

### Goal

Support RPCs with a stream of protobuf request messages followed by one
terminal response.

### Current state

- `UnaryClient::call` sends one complete request body.
- `UnaryServer::serve_one` receives a fully buffered request body before
  dispatching a handler.
- HTTP/2 response streaming exists, but request streaming does not.

### Pickup notes

- Add HTTP/2 client request-stream writer primitives parallel to the response
  stream writer.
- Add HTTP/2 server request-stream reader primitives that expose request DATA
  incrementally instead of building a full `Body`.
- Add a typed gRPC client request stream API that frames each request message
  with the existing gRPC 5-byte prefix.
- Add server handler APIs that receive a stackful request stream and return a
  unary response or terminal `Status`.
- Preserve flow-control semantics: do not return inbound request credit until
  the server-side caller consumes each request event.

### Suggested validation

- Stackful client to stackful server tests for ordered request messages.
- Stackful server interop with a Tonic client-streaming client.
- Stackful client interop with a Tonic client-streaming server.
- Oversized streamed request message and truncated request frame protocol tests.

## Bidirectional-streaming RPCs

### Goal

Support RPCs where client and server both send ordered message streams on one
RPC.

### Current state

- Server-streaming response APIs exist.
- Client-streaming request APIs are not implemented.
- No gRPC API currently drives simultaneous read/write progress for one RPC.

### Pickup notes

- Build this after client-streaming request primitives exist.
- Define an explicit stackful driving model for simultaneous send and receive.
  Avoid hidden background tasks unless the public API makes that cost explicit.
- Decide whether the API is half-duplex by caller convention or supports true
  independent send/receive handles.
- Ensure cancellation, EOF, and terminal status semantics are unambiguous when
  one side finishes before the other.

### Suggested validation

- Ordered duplex exchange with interleaved messages.
- Peer half-close while the other side continues sending.
- Cancellation after each side has sent at least one message.
- Tonic bidirectional interop in both directions.

## Same-connection multiplexed gRPC stream handles

### Goal

Allow multiple active server-streaming RPC handles on one gRPC client/HTTP/2
connection.

### Current state

- `ServerStreamingResponse<'a, M>` holds `&'a mut UnaryClient`.
- This borrow model intentionally enforces one active streaming response per
  `UnaryClient`.
- Concurrent server-streaming RPCs are supported through independent client
  connections.
- The lower-level HTTP/2 client can process concurrent stream IDs and queue
  per-stream response events.

### Pickup notes

- Decide on the explicit connection ownership model before coding. Options
  include a connection driver object, split stream handles with explicit polling,
  or a scoped API that multiplexes a set of streams cooperatively.
- Preserve the no-hidden-cost philosophy: if a background driver is introduced,
  make the task and buffering cost visible in the API.
- Preserve backpressure: unread per-stream DATA must remain bounded by HTTP/2
  flow control or an explicit per-stream queue budget.
- Consider clearer public aliases if the existing `UnaryClient` name becomes
  misleading for multiplexed use.

### Suggested validation

- Two active server-streaming RPCs over one connection where responses arrive
  out of order.
- One slow stream must not unbound memory growth for another stream.
- Cancellation of one stream must not reset or corrupt another stream.

## Active cancellation wakeup for idle server producers

### Goal

Allow an idle server producer blocked on its own source to observe client
disconnect or cancellation promptly.

### Current state

- `ServerStream::next` is driven inline by `UnaryServer::serve_one`.
- There is no hidden background reader that watches for `RST_STREAM` while a
  producer is parked.
- Cancellation is observed when stream operations process a reset, wait on flow
  control, or encounter transport I/O errors.

### Pickup notes

- Add an explicit cancellation source if prompt idle cancellation is needed.
- Prefer a `Waitable`-compatible signal that producers can combine with their
  own source readiness via `RuntimeContext::wait_any`.
- Avoid a silent background transport reader; if one is required, expose the
  driver/task requirement in the API.
- Ensure TLS and plaintext transports can surface cancellation consistently.

### Suggested validation

- Producer waits on both message source and cancellation signal.
- Client cancels while producer is idle before any next message.
- Producer exits without sending a placeholder message or leaking a task.

## Per-message compression and compression negotiation

### Goal

Support compressed gRPC messages when both peers negotiate compatible encodings.

### Current state

- The gRPC frame compression flag must be zero.
- `decode_message` and `decode_bytes` reject non-zero compression flags.
- Metadata helpers can carry regular gRPC headers, but no compression policy is
  implemented.

### Pickup notes

- Add explicit support for `grpc-encoding` and `grpc-accept-encoding`.
- Keep compression opt-in and visible in configuration.
- Enforce decompressed-size limits using existing `max_message_len` semantics.
- Avoid per-message heap churn; investigate reusable buffers before adding
  compression on high-volume streams.

### Suggested validation

- Unsupported compression flag remains rejected.
- Negotiated compressed unary and streaming messages round trip.
- Decompressed payloads over the message limit are rejected before delivery.

## General streaming `Body` abstraction

### Goal

Decide whether `kimojio-stack-http` should expose a general streaming body
abstraction beyond the gRPC-specific response stream path.

### Current state

- `Body` is a fully buffered `Bytes` wrapper.
- HTTP/2 response streaming bypasses `Body` and writes DATA frames directly.
- This avoids changing existing unary and HTTP request/response APIs.

### Pickup notes

- Only introduce this if non-gRPC users need generic streaming bodies.
- Avoid trait-object or allocation-heavy designs on the hot path.
- Keep existing `Body` behavior stable for buffered callers.
- Specify how flow-control credit maps to caller consumption.

### Suggested validation

- Buffered-body APIs remain unchanged.
- Streaming body users can apply backpressure without hidden queues.
- HTTP/2 trailers and reset behavior remain observable.

## Streaming benchmarks and allocation baselines

### Goal

Measure the latency and allocation cost of long-lived server-streaming RPCs.

### Current state

- Unary gRPC has Criterion benchmarks and allocation tracking tests.
- Streaming behavior has functional, interop, cancellation, and protocol-limit
  coverage.
- No dedicated streaming benchmark or per-message allocation baseline exists.

### Pickup notes

- Add a Criterion benchmark for plaintext and TLS server-streaming RPCs with
  small and moderate payloads.
- Measure per-message cost across multiple messages on one stream.
- Add an allocation baseline test for warmed streaming loops if the signal is
  stable enough to avoid flaky thresholds.
- Compare direct closure streams and bounded-channel `ReceiverStream`.

### Suggested validation

- `cargo bench -p kimojio-stack-grpc --bench <streaming-bench> -- --test`
- Allocation threshold test for warmed local socketpair streaming.
- Document any expected allocation budget in `STREAMING.md`.

## API discoverability and naming cleanup

### Goal

Improve public API discovery now that `UnaryClient` and `UnaryServer` support
unary-request/server-streaming-response RPCs.

### Current state

- Existing type names remain `UnaryClient` and `UnaryServer` for compatibility.
- Server streaming is exposed through `call_server_streaming` and
  `add_server_streaming`.

### Pickup notes

- Consider non-breaking aliases such as `GrpcClient` and `GrpcServer`.
- Add crate-level docs that explain "unary" in the legacy names means unary
  request, not necessarily unary response.
- Keep examples in `STREAMING.md` aligned with the preferred names.

### Suggested validation

- Generated rustdoc makes server streaming discoverable from the crate root.
- Existing user code importing `UnaryClient` and `UnaryServer` still compiles.

## Protocol helper cleanup

### Goal

Reduce duplicated unary and server-streaming protocol mapping logic.

### Current state

- Unary and server-streaming handlers both decode request frames and map decode
  errors to gRPC statuses.
- Unary and server-streaming response paths both encode messages and assemble
  metadata/trailers.

### Pickup notes

- Extract small private helpers rather than a broad abstraction:
  - decode request frame to `Result<Req, Status>`;
  - encode response message to a gRPC frame;
  - append metadata headers to response builders;
  - build OK trailers merged with trailing metadata.
- Keep helper names protocol-oriented and local to `server.rs` unless they are
  useful outside the crate.

### Suggested validation

- Existing unary tests still pass.
- Existing server-streaming tests still pass.
- Protocol-limit tests confirm unary and streaming status mappings remain
  consistent.

## Optional stricter `ReceiverStream` termination

### Goal

Offer a stricter channel-backed stream adapter for callers that want sender drop
to be treated as an error rather than clean EOF.

### Current state

- `ReceiverStream::new(receiver)` treats all senders being dropped as clean
  stream completion.
- Producers should send `Err(Status)` before dropping if the stream failed.

### Pickup notes

- Keep the current default clean-on-drop behavior for ergonomics and backwards
  compatibility.
- If strict behavior is useful, add a separate constructor or adapter name
  rather than changing `ReceiverStream::new`.
- Document how producers signal clean completion versus failure.

### Suggested validation

- Clean sender drop still returns OK for the default adapter.
- Strict adapter maps sender drop to a non-OK status.
- Explicit `Err(Status)` from either adapter reaches the client unchanged.
