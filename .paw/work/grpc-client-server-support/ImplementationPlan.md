# gRPC Client Server Support Implementation Plan

## Overview

This plan adds two stackful-runtime crates:

- `kimojio-stack-http`: core HTTP/1.1 and HTTP/2 client/server request-response support over plaintext and TLS transports.
- `kimojio-stack-grpc`: unary-first gRPC client/server support layered on the HTTP/2 transport with prost-compatible payloads.

The architecture keeps public crates independent of tokio and other async runtimes while reusing runtime-agnostic protocol/type crates where they fit the low-overhead model. The HTTP crate owns stackful connection driving and protocol state; the gRPC crate owns gRPC framing, status/metadata mapping, and low-level unary method dispatch. Tokio-based crates are limited to dev-dependencies for interoperability tests, matching the spec's test-peer allowance (`.paw/work/grpc-client-server-support/Spec.md:152-159`).

## Current State Analysis

- The workspace currently contains `kimojio`, `kimojio-macros`, `kimojio-stack`, `kimojio-stack-tls`, `kimojio-tls`, `perf`, and `examples`, with shared dependency versions centralized in the root manifest (`Cargo.toml:1-13`, `Cargo.toml:23-58`, `.paw/work/grpc-client-server-support/CodeResearch.md:48-55`).
- `kimojio-stack` provides the required fd-based stackful runtime primitives: scoped coroutines, socket creation, accept/connect, read/write, vectored I/O, send/recv, shutdown, close, async owned-buffer I/O, wait primitives, and fixed-resource APIs (`.paw/work/grpc-client-server-support/CodeResearch.md:57-92`).
- `kimojio-stack-tls` provides a concrete TLS stream over `OwnedFd` using OpenSSL memory BIOs; it exposes `TlsContext`, `TlsStream`, `read`, `read_exact_or_eof`, `write`, `shutdown`, `shutdown_write`, `close`, and `ssl()` for OpenSSL inspection (`.paw/work/grpc-client-server-support/CodeResearch.md:111-129`).
- Existing stack tests are in-crate `#[test]` modules that run `Runtime::new().block_on(...)`, use scoped client/server tasks, and exercise socketpair/TCP patterns (`.paw/work/grpc-client-server-support/CodeResearch.md:131-138`).
- Existing measurement patterns include Criterion `iter_custom`, a `kimojio-stack` runtime baseline benchmark suite, and test-only allocation counting for warmed stackful hot paths (`.paw/work/grpc-client-server-support/CodeResearch.md:140-150`).
- Runtime-agnostic candidate crates documented during research include `http`, `bytes`, `httparse`, `httpdate`, `prost`, and `hpack`; tokio-bound HTTP stacks such as `h2`, `hyper`, and `tonic` are documented as inappropriate for public runtime dependencies but usable as test peers (`.paw/work/grpc-client-server-support/CodeResearch.md:152-178`).
- Current manifests do not include HTTP/gRPC/protobuf or tokio peer-test dependencies, and no existing HTTP transport abstraction exists over the concrete stackful fd/TLS APIs (`.paw/work/grpc-client-server-support/CodeResearch.md:220-226`).
- Verification conventions are `cargo fmt`, `cargo clippy`, `cargo clippy --all-targets --all-features`, and cargo tests, with CI running clippy with `-D warnings`, all-target feature tests, and doctests (`.paw/work/grpc-client-server-support/CodeResearch.md:36-42`).

## Desired End State

- `kimojio-stack-http` is a workspace crate with low-level HTTP/1.1 and HTTP/2 client/server APIs over stackful transports.
- `kimojio-stack-http` supports bounded request/response bodies, explicit limits, typed errors, plaintext transport, TLS transport, HTTP/2 flow control, metadata/trailer handling, and interoperability tests against tokio-based HTTP peers.
- `kimojio-stack-grpc` is a workspace crate with low-level unary client and server APIs using prost-compatible messages over `kimojio-stack-http` HTTP/2.
- `kimojio-stack-grpc` exposes unary status, metadata, response metadata, trailers, configurable message limits, and interoperability tests against tonic-based unary peers.
- Public crates do not depend on tokio, hyper, tonic, or other async runtimes in normal dependencies.
- Benchmarks and allocation-focused tests document representative HTTP and unary gRPC latency/allocation behavior after warmup.
- Documentation captures the final API, supported protocol subset, transport model, interoperability matrix, and future streaming extension points.

## What We're NOT Doing

- Not implementing client streaming, server streaming, or bidirectional gRPC in this workflow.
- Not generating tonic-like client/server stubs in this workflow.
- Not using tokio-bound `h2`, `hyper`, or `tonic` as public dependencies of the new stackful crates.
- Not implementing HTTP/3, WebSocket upgrades, proxy tunneling, HTTP/2 server push, automatic retries, service discovery, load balancing, or broad content-compression negotiation.
- Not implementing CONNECT tunneling, browser-focused HTTP features, automatic name resolution beyond explicit target configuration, or a full replacement for mature general-purpose HTTP clients/servers.
- Not replacing `kimojio-stack-tls`; HTTP and gRPC will integrate with the existing TLS stream instead.
- Not optimizing with zero-copy networking or provided-buffer io_uring paths unless surfaced as a later phase candidate.

## Dependency and Architecture Decisions

1. **Shared HTTP/protobuf types**: Use runtime-agnostic crates for public data types and parsing helpers: `http` for request/response/header/status types, `bytes` for ref-counted/shared byte buffers, `httparse` and `httpdate` for HTTP/1.1 parsing/date handling, and `prost` for protobuf message encoding/decoding. Research documents these crates as runtime-agnostic candidates and documents current absence from the workspace (`.paw/work/grpc-client-server-support/CodeResearch.md:162-178`). Phase 1 dependency-tree checks verify the no-public-async-runtime constraint; Phase 8 allocation and latency measurements verify the overhead side of the spec's dependency assumption.
2. **HTTP/2 implementation**: Do not use the `h2` crate as a public dependency because research documents normal `tokio`/`tokio-util` dependencies for the current `h2` crate (`.paw/work/grpc-client-server-support/CodeResearch.md:176-178`). Implement stackful HTTP/2 connection state locally, using `hpack` only behind an internal adapter so missing encoder/decoder capabilities can be supplied locally without changing public APIs. Treat HPACK Huffman decoding as an explicit interoperability checkpoint because peer-generated headers may use Huffman-encoded literals.
3. **Transport model**: Introduce an internal transport boundary in `kimojio-stack-http` that can drive either plaintext `OwnedFd` I/O through `RuntimeContext` or TLS through `kimojio-stack-tls::TlsStream`; existing concrete runtime and TLS APIs are fd/TLS-stream based (`.paw/work/grpc-client-server-support/CodeResearch.md:72-92`, `.paw/work/grpc-client-server-support/CodeResearch.md:111-121`).
4. **HTTP/2 plaintext interop**: Use HTTP/2 prior-knowledge configuration for plaintext HTTP/2 tests. Do not rely on HTTP/1.1 upgrade because upgrade behavior is outside the initial scope.
5. **Interop tests**: Add tokio/hyper/tonic only as dev-dependencies or test-only dependencies, using separate OS threads/runtimes for tokio peers and kimojio-stack peers. Current tests already use stackful runtime patterns, and the spec explicitly allows tokio-based peer crates as test dependencies (`.paw/work/grpc-client-server-support/CodeResearch.md:131-138`, `.paw/work/grpc-client-server-support/Spec.md:152-159`).
6. **Review strategy**: Use local jj changes only and do not push, per the workflow context (`.paw/work/grpc-client-server-support/WorkflowContext.md:8-23`).

## Phase Status

- [x] **Phase 1: Workspace, dependencies, and transport foundation** - Add crate scaffolding, selected runtime-agnostic dependencies, shared errors/config/body types, and plaintext/TLS transport boundaries.
- [ ] **Phase 2: HTTP/1.1 client and server** - Implement bounded HTTP/1.1 request/response parsing, serialization, connection handling, and stackful tests.
- [ ] **Phase 3: HTTP/2 frame, HPACK, and state core** - Implement HTTP/2 framing, header compression adapter, settings, stream state, and flow-control primitives.
- [ ] **Phase 4: HTTP/2 client and server request/response** - Build stackful HTTP/2 client/server connection APIs over the Phase 3 core.
- [ ] **Phase 5: HTTP TLS and tokio HTTP interoperability** - Validate stackful HTTP/1.1 and HTTP/2 clients/servers against tokio-based peers, including TLS coverage.
- [ ] **Phase 6: Unary gRPC core and local stackful tests** - Add gRPC framing, status/metadata/trailer handling, prost-compatible unary client/server APIs, and local stackful tests.
- [ ] **Phase 7: Tokio gRPC interoperability** - Validate stackful unary gRPC clients/servers against tonic-based peers.
- [ ] **Phase 8: Performance, allocation, and protocol-limit coverage** - Add benchmarks, allocation tests, and focused malformed/limit/backpressure tests.
- [ ] **Phase 9: Documentation** - Add as-built Docs.md and project documentation for public HTTP/gRPC APIs and verification.

## Phase Candidates

<!-- No phase candidates identified during planning. -->

---

## Phase 1: Workspace, dependencies, and transport foundation

### Changes Required:

- **`Cargo.toml`**: Add `kimojio-stack-http` and `kimojio-stack-grpc` workspace members. Add workspace dependency entries for selected public dependencies (`bytes`, `http`, `httparse`, `httpdate`, `hpack`, `prost`) and test-only peer dependencies (`tokio`, `hyper`, `hyper-util`, `http-body-util`, `tonic`) using dev-dependencies in the new crates. Follow existing workspace dependency conventions (`Cargo.toml:1-13`, `Cargo.toml:23-58`, `.paw/work/grpc-client-server-support/CodeResearch.md:46-55`).
- **`kimojio-stack-http/Cargo.toml`**: Add workspace package metadata, normal dependencies on `kimojio-stack`, `kimojio-stack-tls`, and runtime-agnostic HTTP/buffer/protocol crates. Add dev-dependencies for Criterion and tokio peer tests.
- **`kimojio-stack-http/src/lib.rs`**: Establish public module layout for configuration, errors, body buffers, headers/trailers, transport, HTTP/1.1, HTTP/2, client, and server entry points.
- **`kimojio-stack-http/src/error.rs`**: Define inspectable HTTP error categories for I/O, TLS, parse, protocol, unsupported feature, size limit, EOF, and peer reset. Map existing `Errno` and TLS protocol errors through explicit categories, consistent with current stack/TLS error surfaces (`.paw/work/grpc-client-server-support/CodeResearch.md:94-100`).
- **`kimojio-stack-http/src/body.rs`**: Define bounded body representations around `bytes` buffers and explicit configured limits. Keep the API low-level enough for callers to control allocation and copying.
- **`kimojio-stack-http/src/transport.rs`**: Define the internal plaintext/TLS transport boundary over `OwnedFd` and `TlsStream`, with read/write/read-exact/write-all/shutdown/close behavior that uses `RuntimeContext`.
- **`kimojio-stack-grpc/Cargo.toml`**: Add workspace package metadata and normal dependencies on `kimojio-stack-http`, `bytes`, `http`, and `prost`.
- **`kimojio-stack-grpc/src/lib.rs`**: Establish public module layout for errors, status, metadata, codec, client, and server.
- **Tests**: Add crate smoke tests proving the new crates compile, public normal dependencies do not include tokio/hyper/tonic, plaintext transport can move bytes over a socketpair, and TLS transport can move bytes using existing `kimojio-stack-tls` test context patterns (`.paw/work/grpc-client-server-support/CodeResearch.md:123-127`, `.paw/work/grpc-client-server-support/CodeResearch.md:131-138`).
- **Dependency verification**: Add a test or documented cargo-tree check that normal dependencies of the new public crates do not include tokio, hyper, tonic, h2, async-std, smol, or another async runtime dependency.

### Success Criteria:

#### Automated Verification:
- [ ] `cargo fmt --check`
- [ ] `cargo test -p kimojio-stack-http`
- [ ] `cargo test -p kimojio-stack-grpc`
- [ ] `cargo clippy -p kimojio-stack-http --all-targets --all-features -- -D warnings`
- [ ] `cargo clippy -p kimojio-stack-grpc --all-targets --all-features -- -D warnings`
- [ ] `cargo tree -p kimojio-stack-http -e normal` shows no tokio/hyper/tonic normal dependency.
- [ ] `cargo tree -p kimojio-stack-grpc -e normal` shows no tokio/hyper/tonic normal dependency.
- [ ] Normal dependency-tree review confirms no public async-runtime dependency, including tokio, hyper, tonic, h2, async-std, and smol.

#### Manual Verification:
- [ ] Public module boundaries match the spec's low-level/non-generated API direction (`.paw/work/grpc-client-server-support/Spec.md:152-159`).
- [ ] Transport foundation covers both plaintext and existing TLS stream paths without introducing a new runtime model.

---

## Phase 2: HTTP/1.1 client and server

### Changes Required:

- **`kimojio-stack-http/src/http1/codec.rs`**: Implement HTTP/1.1 request and response parse/serialize components using `httparse` and `httpdate`, with explicit header-count, header-byte, body-byte, and line-length limits.
- **`kimojio-stack-http/src/http1/body.rs`**: Implement bounded body handling for `Content-Length`, chunked transfer encoding, and EOF-delimited bodies where valid. Keep request/response body accumulation bounded by config limits.
- **`kimojio-stack-http/src/http1/client.rs`**: Add low-level client connection API for sending one request and receiving one response, plus sequential keep-alive reuse. Do not implement HTTP/1.1 pipelining.
- **`kimojio-stack-http/src/http1/server.rs`**: Add low-level server connection API that reads a request, invokes a stackful handler, and writes a response. Support sequential keep-alive requests on one connection.
- **`kimojio-stack-http/src/client.rs` / `src/server.rs`**: Re-export protocol-neutral client/server entry points that can dispatch to HTTP/1.1.
- **Tests**: Add socketpair tests with stackful client/server tasks for fixed-length bodies, chunked bodies, keep-alive sequential requests, EOF behavior, malformed headers, invalid `Content-Length`, configured size limits, and explicit error mapping. Follow current `Runtime::new().block_on` and scoped task test style (`.paw/work/grpc-client-server-support/CodeResearch.md:131-138`).

### Success Criteria:

#### Automated Verification:
- [ ] `cargo fmt --check`
- [ ] `cargo test -p kimojio-stack-http http1`
- [ ] `cargo clippy -p kimojio-stack-http --all-targets --all-features -- -D warnings`

#### Manual Verification:
- [ ] HTTP/1.1 APIs expose explicit request/response/body limits and do not hide background tasks.
- [ ] HTTP/1.1 unsupported features, such as pipelining and upgrades, fail explicitly rather than being accepted silently.

---

## Phase 3: HTTP/2 frame, HPACK, and state core

### Changes Required:

- **`kimojio-stack-http/src/h2/frame.rs`**: Implement HTTP/2 frame encoding/decoding for DATA, HEADERS, CONTINUATION, SETTINGS, WINDOW_UPDATE, RST_STREAM, GOAWAY, PING, and trailers represented as terminal HEADERS. Enforce frame-size and connection-preface validation.
- **`kimojio-stack-http/src/h2/hpack.rs`**: Add an internal HPACK adapter around the selected runtime-agnostic `hpack` crate, with local behavior for any required missing encode/decode path. Keep this module private to prevent dependency leakage.
- **`kimojio-stack-http/src/h2/settings.rs`**: Model peer/local SETTINGS, acknowledgement, initial window sizes, max frame size, max concurrent streams, and header-list size.
- **`kimojio-stack-http/src/h2/stream.rs`**: Model stream IDs, states, end-stream/end-headers flags, trailers, per-stream inbound/outbound flow-control windows, and bounded body buffers.
- **`kimojio-stack-http/src/h2/connection.rs`**: Add connection-level state for preface, settings, stream map, connection flow-control window, pending outbound frames, and protocol error propagation.
- **Tests**: Add frame round-trip tests, HPACK header round-trip tests, HPACK Huffman-decode compatibility tests, SETTINGS ack tests, stream-state transition tests, flow-control window tests, frame-size/header-list/body-limit tests, and malformed-frame tests.

### Success Criteria:

#### Automated Verification:
- [ ] `cargo fmt --check`
- [ ] `cargo test -p kimojio-stack-http h2::`
- [ ] `cargo clippy -p kimojio-stack-http --all-targets --all-features -- -D warnings`

#### Manual Verification:
- [ ] HTTP/2 core has no dependency on tokio/hyper/h2 runtime types.
- [ ] Flow-control state is explicit and bounded before request/response APIs are layered on top.
- [ ] HPACK tests prove peer-generated Huffman-encoded headers can be decoded or identify a local decoder path before HTTP/2 interop phases depend on it.

---

## Phase 4: HTTP/2 client and server request/response

### Changes Required:

- **`kimojio-stack-http/src/h2/client.rs`**: Implement a stackful HTTP/2 client connection API that sends unary-style request/response exchanges, handles response headers/body/trailers, respects flow-control windows, and matches responses to stream IDs.
- **`kimojio-stack-http/src/h2/server.rs`**: Implement a stackful HTTP/2 server connection API that accepts inbound request streams, invokes low-level handlers, and sends response headers/body/trailers.
- **`kimojio-stack-http/src/h2/connection.rs`**: Extend the connection driver to coordinate inbound frames, outbound frames, stream completion, and bounded per-stream queues using stackful runtime synchronization primitives. Existing primitives include bounded channels, semaphores, notify, mutex, and wait APIs (`.paw/work/grpc-client-server-support/CodeResearch.md:102-109`).
- **`kimojio-stack-http/src/client.rs` / `src/server.rs`**: Add protocol-neutral dispatch for HTTP/2.
- **Tests**: Add stackful socketpair tests for client request/response, server request/response, trailers, concurrent streams, out-of-order stream completion, flow-control blocking/unblocking, malformed pseudo-headers, and stream reset/goaway behavior.

### Success Criteria:

#### Automated Verification:
- [ ] `cargo fmt --check`
- [ ] `cargo test -p kimojio-stack-http h2`
- [ ] `cargo clippy -p kimojio-stack-http --all-targets --all-features -- -D warnings`

#### Manual Verification:
- [ ] HTTP/2 request/response APIs preserve stream association under concurrent streams.
- [ ] Backpressure uses bounded queues/windows and stackful parking rather than unbounded buffering or spinning.

---

## Phase 5: HTTP TLS and tokio HTTP interoperability

### Changes Required:

- **`kimojio-stack-http/src/tls.rs`**: Add HTTP-level TLS helper types around `kimojio-stack-tls::TlsContext`/`TlsStream`, including protocol selection/validation hooks for HTTP/1.1 and HTTP/2 where OpenSSL ALPN is configured by callers or tests. Existing TLS exposes `TlsStream::ssl()` for OpenSSL inspection (`.paw/work/grpc-client-server-support/CodeResearch.md:115-121`).
- **`kimojio-stack-http/tests/alpn.rs`**: Add an early HTTP/2-over-TLS feasibility test proving ALPN negotiation works end-to-end through the existing `kimojio-stack-tls` path, or explicitly surfacing any required `kimojio-stack-tls` extension before gRPC-over-TLS interop depends on it. Code research identifies TLS ALPN as untested in current stack TLS coverage (`.paw/work/grpc-client-server-support/CodeResearch.md:222-224`).
- **`kimojio-stack-http/tests/http1_interop.rs`**: Add tests where the stackful HTTP/1.1 client talks to a tokio-based HTTP server and a tokio-based HTTP client talks to a stackful HTTP/1.1 server.
- **`kimojio-stack-http/tests/h2_interop.rs`**: Add tests where the stackful HTTP/2 client talks to a tokio-based HTTP/2 server and a tokio-based HTTP/2 client talks to a stackful HTTP/2 server.
- **`kimojio-stack-http/tests/tls_interop.rs`**: Add TLS coverage for at least one stackful HTTP client path and one stackful HTTP server path, satisfying SC-005 (`.paw/work/grpc-client-server-support/Spec.md:145-145`).
- **`kimojio-stack-http/tests/tls_errors.rs`**: Add negative TLS coverage for handshake failure, certificate verification failure, close-notify/EOF mapping, and resource cleanup/error-category assertions. This carries forward the spec edge case for TLS handshake, certificate verification, and close-notify behavior (`.paw/work/grpc-client-server-support/Spec.md:89-89`).
- **Test infrastructure**: Add shared test helpers for running tokio peers on separate OS threads with ephemeral localhost sockets and running stackful peers through `Runtime::new().block_on(...)`. This keeps tokio out of public crates while satisfying interoperability tests.

### Success Criteria:

#### Automated Verification:
- [ ] `cargo fmt --check`
- [ ] `cargo test -p kimojio-stack-http --test alpn`
- [ ] `cargo test -p kimojio-stack-http --test http1_interop`
- [ ] `cargo test -p kimojio-stack-http --test h2_interop`
- [ ] `cargo test -p kimojio-stack-http --test tls_interop`
- [ ] `cargo test -p kimojio-stack-http --test tls_errors`
- [ ] `cargo clippy -p kimojio-stack-http --all-targets --all-features -- -D warnings`
- [ ] `cargo tree -p kimojio-stack-http -e normal` shows tokio/hyper/tonic are absent from normal dependencies.

#### Manual Verification:
- [ ] Test matrix covers stackful-client-to-tokio-server and tokio-client-to-stackful-server for both HTTP versions.
- [ ] TLS/plaintext behavior is documented in test names and helper configuration.
- [ ] TLS failure tests assert inspectable errors for handshake failure, certificate verification failure, and close-notify/EOF behavior.
- [ ] HTTP/1.1 interop tests include legal header casing variations.
- [ ] HTTP/2 interop tests include legal SETTINGS variations such as initial window size, max frame size, header-list size, or concurrent-stream settings.

---

## Phase 6: Unary gRPC core and local stackful tests

### Changes Required:

- **`kimojio-stack-grpc/src/status.rs`**: Define gRPC status code/message/details representation and mapping to/from HTTP/2 trailers.
- **`kimojio-stack-grpc/src/metadata.rs`**: Define metadata map behavior over HTTP headers/trailers, including binary metadata encoding boundaries and validation.
- **`kimojio-stack-grpc/src/codec.rs`**: Implement unary gRPC message framing: compression flag, message length, configured message limits, prost-compatible encode/decode, and unsupported compression errors.
- **`kimojio-stack-grpc/src/client.rs`**: Add low-level unary client API that takes a method path, metadata, request message, and limit/config values, then returns response metadata, decoded response, status, and trailers.
- **`kimojio-stack-grpc/src/server.rs`**: Add low-level unary server registry/dispatcher that maps method paths to handlers, decodes one request message, invokes stackful handler logic, and writes one response or status error.
- **`kimojio-stack-grpc/src/error.rs`**: Define inspectable error categories for HTTP transport, gRPC protocol, prost encode/decode, size limit, status error, and unsupported compression.
- **Tests**: Add local stackful HTTP/2 socketpair tests for successful unary calls, handler error status, metadata propagation, trailers, invalid content type, missing trailers, invalid compressed flag, malformed message length, and configured message-size limits.

### Success Criteria:

#### Automated Verification:
- [ ] `cargo fmt --check`
- [ ] `cargo test -p kimojio-stack-grpc`
- [ ] `cargo clippy -p kimojio-stack-grpc --all-targets --all-features -- -D warnings`
- [ ] `cargo tree -p kimojio-stack-grpc -e normal` shows tokio/hyper/tonic are absent from normal dependencies.

#### Manual Verification:
- [ ] Public gRPC APIs can make unary calls and register unary handlers without generated stubs, satisfying SC-009 (`.paw/work/grpc-client-server-support/Spec.md:149-150`).
- [ ] API naming and modules make future streaming extension points visible without exposing streaming call shapes in this phase.

---

## Phase 7: Tokio gRPC interoperability

### Changes Required:

- **`kimojio-stack-grpc/tests/interop_proto.rs`**: Define simple prost-compatible test messages directly or through test-only generated code.
- **`kimojio-stack-grpc/tests/client_interop.rs`**: Add tests where the stackful unary gRPC client calls a tonic-based unary server for success, application error status, metadata, and trailers.
- **`kimojio-stack-grpc/tests/server_interop.rs`**: Add tests where a tonic-based unary client calls the stackful gRPC server for success, application error status, metadata, and trailers.
- **`kimojio-stack-grpc/tests/tls_interop.rs`**: Add gRPC-over-HTTP/2 TLS coverage using the selected HTTP/2 transport mode and test certificates, satisfying the gRPC clause of SC-005 (`.paw/work/grpc-client-server-support/Spec.md:145-145`).
- **Test infrastructure**: Reuse the HTTP interop thread model: tokio runtime and stackful runtime run independently, with explicit startup synchronization and clean shutdown.

### Success Criteria:

#### Automated Verification:
- [ ] `cargo fmt --check`
- [ ] `cargo test -p kimojio-stack-grpc --test client_interop`
- [ ] `cargo test -p kimojio-stack-grpc --test server_interop`
- [ ] `cargo test -p kimojio-stack-grpc --test tls_interop`
- [ ] `cargo clippy -p kimojio-stack-grpc --all-targets --all-features -- -D warnings`
- [ ] `cargo tree -p kimojio-stack-grpc -e normal` shows tokio/hyper/tonic are absent from normal dependencies.

#### Manual Verification:
- [ ] Interop tests cover both directions required by SC-003 and SC-004 (`.paw/work/grpc-client-server-support/Spec.md:143-144`).
- [ ] Test failures expose protocol/status details rather than opaque peer errors.
- [ ] Interop tests include gRPC metadata variation, including binary metadata where supported by the low-level API.

---

## Phase 8: Performance, allocation, and protocol-limit coverage

### Changes Required:

- **`kimojio-stack-http/benches/http_request_response.rs`**: Add Criterion benchmarks for HTTP/1.1 and HTTP/2 request/response loops with representative small headers, small body, and larger body cases, with separate plaintext and TLS observations whenever the corresponding TLS path is implemented. Follow existing Criterion `iter_custom` style (`.paw/work/grpc-client-server-support/CodeResearch.md:146-150`).
- **`kimojio-stack-grpc/benches/unary_rpc.rs`**: Add Criterion benchmarks for unary gRPC request/response loops with small and moderate prost payloads, with separate plaintext and TLS observations whenever the corresponding TLS path is implemented.
- **`kimojio-stack-http/src/lib.rs` test modules or test support**: Add allocation-counting tests for warmed HTTP/1.1 and HTTP/2 local loops, following the existing stack allocation test model (`.paw/work/grpc-client-server-support/CodeResearch.md:142-145`).
- **`kimojio-stack-grpc/src/lib.rs` test modules or test support**: Add allocation-counting tests for warmed unary gRPC local loops.
- **`kimojio-stack-http/tests/protocol_limits.rs`**: Add focused malformed/limit/backpressure tests not already covered: oversized headers, oversized bodies, invalid frames, stalled flow-control windows, and peer close during in-flight exchange.
- **`kimojio-stack-grpc/tests/protocol_limits.rs`**: Add focused malformed/limit tests for gRPC message framing, missing status trailers, unsupported compression, oversized messages, and invalid metadata.

### Success Criteria:

#### Automated Verification:
- [ ] `cargo fmt --check`
- [ ] `cargo test -p kimojio-stack-http protocol_limits`
- [ ] `cargo test -p kimojio-stack-grpc protocol_limits`
- [ ] `cargo bench -p kimojio-stack-http --bench http_request_response -- --noplot`
- [ ] `cargo bench -p kimojio-stack-grpc --bench unary_rpc -- --noplot`
- [ ] `cargo clippy -p kimojio-stack-http --all-targets --all-features -- -D warnings`
- [ ] `cargo clippy -p kimojio-stack-grpc --all-targets --all-features -- -D warnings`

#### Manual Verification:
- [ ] Benchmark output records representative latency observations for HTTP and gRPC, satisfying SC-008 (`.paw/work/grpc-client-server-support/Spec.md:148-148`).
- [ ] Benchmark output distinguishes plaintext and TLS measurements for HTTP and unary gRPC whenever TLS support is implemented; any skipped TLS measurement has a documented reason.
- [ ] Any measured hot-path allocation is documented with source and rationale rather than left implicit.

---

## Phase 9: Documentation

### Changes Required:

- **`.paw/work/grpc-client-server-support/Docs.md`**: Create as-built technical reference with crate/module overview, public API summary, transport model, HTTP/1.1 support matrix, HTTP/2 support matrix, gRPC unary support matrix, limits/backpressure behavior, interoperability tests, benchmarks, and future streaming extension points.
- **`README.md`**: Add concise links/descriptions for `kimojio-stack-http` and `kimojio-stack-grpc` because this workflow adds user-facing crates. Root README currently links plain Markdown docs directly (`.paw/work/grpc-client-server-support/CodeResearch.md:27-34`).
- **`docs/stack-http-grpc.md`**: Add a user-facing guide for low-level HTTP/gRPC usage, supported protocol subset, TLS/plaintext setup, tokio interop guarantees, and why generated stubs/streaming are deferred.
- **`kimojio-stack-http/README.md`** and **`kimojio-stack-grpc/README.md`**: Add crate-local quick references if crate-local READMEs are consistent with workspace metadata and publishing expectations.
- **Streaming extension note**: Document how the unary APIs leave room for future client/server/bidirectional streaming, satisfying SC-010 (`.paw/work/grpc-client-server-support/Spec.md:149-150`).

### Success Criteria:

#### Automated Verification:
- [ ] `cargo fmt --check`
- [ ] `cargo test --doc`
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cargo test`

#### Manual Verification:
- [ ] Docs accurately describe the implemented protocol subset and explicit out-of-scope items.
- [ ] Docs include verification commands and benchmark summaries from implementation.
- [ ] Public documentation does not imply generated stubs or streaming support are available in the initial release.

---

## Final Verification Before Workflow Completion

- [ ] `cargo fmt --check`
- [ ] `cargo clippy`
- [ ] `cargo clippy --all-targets --all-features`
- [ ] `cargo test`
- [ ] `cargo test --doc`
- [ ] Focused HTTP interoperability tests pass.
- [ ] Focused gRPC interoperability tests pass.
- [ ] HTTP and gRPC benchmarks have been run in release/bench profile.
- [ ] Normal dependency trees for `kimojio-stack-http` and `kimojio-stack-grpc` do not include tokio/hyper/tonic.

## References

- Issue: none
- Workflow Context: `.paw/work/grpc-client-server-support/WorkflowContext.md`
- Spec: `.paw/work/grpc-client-server-support/Spec.md`
- Research: `.paw/work/grpc-client-server-support/CodeResearch.md`
