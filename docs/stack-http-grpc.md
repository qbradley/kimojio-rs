# Stackful HTTP and gRPC

`kimojio-stack-http` and `kimojio-stack-grpc` provide low-level HTTP/1.1,
HTTP/2, and unary, client-streaming, and server-streaming gRPC support for
`kimojio-stack` applications. They are
designed for explicit scheduling and predictable latency: callers drive all
work from stackful tasks, pass explicit transports, and configure limits
directly.

The public crates do not depend on tokio, hyper, tonic, or another async
runtime. Those crates are used only in tests as interoperability peers.

## Crates

| Crate | Purpose |
|-------|---------|
| `kimojio-stack-http` | HTTP body/limit types, plaintext/TLS transport boundary, HTTP/1.1 client/server, HTTP/2 client/server |
| `kimojio-stack-grpc` | Unary, client-streaming, and server-streaming gRPC client/server, metadata, status/trailer mapping, prost message framing |

## Transport model

HTTP connections are built over `StackTransport`, which wraps either:

- a connected plaintext `OwnedFd`, or
- a `kimojio_stack_tls::TlsStream`.

For TLS, enable the `tls` feature (enabled by default for
`kimojio-stack-http`) and use `kimojio_stack_http::tls::client_transport` or
`server_transport`. Client handshakes require a server name, which is applied to
SNI and OpenSSL hostname verification. Pass `Some(HttpProtocol::Http1)` or
`Some(HttpProtocol::Http2)` when the OpenSSL contexts are configured with ALPN
and the connection should reject the wrong negotiated protocol. Plaintext-only
users can disable HTTP default features to avoid the TLS/OpenSSL dependency
surface.

`StackTransport` is the stack-core compatibility alias for
`RuntimeStackTransport<IoFd>`. The HTTP/1.1, HTTP/2, and protocol-neutral client
and server connection types also have runtime-generic forms that work with any
socket handle implementing the shared `kimojio-stack::runtime_api` socket
contract. This keeps runtime choice explicit: stack-core callers can use the
existing names, while companion runtime adapters such as `kimojio-stack-steal`
and `kimojio-stack-ring-pool` can pass their own socket handles without adding
helper threads, mandatory dynamic dispatch, or a second HTTP runtime trait.

Transport I/O deadlines are transport-local. Plaintext deadlines use runtime
socket async handles, TLS deadlines use generic TLS async handles, and both wait
on a runtime-neutral timer handle instead of zero-duration polling. Stalled
TLS reads/writes can be canceled and drained on both stack-core and
`kimojio-stack-steal` before the transport is closed.

## HTTP usage

Use protocol-specific connections when the protocol is known:

- `http1::ClientConnection` and `http1::ServerConnection`
- `h2::ClientConnection` and `h2::ServerConnection`

Use `client::ClientConnection` or `server::ServerConnection` when code wants a
single protocol-neutral wrapper around an already-selected HTTP/1.1 or HTTP/2
connection.

Current runtime-agnostic status:

| Layer | Status |
|-------|--------|
| HTTP/1.1 plaintext/TLS | Runtime-generic over `RuntimeStackTransport<S>` |
| HTTP/2 | Runtime-generic over `RuntimeStackTransport<S>` |
| Protocol-neutral HTTP wrappers | Runtime-generic over HTTP/1.1 or HTTP/2 connections |
| gRPC client | Runtime-generic over the owned HTTP/2 client connection |
| gRPC server | Runtime-generic over a `GrpcRuntime` marker and owned HTTP/2 server connection |

Ring-pool stack compatibility is covered by default tests for HTTP/1.1,
HTTP/2, gRPC unary/client-streaming/server-streaming, status/error propagation,
cancellation, and TLS-over-HTTP/2 smoke paths.

Bodies are represented by `Body` and bounded by `BodyLimits`. Parser and
connection limits live in `HttpConfig`, including header, body, frame, and read
buffer limits.

Supported HTTP behavior:

| Protocol | Supported |
|----------|-----------|
| HTTP/1.1 | fixed-length request/response bodies, chunked request bodies, EOF-delimited responses, sequential keep-alive, TLS |
| HTTP/2 | prior-knowledge plaintext, TLS with ALPN, settings, HPACK, trailers, concurrent streams, flow control, reset/goaway handling |

Advanced browser/proxy features such as WebSockets, CONNECT tunneling, HTTP/2
server push, and concurrent HTTP/1.1 pipeline processing are not part of the
initial scope. Already-buffered pipelined requests are served sequentially.

HTTP/2 forwarding uses ordered `h2::HeaderField` occurrences. Build or inspect
typed request/response blocks with `h2::{request_from_header_fields,
request_header_fields,response_from_header_fields,response_header_fields}`.
`Trailers::{from_h2_fields,as_h2_fields,into_h2_fields}` retains duplicate
order and sensitivity. `HeaderMap` and `Trailers::into_map` are convenience
projections; do not use a projected map to forward a received canonical block.
If a convenience projection's bytes, structure, or sensitivity was modified,
stack HTTP returns a protocol error instead of silently re-encoding stale or
reordered fields.

### HPACK ownership, limits, and diagnostics

Each stack HTTP/2 `ConnectionState` owns one inbound decoder and one outbound
encoder from the repository-owned FSM HTTP codec. Header preparation is
reversible, and the encoder advances only when the complete block is assigned
to connection wire order. The FSM validation layer runs in external-HPACK mode,
so stack and FSM views do not maintain competing dynamic tables.

Canonical `h2::HeaderField` occurrences retain byte values, duplicate order,
and the sensitivity copied from `http::HeaderValue`. Sensitive fields remain
never indexed through HTTP and gRPC metadata/status conversion. Pseudo-fields
are typed request/response roles; converting one to a regular map entry is
rejected. `HeaderMap` and legacy metadata maps are inspection projections, not
forwarding authorities.

The local HTTP/2 settings bound decoded header-list size and table capacity.
Encoded assembly has separate checked, fallible accounting: runtime
connections use `HttpConfig::max_header_bytes`, while low-level callers can use
`ConnectionState::new_with_encoded_header_block_limit`. Encoded limit,
accounting, allocation, and compression failures record one stable terminal
connection error; a decoded-list limit leaves HPACK synchronized and reusable.
Projection or HPACK failures surface through the existing protocol/limit error
categories rather than silently dropping bytes or sensitivity.
`ConnectionState::inbound_hpack_diagnostics` and
`outbound_hpack_diagnostics` expose content-free directional block, byte,
representation, Huffman, table, and failure counters.

The [HPACK and Header Representation Reference](hpack-header-representation.md)
defines the codec policy, stable error categories, exact diagnostics, and the
fixed, target/action-level six-package default/all-feature compatibility
matrix, including explicit required-feature non-applicability.

## gRPC usage

The gRPC layer is unary-first and prost-compatible, with client-streaming and
server-streaming support. Client-streaming sends an ordered stream of protobuf
requests followed by one protobuf response and one terminal status.
Server-streaming sends one protobuf request followed by an ordered stream of
protobuf responses and one terminal status.

Generated client/server stubs are intentionally deferred. The initial API keeps
method paths, metadata, message limits, and transports explicit until the
stackful transport and service model are proven without adding hidden runtime
or allocation costs.

The gRPC runtime migration boundary is the owned HTTP/2 client or server
connection supplied by the caller. Stack-core compatibility aliases preserve the
existing `UnaryClient`, `UnaryServer`, and `ServerStreamingResponse` names, while
`RuntimeUnaryClient`, `RuntimeUnaryServer`, and `RuntimeServerStreamingResponse`
allow companion runtimes to supply their own HTTP/2 socket handles.

`UnaryClient::call` and `UnaryServer::serve_one` are sequential convenience
helpers for unary RPCs. `UnaryClient::call_server_streaming` returns a
`ServerStreamingResponse` whose `next(cx)` method yields decoded messages until
clean EOF or a terminal `Status` error. `UnaryServer::add_server_streaming`
registers handlers that return `ServerStreamingReply` backed by a
`ServerStream` or bounded-channel `ReceiverStream`.

Client flow:

1. Create an HTTP/2 client connection.
2. Wrap it with `UnaryClient::new(http, ClientConfig::default())`.
3. Call `call::<Req, Resp>(cx, "/package.Service/Method", metadata, &request)`.
4. Read `UnaryResponse` for response metadata, decoded message, status, and
   trailers. Use `call_client_streaming` or `call_server_streaming` for the
   streaming variants.

Server flow:

1. Create an HTTP/2 server connection.
2. Create `UnaryServer::new(ServerConfig::default())`.
3. Register handlers with `add_unary::<Req, Resp, _>(path, handler)`,
   `add_client_streaming`, or `add_server_streaming`.
4. Call `serve_one(cx, &mut http)` for each request.

Handlers return `UnaryReply<Resp>` for successful responses or `Status` for
peer-visible gRPC errors.

Server-streaming handlers return `ServerStreamingReply<S>`. Each yielded
`Ok(message)` is encoded and sent as an individual gRPC frame; `Ok(None)` sends
`grpc-status: 0` trailers, and `Err(Status)` sends that status as the terminal
trailers. One active streaming response is supported per `UnaryClient`; use
independent client connections for concurrent server-streaming RPCs.

Metadata is represented by `Metadata`. Use `insert_bin` and `get_bin` for
binary `-bin` metadata. Status messages are percent-encoded on the wire, and
binary status details use `grpc-status-details-bin`.

`Metadata::as_h2_fields` and `into_h2_fields` use the stack-facing
`h2::HeaderField` carrier and are the forwarding APIs. The `as_http_headers`,
`into_http_headers`, `as_headers`, and `into_headers` methods remain
compatibility projections and can panic for canonical data that is not
representable by `HeaderMap`; use `try_as_http_headers` or
`try_into_http_headers` for fallible inspection. Stack gRPC request, response,
and status paths do not use those lossy projections.

## Limits and backpressure

The implementation favors explicit limits and bounded buffering:

- `HttpConfig` controls HTTP start-line, header, body, frame, and read-buffer
  limits.
- `ClientConfig` and `ServerConfig` are exported from `kimojio-stack-grpc` and
  control unary and server-streaming gRPC message sizes.
- HTTP/2 flow-control windows are tracked explicitly. Repeated terminal DATA
  frames replenish the connection receive window so long-running unary-style
  connections do not stall after the initial connection window is consumed.
- Server-streaming producers are driven inline. Bounded channel-backed streams
  use caller-owned capacity for backpressure; the gRPC layer does not add hidden
  unbounded response queues or background readers.

Errors are inspectable through `ErrorKind` and `LimitKind`, allowing callers and
tests to distinguish protocol errors, peer resets, EOF, TLS failures, and size
limits.

## Interoperability

The test suite validates both stackful-to-tokio and tokio-to-stackful
interoperability:

- HTTP/1.1 client and server against tokio/hyper peers.
- HTTP/2 client and server against tokio/hyper peers.
- Unary and server-streaming gRPC client and server against tonic peers.
- gRPC over HTTP/2 TLS for the stackful client path.

Tokio-based crates remain dev-dependencies for these tests and are not normal
dependencies of the public stackful crates.

## Benchmarks

Representative final Criterion medians:

| Benchmark | Median |
|-----------|--------|
| HTTP/1.1 plaintext small / large body | ~6.55 us / ~13.72 us |
| HTTP/1.1 TLS small / large body | ~14.35 us / ~44.96 us |
| HTTP/2 plaintext small / large body | ~18.27 us / ~24.46 us |
| HTTP/2 TLS small / large body | ~54.17 us / ~84.10 us |
| gRPC plaintext small / moderate payload | ~25.61 us / ~46.65 us |
| gRPC TLS small / moderate payload | ~72.41 us / ~102.26 us |

Run the benchmark suites with:

```sh
cargo test -p kimojio-stack-http --bench http_request_response -- --test
cargo test -p kimojio-stack-tls --bench tls_read_write -- --test
cargo bench -p kimojio-stack-http --bench http_request_response -- --noplot
cargo bench -p kimojio-stack-tls --bench tls_read_write -- --noplot
cargo bench -p kimojio-stack-grpc --bench unary_rpc -- --noplot
```

The HTTP benchmark labels are explicit about runtime, protocol, transport, and
deadline path: for example
`stack-core/http1/plaintext/request_response/small_body`,
`stack-core/h2/plaintext/deadline/request_response/large_body`, and
`stack-steal/worker-local/h2/plaintext/request_response/small_body`.
`stack-steal/shared-root/*` uses stack-steal shared-ring routing from non-worker
stackful tasks; `stack-steal/worker-local/*` runs socket I/O from worker
contexts. Criterion output should be interpreted as latency distributions for
the labeled path; compare medians and tail percentiles only between labels with
the same body size and protocol.

The TLS benchmark labels separate stack-core synchronous and waitable async
read/write paths from stack-steal worker-local and shared-root synchronous
read/write paths, for example `stack-core/tls/read_async/16KiB` and
`stack-steal/shared-root/tls/write/1KiB`.

Allocation-focused tests cover warmed plaintext HTTP/1.1, HTTP/2, and unary
gRPC local loops. TLS paths are benchmarked for latency/throughput instead of
using allocation budgets because OpenSSL handshake and record-layer internals
dominate allocator counts outside the stackful HTTP/gRPC hot-path code.

## Future streaming extensions

Bidirectional streaming is not yet implemented. The existing HTTP/2 transport,
metadata, status, and message-limit types leave room for it without changing
unary, client-streaming, or server-streaming callers.
