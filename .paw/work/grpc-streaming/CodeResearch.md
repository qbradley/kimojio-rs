---
date: 2026-06-08T08:40:08.563+00:00
git_commit: "b75f05e32a12c8b7a2382c2c8e0aa417856467ef"
branch: feature/grpc-streaming
repository: kimojio-rs
topic: "gRPC server-side streaming support code research"
tags: [research, codebase, grpc, http2, kimojio-stack, streaming, flow-control, channels]
status: complete
last_updated: 2026-06-08
---

# Research: gRPC Server-Side Streaming Support

## Research Question

Map the existing `kimojio-stack-grpc` crate structure (public API, client call path, server handler path, status/metadata handling, protobuf body encoding/decoding), the `kimojio-stack-http` HTTP/2 request/response/body streaming and flow-control primitives, the scheduler/wait/channel/task patterns available for async stream production and consumption without unbounded buffering, and the tests and examples that must be extended to support server-side streaming RPCs.

## Summary

- `kimojio-stack-grpc` currently exposes a unary-only public API: `UnaryClient::call` encodes one request, sends it, and blocks until one response arrives; `UnaryServer::add_unary` registers a handler that decodes one request, calls a closure, and returns one reply (`kimojio-stack-grpc/src/lib.rs:13-18`, `kimojio-stack-grpc/src/client.rs:46-62`, `kimojio-stack-grpc/src/server.rs:44-76`).
- The gRPC codec already handles the 5-byte gRPC data frame header (1-byte compression flag + 4-byte big-endian message length) for encoding and decoding individual frames; it is not tied to any streaming model (`kimojio-stack-grpc/src/codec.rs:9-109`).
- The `Body` type in `kimojio-stack-http` is a fully-buffered `Bytes` value; there is no incremental or streaming body abstraction today (`kimojio-stack-http/src/body.rs:39-75`). `BodyBuilder` accumulates chunks for receive-side incremental construction but still finalizes to a single `Body` (`kimojio-stack-http/src/body.rs:77-104`).
- `h2::ServerConnection::send_response_with_trailers` sends response headers, all body DATA frames, and optional trailer HEADERS in one synchronous call, blocking on transport flow control (`kimojio-stack-http/src/h2/server.rs:103-148`).
- `h2::ClientConnection::read_response_with_trailers` accumulates all DATA frames into a single completed `Body` before returning; it does not expose individual frames to the caller (`kimojio-stack-http/src/h2/client.rs:128-160`).
- `ConnectionState` exposes per-stream and per-connection `FlowControlWindow`, a `pending_outbound: VecDeque<Frame>` for queued control frames, `outbound_capacity(stream_id)`, and `queue_stream_window_update_to_target` — these primitives are already sufficient to back incremental frame-by-frame sending with bounded window usage (`kimojio-stack-http/src/h2/connection.rs:16-312`).
- `Stream` tracks `state: StreamState`, `inbound_window`, `outbound_window`, `received_body_len`, and `trailers: Option<Vec<Header>>`; state transitions for `send_data(end_stream)` and `receive_trailers` already exist (`kimojio-stack-http/src/h2/stream.rs:98-289`).
- `kimojio-stack` provides bounded, unbounded, once, and cross-thread channels; bounded `channel::bounded` parks senders on a full queue and wakes one receiver after each send, providing explicit backpressure for stream production (`kimojio-stack/src/channel/bounded.rs:13-239`).
- `Scope::spawn` / `JoinHandle::join` and `RuntimeContext::park()` / `cx.waiter()` are the native blocking-coroutine coordination primitives; cooperative parks suspend only the current coroutine and never block the OS thread (`kimojio-stack/src/lib.rs:1781-1877`).
- Existing tests cover unary RPC round-trips against live Tonic peers (TLS and plaintext), flow-control unblocking via WINDOW_UPDATE, RST_STREAM and GOAWAY error propagation, metadata encode/decode, and allocation hot-path measurements.

## Documentation System

- **Framework**: Plain Markdown plus Rustdoc/Cargo docs.
- **Docs Directory**: `docs/` at repo root; crate-local docs under `kimojio-stack-tls/docs/`.
- **Navigation Config**: N/A; documentation linked from `README.md`.
- **Style Conventions**: H1/H2/H3 headings, fenced code blocks, bullet lists, tables.
- **Build Command**: `cargo +<toolchain> test --doc` (`.github/workflows/rust.yml:43-44`).
- **Standard Files**: `README.md`, `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md`, `LICENSE.txt`.

## Verification Commands

- **Format Command**: `cargo fmt --all -- --check` (`.github/workflows/rust.yml:46-47`).
- **Lint Command**: `cargo clippy --all-targets -- -D warnings` and `cargo clippy --all-targets --all-features -- -D warnings` (`.github/workflows/rust.yml:34-50`).
- **Test Command**: `cargo test --all-targets --features ssl2`, `cargo test --all-targets --features virtual-clock`, `cargo test --doc` (`.github/workflows/rust.yml:37-44`).
- **Build Command**: CI uses clippy-based build steps; example binaries: `cargo build --release -p examples --bin tls-echo-server --features tls`.
- **Toolchain**: Pinned at Rust `1.92.0` in `rust-toolchain.toml`; CI matrix tests `1.90.0` and `1.92.0`.

---

## Detailed Findings

### 1. `kimojio-stack-grpc` Crate Structure

#### 1.1 Module Layout and Public API

- The crate root declares six public submodules: `client`, `codec`, `error`, `metadata`, `server`, `status` (`kimojio-stack-grpc/src/lib.rs:6-11`).
- The crate re-exports its entire public surface at the crate root (`kimojio-stack-grpc/src/lib.rs:13-18`):
  - `ClientConfig`, `UnaryClient`, `UnaryResponse` from `client`
  - `decode_message`, `encode_message` from `codec`
  - `Error`, `ErrorKind` from `error`
  - `Metadata` from `metadata`
  - `ServerConfig`, `UnaryReply`, `UnaryServer` from `server`
  - `Status`, `StatusCode` from `status`
- A test-only `CountingAllocator` is installed as `#[global_allocator]` under `#[cfg(test)]`, identical in structure to the one in `kimojio-stack-http` (`kimojio-stack-grpc/src/lib.rs:20-123`).

#### 1.2 Client Call Path

- `UnaryClient` wraps a `h2::ClientConnection` and `ClientConfig` (`kimojio-stack-grpc/src/client.rs:36-43`).
- `ClientConfig::max_message_len` defaults to 4 MiB; `ClientConfig` is `Copy` (`kimojio-stack-grpc/src/client.rs:14-26`).
- `UnaryResponse<M>` holds `metadata: Metadata`, `message: M`, `status: Status`, `trailers: Metadata` (`kimojio-stack-grpc/src/client.rs:28-34`).
- `UnaryClient::call<Req, Resp>` executes the full unary path in five steps (`kimojio-stack-grpc/src/client.rs:46-62`):
  1. `codec::encode_message(request, max_message_len)` → `Bytes` gRPC frame.
  2. `build_request(path, metadata, body)` → `Request<Body>` with `POST`, `Content-Type: application/grpc`, `TE: trailers`, and metadata headers.
  3. `self.http.send_request(cx, &http_request)` → `StreamId`.
  4. `self.http.read_response_with_trailers(cx, stream_id)` → `ResponseWithTrailers` (blocks until all data arrives).
  5. `parse_response(response, max_message_len)` → validates HTTP status code and content-type, resolves gRPC status from trailers (or response headers as fallback), builds `UnaryResponse`.
- `response_status` falls back to checking response headers when trailers do not contain `grpc-status` — this handles the trailers-only pattern (`kimojio-stack-grpc/src/client.rs:117-127`).
- `validate_content_type` accepts `application/grpc` or any `application/grpc+` prefix (`kimojio-stack-grpc/src/client.rs:129-143`).
- `UnaryClient::close` delegates to `self.http.close(cx)` (`kimojio-stack-grpc/src/client.rs:64-66`).

#### 1.3 Server Registration and Handler Path

- `UnaryServer` stores `handlers: BTreeMap<String, Box<dyn UnaryHandler>>` and `config: ServerConfig` (`kimojio-stack-grpc/src/server.rs:31-34`).
- `ServerConfig::max_message_len` defaults to 4 MiB (`kimojio-stack-grpc/src/server.rs:18-28`).
- `UnaryServer::add_unary<Req, Resp, F>` accepts any `F: for<'a> Fn(&'a RuntimeContext<'a>, Metadata, Req) -> Result<UnaryReply<Resp>, Status> + 'static`, wraps it in a `TypedUnaryHandler`, and boxes it (`kimojio-stack-grpc/src/server.rs:44-60`).
- `UnaryServer::serve_one(cx, http)` calls `http.accept(cx)` → dispatches → writes reply or status; returns `Ok(false)` when no more requests are available (`kimojio-stack-grpc/src/server.rs:62-76`).
- `dispatch` validates the request (POST, correct content-type, valid TE), looks up the path in `handlers`, extracts metadata, and calls `handler.call(cx, metadata, frame_bytes)` (`kimojio-stack-grpc/src/server.rs:78-91`).
- `validate_request` returns `Status` errors for non-POST, missing/wrong content-type, and invalid TE header (`kimojio-stack-grpc/src/server.rs:167-192`).
- `TypedUnaryHandler::call` decodes the request frame via `codec::decode_message`, calls the user closure, and `encode_bytes`-encodes the response message (`kimojio-stack-grpc/src/server.rs:137-165`). The encode stage uses `reply.message.encode(&mut Vec)` followed by `codec::encode_bytes`, not `encode_message`, because it avoids a second `prost::Message::encoded_len` call.
- `write_reply` sends `200 OK` + `Content-Type: application/grpc` + metadata headers + body + `Status::ok().to_trailers()` merged with reply trailers (`kimojio-stack-grpc/src/server.rs:198-225`).
- `write_status` sends `200 OK` + `Content-Type: application/grpc` + empty body + `status.to_trailers()` (`kimojio-stack-grpc/src/server.rs:227-241`).
- `UnaryReply<M>` holds `metadata: Metadata`, `message: M`, `trailers: Metadata`; constructor `new(message)` leaves both metadata fields empty (`kimojio-stack-grpc/src/server.rs:94-108`).

#### 1.4 Protobuf Body Encoding and Decoding (codec.rs)

- Frame constants: `HEADER_LEN = 5`, `UNCOMPRESSED_FLAG: u8 = 0` (`kimojio-stack-grpc/src/codec.rs:9-10`).
- `encode_message<M: Message>(message, max_len)` allocates a `BytesMut` of capacity `5 + encoded_len`, writes compression flag byte, big-endian u32 length, then calls `message.encode(&mut bytes)` (`kimojio-stack-grpc/src/codec.rs:13-36`).
- `decode_message<M: Message + Default>(frame, max_len)` checks frame length ≥ 5, validates compression flag == 0, reads big-endian u32 length, validates payload length matches, calls `M::decode(&frame[5..])` (`kimojio-stack-grpc/src/codec.rs:39-66`).
- `encode_bytes` and `decode_bytes` are raw byte variants that bypass prost encode/decode, used by `TypedUnaryHandler` to avoid re-encoding already-encoded bytes (`kimojio-stack-grpc/src/codec.rs:68-109`).
- Errors: `Error::SizeLimit` for length checks, `Error::UnsupportedCompression(flag)` for non-zero flags, `Error::Protocol` for malformed headers, `Error::Decode(prost::DecodeError)` for prost failures (`kimojio-stack-grpc/src/codec.rs:18-65`).

#### 1.5 Status and Metadata Handling

- `StatusCode` is a `#[repr(u8)]` enum with 17 variants (Ok=0 through Unauthenticated=16); `as_grpc_code()` returns the numeric value and `from_grpc_code(u8) -> Option<Self>` maps back (`kimojio-stack-grpc/src/status.rs:22-70`).
- `Status` holds `code: StatusCode`, `message: String`, `details: Bytes`, and `metadata: Box<Metadata>` (`kimojio-stack-grpc/src/status.rs:73-79`). `Box<Metadata>` avoids large stack size.
- `Status::to_trailers` encodes into `Trailers`: metadata headers first, then `grpc-status`, then percent-encoded `grpc-message` (if non-empty), then base64 `grpc-status-details-bin` (if non-empty) (`kimojio-stack-grpc/src/status.rs:137-166`).
- `grpc-message` percent-encoding: ASCII printable bytes 0x20–0x7E (except `%`) are emitted as-is; all others are `%XX` hex-encoded (`kimojio-stack-grpc/src/status.rs:218-232`).
- Binary metadata header names must end with `-bin`; their values are base64-encoded/decoded with STANDARD (pad) or STANDARD_NO_PAD fallback (`kimojio-stack-grpc/src/status.rs:266-270`, `kimojio-stack-grpc/src/metadata.rs:32-42`).
- `Metadata` wraps `HeaderMap`; `insert` / `get` for ASCII metadata, `insert_bin` / `get_bin` for binary; name validation rejects reserved names (`:*`, `connection`, `te`, `transfer-encoding`, `upgrade`, `content-type`, `grpc-status`, `grpc-message`, `grpc-status-details-bin`) (`kimojio-stack-grpc/src/metadata.rs:23-104`).
- `Metadata::from_headers` iterates the header map, skips continuation entries (None name key), filters out reserved transport names (`content-type`, `te`, `grpc-status`, `grpc-message`, `grpc-status-details-bin`), and validates names (`kimojio-stack-grpc/src/metadata.rs:68-85`).
- Three public status header name constants: `GRPC_STATUS`, `GRPC_MESSAGE`, `GRPC_STATUS_DETAILS_BIN` (`kimojio-stack-grpc/src/status.rs:16-18`).

---

### 2. `kimojio-stack-http` HTTP/2 Primitives

#### 2.1 `Body`, `BodyBuilder`, and `BodyLimits`

- `Body` holds a single `Bytes`; it is never incrementally readable — `as_bytes()` returns the whole body, `into_bytes()` consumes it (`kimojio-stack-http/src/body.rs:39-75`).
- `Body::empty()` creates a zero-length body; `Body::from_bytes(bytes, limits)` checks the limit at construction; `Body::copy_from_slice` copies and checks (`kimojio-stack-http/src/body.rs:45-74`).
- `BodyBuilder` accumulates `&[u8]` chunks into a `BytesMut` via `append`, checks the limit on each call, and finalizes via `finish() -> Body` (`kimojio-stack-http/src/body.rs:77-104`). Used by both `ClientConnection::process_data` and `ServerConnection::process_data` to accumulate inbound DATA frames.
- `BodyLimits::max_len` default is 4 MiB; a limit of `usize::MAX` is used internally to bypass the check (`kimojio-stack-http/src/body.rs:14-36`).

#### 2.2 `StackTransport`

- `StackTransport` is an enum: `Plaintext(OwnedFd)` or `Tls(TlsStream)` (behind the `tls` feature flag) (`kimojio-stack-http/src/transport.rs:12-16`).
- Methods: `read(cx, buf) -> Result<usize, Error>`, `write(cx, buf) -> Result<usize, Error>`, `read_exact_or_eof(cx, buf) -> Result<usize, Error>`, `write_all(cx, buf) -> Result<(), Error>`, `shutdown_write(cx)`, `shutdown(cx)`, `close(self, cx)` (`kimojio-stack-http/src/transport.rs:28-93`).
- `write_all` returns `Err(Error::io(Errno::PIPE))` on zero-byte writes (treat as peer closed) (`kimojio-stack-http/src/transport.rs:60-69`).
- `peer_closed` in `h2/server.rs` matches `Error::Io(Errno::PIPE) | Error::Tls(Errno::PIPE)` (`kimojio-stack-http/src/h2/server.rs:432-434`).

#### 2.3 `h2::ServerConnection` — Response Writing and Flow Control

- `ServerConnection` stores: `transport: StackTransport`, `state: ConnectionState`, `pending: BTreeMap<StreamId, PendingRequest>`, `completed: VecDeque<IncomingRequest>`, `goaway: Option<Error>` (`kimojio-stack-http/src/h2/server.rs:37-44`).
- `accept(cx)` checks `completed` first (no I/O), then loops reading frames until a request completes or EOF/GOAWAY (`kimojio-stack-http/src/h2/server.rs:69-92`).
- `send_response_with_trailers(cx, stream_id, response, trailers)` performs one atomic flush of the entire response in sequence: response HEADERS, all body DATA (flow-control loop), optional trailer HEADERS, then calls `state.remove_stream(stream_id)` (`kimojio-stack-http/src/h2/server.rs:103-148`).
- `write_data` loops: if `outbound_capacity(stream_id) == 0`, reads an inbound frame (which may carry WINDOW_UPDATE), then retries; otherwise writes a DATA frame bounded by `min(remaining, capacity, peer_max_frame_size)` (`kimojio-stack-http/src/h2/server.rs:243-284`).
- After `send_response_with_trailers` returns, `remove_stream` moves the stream to `closed_streams`; the stream is no longer in `state.streams` (`kimojio-stack-http/src/h2/connection.rs:150-156`).
- `reset_stream(cx, stream_id, error_code)` sends RST_STREAM and does not remove the stream from `state` (`kimojio-stack-http/src/h2/server.rs:164-176`).
- `goaway(cx, last_stream_id, error_code)` sends GOAWAY frame (`kimojio-stack-http/src/h2/server.rs:178-190`).
- `shutdown_write_and_close_after_peer(cx)` half-closes writes, drains until peer EOF, then closes (`kimojio-stack-http/src/h2/server.rs:197-215`).

#### 2.4 `h2::ClientConnection` — Response Reading and Flow Control

- `ClientConnection` stores: `transport`, `state`, `next_stream_id: u32`, `pending: BTreeMap<StreamId, PendingResponse>`, `completed: BTreeMap<StreamId, ResponseWithTrailers>`, `reset_streams: BTreeMap<StreamId, Error>`, `goaway: Option<Error>` (`kimojio-stack-http/src/h2/client.rs:38-48`).
- `send_request(cx, request)` sends HEADERS + DATA (with flow-control loop) and returns the `StreamId`; it does not wait for a response (`kimojio-stack-http/src/h2/client.rs:84-117`).
- `read_response_with_trailers(cx, stream_id)` checks `completed` and `reset_streams` first, then loops reading frames until the target stream completes or errors (`kimojio-stack-http/src/h2/client.rs:128-160`).
- `process_data` accumulates DATA frames into `PendingResponse::body` via `BodyBuilder::append`; it issues WINDOW_UPDATE for the connection (always) and for the stream (when below threshold) after each DATA frame (`kimojio-stack-http/src/h2/client.rs:337-366`).
- `complete_response` removes the entry from `pending` and inserts into `completed` only when both headers and the terminal DATA/trailer frame arrive (`kimojio-stack-http/src/h2/client.rs:368-386`).
- `ResponseWithTrailers` exposes `response: Response<Body>` and `trailers: Trailers` (`kimojio-stack-http/src/h2/client.rs:17-20`).

#### 2.5 `ConnectionState` — Flow Control and Stream Registry

- `ConnectionState` holds: `local_settings: Settings`, `peer_settings: Settings`, `streams: BTreeMap<StreamId, Stream>`, `closed_streams: BTreeSet<StreamId>`, `connection_window: FlowControlWindow`, `inbound_connection_window: FlowControlWindow`, `pending_outbound: VecDeque<Frame>`, `header_encoder/decoder` (`kimojio-stack-http/src/h2/connection.rs:16-28`).
- `outbound_capacity(stream_id) -> Result<usize>`: returns `min(connection_window.available(), stream.outbound_window.available()).max(0)` (`kimojio-stack-http/src/h2/connection.rs:166-176`).
- `consume_outbound_window(stream_id, amount)`: decrements both connection and stream windows (`kimojio-stack-http/src/h2/connection.rs:178-189`).
- `queue_connection_window_update(amount)` / `queue_stream_window_update(stream_id, amount)`: push WINDOW_UPDATE frames onto `pending_outbound`, increase the corresponding inbound window (`kimojio-stack-http/src/h2/connection.rs:191-224`).
- `queue_stream_window_update_to_target(stream_id)`: sends WINDOW_UPDATE when inbound stream window falls below `initial_window_size / 2` (`kimojio-stack-http/src/h2/connection.rs:226-251`).
- `queue_frame(frame)`: pushes any arbitrary frame onto `pending_outbound` — the general-purpose queuing hook (`kimojio-stack-http/src/h2/connection.rs:305-307`).
- `pop_outbound()`: pops one frame from `pending_outbound`; consumed by `codec::flush_pending` (`kimojio-stack-http/src/h2/connection.rs:309-311`).
- `receive_window_update`: updates `connection_window` or per-stream `outbound_window`; ignores updates for streams in `closed_streams` (`kimojio-stack-http/src/h2/connection.rs:288-303`).

#### 2.6 `Stream` — Per-Stream State

- `Stream` tracks: `id: StreamId`, `state: StreamState`, `inbound_window: FlowControlWindow`, `outbound_window: FlowControlWindow`, `received_body_len: usize`, `trailers: Option<Vec<Header>>`, `awaiting_continuation: bool` (`kimojio-stack-http/src/h2/stream.rs:98-107`).
- `StreamState` variants: `Idle`, `Open`, `HalfClosedLocal`, `HalfClosedRemote`, `Closed` (`kimojio-stack-http/src/h2/stream.rs:31-37`).
- `send_data(end_stream)` transitions `Open → HalfClosedLocal` (if end_stream) or `HalfClosedRemote → Closed` (if end_stream) (`kimojio-stack-http/src/h2/stream.rs:245-254`).
- `receive_data(data, end_stream, limits)` consumes `inbound_window`, accumulates `received_body_len`, optionally closes remote side (`kimojio-stack-http/src/h2/stream.rs:220-243`).
- `receive_trailers(headers)` stores `trailers = Some(headers)` and closes remote side (`kimojio-stack-http/src/h2/stream.rs:256-270`).
- `FlowControlWindow::consume(amount)` errors if `amount > available` — this is the hard boundary that prevents oversending (`kimojio-stack-http/src/h2/stream.rs:58-66`). `adjust(delta)` permits negative values (used when SETTINGS reduces initial window size mid-connection) (`kimojio-stack-http/src/h2/stream.rs:85-95`).

#### 2.7 `h2::codec` — Frame Read/Write Path

- `read_frame(cx, transport, max_frame_size)` reads a 9-byte header, validates length ≤ `max_frame_size`, reads the payload, then calls `Frame::decode` (`kimojio-stack-http/src/h2/codec.rs:41-64`).
- `write_frame(cx, transport, frame)` encodes the frame and calls `transport.write_all` (`kimojio-stack-http/src/h2/codec.rs:67-74`).
- `write_header_block(cx, transport, stream_id, block, end_stream, max_frame_size)` fragments header blocks into HEADERS + CONTINUATION frames when the block exceeds `max_frame_size` (`kimojio-stack-http/src/h2/codec.rs:76-122`).
- `flush_pending(cx, transport, state)` drains `state.pop_outbound()` and writes each frame — used after SETTINGS ack, WINDOW_UPDATE, PING ack, and DATA receive (`kimojio-stack-http/src/h2/codec.rs:124-133`).
- `collect_header_block(cx, transport, state, first_frame)` reads CONTINUATION frames until END_HEADERS, assembles a complete header block, enforces the `max_header_list_size` limit (`kimojio-stack-http/src/h2/codec.rs:155-188`).
- `response_headers(response)` produces `[Header { ":status", ... }, ...regular headers...]` for HEADERS frames; `trailers_headers(trailers)` produces only regular headers with no pseudo-headers (`kimojio-stack-http/src/h2/codec.rs:238-252`).

#### 2.8 Disconnect / RST_STREAM / GOAWAY Detection

- In `ServerConnection::process_frame`, a received `RstStream` removes the stream from `pending` and `completed`, and stores the cancel signal only implicitly — the stream vanishes from `state.streams` (`kimojio-stack-http/src/h2/server.rs:312-320`).
- In `ClientConnection::process_frame`, `RstStream` removes the stream and inserts `Error::stream_reset(stream_id, error_code)` into `reset_streams` (`kimojio-stack-http/src/h2/client.rs:265-276`).
- `GOAWAY` from the peer sets `self.goaway` on both client and server; `goaway_for_stream` on the client compares stream IDs against `last_stream_id` to surface the error (`kimojio-stack-http/src/h2/client.rs:388-407`).
- Transport EOF returns `Error::Eof`; `Errno::PIPE` / `Errno::CONNRESET` surface as `Error::Io` or `Error::Tls` and are caught by `peer_closed` (`kimojio-stack-http/src/h2/server.rs:432-434`, `kimojio-stack-http/src/error.rs:33-50`).

---

### 3. Scheduler / Wait / Channel / Task Patterns

#### 3.1 Stackful Coroutines and `RuntimeContext`

- `Runtime::block_on(|cx| { ... })` constructs the scheduler and root `RuntimeContext`; `cx.scope(|scope| { let h = scope.spawn(move |cx| { ... }); h.join(cx) })` creates scoped coroutines that cannot outlive the scope (`kimojio-stack/src/lib.rs:428-493`).
- `cx.park()` suspends the current coroutine cooperatively; `cx.waiter()` returns a `Waiter` that can be stored by a primitive to resume this coroutine (`kimojio-stack/src/lib.rs:1741-1753`).
- `Waitable` trait has `is_ready(&self) -> bool` and `add_waiter(&self, cx)` (`kimojio-stack/src/wait.rs:16-22`).
- `JoinHandle::join(cx)` parks until the coroutine result is ready (`kimojio-stack/src/lib.rs:1850-1877`).
- `RuntimeContext::yield_now(cx)` suspends the current coroutine as ready (not blocked), useful for cooperative yielding within a computation loop (`kimojio-stack/src/lib.rs:495-505`).

#### 3.2 Bounded Channel (Backpressure-Aware)

- `channel::bounded<T>(capacity) -> (Sender<T>, Receiver<T>)` requires `capacity != 0` (`kimojio-stack/src/channel/bounded.rs:13-31`).
- `Sender::send(cx, value)` parks while the queue is full; wakes one receiver on success (`kimojio-stack/src/channel/bounded.rs:56-69`).
- `Receiver::recv(cx)` parks while the queue is empty and at least one sender exists; wakes one sender after popping (`kimojio-stack/src/channel/bounded.rs:139-152`).
- `Sender::try_send` and `Receiver::try_recv` are non-blocking variants returning `TrySendError::{Full, Closed}` or `TryRecvError::{Empty, Closed}` (`kimojio-stack/src/channel/bounded.rs:40-52`, `kimojio-stack/src/channel/bounded.rs:124-136`).
- `Sender::is_closed()` returns true when all receivers are dropped; `Receiver::is_closed()` returns true when all senders are dropped (`kimojio-stack/src/channel/bounded.rs:72-74`, `kimojio-stack/src/channel/bounded.rs:156-158`).
- Both `Sender<T>` and `Receiver<T>` implement `Waitable` for use with `wait_any`/`wait_all` (`kimojio-stack/src/channel/bounded.rs:96-109`, `kimojio-stack/src/channel/bounded.rs:179-191`).
- Sender drop calls `recv_waiters.wake_all()` to unblock receivers; receiver drop calls `send_waiters.wake_all()` to unblock senders (`kimojio-stack/src/channel/bounded.rs:86-94`, `kimojio-stack/src/channel/bounded.rs:169-177`).

#### 3.3 Unbounded Channel

- `channel::unbounded<T>() -> (Sender<T>, Receiver<T>)` with no capacity limit (`kimojio-stack/src/channel/unbounded.rs:13-26`).
- `Sender::send(value)` does not take `cx` and never parks; returns `Err(SendError(value))` only when receiver is dropped (`kimojio-stack/src/channel/unbounded.rs:35-45`).
- `Receiver::recv(cx)` parks while empty and open (`kimojio-stack/src/channel/unbounded.rs:107-119`).
- The unbounded sender has no send-side backpressure — producers must impose limits externally.

#### 3.4 Once Channel (Single-Value Rendezvous)

- `channel::once::channel<T>() -> (Sender<T>, Receiver<T>)` for one-shot value delivery (`kimojio-stack/src/once.rs:13-26`).
- `Sender::send(self, value)` consumes the sender; `Receiver::recv(self, cx)` parks until the value arrives or the sender is dropped (`kimojio-stack/src/once.rs:36-102`).
- `Receiver<T>` implements `Waitable` (`kimojio-stack/src/once.rs:105-115`).

#### 3.5 Cross-Thread Channel (Stackful ↔ Tokio ↔ OS Thread)

- `channel::cross_thread::bounded<T>(capacity)` returns a `Builder` with endpoint-pair constructors: `thread()`, `stackful()`, `tokio()`, `thread_to_stackful()`, `stackful_to_tokio()`, `tokio_to_stackful()`, etc. (`kimojio-stack/src/channel/cross_thread.rs:55-193`).
- `StackfulSender` / `StackfulReceiver` park the current stackful coroutine via `cx.waiter()` + `cx.park()`.
- `AsyncSender` / `AsyncReceiver` implement `Future`/`Waker` integration compatible with Tokio tasks.
- `ThreadSender` / `ThreadReceiver` use `Condvar`-based blocking for OS threads.
- Internal storage uses `crossbeam_queue::ArrayQueue` — a lock-free bounded ring backed by pre-allocated slots (`kimojio-stack/src/channel/cross_thread.rs:47`).
- The module docstring notes adaptive spinning (128 spin retries then 2 yield retries) before parking (`kimojio-stack/src/channel/cross_thread.rs:52-53`).

#### 3.6 `Notify` — Generation-Based Wake

- `Notify::notify_one()` stores one permit; `Notify::notify_waiters()` increments the generation counter and wakes all current waiters at once (`kimojio-stack/src/notify.rs:31-45`).
- `Notified::wait(cx)` consumes a permit or advances generation observation; `Notified::try_wait()` is the non-blocking variant (`kimojio-stack/src/notify.rs:71-103`).
- `Notified` implements `Waitable` for use in `wait_any` (`kimojio-stack/src/notify.rs:106-123`).

#### 3.7 `Waiters` — Inline-First Waiter Storage

- `Waiters` stores one waiter inline and overflows to a `Vec<Waiter>` (`kimojio-stack/src/lib.rs:2685-2735`, noted in prior research). Used by all channel/notify/semaphore/mutex primitives.
- `Waiters::wake_one()` pops and wakes one waiter; `Waiters::wake_all()` wakes every stored waiter, requeuing their task IDs.

---

### 4. Existing Tests and Examples to Extend

#### 4.1 `kimojio-stack-grpc` Unit Tests (in-crate, `lib.rs`)

- `unary_frame_round_trips_prost_message` (`kimojio-stack-grpc/src/lib.rs:152-162`): encode then decode a `TestMessage`; baseline codec test.
- `status_round_trips_grpc_code_header_value` (`kimojio-stack-grpc/src/lib.rs:164-184`): status → trailers → status round-trip including message percent-encoding and binary details.
- `unary_call_succeeds_with_metadata_and_trailers` (`kimojio-stack-grpc/src/lib.rs:186+`): full unary call over a UNIX socketpair with request/response metadata and trailer assertions. Test uses `socket_transport_pair()` helper, `h2::ServerConnection`, `UnaryServer`, `UnaryClient`, `RuntimeContext::scope`.

#### 4.2 `kimojio-stack-grpc` Integration Tests

- `tests/client_interop.rs`: Two tests that connect to a live Tonic server on a real TCP port:
  - `stackful_grpc_client_calls_tonic_server_success_with_metadata` (line 18): success path with ASCII and binary metadata round-trip.
  - `stackful_grpc_client_receives_tonic_error_status_and_metadata` (line 70): error status with `grpc-status-details-bin` and trailer metadata.
  - Pattern: `spawn_tonic_server` starts a Tokio runtime in a new thread, reports the port via `mpsc::channel`; stackful client connects with `cx.socket` + `cx.connect` + `StackTransport::plaintext` (`kimojio-stack-grpc/tests/client_interop.rs`).
- `tests/server_interop.rs`: Two tests where a Tonic client calls a stackful gRPC server:
  - `tonic_client_calls_stackful_grpc_server_success_with_metadata` (line 16): full metadata round-trip.
  - `tonic_client_receives_stackful_error_status_and_trailers` (line 46): error status propagation.
  - Pattern: `spawn_stackful_server` starts a thread with `Runtime::new().block_on`; calls `cx.socket/bind/listen/accept` then `serve_one` (`kimojio-stack-grpc/tests/server_interop.rs`).
- `tests/protocol_limits.rs`: 7 tests covering codec error kinds, missing trailers, non-POST rejection, invalid content-type, invalid metadata names, invalid binary details encoding.
- `tests/tls_interop.rs`: `stackful_grpc_client_calls_tonic_server_over_tls` (line 32) — uses `tls::client_transport` from `kimojio-stack-http` with ALPN h2 and `SslConnector` (`kimojio-stack-grpc/tests/tls_interop.rs`).

#### 4.3 `kimojio-stack-grpc` Bench (`benches/unary_rpc.rs`)

- `bench_grpc` runs plaintext and TLS variants at 64-byte and 16 KiB payloads using `iter_custom` and `Instant::now()` for wall-time measurement (`kimojio-stack-grpc/benches/unary_rpc.rs:45-59`).
- Server runs as a scoped coroutine; client sends `iters + 1` calls, times the inner `iters` calls (`kimojio-stack-grpc/benches/unary_rpc.rs:100-158`).
- TLS path uses `tls::server_transport` / `tls::client_transport` with `HttpProtocol::Http2` ALPN (`kimojio-stack-grpc/benches/unary_rpc.rs:67-98`).

#### 4.4 `kimojio-stack-http/tests` and In-Module Tests

- `h2/mod.rs` tests: `h2_client_server_request_response_over_socketpair`, `h2_response_trailers_over_socketpair`, `h2_concurrent_streams_preserve_association_out_of_order`, `h2_flow_control_blocks_and_unblocks_with_window_updates`, `h2_large_header_block_uses_continuation_frames`, `h2_server_rejects_malformed_pseudo_headers`, `h2_stream_reset_and_goaway_surface_peer_reset`, `h2_protocol_neutral_dispatch_uses_http2`, `h2_settings_ack_is_processed_before_request_frames` (`kimojio-stack-http/src/h2/mod.rs:52-553`).
- `lib.rs` tests: `body_limits_reject_oversized_payloads`, `headers_and_trailers_wrap_header_maps`, `plaintext_transport_moves_bytes_over_socketpair`, `tls_transport_moves_bytes_over_socketpair`, allocation tests for http1 and h2 warm paths (`kimojio-stack-http/src/lib.rs:133-434`).
- `tests/h2_interop.rs` and `tests/protocol_limits.rs`: additional integration tests against Hyper/Tokio peers (file-level; not read in full detail).

#### 4.5 `interop_proto.rs` (shared test helper)

- Defines `TestRequest`, `TestResponse` (prost `Message`), `TonicTestServer` (implements `tonic::server::NamedService` and `Service`), `tonic_unary_call`, `tonic_request` (`kimojio-stack-grpc/tests/interop_proto.rs`).
- `SERVICE_NAME = "interop.TestService"`, `UNARY_PATH = "/interop.TestService/Unary"` — these can be extended with a streaming path (`kimojio-stack-grpc/tests/interop_proto.rs:16-17`).

---

### 5. Current Constraints Relevant to Streaming

- `Body` is always fully buffered: there is no incremental body reader or writer today. Streaming response production will require either a new body representation or bypassing the `Body` type and writing DATA frames directly through `ConnectionState` + `StackTransport` (`kimojio-stack-http/src/body.rs:39-75`).
- `send_response_with_trailers` sends the complete body and trailers atomically in one call; it calls `state.remove_stream(stream_id)` at the end. For streaming, DATA frames would need to be written incrementally before trailers are sent, and `remove_stream` would only be called at stream end (`kimojio-stack-http/src/h2/server.rs:103-148`).
- `read_response_with_trailers` accumulates all DATA frames before returning; for streaming, individual DATA frames would need to be surfaced to the caller as they arrive, rather than being accumulated in `BodyBuilder` (`kimojio-stack-http/src/h2/client.rs:128-160`).
- `UnaryServer::add_unary` handler signature `Fn(&RuntimeContext, Metadata, Req) -> Result<UnaryReply<Resp>, Status>` does not have a streaming return path. A streaming handler would need to return a different type capable of producing multiple responses (`kimojio-stack-grpc/src/server.rs:48-49`).
- `UnaryClient::call` does not expose the `StreamId` to the caller; `send_request` does expose it. The `read_response_with_trailers` loop could be adapted to poll for individual DATA frames, but the return type currently expects a completed `Body` (`kimojio-stack-grpc/src/client.rs:46-62`, `kimojio-stack-http/src/h2/client.rs:84-160`).
- RST_STREAM from the server side is stored transiently in `ServerConnection::process_frame` (stream removed from `state.streams` and `pending`) — there is no active cancellation signal delivered to in-progress server handlers today (`kimojio-stack-http/src/h2/server.rs:312-320`).

---

## Code References

- `kimojio-stack-grpc/src/lib.rs:6-18` — Module declarations and public re-exports.
- `kimojio-stack-grpc/src/client.rs:14-143` — `ClientConfig`, `UnaryClient`, `UnaryResponse`, request build, response parse.
- `kimojio-stack-grpc/src/codec.rs:9-109` — 5-byte gRPC frame encode/decode.
- `kimojio-stack-grpc/src/error.rs:10-85` — `ErrorKind`, `Error`, conversion impls.
- `kimojio-stack-grpc/src/metadata.rs:13-117` — `Metadata` wrapper, ASCII/binary insert/get, `from_headers`.
- `kimojio-stack-grpc/src/server.rs:31-241` — `UnaryServer`, `UnaryHandler` trait, `TypedUnaryHandler`, `write_reply`, `write_status`.
- `kimojio-stack-grpc/src/status.rs:16-270` — `StatusCode`, `Status`, `to_trailers`, `from_trailers`, message percent-encoding.
- `kimojio-stack-http/src/body.rs:10-104` — `BodyLimits`, `Body`, `BodyBuilder`.
- `kimojio-stack-http/src/transport.rs:12-94` — `StackTransport` plaintext/TLS dispatch.
- `kimojio-stack-http/src/h2/client.rs:17-408` — `ResponseWithTrailers`, `ClientConnection` full implementation.
- `kimojio-stack-http/src/h2/server.rs:18-434` — `IncomingRequest`, `ServerConnection` full implementation.
- `kimojio-stack-http/src/h2/connection.rs:16-312` — `ConnectionState`, flow-control window management.
- `kimojio-stack-http/src/h2/stream.rs:9-388` — `StreamId`, `StreamState`, `FlowControlWindow`, `Stream`.
- `kimojio-stack-http/src/h2/codec.rs:41-252` — `read_frame`, `write_frame`, `write_header_block`, `flush_pending`, `collect_header_block`, header conversion functions.
- `kimojio-stack-http/src/h2/mod.rs:1-553` — `h2` module public exports, `H2Config`, `validate_client_preface`, all tests.
- `kimojio-stack-http/src/error.rs:10-149` — `ErrorKind`, `LimitKind`, `Error`, constructors.
- `kimojio-stack-http/src/headers.rs:8-76` — `Headers`, `Trailers` wrappers.
- `kimojio-stack/src/channel/bounded.rs:13-239` — Bounded channel with backpressure.
- `kimojio-stack/src/channel/unbounded.rs:13-196` — Unbounded channel without send-side backpressure.
- `kimojio-stack/src/channel/cross_thread.rs:55-193` — Builder and mixed endpoint constructors.
- `kimojio-stack/src/channel/mod.rs:13-83` — Error types: `SendError`, `TrySendError`, `RecvError`, `TryRecvError`.
- `kimojio-stack/src/once.rs:13-164` — Once channel for single-value rendezvous.
- `kimojio-stack/src/notify.rs:10-172` — `Notify`, `Notified`.
- `kimojio-stack/src/wait.rs:16-50` — `Waitable` trait, `WaitError`.
- `kimojio-stack/src/lib.rs:105-168` — Scheduler model, allocation model, `Waitable` overview docs.
- `kimojio-stack/src/lib.rs:230-236` — Public re-exports of sync primitives.
- `kimojio-stack-grpc/tests/client_interop.rs` — Stackful client ↔ Tonic server TCP tests.
- `kimojio-stack-grpc/tests/server_interop.rs` — Tonic client ↔ stackful server TCP tests.
- `kimojio-stack-grpc/tests/protocol_limits.rs` — Protocol limit and validation tests.
- `kimojio-stack-grpc/tests/interop_proto.rs:16-17` — `SERVICE_NAME`, `UNARY_PATH` constants.
- `kimojio-stack-grpc/benches/unary_rpc.rs:45-158` — Plaintext/TLS unary bench.
- `Cargo.toml:1-89` — Workspace members and dependency versions.

---

## Architecture Documentation

- The runtime is intentionally single-threaded and stackful: coroutines park on the current OS thread; only the io_uring submission path touches the kernel while a coroutine runs (`kimojio-stack/src/lib.rs:105-168`).
- The gRPC layer delegates all framing, HPACK, flow control, and wire I/O to `kimojio-stack-http`; it only adds gRPC frame header interpretation, prost encode/decode, metadata filtering, and status encode/decode.
- `H2Config::default()` sets `initial_stream_window = 65535`, `initial_connection_window = 65535`, `max_frame_size = 16384`, `max_concurrent_streams = 100` (`kimojio-stack-http/src/h2/mod.rs:26-42`). These are the effective defaults for both `ClientConnection` and `ServerConnection` unless `new_with_settings` is called.
- `stream_window_update_to_target` is the only automatic credit-return mechanism: it tops up the inbound stream window to `initial_window_size` whenever it falls below `initial_window_size / 2`. For server-sent DATA frames there is no corresponding automatic credit return — outbound capacity is bounded by the peer's `initial_window_size` until the client sends WINDOW_UPDATE.
- Both `write_data` loops in client and server share the same pattern: check `outbound_capacity`, if zero read one inbound frame and retry, otherwise write a DATA frame up to `min(remaining, capacity, peer_max_frame_size)`. This makes each DATA write directly bounded by the transport window.
- Workspace-level `Cargo.toml` already declares `prost = "0.14"`, `bytes = "1"`, `http = "1"`, `tonic = "0.14"`, `tonic-prost = "0.14"`, `tokio`, `crossbeam-queue = "0.3"`, `base64 = "0.22"`, `hpack = "0.3"`, `httparse = "1"`, `httpdate = "1"` (`Cargo.toml:26-89`).

---

## Open Questions

1. **Streaming server send interface**: The existing `send_response_with_trailers` calls `state.remove_stream(stream_id)` at the end. A streaming send path must write individual DATA frames with `send_data(end_stream=false)` and `flush_pending` calls, then write the terminal trailer HEADERS with `send_headers(end_stream=true)` and only then call `remove_stream`. Whether this bypasses `send_response_with_trailers` entirely or adds a parallel method to `ServerConnection` is a design choice for the planning phase.

2. **Streaming client receive interface**: `read_response_with_trailers` accumulates into `BodyBuilder` inside the `process_data` loop. A streaming receive path needs to surface individual DATA payloads to the caller rather than accumulating them. Whether this is done by adding a new read method to `ClientConnection` (e.g., `read_response_headers` + `read_next_data_frame`) or by factoring out the frame loop is a design choice.

3. **RST_STREAM cancellation signal to server**: When a client disconnects mid-stream, `ServerConnection::process_frame` for `RstStream` removes the stream from `state.streams` but does not actively signal any in-progress server-side stream producer. A streaming server API will need a mechanism to detect this (e.g., checking `is_closed()` on a response channel's sender, or an explicit cancellation flag) — the current server code has no such signal path.

4. **`Body` type extension vs. bypass**: The `Response<Body>` type passed to `send_response_with_trailers` carries a fully-buffered body. Streaming could either (a) bypass `Response<Body>` entirely and write DATA frames directly to `ConnectionState`/`StackTransport`, or (b) introduce an incremental body variant. Option (a) is simpler but requires callers to access lower-level types; option (b) preserves the `Response<T>` abstraction.

5. **Allocation budget for streaming frames**: The current unary allocation test for h2 allows ≤ 512 allocating operations per 4 warmed rounds (≤ 128 per RPC). Each streaming DATA frame allocation should be assessed against this existing budget. The planning phase should decide whether streaming adds a new allocation baseline test or extends the existing one.
