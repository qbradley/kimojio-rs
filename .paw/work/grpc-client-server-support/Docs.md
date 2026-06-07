# gRPC Client Server Support

## Overview

This work adds two stackful networking crates to the workspace:

- `kimojio-stack-http` provides low-level HTTP/1.1 and HTTP/2 client/server request-response support over `kimojio-stack` transports.
- `kimojio-stack-grpc` builds unary gRPC client/server support on top of the HTTP/2 transport.

The implementation is intentionally low-level. Callers provide explicit transports, body limits, message limits, and stackful runtime contexts. The crates do not expose background worker threads, generated stubs, or async-runtime abstractions, and their normal dependencies do not include tokio, hyper, tonic, or the tokio-bound `h2` crate. Tokio, hyper, and tonic are used only as peer implementations in tests and benchmarks.

The initial gRPC scope is unary-only. The APIs are designed so streaming can be added later without replacing the unary client, unary server, metadata, status, or HTTP/2 transport foundations.

## Architecture and Design

### High-Level Architecture

The stack is layered as follows:

1. `kimojio-stack` owns stackful coroutines, structured scopes, socket and fd I/O, parking, and scheduling.
2. `kimojio-stack-tls` adapts OpenSSL TLS streams to stackful socket I/O.
3. `kimojio-stack-http` adds HTTP body buffers, typed errors, plaintext/TLS transports, HTTP/1.1 parsing/serialization, and a local HTTP/2 frame/state implementation.
4. `kimojio-stack-grpc` adds gRPC message framing, status/trailer handling, metadata validation, unary client calls, and unary server dispatch.

HTTP and gRPC calls are driven synchronously from stackful tasks. Blocking I/O parks the current stackful task through the runtime scheduler rather than creating nested event loops or depending on an external async runtime.

### Design Decisions

#### Runtime independence

Public HTTP/gRPC crates use runtime-agnostic building blocks such as `http`, `bytes`, `httparse`, `httpdate`, `hpack`, `prost`, and `base64`. They avoid normal dependencies that require tokio or another async runtime. This keeps scheduling control in `kimojio-stack`.

#### Local HTTP/2 core

HTTP/2 is implemented locally instead of using the ecosystem `h2` crate because the current `h2` crate is tokio-oriented. The local core owns frame encoding/decoding, SETTINGS, HPACK adaptation, stream state, trailers, and flow-control windows. Flow control is explicit and bounded; callers do not get implicit unbounded buffering.

#### Explicit transports

`StackTransport` wraps either a plaintext connected fd or a `kimojio-stack-tls::TlsStream`. TLS helpers perform client/server handshakes and can validate negotiated ALPN (`http/1.1` or `h2`) when callers configure OpenSSL contexts with ALPN.

#### Unary-first gRPC

The gRPC crate exposes unary request/response calls and unary handler registration directly over prost-compatible message types. It does not generate tonic-style stubs and does not implement client streaming, server streaming, or bidirectional streaming yet.

### Integration Points

- HTTP request/response types use the standard `http` crate with `kimojio_stack_http::Body`.
- Protobuf messages implement `prost::Message`.
- TLS uses caller-provided `kimojio_stack_tls::TlsContext`.
- Interoperability tests pair stackful clients/servers with tokio/hyper/tonic peers on separate runtime threads.

## User Guide

### Prerequisites

- Linux with the `kimojio-stack` runtime available.
- Connected sockets represented as `OwnedFd` values.
- Optional TLS contexts from `kimojio-stack-tls` when using TLS. The HTTP `tls` feature is enabled by default and can be disabled by plaintext-only users.
- Prost-compatible request and response message types for gRPC.

### Basic HTTP Usage

Construct a `StackTransport`, create a protocol-specific or protocol-neutral connection, and drive it inside `Runtime::block_on`.

For HTTP/1.1:

- Client: `kimojio_stack_http::http1::ClientConnection::new(transport, HttpConfig::default())`, then `send(cx, &request)`.
- Server: `kimojio_stack_http::http1::ServerConnection::new(transport, HttpConfig::default())`, then `read_request`/`write_response` or `serve_one`.

For HTTP/2:

- Client: `kimojio_stack_http::h2::ClientConnection::new(transport, HttpConfig::default())`.
- Server: `kimojio_stack_http::h2::ServerConnection::new(transport, HttpConfig::default())`.
- Use `send_request` plus `read_response_with_trailers` when the caller needs HTTP/2 trailers.

`kimojio_stack_http::client::ClientConnection` and `kimojio_stack_http::server::ServerConnection` provide protocol-neutral entry points when the protocol is already known by construction.

### Basic gRPC Usage

The gRPC client wraps an HTTP/2 client connection:

1. Create `h2::ClientConnection` over a plaintext or TLS `StackTransport`.
2. Create `UnaryClient::new(http, ClientConfig::default())`.
3. Call `client.call::<Req, Resp>(cx, "/package.Service/Method", metadata, &request)`.
4. Inspect `UnaryResponse { metadata, message, status, trailers }`.

The gRPC server wraps an HTTP/2 server connection:

1. Create `h2::ServerConnection`.
2. Create `UnaryServer::new(ServerConfig::default())`.
3. Register unary handlers with `add_unary::<Req, Resp, _>(path, handler)`.
4. Call `serve_one(cx, &mut http)` for each request to accept and dispatch.

Handlers receive the stackful runtime context, request metadata, and decoded request message. They return `UnaryReply<Resp>` on success or `Status` on application/protocol errors.

`UnaryClient::call` and `UnaryServer::serve_one` are sequential convenience helpers for unary RPCs. Callers that need explicit stream lifecycle control should use the lower-level HTTP/2 `send_request` / `read_response_with_trailers` and `accept` / `send_response_with_trailers` APIs directly until dedicated streaming gRPC handles are introduced.

### TLS Usage

Use `kimojio_stack_http::tls::client_transport` and `server_transport` to perform stackful TLS handshakes and return `StackTransport` values. Client handshakes require a server name for SNI and hostname verification. Pass `Some(HttpProtocol::Http1)` or `Some(HttpProtocol::Http2)` to validate ALPN after the handshake, or `None` when the caller is intentionally operating without ALPN validation.

### Advanced Usage

- Tune `HttpConfig` for start-line, header-count, header-byte, body-byte, and read-buffer limits.
- Tune `ClientConfig::max_message_len` and `ServerConfig::max_message_len` for gRPC frame limits.
- Use `Metadata::insert_bin` and `get_bin` for gRPC binary metadata (`-bin` headers). Binary metadata accepts padded and unpadded base64 from peers.
- Use `Status::with_details` to include `grpc-status-details-bin`.
- Use `h2::ServerConnection::shutdown_write_and_close_after_peer` when a server has sent final HTTP/2 responses/trailers and should half-close writes while draining final peer frames such as WINDOW_UPDATE before closing.

## API Reference

### `kimojio-stack-http`

| Component | Purpose |
|-----------|---------|
| `Body`, `BodyBuilder`, `BodyLimits` | Bounded byte body representation and incremental accumulation |
| `HttpConfig` | Shared HTTP parser, body, frame, header, and buffer limits |
| `StackTransport` | Plaintext fd or TLS stream transport boundary |
| `Error`, `ErrorKind`, `LimitKind` | Inspectable transport, parse, protocol, unsupported, size-limit, EOF, and reset errors |
| `http1::ClientConnection` / `http1::ServerConnection` | Low-level HTTP/1.1 request-response connections |
| `h2::ClientConnection` / `h2::ServerConnection` | Low-level HTTP/2 request-response connections with trailers and flow control |
| `client::ClientConnection` / `server::ServerConnection` | Protocol-neutral wrappers for known HTTP/1.1 or HTTP/2 connections |
| `tls::{client_transport, server_transport, HttpProtocol}` | TLS handshakes, SNI/hostname binding, and ALPN validation for HTTP transports |

### `kimojio-stack-grpc`

| Component | Purpose |
|-----------|---------|
| `UnaryClient` | Unary gRPC client over an HTTP/2 client connection |
| `UnaryResponse<M>` | Response metadata, decoded message, status, and trailers |
| `UnaryServer` | Unary handler registry and dispatcher over an HTTP/2 server connection |
| `UnaryReply<M>` | Successful response message plus initial and trailing metadata |
| `Metadata` | Validated gRPC metadata, including binary metadata helpers |
| `Status`, `StatusCode` | gRPC status/trailer mapping with percent-encoded messages and binary details |
| `encode_message`, `decode_message` | gRPC message frame helpers for prost-compatible messages |
| `ClientConfig`, `ServerConfig` | gRPC message-size limits, also available from `client::ClientConfig` and `server::ServerConfig` |

### Protocol Support Matrix

| Area | Supported | Deferred or rejected |
|------|-----------|----------------------|
| HTTP/1.1 client/server | Fixed-length bodies, chunked request bodies, EOF-delimited responses, keep-alive sequential requests, TLS transport | Pipelining, upgrades, CONNECT/proxying, WebSockets |
| HTTP/2 client/server | Prior-knowledge plaintext, TLS with ALPN, settings, HPACK, DATA/HEADERS/CONTINUATION, trailers, PING, RST_STREAM, GOAWAY, flow control, concurrent streams | Server push, broad priority support, proxy tunneling |
| gRPC | Unary client calls, unary server handlers, metadata, binary metadata, trailers, status details, tonic interop, TLS over HTTP/2 | Generated stubs, client streaming, server streaming, bidirectional streaming, compression negotiation |

### Configuration Options

`HttpConfig::default()` sets:

- start-line limit: 8 KiB
- header count: 100
- header bytes: 64 KiB
- body bytes: 4 MiB
- read buffer: 16 KiB

`kimojio-stack-grpc` defaults unary message limits to 4 MiB for both client and server configs.

## Testing

### How to Test

Primary verification commands:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features
cargo test
cargo test --doc
```

Focused HTTP/gRPC commands:

```sh
cargo test -p kimojio-stack-http
cargo test -p kimojio-stack-grpc
cargo test -p kimojio-stack-http protocol_limits
cargo test -p kimojio-stack-grpc protocol_limits
cargo bench -p kimojio-stack-http --bench http_request_response -- --noplot
cargo bench -p kimojio-stack-grpc --bench unary_rpc -- --noplot
```

### Interoperability Coverage

- Stackful HTTP/1.1 client to tokio HTTP/1.1 server.
- Tokio HTTP/1.1 client to stackful HTTP/1.1 server.
- Stackful HTTP/2 client to tokio HTTP/2 server.
- Tokio HTTP/2 client to stackful HTTP/2 server.
- Stackful unary gRPC client to tonic server, including status and metadata cases.
- Tonic client to stackful unary gRPC server, including status and trailer cases.
- Stackful gRPC client over TLS to tonic TLS server.

### Edge Cases

Focused tests cover:

- Oversized HTTP/1.1 headers and bodies.
- HTTP/2 malformed frames, body limits, peer close during an in-flight exchange, stalled stream windows, and repeated terminal DATA frames.
- gRPC unsupported compression flags, malformed lengths, missing status trailers, invalid metadata, invalid binary status details, and configured message-size limits.
- Graceful HTTP/2 close behavior after final responses/trailers so peers can send terminal flow-control frames without causing TCP reset errors.

### Benchmark Observations

Representative final Criterion medians:

| Benchmark | Median |
|-----------|--------|
| HTTP/1.1 plaintext small / large body | ~6.55 us / ~13.72 us |
| HTTP/1.1 TLS small / large body | ~14.35 us / ~44.96 us |
| HTTP/2 plaintext small / large body | ~18.27 us / ~24.46 us |
| HTTP/2 TLS small / large body | ~54.17 us / ~84.10 us |
| gRPC plaintext small / moderate payload | ~25.61 us / ~46.65 us |
| gRPC TLS small / moderate payload | ~72.41 us / ~102.26 us |

Allocation tests document current warmed plaintext hot-path behavior for HTTP/1.1, HTTP/2, and unary gRPC loops with explicit upper bounds. Remaining allocations are measured rather than treated as invisible. TLS paths are tracked through latency benchmarks rather than allocation budgets because OpenSSL handshake and record-layer internals dominate allocator counts outside the stackful HTTP/gRPC hot path.

## Limitations and Future Work

- gRPC streaming is not implemented. The existing unary APIs can coexist with future streaming APIs that expose request, response, or bidirectional stream handles over the same HTTP/2 transport and metadata/status types.
- Tonic-like generated stubs are not implemented. Callers use explicit method paths and prost message types.
- HTTP/2 prioritization, server push, proxies, CONNECT, and WebSockets are out of scope.
- Compression negotiation is intentionally rejected for now; unsupported compressed gRPC frames return explicit errors.
- The current APIs buffer complete bodies/messages. Future streaming support should preserve configured limits and bounded backpressure while avoiding unbounded buffering.
