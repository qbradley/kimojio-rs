# Kimojio Runtime

`kimojio` is a single-threaded Linux async runtime that uses `io_uring`. It
provides runtime-local tasks, asynchronous file and socket operations, timers,
channels, synchronization primitives, TLS support, and optional HTTP support.

## Responsibilities

The crate owns:

- the thread-local executor and task scheduler;
- `io_uring` submission and completion processing;
- asynchronous file, socket, pipe, and timer operations;
- runtime-local channels and synchronization;
- optional TLS streams;
- optional HTTP/1.1 and HTTP/2 client and server APIs, including TLS with ALPN.

Applications must assign work to runtime threads. Kimojio does not move tasks
between threads or provide automatic load balancing.

## Add Kimojio

```toml
[dependencies]
kimojio = "0.17"
```

The default feature enables TLS. Enable the HTTP API separately:

```toml
[dependencies]
kimojio = { version = "0.17", features = ["http"] }
```

## Use the HTTP client

The HTTP client uses standard `http` methods, headers, versions, requests, and
responses. Cleartext requests use HTTP/1.1 unless the request selects HTTP/2
prior knowledge. HTTPS selects HTTP/1.1 or HTTP/2 through ALPN and requires a
`TlsClientConfig`.

```rust,no_run
use kimojio::http::{Client, Version};

# async fn example() -> kimojio::http::Result<()> {
let response = Client::new()
    .get("http://127.0.0.1:8080/health")
    .send()
    .await?;

assert!(response.status().is_success());

let response = Client::new()
    .get("http://127.0.0.1:8080/health")
    .version(Version::HTTP_2)
    .send()
    .await?;
# let _ = response;
# Ok(())
# }
```

## Use the HTTP server

The server accepts HTTP/1.1 and HTTP/2 prior-knowledge connections on the same
cleartext listener. A `TlsServerConfig` enables TLS and selects the protocol
through ALPN. The handler future is runtime-local and does not need to
implement `Send`.

```rust,no_run
use std::net::{Ipv4Addr, SocketAddr};
use std::rc::Rc;

use kimojio::CancellationToken;
use kimojio::http::{Body, Response, Server};

# async fn example() -> kimojio::http::Result<()> {
let address = SocketAddr::from((Ipv4Addr::LOCALHOST, 8080));
let server = Server::bind(address).await?;
let cancellation = Rc::new(CancellationToken::new());

server
    .serve(
        |_request| async {
            Response::builder()
                .status(200)
                .body(Body::from("hello"))
                .expect("static response is valid")
        },
        cancellation,
    )
    .await
# }
```

`Body::from_chunks` streams infallible chunks, while `Body::from_stream`
propagates producer errors. An HTTP/1 response-source failure retires its
connection; HTTP/2 resets only the failed stream. Buffered bodies retain their
byte-slice accessors. A streaming body has no bytes to borrow in advance, so
`as_bytes`, `into_bytes`, `len`, and `is_empty` panic for that shape; use
`known_len` when accepting either shape.

Inbound bodies remain buffered by default: `Server::serve`, `Client::execute`,
and `RequestBuilder::send` never return a streaming body. Opt in with
`Server::serve_streaming`, `Client::execute_streaming`, or
`RequestBuilder::send_streaming`, then pull owned chunks with
`Body::next_chunk`. Each pull drives at most one transport chunk, so a paused
consumer applies TCP or HTTP/2 flow-control backpressure instead of filling an
adapter queue. Dropping an incomplete client response retires its checked-out
connection; returning from a server handler before its HTTP/1 request ends
likewise closes that connection.

## HTTP limits

`ClientConfig`, `ServerConfig`, and `Limits` control the maximum header count,
encoded header size, accumulated body size, and reusable read-buffer size. Set
these limits for the workload before you accept untrusted traffic. The body
limit continues to protect default buffered requests and responses. A
pull-based body has no total-size cap because only its current chunk and bounded
handoff are resident; read buffers, HTTP/2 receive windows, and queued-DATA
bounds still constrain its memory use and untrusted-input exposure.

`ClientConfig` also gives each streaming-response I/O operation a 30-second
idle timeout and controls its connection pool. By default, idle connections
remain eligible for 90 seconds, with at most eight retained per origin and
protocol key and 64 retained in total. Use `set_connection_io_timeout`,
`set_pool_idle_timeout`, `set_pool_max_idle_per_key`, and
`set_pool_max_idle_total` to tune these bounds; setting any pool bound to zero
disables idle reuse. The shared
`max_requests_per_connection` limit also retires a client connection after the
configured number of completed requests. A cloned `Client` shares its pool;
separately constructed clients do not. HTTP/2 reuse is sequential rather than
multiplexed. Use `execute_with_event_handler` or `send_with_event_handler` to
observe retries and dirty pooled-connection discards.

For HTTP/1.1 requests with `Expect: 100-continue`, the client sends the request
head first and waits up to one second for the interim response. It then sends
the body even if no interim response arrived, avoiding hangs behind
intermediaries that suppress 1xx responses. Use
`ClientConfig::set_expect_continue_timeout` to tune this bound.

`ServerConfig` also has finite defaults for the per-operation connection I/O
timeout, graceful-shutdown timeout, and maximum concurrent connections. The
corresponding `ServerBuilder` methods can tune them for a workload; zero
timeouts and a zero connection limit are rejected during `bind`.

The defaults are safety bounds, not a production capacity claim. Measure the
expected request rate, connection count, payload sizes, tail latency, and
per-connection memory on the target system. Set the limits from those results.

Connection parse/I/O failures and handler panics are isolated from the
listener. `Server::serve` reports them to standard error.
`Server::serve_with_error_handler` instead passes each nonfatal `ServeError`
to a runtime-local callback for application logging or telemetry. Fatal
listener errors are still returned by both methods.

`Server::serve_with_expect_continue` adds a synchronous request-head hook for
supported `Expect: 100-continue` requests. The hook can continue to the normal
buffered handler or return a final response before the server reads the body.
`Server::serve_streaming_with_expect_continue` provides the same one-shot hook
for a streaming handler.
`Server::serve` keeps the default behavior and sends `100 Continue`
automatically.

Handlers receive parsed hop-by-hop request headers. If a handler forwards a
request, it must remove `Connection`, each header named by `Connection`, and
other connection-scoped headers before it sends the request. Kimojio does not
apply proxy policy.

Protocol failures use `Error::Protocol(ProtocolError)`. Applications should
match `ProtocolError::kind()` and the non-exhaustive `ProtocolErrorKind`
categories rather than parsing display text. Transport failures, configured
limit failures, and server connection-task lifecycle failures keep their
dedicated `Error` or `ServeError` variants.

The initial HTTP API has these limits:

- cleartext `http://` and TLS `https://`;
- HTTP/1.1 and HTTP/2 (prior knowledge over cleartext, ALPN over TLS);
- the server multiplexes concurrent HTTP/2 streams on one connection;
- the server serves sequential HTTP/1 requests over persistent connections;
- the client pools idle HTTP/1 and HTTP/2 connections for sequential reuse;
- received request and response bodies are buffered by default and can be
  streamed explicitly;
- client request and server response bodies can be streamed incrementally;
- no redirects, proxy policy, general-purpose retries, or cookies;
- no protocol upgrades, WebSocket, or CONNECT tunnel.

The server reuses HTTP/1 connections sequentially and HTTP/2 connections
across concurrent streams. The client reuses both protocols sequentially and
retries once on a fresh connection when a reused connection fails before any
response bytes arrive. Non-idempotent requests are retried only when the failed
attempt wrote no request bytes.

The HTTP API does not authenticate peers or protect traffic in transit. Do not
expose it to an untrusted network unless a trusted proxy or another secure
transport boundary provides TLS.

Host names are resolved asynchronously. Kimojio prefers systemd-resolved's
local Varlink service and falls back to the operating system resolver on a
process-wide helper thread, so lookups do not block the runtime thread.

The Varlink reply decoder is generated rather than hand-written: the schema and
the desired interface are written down, a language model turns them into a state
machine over the allocation-free `kimojio-json` tokenizer, and the result is
checked in and reviewed like any other source. See the `kimojio-json-decoder`
skill in `.github/skills/`, and the module documentation in
`kimojio/src/resolver/varlink_reply.rs`. Nothing in the shipping binary depends
on a model.

## Platform limits

Kimojio requires Linux kernel 5.15 or later. Tasks are cooperative and remain
on one runtime thread. CPU-intensive work can delay other tasks on that thread.
For multi-core services, run one Kimojio runtime per core and distribute
connections with an external listener or load balancer. Move CPU-intensive
handler work outside the runtime thread.
