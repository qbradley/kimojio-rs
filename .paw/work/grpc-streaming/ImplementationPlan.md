# gRPC Server Streaming Implementation Plan

## Overview

Add server-side streaming support for `kimojio-stack-grpc`: one protobuf request followed by an ordered stream of protobuf responses and one terminal gRPC status. The architecture uses explicit HTTP/2 streaming handles that write and read DATA frames incrementally, then layers typed gRPC client/server stream APIs on top. Existing unary APIs remain fully buffered and unchanged.

The implementation will preserve the kimojio performance model by avoiding hidden queues, avoiding precomputing response collections, preserving HTTP/2 flow-control blocking points, and making producer-side buffering caller-owned and explicit. HTTP/2 streaming will intentionally bypass `Body` for response DATA frames instead of adding a streaming `Body` variant; existing fully buffered `Body` behavior remains the unary path. Cancellation will be detected when stream operations process resets, wait on flow control, or encounter transport errors, without adding a hidden background reader for idle producers.

## Current State Analysis

- `UnaryClient::call` currently encodes one request, sends it through `h2::ClientConnection::send_request`, then waits for `read_response_with_trailers` to return a fully buffered body before decoding one message (`.paw/work/grpc-streaming/CodeResearch.md:66-79`).
- `UnaryServer::add_unary` stores boxed unary handlers whose closure returns one `UnaryReply`; `serve_one` accepts one complete request and writes either one reply or one status (`.paw/work/grpc-streaming/CodeResearch.md:81-92`).
- The gRPC codec already encodes and decodes independent 5-byte-prefixed gRPC message frames, which is suitable for streaming individual messages (`.paw/work/grpc-streaming/CodeResearch.md:94-100`).
- HTTP/2 response send and receive paths are currently fully buffered: server response sending writes headers, all DATA, trailers, and removes stream state in one call; client response reading accumulates all DATA before returning (`.paw/work/grpc-streaming/CodeResearch.md:131-149`).
- HTTP/2 flow-control state already tracks per-stream and connection windows and outbound capacity; DATA send loops block by reading inbound frames when outbound capacity is zero (`.paw/work/grpc-streaming/CodeResearch.md:151-169`, `.paw/work/grpc-streaming/CodeResearch.md:332-339`).
- Bounded stack channels and cross-thread channels provide explicit producer backpressure for tests and callers that want channel-backed response streams (`.paw/work/grpc-streaming/CodeResearch.md:27-28`, `.paw/work/grpc-streaming/CodeResearch.md:222-229`).

## Desired End State

- `kimojio-stack-http` exposes explicit HTTP/2 response streaming primitives that let callers send response headers, send one DATA chunk at a time under flow control, send trailers, and read response headers/DATA/trailers incrementally on the client side.
- `kimojio-stack-grpc` exposes typed server-streaming client and server APIs for unary-request, streaming-response RPCs.
- Client code can call a server-streaming RPC, inspect initial metadata, call a blocking `next(cx)`-style method to receive ordered messages, cancel a still-open stream explicitly, and observe clean EOF or terminal `Status` errors. Initial support targets one active streaming response per `UnaryClient` at a time; concurrent server-streaming RPC coverage uses independent client connections while HTTP/2 primitives continue to support multiple stream IDs on one connection.
- Server code can register stream handlers that return an explicit response stream source; each yielded item is sent only when the transport is ready to accept it.
- Tests cover local stackful streaming, Tonic client/server interoperability, terminal status propagation, metadata, cancellation/disconnect behavior, concurrent streams, and non-regression of unary behavior.
- Technical documentation records the as-built API, behavior, and verification approach.

## What We're NOT Doing

- Client-streaming RPCs.
- Bidirectional-streaming RPCs.
- Custom per-message compression APIs.
- Arrow Flight-style or other advanced exchange protocols.
- Replacing `Body` with a general streaming body abstraction.
- Hidden background tasks or unbounded internal response queues inside the gRPC streaming layer.
- General-purpose same-connection multiplexed gRPC stream handles that can be polled independently through one `UnaryClient`.
- Remote push or remote PR creation.

## Phase Status

- [x] **Phase 1: HTTP/2 Response Stream Primitives** - Add explicit incremental server response send and client response read handles while preserving existing unary HTTP/2 APIs.
- [x] **Phase 2: gRPC Client Streaming API** - Add typed unary-request/server-streaming client support with metadata and terminal status propagation.
- [x] **Phase 3: gRPC Server Streaming API** - Add typed server registration and explicit response stream production using the HTTP/2 stream writer.
- [x] **Phase 4: Interop, Cancellation, and Concurrency Tests** - Extend local and Tonic interop coverage for streaming edge cases and concurrent streams.
- [x] **Phase 5: Documentation and Validation** - Add Docs.md, update project docs, and run full repository validation.

## Phase Candidates

---

## Phase 1: HTTP/2 Response Stream Primitives

### Changes Required:

- **`kimojio-stack-http/src/h2/server.rs`**: Add a response stream writer API parallel to `send_response_with_trailers`.
  - Use explicit stream writer methods that bypass `Response<Body>` for DATA frames; only response headers and trailers reuse existing header conversion helpers.
  - Begin a response by sending response HEADERS without ending the stream unless the caller explicitly finishes immediately.
  - Send one DATA chunk at a time through the same outbound capacity loop used by the current `write_data` path, preserving per-frame limits and flow-control blocking.
  - Finish with optional trailer HEADERS using `END_STREAM`, then remove stream state only after stream completion.
  - Surface peer reset, GOAWAY, EOF, or transport errors through returned `Error` values instead of silently treating them as success.
- **`kimojio-stack-http/src/h2/client.rs`**: Add incremental response read support.
  - Read and return response headers for a target stream without waiting for the entire body.
  - Read subsequent stream events for the target stream: DATA chunk, trailers/end, reset/error.
  - Add an explicit client-side stream cancellation path that sends `RST_STREAM` for an open response stream and removes local stream state.
  - Continue processing unrelated SETTINGS, WINDOW_UPDATE, PING, GOAWAY, and other-stream frames so concurrent streams remain usable.
  - Return inbound connection and stream flow-control credit only after each DATA chunk is consumed by the caller.
- **`kimojio-stack-http/src/h2/mod.rs`**: Re-export new HTTP/2 stream event/handle types.
- **Tests: `kimojio-stack-http/src/h2/mod.rs`**: Add socketpair tests for incremental response DATA, response trailers, flow-control blocking/unblocking, and concurrent streams using the new primitives.

### Success Criteria:

#### Automated Verification:
- [ ] Tests pass: `cargo test -p kimojio-stack-http h2_`
- [ ] Lint/typecheck: `cargo clippy -p kimojio-stack-http --all-targets -- -D warnings`

#### Manual Verification:
- [ ] Existing `send_response_with_trailers` and `read_response_with_trailers` behavior remains unchanged.
- [ ] Incremental DATA send path does not require constructing a complete `Body`.
- [ ] Stream state is removed exactly once after terminal trailers or reset/error.
- [ ] Client cancellation sends `RST_STREAM` for an open response stream and does not require draining the remaining response.
- [ ] Slow-reader behavior is bounded by existing HTTP/2 flow-control windows.

---

## Phase 2: gRPC Client Streaming API

### Changes Required:

- **`kimojio-stack-grpc/src/client.rs`**: Add typed server-streaming client types and methods.
  - Add a streaming response type that owns the target HTTP/2 stream state through the existing `UnaryClient`.
  - Add a client call method for unary-request/server-streaming RPCs that sends one encoded request and validates initial response headers/content-type.
  - Expose initial metadata from response headers.
  - Expose `next(cx)`-style message consumption that decodes one gRPC frame per response item, returns `Ok(Some(message))` for messages, `Ok(None)` for clean terminal OK, and `Err(Error::Status(status))` for terminal non-OK status.
  - Expose an explicit `cancel(cx)` path for callers that want to stop an active stream before terminal status.
  - Preserve unary status-location behavior: header-carried `grpc-status` with `END_STREAM` is a terminal outcome, trailer-carried status is a terminal outcome, and status is reported exactly once.
  - Maintain a bounded gRPC frame reassembly buffer in the streaming response handle so HTTP/2 DATA boundaries do not need to align with gRPC message boundaries.
  - Bound the reassembly buffer by `ClientConfig::max_message_len` plus the 5-byte gRPC frame header; return the existing size-limit error path when a streamed frame exceeds that bound.
  - Preserve trailers metadata for callers after stream completion.
  - Document and test the initial client concurrency model: one active streaming response per `UnaryClient`, with concurrent streaming RPCs supported through separate client connections in this phase.
  - Preserve `UnaryClient::call` behavior and signatures.
- **`kimojio-stack-grpc/src/lib.rs`**: Re-export new client streaming response type(s).
- **`kimojio-stack-grpc/src/codec.rs`**: Reuse existing frame encode/decode functions; only add small helpers if needed for partial frame boundaries discovered during implementation.
- **Tests: `kimojio-stack-grpc/src/lib.rs`**: Add local stackful client tests for ordered messages, empty stream, terminal OK, and terminal non-OK status.
  - Include one test where a stream completes with initial metadata and no messages.
  - Include one test for a trailers-only or header-carried terminal status with no DATA frames.

### Success Criteria:

#### Automated Verification:
- [ ] Tests pass: `cargo test -p kimojio-stack-grpc streaming`
- [ ] Lint/typecheck: `cargo clippy -p kimojio-stack-grpc --all-targets -- -D warnings`

#### Manual Verification:
- [ ] Client reads messages incrementally and does not wait for all responses before returning the first message.
- [ ] Initial metadata is available before the first message is consumed.
- [ ] Terminal status is reported exactly once.
- [ ] One gRPC message split across multiple HTTP/2 DATA frames and multiple gRPC messages coalesced into one DATA frame are both decoded correctly.
- [ ] Immediate OK and immediate non-OK streams with no DATA frames are handled through the same terminal status path as unary responses.
- [ ] Existing unary client tests still pass unchanged.

---

## Phase 3: gRPC Server Streaming API

### Changes Required:

- **`kimojio-stack-grpc/src/server.rs`**: Add server-streaming registration and dispatch.
  - Extend server handler storage to distinguish unary and server-streaming handlers for paths.
  - Add a typed `add_server_streaming` registration method with one decoded request and an explicit response producer return type.
  - Add a server response stream trait or concrete adapter API that can yield `Result<Message, Status>` one item at a time from stackful code.
  - Write response metadata headers before yielding response messages.
  - Encode and send each yielded message as a separate gRPC frame through the HTTP/2 stream writer.
  - On success, send `grpc-status: 0` trailers merged with caller trailers; on yielded status error, send that status as terminal trailers.
  - Map request decode/validation errors to status-only responses as unary does today.
  - Return transport reset/disconnect errors to the serving caller when stream operations process resets, wait on flow control, or encounter transport read/write errors so handler-owned producers can stop.
  - Document that idle producers blocked on caller-owned sources are not woken by hidden background readers; they observe cancellation when a later stream operation processes a reset, waits on flow control, encounters transport errors, or when the caller wires cancellation into its own source.
- **`kimojio-stack-grpc/src/lib.rs`**: Re-export new server streaming reply/source types.
- **Tests: `kimojio-stack-grpc/src/lib.rs`**: Add local stackful server tests using explicit bounded channel or scope-spawned producer patterns.
  - Include an idle stream source that initially has no message, parks without placeholders, later produces a message, and then completes.

### Success Criteria:

#### Automated Verification:
- [ ] Tests pass: `cargo test -p kimojio-stack-grpc streaming`
- [ ] Lint/typecheck: `cargo clippy -p kimojio-stack-grpc --all-targets -- -D warnings`

#### Manual Verification:
- [ ] Handler response production is incremental; the server does not collect all messages before sending.
- [ ] Channel-backed production is explicitly bounded by caller-provided channel capacity.
- [ ] Disconnect/reset during streaming is observable as an error from stream sending or serving.
- [ ] A pending idle stream suspends without busy-waiting or placeholder responses and resumes only when a message or terminal status is produced.
- [ ] Existing unary server tests still pass unchanged.

---

## Phase 4: Interop, Cancellation, and Concurrency Tests

### Changes Required:

- **`kimojio-stack-grpc/tests/interop_proto.rs`**: Extend the shared Tonic test service with a server-streaming path and helper client call.
- **`kimojio-stack-grpc/tests/client_interop.rs`**: Add stackful client to Tonic server-streaming tests covering ordered messages, metadata, clean EOF, and status error.
- **`kimojio-stack-grpc/tests/server_interop.rs`**: Add Tonic client to stackful server-streaming tests covering ordered messages, metadata, clean EOF, and status error.
- **`kimojio-stack-grpc/tests/protocol_limits.rs`**: Add streaming protocol tests for oversized streamed message, invalid terminal status, and missing/invalid content-type where relevant.
- **`kimojio-stack-http/src/h2/mod.rs` or `kimojio-stack-grpc/src/lib.rs`**: Add cancellation and concurrent streaming tests that do not require network timing assumptions.
  - Concurrency tests should use independent client connections for gRPC wrapper behavior, matching the spec contract.
  - Cancellation tests should cover explicit client cancellation before the next message and server observation on the next stream interaction.
- **`kimojio-stack-grpc/src/lib.rs`**: Add a streaming allocation baseline test if the implemented hot path introduces measurable per-message allocation behavior beyond the existing unary baseline; otherwise record the no-new-baseline decision in Docs.md with measured evidence from the test suite.
- **`kimojio-stack-grpc/benches/unary_rpc.rs` or a new bench file**: Add a lightweight streaming benchmark only if it can reuse existing benchmark infrastructure without broad scope expansion.

### Success Criteria:

#### Automated Verification:
- [ ] Tests pass: `cargo test -p kimojio-stack-grpc --all-targets`
- [ ] Tests pass: `cargo test -p kimojio-stack-http --all-targets`
- [ ] Lint/typecheck: `cargo clippy --all-targets -- -D warnings`

#### Manual Verification:
- [ ] Stackful client interoperates with Tonic server-streaming responses.
- [ ] Tonic client interoperates with stackful server-streaming responses.
- [ ] Multiple concurrent server-streaming RPCs preserve per-stream ordering and independent terminal outcomes.
- [ ] Cancellation/disconnect behavior is covered without relying on unbounded sleeps or timing-sensitive assertions.
- [ ] Metadata on zero-message streams and trailers-only/header-carried terminal statuses are covered.
- [ ] Streaming allocation behavior is measured or explicitly documented as covered by existing hot-path allocation tests.

---

## Phase 5: Documentation and Validation

### Changes Required:

- **`.paw/work/grpc-streaming/Docs.md`**: Technical as-built reference describing public API, stream lifecycle, metadata/status behavior, backpressure/cancellation behavior, and test coverage.
- **`docs/stack-http-grpc.md`**: Update project documentation to replace the current note that streaming callers must use lower-level HTTP/2 APIs directly, and document the new server-streaming gRPC capability at the same concise level as existing HTTP/gRPC docs.
- **`README.md` or crate docs**: Update only if implementation introduces a public API that is already summarized there.
- **Validation**: Run repository-required format, clippy, feature tests, and doc tests.

### Success Criteria:

#### Automated Verification:
- [ ] Format: `cargo fmt --all -- --check`
- [ ] Lint/typecheck: `cargo clippy --all-targets -- -D warnings`
- [ ] Lint/typecheck all features: `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] Tests: `cargo test --all-targets --features ssl2`
- [ ] Tests: `cargo test --all-targets --features virtual-clock`
- [ ] Docs: `cargo test --doc`

#### Manual Verification:
- [ ] Docs accurately describe the implemented API and observable behavior.
- [ ] Documentation notes server-side streaming scope and does not imply client-streaming or bidirectional support.
- [ ] Workflow stops locally after final review; no push or remote PR is created.

---

## References

- Issue: none
- Spec: `.paw/work/grpc-streaming/Spec.md`
- Research: `.paw/work/grpc-streaming/CodeResearch.md`
