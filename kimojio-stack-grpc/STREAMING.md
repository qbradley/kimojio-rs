# gRPC Server Streaming

## Overview

`kimojio-stack-grpc` now supports unary-request, server-streaming-response RPCs over the stackful HTTP/2 transport. A client sends one protobuf request and then consumes zero or more ordered protobuf response messages followed by one terminal gRPC status. A server handler decodes one request and returns an explicit response source that the server drives one item at a time.

The design preserves the existing unary API and avoids hidden queues or background tasks. Streaming DATA frames bypass the buffered `Body` response path and are written through explicit HTTP/2 stream methods, so response production remains incremental and bounded by HTTP/2 flow control plus any caller-owned source buffering.

## Architecture and Design

### High-Level Architecture

The implementation has two layers:

- `kimojio-stack-http::h2` exposes incremental response-stream primitives: response headers, DATA events, trailers, and explicit client cancellation.
- `kimojio-stack-grpc` layers typed prost encode/decode, metadata filtering, and `Status` handling on those HTTP/2 events.

Client streaming calls borrow the `UnaryClient` while the stream is active. This keeps the API simple and makes the one-active-stream-per-client contract explicit at compile time. Concurrent server-streaming RPCs are supported by using independent client connections.

Server streaming handlers are registered on the existing `UnaryServer` with `add_server_streaming`. The server writes initial response metadata before the first response message, sends each yielded message as a separate gRPC-framed DATA payload, and finishes with either `grpc-status: 0` plus trailing metadata or a yielded non-OK `Status`.

### Design Decisions

- **No streaming `Body` variant**: HTTP/2 response streaming bypasses `Body` for DATA frames instead of introducing a general streaming body abstraction.
- **Caller-owned buffering**: Channel-backed server streams use caller-provided bounded channels. The gRPC layer does not add unbounded response queues.
- **Bounded frame reassembly**: The client reassembles gRPC frames across arbitrary HTTP/2 DATA boundaries using `ClientConfig::max_message_len` plus the gRPC frame header as the bound.
- **Explicit cancellation**: Clients call `cancel(cx)` to send `RST_STREAM` for a still-open stream. Dropping a stream handle without cancellation does not start hidden cleanup work.
- **Idle producer contract**: The server drives `ServerStream::next` inline. An idle producer blocked on its own source is not woken by a hidden transport reader; it observes cancellation when a later stream operation processes a reset, waits on flow control, or encounters transport I/O errors.

### Integration Points

- `UnaryClient::call_server_streaming` sends the request and returns `ServerStreamingResponse`.
- `ServerStreamingResponse::next` returns `Ok(Some(message))`, `Ok(None)`, or `Err(Error::Status(status))`.
- `ServerStreamingResponse::cancel` sends HTTP/2 `RST_STREAM` for explicit client-side cancellation.
- `UnaryServer::add_server_streaming` registers a handler returning `ServerStreamingReply<S>`.
- `ServerStream<M>` is the producer trait. Closures implementing `FnMut(&RuntimeContext) -> Result<Option<M>, Status>` can be used directly.
- `ReceiverStream<M>` adapts a bounded channel receiver of `Result<M, Status>` into a server response stream.

## User Guide

### Prerequisites

Server-streaming RPCs require an HTTP/2 connection (`h2::ClientConnection` or `h2::ServerConnection`) and prost-compatible request/response message types. Message-size limits use `ClientConfig::max_message_len` and `ServerConfig::max_message_len`.

### Basic Usage

Client code creates a `UnaryClient`, calls `call_server_streaming`, inspects initial metadata, and repeatedly calls `next(cx)` until EOF or error. The stream handle must be dropped before the client can be closed or reused because it holds a mutable borrow of the client.

Server code registers `add_server_streaming` with a handler that returns `ServerStreamingReply::new(stream)`. The stream can be a closure for simple finite streams or `ReceiverStream::new(receiver)` for channel-backed production. Returning `Ok(None)` completes with OK; returning `Err(Status)` terminates with that status.

### Advanced Usage

Use bounded channels for task-backed response production when the producer and server handler are separate stackful tasks. The channel capacity is the explicit application buffer. If the client slows down, HTTP/2 flow control bounds transport writes; if the channel fills, the producer parks in the channel send path.

Use `cancel(cx)` on the client stream when the caller stops early and wants the server to observe cancellation through `RST_STREAM`. This is the low-cost cancellation path; the implementation does not spawn background readers only to detect idle disconnects, so server-side observation occurs when stream operations process the reset, wait on flow control, or hit transport I/O errors.

## API Reference

### Key Components

- `ServerStreamingResponse<'a, M>`: client-side stream handle for messages of type `M`.
- `UnaryClient::call_server_streaming`: starts a unary-request/server-streaming-response RPC.
- `ServerStreamingResponse::next`: consumes one message or terminal outcome.
- `ServerStreamingResponse::cancel`: explicitly cancels a stream.
- `UnaryServer::add_server_streaming`: registers a server-streaming handler.
- `ServerStreamingReply<S>`: server handler return value with initial metadata, stream source, and clean-completion trailers.
- `ServerStream<M>`: stackful response source trait.
- `ReceiverStream<M>`: bounded-channel adapter for `Result<M, Status>` items.

### Configuration Options

- `ClientConfig::max_message_len`: maximum decoded streamed response message size and request size for the client.
- `ServerConfig::max_message_len`: maximum decoded request size and encoded streamed response message size for the server.
- HTTP/2 settings and flow-control behavior remain configured through `HttpConfig`/HTTP/2 connection setup.

## Testing

### How to Test

The test suite covers:

- Local stackful server-streaming client/server behavior.
- gRPC frame reassembly where one message is split across DATA frames and multiple messages share one DATA frame.
- Empty streams with initial metadata.
- Trailer-carried and header-carried terminal statuses.
- Explicit cancellation and server-side reset observation.
- Stackful client interoperability with Tonic server-streaming services.
- Tonic client interoperability with stackful server-streaming handlers.
- Streaming protocol-limit errors for oversized messages and invalid terminal statuses.

Run focused validation with:

```sh
cargo test -p kimojio-stack-grpc --all-targets
cargo test -p kimojio-stack-http --all-targets
```

### Edge Cases

- A stream can complete without messages and still expose initial metadata.
- Terminal non-OK status after prior messages is returned exactly once as `Error::Status`.
- Header-carried terminal status is handled for immediate no-DATA responses.
- Client-side oversized response frames are rejected before decoding the payload.
- Client cancellation is explicit and low-cost through `RST_STREAM`.

## Limitations and Future Work

- Client streaming and bidirectional streaming are not implemented.
- One active server-streaming response is supported per `UnaryClient`; use independent client connections for concurrent streaming RPCs.
- No custom per-message compression API is provided.
- The implementation does not include a hidden cancellation reader for idle producers. Applications needing immediate idle cancellation should wire their own cancellation signal into the producer source.
- No new streaming benchmark was added; existing tests validate behavior and the documented buffering model. A streaming-specific benchmark can be added later if needed without changing the API.
