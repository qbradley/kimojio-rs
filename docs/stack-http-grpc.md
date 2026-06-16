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
| `kimojio-stack-grpc` | Unary and server-streaming gRPC client/server, metadata, status/trailer mapping, prost message framing |

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
can pass their own socket handles without adding helper threads, mandatory
dynamic dispatch, or a second HTTP runtime trait.

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

The gRPC layer is unary-first and prost-compatible, with server-streaming
support for one protobuf request followed by an ordered stream of protobuf
responses and one terminal status.

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
   trailers.

Server flow:

1. Create an HTTP/2 server connection.
2. Create `UnaryServer::new(ServerConfig::default())`.
3. Register handlers with `add_unary::<Req, Resp, _>(path, handler)`.
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

Client-streaming and bidirectional-streaming are not yet implemented. The
existing HTTP/2 transport, metadata, status, and message-limit types leave room
for these without changing unary or server-streaming callers.
