# Stackful HTTP and gRPC

`kimojio-stack-http` and `kimojio-stack-grpc` provide low-level HTTP/1.1,
HTTP/2, and unary gRPC support for `kimojio-stack` applications. They are
designed for explicit scheduling and predictable latency: callers drive all
work from stackful tasks, pass explicit transports, and configure limits
directly.

The public crates do not depend on tokio, hyper, tonic, or another async
runtime. Those crates are used only in tests as interoperability peers.

## Crates

| Crate | Purpose |
|-------|---------|
| `kimojio-stack-http` | HTTP body/limit types, plaintext/TLS transport boundary, HTTP/1.1 client/server, HTTP/2 client/server |
| `kimojio-stack-grpc` | Unary gRPC client/server, metadata, status/trailer mapping, prost message framing |

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

## HTTP usage

Use protocol-specific connections when the protocol is known:

- `http1::ClientConnection` and `http1::ServerConnection`
- `h2::ClientConnection` and `h2::ServerConnection`

Use `client::ClientConnection` or `server::ServerConnection` when code wants a
single protocol-neutral wrapper around an already-selected HTTP/1.1 or HTTP/2
connection.

Bodies are represented by `Body` and bounded by `BodyLimits`. Parser and
connection limits live in `HttpConfig`, including header, body, frame, and read
buffer limits.

Supported HTTP behavior:

| Protocol | Supported |
|----------|-----------|
| HTTP/1.1 | fixed-length request/response bodies, chunked request bodies, EOF-delimited responses, sequential keep-alive, TLS |
| HTTP/2 | prior-knowledge plaintext, TLS with ALPN, settings, HPACK, trailers, concurrent streams, flow control, reset/goaway handling |

Advanced browser/proxy features such as WebSockets, CONNECT tunneling, HTTP/2
server push, and HTTP/1.1 pipelining are not part of the initial scope.

## gRPC usage

The gRPC layer is unary-first and prost-compatible.

Generated client/server stubs are intentionally deferred. The initial API keeps
method paths, metadata, message limits, and transports explicit until the
stackful transport and service model are proven without adding hidden runtime
or allocation costs.

`UnaryClient::call` and `UnaryServer::serve_one` are sequential convenience
helpers for unary RPCs. Callers that need explicit stream lifecycle control
should use the lower-level HTTP/2 `send_request` / `read_response_with_trailers`
and `accept` / `send_response_with_trailers` APIs directly until dedicated
streaming gRPC handles are introduced.

Client flow:

1. Create an HTTP/2 client connection.
2. Wrap it with `UnaryClient::new(http, ClientConfig::default())`.
3. Call `call::<Req, Resp>(cx, "/package.Service/Method", metadata, &request)`.
4. Read `UnaryResponse` for response metadata, decoded message, status, and
   trailers.

Server flow:

1. Create an HTTP/2 server connection.
2. Create `UnaryServer::new(ServerConfig::default())`.
3. Register handlers with `add_unary::<Req, Resp, _>(path, handler)`.
4. Call `serve_one(cx, &mut http)` for each request.

Handlers return `UnaryReply<Resp>` for successful responses or `Status` for
peer-visible gRPC errors.

Metadata is represented by `Metadata`. Use `insert_bin` and `get_bin` for
binary `-bin` metadata. Status messages are percent-encoded on the wire, and
binary status details use `grpc-status-details-bin`.

## Limits and backpressure

The implementation favors explicit limits and bounded buffering:

- `HttpConfig` controls HTTP start-line, header, body, frame, and read-buffer
  limits.
- `ClientConfig` and `ServerConfig` are exported from `kimojio-stack-grpc` and
  control unary gRPC message sizes.
- HTTP/2 flow-control windows are tracked explicitly. Repeated terminal DATA
  frames replenish the connection receive window so long-running unary-style
  connections do not stall after the initial connection window is consumed.

Errors are inspectable through `ErrorKind` and `LimitKind`, allowing callers and
tests to distinguish protocol errors, peer resets, EOF, TLS failures, and size
limits.

## Interoperability

The test suite validates both stackful-to-tokio and tokio-to-stackful
interoperability:

- HTTP/1.1 client and server against tokio/hyper peers.
- HTTP/2 client and server against tokio/hyper peers.
- Unary gRPC client and server against tonic peers.
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

Run them with:

```sh
cargo bench -p kimojio-stack-http --bench http_request_response -- --noplot
cargo bench -p kimojio-stack-grpc --bench unary_rpc -- --noplot
```

Allocation-focused tests cover warmed plaintext HTTP/1.1, HTTP/2, and unary
gRPC local loops. TLS paths are benchmarked for latency/throughput instead of
using allocation budgets because OpenSSL handshake and record-layer internals
dominate allocator counts outside the stackful HTTP/gRPC hot-path code.

## Future streaming support

Streaming is intentionally deferred. The current unary APIs leave room for
future client-streaming, server-streaming, and bidirectional-streaming handles
that reuse the existing HTTP/2 transport, metadata, status, and message-limit
types without changing unary callers.
