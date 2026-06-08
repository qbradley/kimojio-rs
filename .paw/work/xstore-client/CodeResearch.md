---
date: 2026-06-08T06:23:35+00:00
git_commit: d4ed3c9c80bce393b221aa72faead78ca2ca3aa3
branch: "HEAD (detached)"
repository: "kimojio-rs"
topic: "Durable Storage Client Code Research"
tags: [research, codebase, durable-storage, stack-http, stack-runtime]
status: complete
last_updated: 2026-06-08
---

# Research: Durable Storage Client

## Research Question

Map the existing stackful HTTP transport/body/HTTP/2/TLS/error/config APIs, stack runtime socket/read/write/connect/timeout/cancellation surfaces, low-allocation client patterns, workspace conventions, documentation system, verification commands, and any existing durable-storage-related source code relevant to a low-overhead durable storage client (`Spec.md:8-12`, `Spec.md:119-123`, `Spec.md:163-170`).

## Summary

The feature specification describes a durable object-storage client for storage-heavy services with explicit failure behavior, bounded memory behavior, authentication/retry/cancellation visibility, and storage-domain operations for page, metadata, ownership, snapshot, archive, and configuration workflows (`Spec.md:8-12`, `Spec.md:16-21`, `Spec.md:180-190`). The existing HTTP stack exposes buffered `Body` values, bounded parser/body config, plaintext/TLS `StackTransport`, HTTP/1.1 and HTTP/2 clients/servers, trailers, and inspectable transport/protocol errors (`kimojio-stack-http/src/lib.rs:4-25`, `kimojio-stack-http/src/body.rs:38-104`, `kimojio-stack-http/src/error.rs:8-100`). The stack runtime exposes stackful coroutine scopes plus io_uring-backed sockets, connect, read/write, async owned-buffer I/O, waitables, timeouts, and cancellation surfaces (`kimojio-stack/src/lib.rs:12-18`, `kimojio-stack/src/lib.rs:59-106`, `kimojio-stack/src/lib.rs:1151-1629`, `kimojio-stack/src/lib.rs:1631-1661`). The telemetry crate shows a caller-controlled client pattern over HTTP/2/gRPC with explicit construction, no background workers, caller-owned batches, size checks, and explicit finish/close (`kimojio-stack-opentelemetry/src/lib.rs:4-8`, `kimojio-stack-opentelemetry/src/client.rs:37-98`, `kimojio-stack-opentelemetry/src/logs.rs:192-260`, `kimojio-stack-opentelemetry/src/metrics.rs:289-355`).

## Documentation System

- **Framework**: Plain Markdown in repository docs and README links; the README links the stack HTTP/gRPC and OpenTelemetry guides directly (`README.md:32-39`).
- **Docs Directory**: `docs/`; README points to the virtual clock guide/design docs and cross-runtime channel guide under `docs/` (`README.md:99-107`).
- **Navigation Config**: N/A for a static-site nav file in this research; README reference links act as entry points for stack guides (`README.md:32-39`, `README.md:99-107`).
- **Style Conventions**: Guides use H1 titles, short introductory paragraphs, Markdown tables, explicit behavior sections, and fenced shell commands (`docs/stack-http-grpc.md:1-17`, `docs/stack-http-grpc.md:97-111`, `docs/stack-http-grpc.md:139-144`, `docs/stack-opentelemetry.md:1-16`, `docs/stack-opentelemetry.md:49-57`).
- **Build Command**: Rust doc tests are checked by CI with `cargo +${{ matrix.rust }} test --doc` (`.github/workflows/rust.yml:43-44`).
- **Standard Files**: `README.md` describes project purpose, stackful HTTP/gRPC crates, virtual clock docs, and cross-runtime channels (`README.md:1-19`, `README.md:21-39`, `README.md:79-108`); `CONTRIBUTING.md` contains contribution, CLA, and code of conduct information (`CONTRIBUTING.md:1-14`).

## Verification Commands

- **Toolchain**: The checked-in Rust toolchain channel is `1.92.0` (`rust-toolchain.toml:1-7`).
- **Format Command**: `cargo fmt --all -- --check` is run in CI; local formatting can use `cargo fmt` before checking (`.github/workflows/rust.yml:46-47`).
- **Lint Command**: CI runs `cargo clippy --all-targets -- -D warnings` and `cargo clippy --all-targets --all-features -- -D warnings` (`.github/workflows/rust.yml:34-35`, `.github/workflows/rust.yml:49-50`).
- **Test Command**: CI runs `cargo test --all-targets --features ssl2`, `cargo test --all-targets --features virtual-clock`, and `cargo test --doc` (`.github/workflows/rust.yml:37-44`).
- **Build/Type Check**: The CI build step is implemented through clippy over all targets, which compiles the workspace targets while enforcing warnings as errors (`.github/workflows/rust.yml:34-35`).
- **Optional Interop/Bench Commands**: Stack HTTP/gRPC docs list Criterion benchmark commands for HTTP and gRPC (`docs/stack-http-grpc.md:126-144`); OpenTelemetry docs list opt-in receiver interop tests with environment variables (`docs/stack-opentelemetry.md:49-57`).

## Detailed Findings

### Workspace and Crate Conventions

- The workspace includes the runtime crate, macros, stack runtime, stack gRPC, stack HTTP, stack telemetry, stack TLS, core TLS, perf, and examples as members (`Cargo.toml:1-16`).
- Workspace package metadata centralizes version `0.16.4`, Rust edition `2024`, license, homepage, repository, and README values (`Cargo.toml:18-25`).
- Workspace dependencies centralize shared crates such as `bytes`, `http`, `hpack`, `rustix`, `rustix-uring`, `tokio`, `tonic`, `prost`, `openssl`, and workspace path dependencies for the kimojio crates (`Cargo.toml:26-89`).
- `kimojio-stack-http` has default feature `tls`, an optional dependency on `kimojio-stack-tls`, normal dependencies on `bytes`, `hpack`, `http`, `httparse`, `httpdate`, `kimojio-stack`, and `rustix`, and dev-dependencies on Hyper/Tokio/OpenSSL for tests (`kimojio-stack-http/Cargo.toml:11-31`).
- `kimojio-stack` depends on `corosensei`, `crossbeam-queue`, `libc`, `rustix` with event/fs/mm/net/param/pipe features, and `rustix-uring`; it uses Criterion and Tokio as dev-dependencies (`kimojio-stack/Cargo.toml:1-24`).
- `kimojio-stack-opentelemetry` has no default features, exposes optional `tls`, depends on `bytes`, `kimojio-stack`, `kimojio-stack-grpc`, `kimojio-stack-http` without default features, and `prost`, and uses receiver/test dependencies under dev-dependencies (`kimojio-stack-opentelemetry/Cargo.toml:11-29`).
- Source files in the stack crates start with the copyright/license header and crate-level/module-level docs before public modules or types (`kimojio-stack-http/src/lib.rs:1-7`, `kimojio-stack/src/lib.rs:1-18`, `kimojio-stack-opentelemetry/src/lib.rs:1-8`).
- The README characterizes the runtime as a single-threaded cooperative runtime with explicit concurrency and no automatic load balancing (`README.md:7-19`).

### Feature-Spec Requirements Relevant to Existing APIs

- The specification requires explicit operation inputs, deadline, cancellation, response status, response metadata, and streamed output chunks (`Spec.md:117-123`).
- The specification requires replayability visibility for retried write bodies and default chunk-stream consumption for responses (`Spec.md:119-123`).
- The specification requires bounded connection reuse, idle duration, connection establishment time, total operation duration, per-destination idle capacity, and concurrent operations (`Spec.md:121-123`).
- The specification includes local emulator endpoints, multi-account routing, container operations, page objects, block objects, metadata, leases, listing, snapshots, copy, retry, observability, and bounded concurrency as in-scope (`Spec.md:128-171`, `Spec.md:200-205`).

### Stack HTTP Public Surface

- `kimojio-stack-http` exposes modules for `body`, `client`, `config`, `error`, `h2`, `headers`, `http1`, `server`, optional `tls`, and `transport`, and re-exports `Body`, `BodyBuilder`, `BodyLimits`, `HttpConfig`, `Error`, `ErrorKind`, `LimitKind`, `Headers`, `Trailers`, and `StackTransport` (`kimojio-stack-http/src/lib.rs:9-25`).
- `ClientConnection` is protocol-neutral over HTTP/1.1 or HTTP/2, keeps concrete connections inline with an allow note for `large_enum_variant`, constructs protocol-specific clients from `StackTransport` plus `HttpConfig`, and sends `Request<Body>` to return `Response<Body>` (`kimojio-stack-http/src/client.rs:9-48`).
- `Headers` and `Trailers` wrap `http::HeaderMap`, provide `new`, `insert`, `get`, `len`, `is_empty`, `as_map`, and `into_map`, and are separate types for request/response headers versus trailers (`kimojio-stack-http/src/headers.rs:4-76`).
- HTTP tests include socketpair-based plaintext transport movement and TLS transport movement through `StackTransport` (`kimojio-stack-http/src/lib.rs:234-323`).
- HTTP allocation tests warm local HTTP/1.1 and HTTP/2 loops, measure repeated sends with a counting allocator, and assert operation and byte ceilings (`kimojio-stack-http/src/lib.rs:326-416`).

### HTTP Config and Body Model

- `HttpConfig` contains max start-line length, header count, header bytes, body bytes, and read-buffer size fields, with defaults of 8 KiB start line, 100 headers, 64 KiB headers, 4 MiB body, and 16 KiB read buffer (`kimojio-stack-http/src/config.rs:4-24`).
- `BodyLimits` stores a maximum buffered body length, exposes `new`, `max_len`, and `check_body_len`, and reports `LimitKind::Body` through `Error::size_limit` when exceeded (`kimojio-stack-http/src/body.rs:8-35`).
- `Body` stores a `bytes::Bytes`, supports empty construction, construction from `Bytes` subject to `BodyLimits`, `copy_from_slice`, length/empty checks, borrowed byte access, and conversion into `Bytes` (`kimojio-stack-http/src/body.rs:38-75`).
- `BodyBuilder` stores `BodyLimits` plus `BytesMut`, appends slices after checking the next length, and freezes the accumulated bytes into a `Body` (`kimojio-stack-http/src/body.rs:77-104`).
- HTTP/1.1 body reading dispatches `Empty`, fixed content length, chunked, and EOF-delimited body kinds into a buffered `Body` (`kimojio-stack-http/src/http1/body.rs:9-30`).
- HTTP/1.1 content-length reads allocate a `Bytes` of the requested length and wrap it as `Body`; chunked and EOF-delimited reads append chunks into `BodyBuilder` (`kimojio-stack-http/src/http1/body.rs:45-86`, `kimojio-stack-http/src/http1/body.rs:88-132`).
- HTTP/1.1 body writing either writes chunk framing and the body bytes or directly extends the output buffer from `Body::as_bytes` (`kimojio-stack-http/src/http1/body.rs:32-43`).

### HTTP/1.1 Client/Codec Behavior

- The HTTP/1.1 client stores a `StackTransport`, `HttpConfig`, and a `Vec<u8>` read buffer initialized with `config.read_buffer_size` capacity (`kimojio-stack-http/src/http1/client.rs:11-25`).
- HTTP/1.1 client `send` writes a request with the codec, reads one response with the same transport/read buffer/config, shuts down write when the response requires close, and returns the response body (`kimojio-stack-http/src/http1/client.rs:27-44`).
- The HTTP/1.1 codec parses requests and responses with `httparse`, constructs `http::Request<Body>` and `http::Response<Body>`, rejects too many headers using `LimitKind::Headers`, and reads bodies through `read_body` with `BodyLimits::new(config.max_body_bytes)` (`kimojio-stack-http/src/http1/codec.rs:26-85`, `kimojio-stack-http/src/http1/codec.rs:87-139`).
- The HTTP/1.1 codec writes requests and responses by constructing a `Vec<u8>`, serializing start lines, headers, content length or chunked framing, and then calling `transport.write_all` (`kimojio-stack-http/src/http1/codec.rs:141-186`, `kimojio-stack-http/src/http1/codec.rs:273-297`).
- HTTP/1.1 head reading enforces combined start-line/header limits, fills a temporary read buffer sized from `config.read_buffer_size.max(1)`, drains the parsed head from the shared read buffer, and returns EOF when no head bytes were buffered and the transport read returns zero (`kimojio-stack-http/src/http1/codec.rs:188-240`).
- HTTP/1.1 request parsing rejects non-empty leftover `read_buf` after body parsing as unsupported pipelining (`kimojio-stack-http/src/http1/codec.rs:72-74`).

### HTTP/2 Client, Frames, Settings, and Flow Control

- The HTTP/2 module re-exports client/server connection types, `ConnectionState`, frame types, `Header`, settings, streams, flow-control windows, and stream states (`kimojio-stack-http/src/h2/mod.rs:4-19`).
- `H2Config` has initial stream window, initial connection window, max frame size, and max concurrent stream fields with defaults of 65,535-byte windows, 16,384-byte max frame size, and 100 max concurrent streams (`kimojio-stack-http/src/h2/mod.rs:21-42`).
- HTTP/2 `Settings` models header table size, push, max concurrent streams, initial window, max frame size, and max header list size; invalid push/window/frame-size values return protocol errors (`kimojio-stack-http/src/h2/settings.rs:47-105`).
- `settings_from_config` disables push, maps `HttpConfig::max_header_bytes` into `max_header_list_size`, and uses the default HTTP/2 max-concurrent-streams value (`kimojio-stack-http/src/h2/codec.rs:18-25`).
- `ConnectionState` stores local/peer settings, active/closed streams, connection windows, pending outbound frames, HPACK encoder/decoder, continuation state, and preface state (`kimojio-stack-http/src/h2/connection.rs:16-28`).
- `ConnectionState` enforces local and peer max-concurrent-streams separately for inbound and outbound stream creation (`kimojio-stack-http/src/h2/connection.rs:130-148`).
- `ConnectionState::outbound_capacity` returns the minimum of connection and stream outbound window availability, and `consume_outbound_window` consumes both connection and stream windows (`kimojio-stack-http/src/h2/connection.rs:166-189`).
- `ConnectionState` queues connection and stream `WINDOW_UPDATE` frames and updates inbound flow-control windows as data is consumed (`kimojio-stack-http/src/h2/connection.rs:191-251`).
- `FlowControlWindow` validates maximum window size, rejects consumption beyond availability, rejects zero window-update increments, and detects overflow/excess window size on increases and adjustments (`kimojio-stack-http/src/h2/stream.rs:39-96`).
- `Stream` tracks stream state, inbound/outbound windows, received body length, trailers, and continuation state; `receive_data` consumes inbound window, checks accumulated body length against `BodyLimits`, and closes the remote side on END_STREAM (`kimojio-stack-http/src/h2/stream.rs:98-107`, `kimojio-stack-http/src/h2/stream.rs:220-243`).
- `Frame` supports DATA, HEADERS, CONTINUATION, SETTINGS, RST_STREAM, PING, GOAWAY, and WINDOW_UPDATE payloads, and encodes/decodes frames with size-limit checks (`kimojio-stack-http/src/h2/frame.rs:13-24`, `kimojio-stack-http/src/h2/frame.rs:66-84`, `kimojio-stack-http/src/h2/frame.rs:195-243`).
- HTTP/2 client state stores transport, config, connection state, initialization flag, next stream id, pending/completed response maps, reset stream map, and optional GOAWAY error (`kimojio-stack-http/src/h2/client.rs:38-48`).
- HTTP/2 `send_request` initializes the connection, allocates the next odd stream id, opens an outbound stream, encodes headers, writes HEADERS frames, and writes DATA frames when the body is non-empty (`kimojio-stack-http/src/h2/client.rs:84-117`).
- HTTP/2 `write_data` waits for outbound flow-control capacity by reading frames when capacity is zero, bounds each chunk by stream/connection capacity and peer max frame size, consumes outbound windows, and writes DATA frames with `Bytes::copy_from_slice` (`kimojio-stack-http/src/h2/client.rs:197-237`).
- HTTP/2 `read_response_with_trailers` loops reading frames until the requested stream has a completed response, reset error, or applicable GOAWAY error (`kimojio-stack-http/src/h2/client.rs:128-160`).
- HTTP/2 response DATA processing consumes inbound connection window, queues connection and stream window updates, appends DATA to a pending `BodyBuilder`, flushes pending updates when needed, and completes the response at END_STREAM (`kimojio-stack-http/src/h2/client.rs:337-366`).
- HTTP/2 response completion converts pending headers and buffered body into a `Response<Body>`, stores trailers separately in `ResponseWithTrailers`, and removes the stream from connection state (`kimojio-stack-http/src/h2/client.rs:368-385`).
- HTTP/2 tests cover stackful client to Hyper server, Hyper client to stackful server, settings variations, and out-of-order concurrent streams preserving association (`kimojio-stack-http/tests/h2_interop.rs:23-65`, `kimojio-stack-http/tests/h2_interop.rs:67-120`, `kimojio-stack-http/tests/h2_interop.rs:122-179`, `kimojio-stack-http/src/h2/mod.rs:192-220`).
- Protocol-limit tests cover oversized HTTP/1.1 headers/body, invalid HTTP/2 frame type, oversized HTTP/2 frame, oversized header block, and oversized HTTP/2 body paths (`kimojio-stack-http/tests/protocol_limits.rs:11-88`, `kimojio-stack-http/tests/protocol_limits.rs:90-209`).

### Stack Transport and TLS Integration

- `StackTransport` is an enum over plaintext `OwnedFd` and optional TLS `TlsStream`, with constructors for plaintext and TLS (`kimojio-stack-http/src/transport.rs:11-26`).
- `StackTransport::read`, `write`, `shutdown_write`, `shutdown`, and `close` dispatch plaintext operations through `RuntimeContext` and TLS operations through `TlsStream`, mapping errors into HTTP `Error` (`kimojio-stack-http/src/transport.rs:28-34`, `kimojio-stack-http/src/transport.rs:52-58`, `kimojio-stack-http/src/transport.rs:71-94`).
- `StackTransport::read_exact_or_eof` loops until the requested buffer is filled or a read returns zero; `write_all` loops until the full slice is written and treats zero write as `Errno::PIPE` (`kimojio-stack-http/src/transport.rs:36-69`).
- HTTP TLS support defines ALPN protocol IDs for HTTP/1.1 and HTTP/2 and validates selected ALPN against an expected protocol when requested (`kimojio-stack-http/src/tls.rs:10-50`).
- HTTP TLS helpers perform client and server handshakes through `TlsContext`, accept a caller-provided TLS buffer size, wrap the result as `StackTransport`, and optionally validate the negotiated HTTP protocol (`kimojio-stack-http/src/tls.rs:52-85`).
- `kimojio-stack-tls` adapts OpenSSL memory BIO integration to stackful I/O; its crate docs state that `RuntimeContext::read` fills the OpenSSL encrypted read buffer and `RuntimeContext::write` drains the encrypted write buffer without an intermediate copy between socket and OpenSSL encrypted buffers (`kimojio-stack-tls/src/lib.rs:4-12`).
- `TlsContext` can be built from an OpenSSL `SslContext`, performs server/client handshakes over an `OwnedFd`, and applies the server name for client-side hostname/SNI configuration (`kimojio-stack-tls/src/lib.rs:21-69`).
- `TlsStream::read` and `write` drive OpenSSL `WantRead`/`WantWrite` by filling/draining TLS buffers through the stack runtime; `shutdown_write` calls `RuntimeContext::shutdown` and `close` calls `RuntimeContext::close` (`kimojio-stack-tls/src/lib.rs:101-178`, `kimojio-stack-tls/src/lib.rs:180-235`).
- TLS interop tests construct TLS transports with expected ALPN and exchange an HTTP/1.1 request/response over stackful TLS (`kimojio-stack-http/tests/tls_interop.rs:13-73`).

### HTTP Error Surface

- `ErrorKind` categorizes HTTP failures as I/O, TLS, parse, protocol, unsupported, size limit, EOF, or peer reset (`kimojio-stack-http/src/error.rs:8-19`).
- `LimitKind` categorizes size limits as start line, headers, body, frame, or message (`kimojio-stack-http/src/error.rs:21-29`).
- `Error` variants preserve `Errno` for I/O/TLS, static parse/protocol/unsupported messages, size-limit kind/limit/actual values, EOF, and peer reset data including stream id, last stream id, error code, and debug data (`kimojio-stack-http/src/error.rs:31-51`).
- `Error::kind` maps every error variant into its stable `ErrorKind`, and `Display` formats variant-specific messages including peer reset details (`kimojio-stack-http/src/error.rs:53-100`, `kimojio-stack-http/src/error.rs:108-148`).

### Stack Runtime Scheduling and I/O Surface

- `Runtime::block_on` creates the scheduler, builds a root `RuntimeContext`, runs the supplied closure, and returns the closure output (`kimojio-stack/src/lib.rs:12-18`, `kimojio-stack/src/lib.rs:506-530`).
- Stackful work is created through `RuntimeContext::scope` and `Scope::spawn`, and the scope waits for all children before returning (`kimojio-stack/src/lib.rs:19-28`, `kimojio-stack/src/lib.rs:547-571`, `kimojio-stack/src/lib.rs:1890-1977`).
- Runtime configuration includes stack size, ring entries, ring enter policy, registered file slots, and registered buffer slots, with defaults set in `RuntimeConfig::default` (`kimojio-stack/src/lib.rs:418-443`).
- `Runtime` constructors support default runtime, custom stack size, custom config, and registered fixed-file/fixed-buffer slot counts (`kimojio-stack/src/lib.rs:466-504`).
- Runtime docs state that blocking-from-coroutine operations submit one SQE, park only the current stackful coroutine, and return after the CQE is reaped (`kimojio-stack/src/lib.rs:59-78`).
- `RuntimeContext::socket` uses io_uring when the socket opcode is supported and otherwise falls back to `socket(2)` synchronously; `bind` and `listen` are synchronous setup helpers (`kimojio-stack/src/lib.rs:1151-1190`).
- `RuntimeContext::accept` and `connect` use io_uring accept/connect opcodes and return an `OwnedFd` or unit result (`kimojio-stack/src/lib.rs:1192-1213`).
- `RuntimeContext::read`, `readv`, `write`, and `writev` submit io_uring read/write operations against borrowed fds and borrowed buffers, returning byte counts (`kimojio-stack/src/lib.rs:1215-1241`, `kimojio-stack/src/lib.rs:1389-1416`).
- Registered fd and registered buffer APIs support fixed-resource read/write combinations for lower-overhead I/O (`kimojio-stack/src/lib.rs:97-106`, `kimojio-stack/src/lib.rs:640-657`, `kimojio-stack/src/lib.rs:1243-1294`, `kimojio-stack/src/lib.rs:1418-1469`).
- Socket-specific `send`, `sendmsg`, `recv`, and `recvmsg` are available as io_uring operations on connected sockets (`kimojio-stack/src/lib.rs:1471-1512`).
- `RuntimeContext::close` consumes an `OwnedFd` and submits close through io_uring; `shutdown` submits socket shutdown through io_uring (`kimojio-stack/src/lib.rs:1607-1624`).
- Scheduler ring entry runs ready tasks according to `RingEnterPolicy`, enters io_uring in wait mode only when no tasks are ready and I/O is in flight, and handles external wake readiness when present (`kimojio-stack/src/lib.rs:37-57`, `kimojio-stack/src/lib.rs:3566-3632`).

### Async Buffers, Waitables, Timeouts, and Cancellation

- Runtime docs state that `read_async` and `write_async` return `IoResult`, `try_get` observes reaped completions without driving the scheduler, `get` parks cooperatively, and `RuntimeContext::join` can wait for multiple `Waitable` values with an optional timeout (`kimojio-stack/src/lib.rs:80-86`).
- Runtime docs state that async I/O owns buffers through `IoReadBuffer` and `IoWriteBuffer`, returns buffers on successful completion, and intentionally leaks backing buffers if a pending `IoResult` is dropped to preserve kernel-visible memory safety (`kimojio-stack/src/lib.rs:87-95`).
- `IoReadBuffer` and `IoWriteBuffer` define pointer/length access for owned buffers, and `Vec<u8>` plus `Box<[u8]>` implement both traits (`kimojio-stack/src/lib.rs:2090-2162`).
- `read_async` and `write_async` submit read/write SQEs using owned buffers and return `IoResult<ReadOutput<B>, B>` or `IoResult<WriteOutput<B>, B>` (`kimojio-stack/src/lib.rs:1296-1314`, `kimojio-stack/src/lib.rs:1514-1532`).
- `ReadOutput` and `WriteOutput` return byte counts plus the owned buffer used by the operation (`kimojio-stack/src/lib.rs:2164-2180`).
- `IoResult` is `must_use`, stores scheduler state plus the owned buffer, supports `try_get`, `get`, waitable readiness, and leaks the pending buffer on drop when the CQE has not completed (`kimojio-stack/src/lib.rs:2473-2592`).
- `Waitable` is a readiness-only trait with `is_ready` and hidden waiter registration; `RuntimeContext::wait_all`, `wait_any`, `select`, and `join` wait on heterogeneous waitables by reference and support optional timeouts (`kimojio-stack/src/wait.rs:9-22`, `kimojio-stack/src/wait.rs:52-161`).
- `WaitError` distinguishes timed-out waits from empty wait input (`kimojio-stack/src/wait.rs:29-50`).
- `RuntimeContext::sleep` submits a timeout and treats either successful timeout completion or `Errno::TIME` as success (`kimojio-stack/src/lib.rs:628-638`).
- `RuntimeContext::timeout` returns a waitable `Timeout` handle; `Timeout` supports `try_wait`, `wait`, `update`, and `cancel`, and implements `Waitable` (`kimojio-stack/src/lib.rs:1626-1629`, `kimojio-stack/src/lib.rs:2649-2780`).
- Runtime context cancellation supports `cancel_io` for a pending `IoResult` via `AsyncCancel` and `cancel_matching` via `AsyncCancel2`, returning `OPNOTSUPP` when the kernel opcode is not supported (`kimojio-stack/src/lib.rs:1631-1661`).
- Internal timeout cancellation submits `TimeoutRemove`, waits for both original timeout and cancel state, and recycles both I/O states (`kimojio-stack/src/lib.rs:1807-1825`).
- Tests cover a join timeout that leaves the original async result usable, followed by completing the read/write and retrieving both buffers/results (`kimojio-stack/src/lib.rs:4947-4972`).
- Runtime allocation tests warm blocking pipe ping-pong and async pipe ping-pong with reused buffers, measure hot loops with a counting allocator, and assert zero allocating operations and zero allocated/reallocated bytes (`kimojio-stack/src/lib.rs:4975-5042`, `kimojio-stack/src/lib.rs:5044-5117`).

### Runtime Concurrency and Interop Surfaces

- `Semaphore` exposes `new`, `available_permits`, `add_permits`, `try_acquire`, and stackful `acquire`, and parks the current coroutine when no permit is available (`kimojio-stack/src/semaphore.rs:9-70`).
- `Semaphore` implements `Waitable` by reporting readiness when permits are available and registering waiters only when not ready (`kimojio-stack/src/semaphore.rs:72-84`).
- The channel module exposes bounded, unbounded, and cross-thread channels plus send/receive error types (`kimojio-stack/src/channel/mod.rs:4-15`, `kimojio-stack/src/channel/mod.rs:16-83`).
- Cross-thread channels are bounded, use explicit endpoint types for OS-thread, stackful, and Tokio-compatible participants, and use a preallocated `ArrayQueue` for message storage (`kimojio-stack/src/channel/cross_thread.rs:4-18`, `kimojio-stack/src/channel/cross_thread.rs:36-52`, `docs/cross-runtime-channel.md:96-108`).
- Cross-runtime channel docs list endpoint combinations, nonblocking fast paths, drop behavior, and stackful waiting guidance (`docs/cross-runtime-channel.md:10-15`, `docs/cross-runtime-channel.md:57-94`).

### Low-Allocation Client Patterns in Stack Telemetry

- The telemetry crate explicitly exposes caller-controlled data construction/export building blocks and does not install a global provider, start background workers, or perform implicit periodic export (`kimojio-stack-opentelemetry/src/lib.rs:4-8`).
- `ExportClientConfig` stores maximum gRPC message length and encoded request limits, with defaults of 4 MiB for both message and encoded request limits (`kimojio-stack-opentelemetry/src/client.rs:11-35`, `kimojio-stack-opentelemetry/src/lib.rs:278-291`).
- `UnaryExportClient` wraps `UnaryClient`, can be constructed from an HTTP/2 client, `StackTransport`, plaintext socket, or TLS stream, and exports caller-provided prost messages through a path and empty metadata (`kimojio-stack-opentelemetry/src/client.rs:37-89`).
- `UnaryExportClient::finish` closes the underlying gRPC connection through the runtime context (`kimojio-stack-opentelemetry/src/client.rs:91-98`).
- Logs and metrics public clients provide constructors from caller-created HTTP/2 connections, stack transports, plaintext sockets, and TLS streams, and `finish` methods that close the underlying connection (`kimojio-stack-opentelemetry/src/logs.rs:192-260`, `kimojio-stack-opentelemetry/src/metrics.rs:289-355`).
- Empty log and metric batches return default export results locally without sending to transport (`kimojio-stack-opentelemetry/src/logs.rs:239-255`, `kimojio-stack-opentelemetry/src/metrics.rs:336-349`, `kimojio-stack-opentelemetry/src/metrics.rs:520-548`).
- Log batch encoding constructs one resource/scope container and maps caller-owned records into protocol records; `encode_request` checks encoded length before returning encoded bytes (`kimojio-stack-opentelemetry/src/logs.rs:87-154`).
- Metric batch encoding groups monotonic sums and gauges by name through `BTreeMap`, validates non-empty names and timestamp intervals, rejects conflicting metric shapes by name, and checks encoded length before encoding (`kimojio-stack-opentelemetry/src/metrics.rs:116-213`).
- Telemetry errors expose stable categories for validation, size limit, transport, protocol, encode, decode, status, unsupported compression, and unsupported features, with gRPC error kinds mapped into telemetry error kinds (`kimojio-stack-opentelemetry/src/error.rs:6-71`).
- Telemetry allocation tests warm local export loops, measure repeated exports with a counting allocator, and assert current hot-path allocation ceilings for logs and metrics (`kimojio-stack-opentelemetry/src/logs.rs:512-575`, `kimojio-stack-opentelemetry/src/metrics.rs:720-783`).
- Stack telemetry docs describe explicit scheduling and predictable costs, caller-built batches, direct export calls, explicit close, empty batch local behavior, size-limit errors, unsupported shape errors, and no automatic retry/background batching (`docs/stack-opentelemetry.md:1-16`, `docs/stack-opentelemetry.md:27-47`).

### Durable-Storage-Related Existing Source

- The workspace member list does not include a dedicated durable-storage client crate; listed members are runtime, macros, stack runtime/protocol/telemetry/TLS crates, perf, and examples (`Cargo.toml:1-16`).
- Existing storage-worded source examined in this repository is local file/runtime I/O: `kimojio::operations::open` opens files with `openat` and returns an `OwnedFdFuture` (`kimojio/src/operations.rs:80-97`).
- The `examples` copyfile utility demonstrates local file copying with configurable block size, in-flight I/O count, and polled/O_DIRECT mode (`examples/src/bin/copyfile.rs:4-14`, `examples/src/bin/copyfile.rs:73-108`).
- The copyfile example uses aligned buffers for O_DIRECT mode and documents alignment requirements for buffer address, I/O size, and file offset (`examples/src/bin/copyfile.rs:111-132`).

## Code References

- `Spec.md:8-12` - Durable storage client overview and operational-safety framing.
- `Spec.md:119-123` - Explicit operation inputs, cancellation, streamed output, metadata/status preservation, and connection/concurrency bounds.
- `Cargo.toml:1-16` - Workspace members.
- `Cargo.toml:18-25` - Workspace package metadata.
- `.github/workflows/rust.yml:34-50` - CI clippy, tests, docs, format, and all-features checks.
- `rust-toolchain.toml:1-7` - Pinned Rust toolchain channel.
- `kimojio-stack-http/src/lib.rs:9-25` - HTTP crate modules and re-exports.
- `kimojio-stack-http/src/config.rs:4-24` - HTTP config fields/defaults.
- `kimojio-stack-http/src/body.rs:8-104` - Body limits, buffered body, and body builder.
- `kimojio-stack-http/src/transport.rs:11-94` - Plaintext/TLS stack transport operations.
- `kimojio-stack-http/src/client.rs:9-48` - Protocol-neutral HTTP client wrapper.
- `kimojio-stack-http/src/http1/client.rs:11-44` - HTTP/1.1 client connection.
- `kimojio-stack-http/src/http1/codec.rs:26-186` - HTTP/1.1 request/response read/write codec.
- `kimojio-stack-http/src/h2/mod.rs:4-42` - HTTP/2 exports and config defaults.
- `kimojio-stack-http/src/h2/client.rs:38-48` - HTTP/2 client state fields.
- `kimojio-stack-http/src/h2/client.rs:84-117` - HTTP/2 request send path.
- `kimojio-stack-http/src/h2/client.rs:197-237` - HTTP/2 DATA write and flow-control path.
- `kimojio-stack-http/src/h2/client.rs:337-385` - HTTP/2 response DATA processing and completion.
- `kimojio-stack-http/src/h2/connection.rs:16-28` - HTTP/2 connection state fields.
- `kimojio-stack-http/src/h2/connection.rs:166-251` - HTTP/2 flow-control capacity and window updates.
- `kimojio-stack-http/src/tls.rs:10-85` - HTTP ALPN and TLS transport helpers.
- `kimojio-stack-tls/src/lib.rs:4-12` - Stack TLS memory-BIO data path documentation.
- `kimojio-stack-tls/src/lib.rs:101-235` - TLS handshake/read/write/shutdown/close loops.
- `kimojio-stack-http/src/error.rs:8-148` - HTTP error kinds, variants, and display.
- `kimojio-stack/src/lib.rs:59-106` - Runtime I/O API and async/fixed-resource model docs.
- `kimojio-stack/src/lib.rs:418-504` - Runtime configuration and constructors.
- `kimojio-stack/src/lib.rs:1151-1629` - Socket, connect, read/write, send/recv, close/shutdown, timeout APIs.
- `kimojio-stack/src/lib.rs:1631-1661` - Async I/O cancellation APIs.
- `kimojio-stack/src/lib.rs:2090-2180` - Owned buffer traits and read/write outputs.
- `kimojio-stack/src/lib.rs:2473-2592` - `IoResult` readiness/get/drop behavior.
- `kimojio-stack/src/lib.rs:2649-2780` - Waitable timeout handle.
- `kimojio-stack/src/wait.rs:9-161` - Waitable trait and wait/select/join APIs.
- `kimojio-stack/src/semaphore.rs:9-84` - Stackful semaphore and waitable implementation.
- `kimojio-stack/src/channel/cross_thread.rs:4-18` - Cross-thread endpoint model.
- `kimojio-stack-opentelemetry/src/lib.rs:4-8` - Caller-controlled telemetry export scope.
- `kimojio-stack-opentelemetry/src/client.rs:11-98` - Export client config and unary client wrapper.
- `kimojio-stack-opentelemetry/src/logs.rs:87-260` - Log batch/client construction and export.
- `kimojio-stack-opentelemetry/src/metrics.rs:116-355` - Metric batch/client construction and export.
- `kimojio-stack-opentelemetry/src/error.rs:6-71` - Telemetry error taxonomy.
- `docs/stack-http-grpc.md:1-17` - Stack HTTP/gRPC guide purpose and crate table.
- `docs/stack-opentelemetry.md:1-16` - Telemetry guide purpose and supported signals.
- `docs/cross-runtime-channel.md:10-15` - Cross-runtime endpoint types.
- `kimojio/src/operations.rs:80-97` - Local file open operation future.
- `examples/src/bin/copyfile.rs:4-14` - Local file copy example purpose.

## Architecture Documentation

- The stack HTTP/gRPC docs state that public stack crates do not depend on Tokio, Hyper, Tonic, or another async runtime, and use those crates only as test interoperability peers (`docs/stack-http-grpc.md:1-10`).
- The HTTP transport model uses caller-provided connected plaintext `OwnedFd` or `TlsStream`, with TLS enabled by feature and ALPN validation available through helper constructors (`docs/stack-http-grpc.md:19-33`, `kimojio-stack-http/src/transport.rs:11-26`, `kimojio-stack-http/src/tls.rs:52-85`).
- The HTTP guide describes supported behavior as HTTP/1.1 fixed-length/chunked/EOF bodies, sequential keep-alive, TLS, and HTTP/2 prior-knowledge/TLS ALPN/settings/HPACK/trailers/concurrent streams/flow control/reset/goaway handling (`docs/stack-http-grpc.md:35-58`).
- The HTTP guide describes explicit limits and bounded buffering through `HttpConfig`, gRPC client/server configs, HTTP/2 flow-control windows, and inspectable `ErrorKind`/`LimitKind` (`docs/stack-http-grpc.md:97-111`).
- The runtime allocation model documents bounded setup allocations, hot I/O paths designed to avoid Rust heap allocation after warmup, reuse of I/O state and completion scratch storage, inline single-waiter storage, and test-only allocation assertions (`kimojio-stack/src/lib.rs:144-161`).
- The stack telemetry docs describe an explicit lifecycle: build metadata, build batch, create client, call export, and finish/close (`docs/stack-opentelemetry.md:27-36`).
- Cross-runtime channel docs describe preallocated queue storage, ready paths without channel-owned heap allocation after construction, adaptive retry/yield behavior, and external wake integration for stackful cross-thread wakeups (`docs/cross-runtime-channel.md:96-108`).

## Open Questions

None from this descriptive code research.
