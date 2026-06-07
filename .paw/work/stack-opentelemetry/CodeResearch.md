---
date: 2026-06-07T22:17:45.655+00:00
git_commit: "e3a962793e73f199a5472a025c51610fd90b02b5"
branch: "detached HEAD (origin/stackful, stackful)"
repository: kimojio-rs
topic: "Stack OpenTelemetry code research"
tags: [research, codebase, opentelemetry, grpc, http, tls, prost, workspace, testing]
status: complete
last_updated: 2026-06-07
---

# Research: Stack OpenTelemetry

## Research Question

Map the existing implementation details relevant to adding a low-level OpenTelemetry-compatible logs and metrics export capability: stackful gRPC client/server APIs, stackful HTTP transport and TLS patterns, prost/prost-build usage, workspace crate conventions, integration-test patterns with tonic/tokio and TLS receiver/server setup, documentation infrastructure, verification commands, and dependency context for OpenTelemetry protocol crates. The feature specification calls for logs and metrics export to an OpenTelemetry-compatible destination, explicit caller-controlled connection/export/error behavior, secure and insecure transport compatibility, and receiver-backed interoperability testing (`.paw/work/stack-opentelemetry/Spec.md:6-20`, `.paw/work/stack-opentelemetry/Spec.md:57-88`, `.paw/work/stack-opentelemetry/Spec.md:105-122`).

## Summary

- `kimojio-stack-grpc` is a unary-first crate over `kimojio-stack-http` HTTP/2; its public surface exports `UnaryClient`, `UnaryServer`, `Metadata`, `Status`, prost message frame helpers, and inspectable errors (`kimojio-stack-grpc/src/lib.rs:4-18`, `kimojio-stack-grpc/src/client.rs:36-67`, `kimojio-stack-grpc/src/server.rs:31-76`).
- The gRPC client encodes a `prost::Message`, builds a POST request with `application/grpc` and `TE: trailers`, sends it through `h2::ClientConnection`, reads response trailers, validates HTTP status/content type/gRPC status, and decodes one response message (`kimojio-stack-grpc/src/client.rs:46-62`, `kimojio-stack-grpc/src/client.rs:69-89`, `kimojio-stack-grpc/src/client.rs:91-115`).
- The gRPC server stores path-keyed unary handlers, accepts one HTTP/2 request, validates method/content type/TE, converts headers to metadata, decodes one request frame, invokes the handler, and writes either a response with gRPC trailers or a status response (`kimojio-stack-grpc/src/server.rs:31-76`, `kimojio-stack-grpc/src/server.rs:78-91`, `kimojio-stack-grpc/src/server.rs:131-165`, `kimojio-stack-grpc/src/server.rs:167-240`).
- `kimojio-stack-http` exposes plaintext/TLS `StackTransport` over `OwnedFd` or `TlsStream`, protocol-specific HTTP/1.1 and HTTP/2 connections, protocol-neutral wrappers, bounded `Body`/`BodyBuilder`, and inspectable transport/protocol errors (`kimojio-stack-http/src/lib.rs:4-25`, `kimojio-stack-http/src/transport.rs:11-94`, `kimojio-stack-http/src/client.rs:15-48`, `kimojio-stack-http/src/server.rs:15-55`, `kimojio-stack-http/src/body.rs:8-104`, `kimojio-stack-http/src/error.rs:8-148`).
- TLS is feature-gated in `kimojio-stack-http`; default features include `tls`, the optional dependency is `kimojio-stack-tls`, and helper functions perform client/server handshakes, ALPN protocol inspection, and optional expected-protocol validation (`kimojio-stack-http/Cargo.toml:11-23`, `kimojio-stack-http/src/tls.rs:10-85`).
- Existing interoperability tests use local Tokio/hyper/tonic peers, stackful servers with `Runtime::new().block_on`, `mpsc` channels for bound addresses, and OpenSSL-generated self-signed TLS material for in-process TLS cases (`kimojio-stack-http/tests/support/mod.rs:40-76`, `kimojio-stack-grpc/tests/client_interop.rs:127-153`, `kimojio-stack-grpc/tests/server_interop.rs:78-170`, `kimojio-stack-grpc/tests/tls_interop.rs:94-140`).
- The workspace centralizes shared package metadata and dependency versions in the root manifest; stackful crates inherit workspace metadata and use `.workspace = true`, while `kimojio-stack-grpc` depends normally on `prost` but keeps `tonic`, `tokio`, `hyper`, and TLS-enabled HTTP as dev-dependencies for interoperability tests (`Cargo.toml:1-23`, `Cargo.toml:25-84`, `kimojio-stack-grpc/Cargo.toml:11-30`).

## Documentation System

- **Framework**: Plain Markdown plus Rustdoc/Cargo docs; the README links to docs.rs and repository Markdown guides, and CI validates doc tests with `cargo +${{ matrix.rust }} test --doc` (`README.md:5-5`, `README.md:21-35`, `README.md:94-103`, `.github/workflows/rust.yml:43-44`).
- **Docs Directory**: `docs/` contains Markdown guides such as `docs/stack-http-grpc.md`, `docs/virtual-clock-guide.md`, `docs/virtual-clock-design.md`, and `docs/cross-runtime-channel.md`; README links the stackful HTTP/gRPC guide and virtual-clock/cross-runtime guides (`docs/stack-http-grpc.md:1-18`, `docs/virtual-clock-guide.md:1-18`, `README.md:30-35`, `README.md:94-103`).
- **Navigation Config**: Repository docs are linked directly from `README.md`; no static-site navigation file is referenced by the README docs links (`README.md:30-35`, `README.md:94-103`).
- **Style Conventions**: Docs use H1/H2 headings, short paragraphs, tables, ordered lists, and fenced code blocks; examples include the stackful HTTP/gRPC guide tables and shell fences, plus virtual-clock TOML/Rust fences (`docs/stack-http-grpc.md:12-18`, `docs/stack-http-grpc.md:50-58`, `docs/stack-http-grpc.md:75-95`, `docs/stack-http-grpc.md:139-144`, `docs/virtual-clock-guide.md:8-35`).
- **Build Command**: The observed docs verification command is `cargo +${{ matrix.rust }} test --doc` in CI (`.github/workflows/rust.yml:43-44`).
- **Standard Files**: Root standard files include `README.md`, `CONTRIBUTING.md`, `SECURITY.md`, and `LICENSE.txt`; README links contributing and license sections, and the contributing/security files contain project-level contribution/security text (`README.md:70-107`, `CONTRIBUTING.md:1-14`, `SECURITY.md:1-14`).

## Verification Commands

- **Format Command**: Repository instructions list `cargo fmt`, and CI checks formatting with `cargo +${{ matrix.rust }} fmt --all -- --check` (`.github/copilot-instructions.md:7-10`, `.github/workflows/rust.yml:46-47`).
- **Lint Command**: Repository instructions list `cargo clippy` and `cargo clippy --all-targets --all-features`; CI runs `cargo +${{ matrix.rust }} clippy --all-targets -- -D warnings` and `cargo +${{ matrix.rust }} clippy --all-targets --all-features -- -D warnings` (`.github/copilot-instructions.md:7-16`, `.github/workflows/rust.yml:34-50`).
- **Test Command**: CI runs `cargo +${{ matrix.rust }} test --all-targets --features ssl2`, `cargo +${{ matrix.rust }} test --all-targets --features virtual-clock`, and `cargo +${{ matrix.rust }} test --doc` (`.github/workflows/rust.yml:37-44`).
- **Build Command**: The CI step named `Build` uses clippy over all targets with warnings denied (`.github/workflows/rust.yml:34-35`).
- **Type Check**: No separate `cargo check` command is shown in the inspected CI; Rust compilation/type checking occurs through the clippy and test commands listed above (`.github/workflows/rust.yml:34-50`).
- **Toolchain**: The repository pins Rust `1.92.0` in `rust-toolchain.toml`, while CI tests Rust `1.90.0` and `1.92.0` (`rust-toolchain.toml:1-7`, `.github/workflows/rust.yml:18-32`).

## Detailed Findings

### Workspace Structure and Crate Conventions

- The root workspace includes `kimojio`, `kimojio-macros`, `kimojio-stack`, `kimojio-stack-grpc`, `kimojio-stack-http`, `kimojio-stack-tls`, `kimojio-tls`, `perf`, and `examples`, with resolver `3` (`Cargo.toml:1-15`).
- Workspace package metadata centralizes version `0.16.4`, edition `2024`, MIT license, homepage, repository, and README settings (`Cargo.toml:17-23`).
- Workspace dependency versions are centralized under `[workspace.dependencies]`, including `bytes`, `http`, `hpack`, `httparse`, `hyper`, `kimojio-stack-grpc`, `kimojio-stack-http`, `kimojio-stack-tls`, `prost`, `tokio`, `tonic`, and `tonic-prost` (`Cargo.toml:25-84`).
- Production crates inherit workspace package metadata with `version.workspace`, `edition.workspace`, `license.workspace`, `homepage.workspace`, `repository.workspace`, and `readme.workspace` entries (`kimojio-stack/Cargo.toml:1-9`, `kimojio-stack-http/Cargo.toml:1-9`, `kimojio-stack-grpc/Cargo.toml:1-9`, `kimojio-stack-tls/Cargo.toml:1-9`, `kimojio-tls/Cargo.toml:1-9`, `kimojio-macros/Cargo.toml:1-9`).
- Member manifests use workspace dependencies with `.workspace = true`, while feature-specific dependencies remain explicit; examples include `kimojio-stack` using `rustix` features and `kimojio-stack-http` making `kimojio-stack-tls` optional behind the `tls` feature (`kimojio-stack/Cargo.toml:11-20`, `kimojio-stack-http/Cargo.toml:11-23`).
- The `examples` package is non-published, has feature flags that map to `kimojio/tls` and `kimojio/virtual-clock`, and declares binaries with `required-features` for TLS and virtual-clock examples (`examples/Cargo.toml:1-17`, `examples/Cargo.toml:19-36`).
- Benchmarks are declared per crate with `[[bench]]` and `harness = false`, including `kimojio-stack-http`'s `http_request_response`, `kimojio-stack-grpc`'s `unary_rpc`, `kimojio-stack`'s `runtime_baseline`, and `kimojio` runtime benchmarks (`kimojio-stack-http/Cargo.toml:33-35`, `kimojio-stack-grpc/Cargo.toml:32-34`, `kimojio-stack/Cargo.toml:22-24`, `kimojio/Cargo.toml:63-77`).

### Stackful Runtime and I/O Model Used by HTTP/gRPC

- `kimojio-stack` crate docs describe stackful coroutines as cooperative, current-OS-thread execution, scoped spawning, and a `RuntimeContext` capability for runtime services (`kimojio-stack/src/lib.rs:4-17`).
- `Runtime::block_on` creates scheduler state, builds a root `RuntimeContext`, invokes the supplied closure, and asserts scoped coroutines and in-flight I/O are drained before return (`kimojio-stack/src/lib.rs:506-530`).
- `RuntimeContext::socket` uses io_uring `IORING_OP_SOCKET` when available and falls back to synchronous `socket(2)` otherwise; `bind` and `listen` are synchronous setup helpers (`kimojio-stack/src/lib.rs:1151-1189`).
- `RuntimeContext::accept` and `RuntimeContext::connect` submit io_uring accept/connect operations for socket connections (`kimojio-stack/src/lib.rs:1192-1213`).
- `RuntimeContext::read` and `RuntimeContext::write` submit io_uring read/write operations and block only the current stackful coroutine (`kimojio-stack/src/lib.rs:1215-1229`, `kimojio-stack/src/lib.rs:1389-1403`).
- `RuntimeContext::close` consumes an `OwnedFd` and closes it through io_uring; `RuntimeContext::shutdown` submits socket shutdown through io_uring (`kimojio-stack/src/lib.rs:1607-1624`).
- The runtime parks a coroutine waiting for I/O by adding a waiter and calling `park`, while the root scheduler drives tasks and panics on deadlock if no runnable coroutine can progress (`kimojio-stack/src/lib.rs:1780-1788`, `kimojio-stack/src/lib.rs:1835-1845`).

### kimojio-stack-http Transport, TLS, and Protocol Boundaries

- `kimojio-stack-http` declares low-level transport, buffering, error, HTTP/1.1, HTTP/2, and TLS boundary modules and re-exports `Body`, `BodyLimits`, `HttpConfig`, `Error`, `Headers`, `Trailers`, and `StackTransport` (`kimojio-stack-http/src/lib.rs:4-25`).
- `StackTransport` stores either `Plaintext(OwnedFd)` or, with the `tls` feature, `Tls(TlsStream)` (`kimojio-stack-http/src/transport.rs:11-26`).
- `StackTransport::read`, `write`, `shutdown_write`, `shutdown`, and `close` delegate to `RuntimeContext` for plaintext and to `TlsStream` for TLS, mapping errors through `Error::io` or `Error::tls` (`kimojio-stack-http/src/transport.rs:28-94`).
- `StackTransport::read_exact_or_eof` loops until the requested buffer is filled or a zero-length read is observed, and `write_all` loops until all bytes are written or returns `Errno::PIPE` on zero write progress (`kimojio-stack-http/src/transport.rs:36-69`).
- `HttpConfig` carries start-line, header count, header bytes, body bytes, and read-buffer limits, with default maximum body bytes of `4 * 1024 * 1024` and read buffer size `16 * 1024` (`kimojio-stack-http/src/config.rs:4-24`).
- `BodyLimits` enforces maximum buffered body size; `Body` wraps `bytes::Bytes`, and `BodyBuilder` incrementally accumulates bytes with limit checks before freezing to `Body` (`kimojio-stack-http/src/body.rs:8-104`).
- Protocol-neutral `client::ClientConnection` and `server::ServerConnection` keep concrete HTTP/1.1 and HTTP/2 connections inline in enums and delegate `send`, `serve_one`, `close`, and HTTP/2 shutdown behavior to the selected protocol implementation (`kimojio-stack-http/src/client.rs:15-48`, `kimojio-stack-http/src/server.rs:15-55`).
- HTTP errors are represented by `ErrorKind`, `LimitKind`, and `Error` variants for I/O, TLS, parse, protocol, unsupported, size limits, EOF, and peer resets (`kimojio-stack-http/src/error.rs:8-51`, `kimojio-stack-http/src/error.rs:53-148`).

### HTTP/2 Client and Server Patterns

- `h2::ClientConnection` stores a `StackTransport`, `HttpConfig`, `ConnectionState`, initialization flag, next outbound stream ID, pending/completed response maps, reset stream map, and optional GOAWAY error (`kimojio-stack-http/src/h2/client.rs:38-48`).
- `h2::ClientConnection::send_request` ensures the connection is initialized, assigns odd stream IDs, opens outbound stream state, encodes request headers with HPACK, writes HEADERS, and writes DATA when the body is non-empty (`kimojio-stack-http/src/h2/client.rs:84-117`).
- `h2::ClientConnection::read_response_with_trailers` reads frames until a target stream completes, a reset is recorded, or GOAWAY applies to that stream (`kimojio-stack-http/src/h2/client.rs:128-160`).
- The HTTP/2 client initialization path writes the client preface and local SETTINGS, reads frames until peer SETTINGS are received, acknowledges settings through `ConnectionState`, and flushes pending frames (`kimojio-stack-http/src/h2/client.rs:166-195`).
- HTTP/2 client DATA writing observes both connection and stream outbound capacity, reads inbound frames while capacity is zero, chunks payload by peer max frame size, consumes outbound windows, and emits DATA frames (`kimojio-stack-http/src/h2/client.rs:197-237`).
- HTTP/2 client response completion handles HEADERS, DATA, trailers, body limits, connection/window updates, and moves completed responses to the `completed` map (`kimojio-stack-http/src/h2/client.rs:304-386`).
- `h2::ServerConnection` stores a `StackTransport`, config, connection state, initialization flag, pending request map, completed request queue, and optional GOAWAY error (`kimojio-stack-http/src/h2/server.rs:37-45`).
- `h2::ServerConnection::accept` initializes the connection, returns queued completed requests, reads frames until a request completes, and returns `Ok(None)` on EOF with no pending requests or after GOAWAY with no pending requests (`kimojio-stack-http/src/h2/server.rs:69-92`).
- `h2::ServerConnection::send_response_with_trailers` writes response HEADERS, body DATA, and optional trailer HEADERS with END_STREAM before removing stream state (`kimojio-stack-http/src/h2/server.rs:103-149`).
- `h2::ServerConnection::shutdown_write_and_close_after_peer` half-closes the write side, reads until EOF or peer-closed errors, and then closes the transport (`kimojio-stack-http/src/h2/server.rs:196-216`).
- HTTP/2 server initialization reads the client preface and SETTINGS, sends local SETTINGS, flushes pending settings ACKs/window updates, and marks the connection initialized (`kimojio-stack-http/src/h2/server.rs:218-240`).
- The HTTP/2 codec converts `HttpConfig` to SETTINGS, reads frame headers and payloads with max frame-size enforcement, writes frame bytes through `StackTransport::write_all`, writes/reads the client preface, collects CONTINUATION blocks, and converts pseudo-headers to `http::Request`/`Response` values (`kimojio-stack-http/src/h2/codec.rs:18-65`, `kimojio-stack-http/src/h2/codec.rs:67-153`, `kimojio-stack-http/src/h2/codec.rs:155-198`, `kimojio-stack-http/src/h2/codec.rs:200-251`, `kimojio-stack-http/src/h2/codec.rs:254-360`).
- `ConnectionState` owns local/peer settings, active/closed streams, flow-control windows, pending outbound frames, HPACK encoder/decoder, continuation tracking, and preface state (`kimojio-stack-http/src/h2/connection.rs:16-28`).
- `ConnectionState` enforces max concurrent inbound/outbound streams, tracks stream lifecycle, computes outbound capacity from connection and stream windows, and queues connection/stream `WINDOW_UPDATE` frames (`kimojio-stack-http/src/h2/connection.rs:130-148`, `kimojio-stack-http/src/h2/connection.rs:166-223`, `kimojio-stack-http/src/h2/connection.rs:226-251`).

### HTTP/1.1 Client and Server Patterns

- HTTP/1.1 client connections store a `StackTransport`, `HttpConfig`, and read buffer; `send` writes one request, reads one response, and shuts down writes when the response indicates connection close (`kimojio-stack-http/src/http1/client.rs:11-44`).
- HTTP/1.1 server connections store a `StackTransport`, `HttpConfig`, and read buffer; `serve_one` reads one request, calls a handler, writes one response, and shuts down writes when the request path requires close-after-response (`kimojio-stack-http/src/http1/server.rs:11-65`).

### Stack TLS and HTTP TLS Patterns

- `kimojio-stack-http` enables its `tls` feature by default and maps that feature to the optional `kimojio-stack-tls` dependency (`kimojio-stack-http/Cargo.toml:11-23`).
- `kimojio-stack-http::tls::HttpProtocol` maps `Http1` to raw ALPN `http/1.1` and `Http2` to raw ALPN `h2` (`kimojio-stack-http/src/tls.rs:10-27`).
- `tls::selected_protocol` inspects `TlsStream::ssl().selected_alpn_protocol`, and `tls::validate_protocol` returns a protocol error when the negotiated protocol does not match the expected `HttpProtocol` (`kimojio-stack-http/src/tls.rs:34-50`).
- `tls::client_transport` performs a client TLS handshake through `TlsContext::client`, validates ALPN when an expected protocol is supplied, and returns `StackTransport::tls`; `tls::server_transport` performs the analogous server handshake (`kimojio-stack-http/src/tls.rs:52-85`).
- `kimojio-stack-tls` crate docs describe the memory-BIO adaptation: stackful reads fill OpenSSL push buffers, stackful writes drain OpenSSL pull buffers, and encrypted socket bytes move without an intermediate copy between socket and OpenSSL buffers (`kimojio-stack-tls/src/lib.rs:4-12`).
- `TlsContext::from_openssl` transfers an `openssl::ssl::SslContext` into a `kimojio_tls::TlsServerContext`; `TlsContext::server` and `TlsContext::client` create `TlsStream` values and perform the corresponding handshake (`kimojio-stack-tls/src/lib.rs:21-69`).
- `TlsStream` exposes immutable/mutable `SslRef` access for inspection and pre-handshake configuration, loops on WANT_READ/WANT_WRITE during client/server handshakes, reads decrypted bytes, writes plaintext, sends close notify, shuts down the write half, and closes the socket through `RuntimeContext` (`kimojio-stack-tls/src/lib.rs:72-206`).
- `TlsStream::fill_tls_read` obtains the OpenSSL push buffer, calls `cx.read` on the socket, advances the push buffer, and treats zero bytes as `Errno::PIPE`; `flush_tls_write` obtains pull buffers, calls `cx.write`, advances the pull buffer, and treats zero bytes as `Errno::PIPE` (`kimojio-stack-tls/src/lib.rs:208-235`).
- Lower-level `kimojio-tls` declares FFI functions for TLS handle creation, push/pull buffer access, SSL read/write, handshakes, shutdown, and OpenSSL error inspection (`kimojio-tls/src/lib.rs:144-184`).
- `kimojio-tls::Response` represents TLS operation outcomes as `Success`, `Fail`, `Eof`, `WantRead`, and `WantWrite`; `get_response` maps OpenSSL WANT_READ/WANT_WRITE/EOF-style cases to those variants or `Errno`/TLS error values (`kimojio-tls/src/lib.rs:204-250`).

### kimojio-stack-grpc Public API and Data Flow

- The gRPC crate exposes modules for `client`, `codec`, `error`, `metadata`, `server`, and `status`, and re-exports its primary public types and frame helpers from `lib.rs` (`kimojio-stack-grpc/src/lib.rs:4-18`).
- `ClientConfig` and `ServerConfig` both default `max_message_len` to `4 * 1024 * 1024` (`kimojio-stack-grpc/src/client.rs:14-26`, `kimojio-stack-grpc/src/server.rs:17-29`).
- `UnaryClient` owns an `h2::ClientConnection` and `ClientConfig`; `call` encodes the request message, builds an HTTP/2 request, sends it, reads response trailers, and parses the response (`kimojio-stack-grpc/src/client.rs:36-62`).
- The client request builder always uses POST, the caller-supplied path, `content-type: application/grpc`, `te: trailers`, and caller metadata converted into HTTP headers (`kimojio-stack-grpc/src/client.rs:69-89`).
- Client response parsing requires HTTP 200, validates `application/grpc` or `application/grpc+...`, reads gRPC status from trailers or headers, returns `Error::Status` for non-OK gRPC status, converts response headers/trailers to `Metadata`, and decodes the response frame (`kimojio-stack-grpc/src/client.rs:91-143`).
- `UnaryServer` stores path-keyed boxed `UnaryHandler` values in a `BTreeMap`, registers typed handlers with `add_unary`, and handles one incoming HTTP/2 request per `serve_one` call (`kimojio-stack-grpc/src/server.rs:31-76`).
- Server dispatch validates the request, finds a handler by URI path, converts HTTP headers to metadata, and passes the request body bytes to the handler (`kimojio-stack-grpc/src/server.rs:78-91`).
- Typed server handlers decode the request frame with `codec::decode_message`, map size-limit decode failures to `ResourceExhausted` status and other decode failures to `InvalidArgument`, call the user handler, prost-encode the response, wrap it with gRPC frame bytes, and return raw reply metadata/message/trailers (`kimojio-stack-grpc/src/server.rs:131-165`).
- Server request validation requires POST, a gRPC content type, and either absent `TE` or `TE: trailers` (`kimojio-stack-grpc/src/server.rs:167-196`).
- Successful server replies write HTTP 200 `application/grpc`, response metadata, a bounded `Body` containing the gRPC-framed message, `grpc-status: 0` trailers plus caller trailers, and `send_response_with_trailers` over HTTP/2 (`kimojio-stack-grpc/src/server.rs:198-225`).
- Error status replies write HTTP 200 `application/grpc` with an empty body and status trailers (`kimojio-stack-grpc/src/server.rs:227-240`).
- `codec::encode_message` prefixes prost-encoded messages with a 5-byte gRPC frame header containing an uncompressed flag and u32 message length; it enforces caller and u32 size limits before encoding (`kimojio-stack-grpc/src/codec.rs:9-35`).
- `codec::decode_message` requires the 5-byte header, rejects nonzero compression flags, enforces caller size limits, checks exact payload length, and decodes via `prost::Message::decode` (`kimojio-stack-grpc/src/codec.rs:38-65`).
- Byte-oriented `encode_bytes`/`decode_bytes` provide the same gRPC frame shape for pre-encoded payloads (`kimojio-stack-grpc/src/codec.rs:68-109`).
- `Metadata` wraps `http::HeaderMap`, validates reserved metadata names, requires `insert_bin`/`get_bin` for `-bin` metadata, base64-encodes binary metadata, filters transport-reserved names when constructed from HTTP headers, and supports padded or no-pad base64 decode (`kimojio-stack-grpc/src/metadata.rs:12-117`).
- `StatusCode` enumerates gRPC status codes 0 through 16, and `Status` stores code, message, binary details, and metadata (`kimojio-stack-grpc/src/status.rs:16-80`).
- `Status::to_trailers` serializes status metadata, `grpc-status`, percent-encoded `grpc-message`, and base64 `grpc-status-details-bin`; `Status::from_trailers` parses those fields back into a `Status` (`kimojio-stack-grpc/src/status.rs:137-204`, `kimojio-stack-grpc/src/status.rs:218-270`).
- gRPC errors are inspectable by stable `ErrorKind` categories and include transport, protocol, prost encode/decode, size limit, gRPC status, and unsupported compression variants (`kimojio-stack-grpc/src/error.rs:8-85`).

### Prost, Code Generation, and OpenTelemetry Protocol Dependency Context

- The workspace dependency table contains `prost = "0.14"`, `tonic = "0.14"`, and `tonic-prost = "0.14"` entries, while the displayed workspace dependency table does not include a `prost-build` entry (`Cargo.toml:25-84`).
- `kimojio-stack-grpc` normal dependencies include `prost.workspace = true` and `kimojio-stack-http` with `default-features = false`; its dev-dependencies include TLS-enabled `kimojio-stack-http`, `tokio`, `tonic` with `transport` and `tls-ring`, and `tonic-prost` (`kimojio-stack-grpc/Cargo.toml:11-30`).
- gRPC message types in unit/integration tests and benchmarks are handwritten Rust types deriving `prost::Message` with `#[prost(...)]` field attributes rather than generated modules in the inspected files (`kimojio-stack-grpc/src/lib.rs:146-150`, `kimojio-stack-grpc/tests/interop_proto.rs:19-29`, `kimojio-stack-grpc/tests/protocol_limits.rs:14-18`, `kimojio-stack-grpc/benches/unary_rpc.rs:37-43`).
- The observed build script in the repository compiles `kimojio-tls/src/openssl_stream.c` with `cc` and emits OpenSSL link directives/rerun directives; those lines do not include protobuf/prost generation commands (`kimojio-tls/build.rs:3-13`).
- `kimojio-stack-grpc::codec` operates on the `prost::Message` trait directly, so generated or handwritten prost-compatible message types enter the current gRPC path through the same trait bounds (`kimojio-stack-grpc/src/codec.rs:4-15`, `kimojio-stack-grpc/src/codec.rs:38-42`).
- The current repository text search found OpenTelemetry mentions in `TODO.md` and the current PAW work context, while the root dependency table shown for workspace dependencies contains no OpenTelemetry crate names (`TODO.md:5-5`, `.paw/work/stack-opentelemetry/WorkflowContext.md:23-23`, `Cargo.toml:25-84`).
- External crate metadata observed with `cargo info opentelemetry-proto@0.32.0` on 2026-06-07 reports default feature `full`, feature groups including `gen-tonic`, `gen-tonic-messages`, `logs`, `metrics`, and dependencies including `opentelemetry@0.32`, `opentelemetry_sdk@0.32`, `prost@0.14`, `tonic@0.14.1`, and `tonic-prost@0.14.1` (external crates.io metadata; no repository file path).

### Integration Test Patterns: Tokio/Hyper/Tonic Interop

- HTTP test support defines `run_stackful` as `Runtime::new().block_on`, `spawn_tokio` as a dedicated OS thread running a one-worker multi-thread Tokio runtime, `stack_connect` as stackful socket/connect plus `StackTransport::plaintext`, and `stack_listener` as stackful socket/bind/listen plus `getsockname` (`kimojio-stack-http/tests/support/mod.rs:40-76`).
- HTTP support also defines socketpair transport helpers, TLS socketpair helpers, TLS contexts built from self-signed OpenSSL certificates, ALPN byte helpers for `h2` and `http/1.1`, and async helpers for reading/writing HTTP/2 frames and prefaces over Tokio `TcpStream` (`kimojio-stack-http/tests/support/mod.rs:78-141`, `kimojio-stack-http/tests/support/mod.rs:143-225`, `kimojio-stack-http/tests/support/mod.rs:242-262`).
- HTTP/1.1 interop tests run a stackful client against a Tokio TCP server that manually reads an HTTP message and writes a mixed-case-header response, and run a Tokio TCP client against a stackful `http1::ServerConnection` (`kimojio-stack-http/tests/http1_interop.rs:16-49`, `kimojio-stack-http/tests/http1_interop.rs:51-94`).
- HTTP/2 interop tests run a stackful `h2::ClientConnection` against a Hyper HTTP/2 server and run a Hyper HTTP/2 client against a stackful `h2::ServerConnection` (`kimojio-stack-http/tests/h2_interop.rs:23-65`, `kimojio-stack-http/tests/h2_interop.rs:67-120`).
- Additional HTTP/2 interop tests run a stackful client against a raw Tokio HTTP/2 server with custom settings and run a raw Tokio HTTP/2 client against a stackful server using local frame helpers (`kimojio-stack-http/tests/h2_interop.rs:122-179`, `kimojio-stack-http/tests/h2_interop.rs:181-239`).
- gRPC interop support defines prost message structs directly, a tonic `Service` for `interop.TestService`, success/error tonic unary behavior, and a helper `tonic_unary_call` using `tonic::client::Grpc` with `ProstCodec` (`kimojio-stack-grpc/tests/interop_proto.rs:16-75`, `kimojio-stack-grpc/tests/interop_proto.rs:83-145`).
- Stackful gRPC client tests spawn a tonic server in a thread, bind a Tokio `TcpListener` on localhost port 0, send the bound address over `mpsc`, serve with `Server::builder().add_service(...).serve_with_incoming_shutdown`, and use `oneshot` for shutdown (`kimojio-stack-grpc/tests/client_interop.rs:17-67`, `kimojio-stack-grpc/tests/client_interop.rs:127-154`).
- The stackful gRPC client connects with stackful socket/connect, wraps the socket in `StackTransport::plaintext`, builds `h2::ClientConnection`, constructs `UnaryClient`, inserts ASCII and binary metadata, calls a tonic server, closes the client, and asserts response metadata/status/message (`kimojio-stack-grpc/tests/client_interop.rs:23-49`, `kimojio-stack-grpc/tests/client_interop.rs:53-67`).
- Stackful gRPC server tests spawn a stackful server thread, create a listener with stackful socket/bind/listen, send the socket address by `mpsc`, accept one connection, create `StackTransport::plaintext`, `h2::ServerConnection`, and `UnaryServer`, register a unary handler, serve one request, send GOAWAY, close after peer shutdown, and close the listener (`kimojio-stack-grpc/tests/server_interop.rs:78-170`).
- Tonic client tests connect to the stackful gRPC server with `Endpoint::from_shared(format!("http://{addr}"))`, build a tonic `Request` with ASCII and binary metadata, and call the `tonic_unary_call` helper (`kimojio-stack-grpc/tests/server_interop.rs:174-205`).

### Integration Test Patterns: TLS Receiver/Server Setup

- HTTP TLS tests create self-signed server/client OpenSSL contexts with ALPN, use a UNIX socketpair, run client and server in a stackful scope, wrap each side with `tls::server_transport`/`tls::client_transport`, and assert HTTP/1.1 request/response exchange over TLS (`kimojio-stack-http/tests/support/mod.rs:107-132`, `kimojio-stack-http/tests/tls_interop.rs:13-73`).
- HTTP ALPN tests call `TlsContext::server` and `TlsContext::client` directly, inspect `tls::selected_protocol`, close both streams, and assert both sides negotiated `HttpProtocol::Http2` (`kimojio-stack-http/tests/alpn.rs:9-41`).
- HTTP TLS error tests cover plaintext peer handshake failure, certificate verification failure, client handshake EOF, and close-notify EOF mapping, each running client/server peers inside a stackful `scope` over socketpairs (`kimojio-stack-http/tests/tls_errors.rs:9-149`).
- gRPC TLS interop creates self-signed cert/key PEM, a client `TlsContext` with peer verification disabled and ALPN `h2`, and a tonic TLS server using `Identity::from_pem` plus `ServerTlsConfig::new().identity(identity)` (`kimojio-stack-grpc/tests/tls_interop.rs:88-107`, `kimojio-stack-grpc/tests/tls_interop.rs:110-140`).
- The stackful gRPC TLS client creates a stackful TCP socket, connects to the tonic TLS server address, calls `tls::client_transport` with `Some(HttpProtocol::Http2)`, builds `h2::ClientConnection` and `UnaryClient`, inserts metadata, performs one unary call, and closes the client (`kimojio-stack-grpc/tests/tls_interop.rs:31-86`).
- The gRPC TLS test's tonic server binds a Tokio listener on localhost port 0, sends the address over `mpsc`, configures tonic TLS, adds `TonicTestServer`, serves with incoming shutdown, and is stopped by a `oneshot` signal (`kimojio-stack-grpc/tests/tls_interop.rs:110-140`).

### Protocol Limit and Error Test Patterns

- HTTP protocol-limit tests use socketpair transports, stackful scopes, manual frame/request bytes, and configured `HttpConfig` limits to assert oversized headers, bodies, frames, header blocks, max-concurrent-streams, peer close, stalled window updates, and repeated window replenishment behavior (`kimojio-stack-http/tests/protocol_limits.rs:11-50`, `kimojio-stack-http/tests/protocol_limits.rs:52-88`, `kimojio-stack-http/tests/protocol_limits.rs:90-156`, `kimojio-stack-http/tests/protocol_limits.rs:158-306`, `kimojio-stack-http/tests/protocol_limits.rs:308-437`).
- gRPC protocol-limit tests use socketpair transports, `Runtime::new().block_on`, h2 client/server connections, raw gRPC frames, and `UnaryClient`/`UnaryServer` paths to assert unsupported compression, malformed lengths, size limits, missing trailers, non-POST requests, invalid content type, metadata validation, and invalid status details (`kimojio-stack-grpc/tests/protocol_limits.rs:20-46`, `kimojio-stack-grpc/tests/protocol_limits.rs:48-99`, `kimojio-stack-grpc/tests/protocol_limits.rs:101-187`, `kimojio-stack-grpc/tests/protocol_limits.rs:189-225`, `kimojio-stack-grpc/tests/protocol_limits.rs:227-239`).
- gRPC unit tests include a local unary call over socketpair transports that asserts metadata, binary metadata, response metadata, trailers, success status, handler error status propagation, invalid content-type rejection, missing-trailer rejection, message-size enforcement, and allocation-count budgets (`kimojio-stack-grpc/src/lib.rs:186-278`, `kimojio-stack-grpc/src/lib.rs:280-416`, `kimojio-stack-grpc/src/lib.rs:418-485`).

### Benchmark and Allocation Observation Patterns

- `kimojio-stack-http` benchmarks run plaintext/TLS HTTP/1.1 and HTTP/2 request/response loops inside stackful scopes over socketpairs, with ALPN-specific TLS setup for HTTP/1.1 and h2 (`kimojio-stack-http/benches/http_request_response.rs:32-91`, `kimojio-stack-http/benches/http_request_response.rs:103-215`, `kimojio-stack-http/benches/http_request_response.rs:261-315`).
- `kimojio-stack-grpc` benchmarks define a prost `BenchMessage`, run plaintext and TLS unary gRPC loops over `h2::ClientConnection`/`h2::ServerConnection`, register one `UnaryServer` handler, call `UnaryClient::call`, and configure Criterion sample/warmup/measurement settings (`kimojio-stack-grpc/benches/unary_rpc.rs:32-59`, `kimojio-stack-grpc/benches/unary_rpc.rs:61-98`, `kimojio-stack-grpc/benches/unary_rpc.rs:100-169`, `kimojio-stack-grpc/benches/unary_rpc.rs:236-244`).
- Both HTTP and gRPC crates install test-only global counting allocators behind `#[cfg(test)]` and expose `allocation_tracking::measure` within tests to count allocations/reallocations during measured hot paths (`kimojio-stack-http/src/lib.rs:27-131`, `kimojio-stack-grpc/src/lib.rs:20-124`).

## Code References

- `.paw/work/stack-opentelemetry/Spec.md:6-20` - Feature overview/objectives for low-level OpenTelemetry-compatible logs and metrics export.
- `.paw/work/stack-opentelemetry/Spec.md:57-88` - Runtime/transport interoperability and functional requirements including secure/insecure transport and receiver-backed tests.
- `Cargo.toml:1-84` - Workspace members, resolver, shared package metadata, and centralized dependency versions.
- `kimojio-stack-http/Cargo.toml:11-31` - HTTP TLS feature/dependency setup and HTTP interop dev-dependencies.
- `kimojio-stack-grpc/Cargo.toml:11-30` - gRPC normal dependencies and dev-dependencies for tonic/tokio/TLS interop.
- `kimojio-stack/src/lib.rs:4-17` - Stackful runtime model and `RuntimeContext` role.
- `kimojio-stack/src/lib.rs:1151-1229` - Stackful socket/connect/read APIs used by transports.
- `kimojio-stack/src/lib.rs:1389-1403` - Stackful write API used by transports.
- `kimojio-stack/src/lib.rs:1607-1624` - Stackful close/shutdown APIs used by transports.
- `kimojio-stack-http/src/transport.rs:11-94` - Plaintext/TLS transport wrapper over stackful sockets and TLS streams.
- `kimojio-stack-http/src/tls.rs:10-85` - HTTP ALPN protocol enum and TLS client/server transport helpers.
- `kimojio-stack-http/src/h2/client.rs:38-160` - HTTP/2 client request/response lifecycle.
- `kimojio-stack-http/src/h2/server.rs:37-149` - HTTP/2 server accept/response/trailer lifecycle.
- `kimojio-stack-http/src/h2/codec.rs:18-198` - HTTP/2 settings, frame I/O, preface, and header-block handling.
- `kimojio-stack-grpc/src/client.rs:36-143` - Unary gRPC client call, request construction, and response parsing.
- `kimojio-stack-grpc/src/server.rs:31-240` - Unary gRPC server handler registration, dispatch, validation, reply/status writing.
- `kimojio-stack-grpc/src/codec.rs:9-109` - gRPC message frame encode/decode helpers over prost-compatible messages and bytes.
- `kimojio-stack-grpc/src/metadata.rs:12-117` - gRPC metadata wrapper, validation, and binary metadata encoding.
- `kimojio-stack-grpc/src/status.rs:16-270` - gRPC status codes and trailer serialization/parsing.
- `kimojio-stack-grpc/tests/interop_proto.rs:16-145` - Handwritten prost messages and tonic service/client helpers for gRPC interop tests.
- `kimojio-stack-grpc/tests/client_interop.rs:17-169` - Stackful gRPC client to tonic server plaintext interop pattern.
- `kimojio-stack-grpc/tests/server_interop.rs:15-205` - Tonic client to stackful gRPC server plaintext interop pattern.
- `kimojio-stack-grpc/tests/tls_interop.rs:31-140` - Stackful gRPC TLS client to tonic TLS server pattern.
- `kimojio-stack-http/tests/support/mod.rs:40-225` - Shared stackful/Tokio/TLS/HTTP2 frame helpers for interop tests.
- `kimojio-stack-http/tests/h2_interop.rs:23-239` - HTTP/2 stackful/hyper/tokio interop patterns.
- `kimojio-stack-http/tests/tls_interop.rs:13-73` - Stackful HTTP/1.1 TLS client/server in-process pattern.
- `.github/workflows/rust.yml:34-50` - CI build, test, docs, fmt, and feature lint commands.

## Architecture Documentation

- The stackful networking architecture routes user code through `Runtime::block_on` and `RuntimeContext` into explicit socket/connect/read/write/shutdown/close calls, with HTTP `StackTransport` wrapping either plaintext `OwnedFd` or TLS `TlsStream` (`kimojio-stack/src/lib.rs:506-530`, `kimojio-stack/src/lib.rs:1151-1229`, `kimojio-stack/src/lib.rs:1389-1403`, `kimojio-stack-http/src/transport.rs:11-94`).
- HTTP/2 is the transport substrate for current gRPC support: `UnaryClient` consumes an `h2::ClientConnection`, `UnaryServer::serve_one` consumes a mutable `h2::ServerConnection`, and gRPC replies use HTTP/2 response trailers for `grpc-status` and metadata (`kimojio-stack-grpc/src/client.rs:36-62`, `kimojio-stack-grpc/src/server.rs:62-76`, `kimojio-stack-grpc/src/server.rs:198-240`).
- Prost compatibility is trait-based in the current gRPC layer: messages enter `encode_message` and `decode_message` through `prost::Message` bounds, and tests/benchmarks declare prost message shapes inline with `#[derive(Message)]` (`kimojio-stack-grpc/src/codec.rs:12-15`, `kimojio-stack-grpc/src/codec.rs:38-42`, `kimojio-stack-grpc/tests/interop_proto.rs:19-29`, `kimojio-stack-grpc/benches/unary_rpc.rs:37-43`).
- The repository's interop pattern keeps async ecosystem crates as dev-dependencies for stackful HTTP/gRPC crates, while public stackful crates expose explicit stackful transport APIs (`kimojio-stack-http/Cargo.toml:25-31`, `kimojio-stack-grpc/Cargo.toml:19-30`, `docs/stack-http-grpc.md:9-10`, `docs/stack-http-grpc.md:113-124`).
- Documentation for stackful HTTP/gRPC already records supported transport setup, unary gRPC flow, limits/backpressure, interoperability coverage, benchmark commands, and future streaming notes (`docs/stack-http-grpc.md:19-33`, `docs/stack-http-grpc.md:60-95`, `docs/stack-http-grpc.md:97-124`, `docs/stack-http-grpc.md:126-156`).

## Open Questions

None.
