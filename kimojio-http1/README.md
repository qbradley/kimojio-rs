# Kimojio HTTP/1

This crate supplies an async client and server over `kimojio-fsm-http1`.
The core owns HTTP parsing, framing, deadlines, cancellation policy, and connection reuse.
The wrapper owns application metadata, bounded channels, and native transport operations.

## Client

The application supplies an established `SplittableStream` and a unique `ConnectionId`.
`connect` returns a client handle and a caller-owned driver.
The application must poll the driver concurrently with client operations.

```rust,no_run
use kimojio::OwnedFdStream;
use kimojio_http1::{connect, Config, ConnectionId, OutgoingBody, http::Request};

async fn exchange(socket: kimojio::OwnedFd) -> Result<(), kimojio_http1::Error> {
    let config = Config::new(ConnectionId { slot: 1, generation: 1 });
    let (mut client, driver) = connect(OwnedFdStream::new(socket), config);
    let application = async {
        let request = Request::builder()
            .uri("/")
            .header("host", "example.test")
            .body(OutgoingBody::empty())
            .unwrap();
        let mut response = client.send(request).await?;
        let bytes = response.body_mut().collect(1024 * 1024).await?;
        assert!(!bytes.is_empty());
        client.shutdown().await
    };
    let (application, driver) = futures::join!(application, driver.run());
    application?;
    driver
}
```

The client admits one active exchange and at most one queued request.
`send` returns the final response head before the complete body arrives.
A dropped `send` future cancels admission or the active exchange.
The driver never sends an abandoned queued request.
The next exchange waits for the core's retirement notification.

## Server

`serve_connection(stream, config, handler)` serves one established transport.
The async handler accepts `http::Request<IncomingBody>` and returns `Result<http::Response<OutgoingBody>, Error>`.
The wrapper polls the handler alongside transport operations.
Handlers run sequentially on each connection.
The core chooses the response wire version from the request.
Handlers do not reconstruct this choice.

`serve_connection_with_shutdown` also accepts a `Shutdown` handle.
`Shutdown::graceful` stops admission and waits for the current exchange.
`Shutdown::abort` requests cancellation through the core.
The application must continue to poll the server future until shutdown completes.

## Bodies and metadata

`OutgoingBody` supports four sources:

- `empty()` supplies no body.
- `full(bytes)` owns a complete, fixed-length body.
- `from_stream(length, stream)` polls a fallible source directly.
- `from_incoming(body)` forwards data leases and trailers with streaming framing.

The direct source needs no producer task or channel.
`Some(length)` declares a fixed length.
`None` requests streaming framing from the core.
An `OutgoingFrame::Trailers` frame terminates a streaming body.
Each data frame's length and allocated capacity must fit the configured buffer limit.
`OutgoingFrame::Data(Vec<u8>)` retains its existing constructor.
`OutgoingFrame::Forward(BodyChunk)` transfers an incoming lease without a payload copy.
Both frame types can occur in the same source.
Exhaustive matches on `OutgoingFrame` need a `Forward` arm.
A large body needs several bounded frames instead of one large `full` value.
Empty data frames do not terminate a source.
The core's `source_finished` notification releases a producer that it no longer needs.
This release does not imply transport completion.
Pending writes and their receipts retain their own resources until completion.

`IncomingBody::frame` returns data leases and optional trailers.
The first call requests input credit.
This demand prevents an unwanted `100 Continue` before a handler decides to read the request body.
`IncomingBody::accept` requests and acknowledges this credit without waiting for payload.
The echo handler uses it before returning final response headers.
A `BodyChunk` exposes its bytes without a wrapper payload copy.
The chunk's destructor returns its buffer to the core.
For an admitted `Forward` frame, the outgoing receipt owns this return.
Source termination and cancellation requests do not release an outstanding write's lease.
The original write must settle before the receipt returns its lease.
`BodyChunk::retained_capacity()` reports the capacity of the complete receive allocation, not just the visible body range.
The destination rejects a lease whose retained capacity exceeds its buffer limit.
A small visible range does not bypass that limit.
A retained chunk stops further input delivery, but does not stop eligible writes.
`collect(limit)` copies data into a bounded result and ignores trailers.

For a same-connection echo, the handler uses:

```rust,no_run
use kimojio_http1::{Error, IncomingBody, OutgoingBody, http::{Request, Response}};

async fn echo(request: Request<IncomingBody>) -> Result<Response<OutgoingBody>, Error> {
    let mut incoming = request.into_body();
    incoming.accept().await?;
    Ok(Response::new(OutgoingBody::from_incoming(incoming)))
}
```

Cross-connection forwarding requires both drivers to remain polled.
The source connection cannot reuse its receive allocation until the destination returns the outgoing receipt.
A cancelled destination can drop the forwarded `IncomingBody` and cancel an unfinished source response.
The helper does not transfer HTTP headers or change the core's early-response policy.
See [lease forwarding](../docs/http1-wrapper-lab/lease-forwarding.md) for ownership, costs, and limitations.

A dropped client response body cancels its unfinished exchange.
A dropped server request body causes the wrapper to discard further body deliveries within the core's limits.
The core still decides connection reuse and early-response behavior.
This permits a handler to return an early response without retaining an unwanted request body.

The wrapper uses standard `http` metadata.
Header values retain their bytes, including non-UTF-8 values and duplicate fields.
Header names use the normalization rules of `http::HeaderMap`.
The standard metadata does not retain a custom response reason phrase.

The body declaration determines framing.
Caller headers must not contain `Content-Length` or `Transfer-Encoding`.
The client accepts a single `Expect: 100-continue` field and translates it to the core command.
Other `Expect` values return an error.
Informational responses do not reach the application API.

## Transport and cancellation

One reader and one writer operate concurrently.
Each worker owns its half and its current operation.
Neither transport future borrows the HTTP machine.
Native single-slot channels connect workers to the driver.
The wrapper creates no transport task for each frame.

A native write-all success reports the complete offered length.
A write-all error can hide partial progress.
Ordinary errors use `UnknownProgress`, which is terminal and forbids replay.
A confirmed `ECANCELED` uses `CancelledUnknownProgress`.
This distinction lets the core receive a final response after it cancels an upload.
Both error kinds report a lower bound instead of an invented exact acceptance count.

Cancellation requests target the original operation.
Each native operation has a separate `io_scope`.
The worker requests cancellation, then awaits the original result.
After cancellation, each pending poll cancels newly submitted native operations.
This covers write-all continuations after a positive partial completion.
A concurrent success remains a success.
Exchange completion does not imply complete input.
If the core retires an exchange without `incoming_finished`, its input body returns cancellation.
The driver settles reads before it explicitly closes the writer.
It then drops the read half, because `AsyncStreamRead` has no close method.

Custom transports must cooperate with Kimojio cancellation.
A transport future that waits forever outside native cancellation cannot promise bounded shutdown.
Abrupt driver destruction uses native resource destructors, not the normal async shutdown sequence.
Keeping the driver alive through shutdown is part of the API contract.

Cancellation scopes support wrapped `FuturesUnordered` wakers.
Scope cancellation and cleanup release the runtime state borrow before they invoke these wakers.
The server example still uses bounded native tasks.
See the [runtime cancellation record](../docs/http1-wrapper-lab/runtime-cancellation.md) for regression evidence and remaining limits.

## Bounds, scheduling, and costs

The core configuration bounds headers, body sizes, buffers, request counts, and deadlines.
The wrapper adds fixed channel slots and one outgoing source.
The source must bound its own retained application state.
The wrapper rejects an oversized data frame after the source returns it.
It cannot prevent the source itself from allocating excessive memory.

Input selection rotates between ready work sources.
The driver drains core notifications before it polls an outgoing source.
This prevents stale capacity from admitting source output after the core revokes it.
If the core rejects a late data frame after revocation, the wrapper drops its unadmitted buffer and producer.
The wrapper preserves the response and all previously admitted transport operations.
Each turn has a configurable progress budget.
The driver uses `kimojio::clock_now()` and `operations::sleep_until`.
It retains one timer for the current core deadline across unrelated events.
It records current time before each command or completion.
The `virtual-clock` feature preserves the runtime's virtual time domain.

The wrapper is not allocation-free.
It allocates channels, metadata, per-operation cancellation tokens, and boxed handler or body sources.
Connection-level boxes keep large native transport buffers out of caller future frames.
The native `OwnedFdStream` also copies received bytes through its own 16-KiB buffer.
These costs belong in adapter measurements, separate from core measurements.
No throughput or allocation improvement is claimed here.

## Runnable examples

Start the fixture server:

```sh
cargo run -p kimojio-http1 --example server -- --bind 127.0.0.1:0
```

The first stdout line is `LISTEN 127.0.0.1:PORT`.
`--connections N` stops admission after N accepted connections.
The example permits at most 32 concurrent connections.
Socket creation and binding are synchronous setup operations.

| Path | Response |
| --- | --- |
| `/` | `hello from kimojio-http1\n` |
| `/echo` | Request data and request trailers, streamed directly |
| `/trailers` | `first\nsecond\n`, then `x-finished: yes` |
| `/early` | Status 413 with an empty body |
| `/bytes/N` | N bytes of `x`, up to 16 MiB |

The echo fixture does not collect the complete upload or copy each chunk.
Its outgoing frame owns the incoming lease until the write settles and its receipt returns.
It can return response headers before the first upload byte arrives.
The current core conservatively closes exchanges whose response starts before the complete request arrives.
Ordinary exchanges can reuse a connection when the handler consumes the request before it responds.

Run the client against the printed address:

```sh
cargo run -p kimojio-http1 --example client -- \
  --connect 127.0.0.1:PORT --method POST --path /echo \
  --body hello --chunked
```

The client prints `STATUS`, response data, and `TRAILER` lines.
Both examples use connected transports without DNS or TLS setup inside the wrapper.

`--body-file FILE` supplies binary request data, up to 16 MiB.
The example reads this file during synchronous setup, then produces 16-KiB data frames.
`--expect-continue` requests the HTTP continue handshake.
`--count N` is an alias for `--repeat N`.

`--result-file FILE` writes one JSON record per line after each complete response.
Each record contains `status`, `body_base64`, `headers`, `trailers`, and `error`.
Each header or trailer is a `[name, base64_value]` pair.
A failed exchange produces an error record and a nonzero exit status.
The application API does not expose informational responses, so these records omit them.

## Scope

This crate does not supply pools, redirects, retries, DNS, TLS configuration, or a URL framework.
It does not expose HTTP upgrades or WebSocket APIs.
It inherits the documented protocol scope and current limitations of the standalone core.

## Checks

Run the focused checks:

```sh
timeout 60s cargo test -p kimojio-http1 --all-targets --features virtual-clock
cargo clippy -p kimojio-http1 --all-targets --all-features -- -D warnings
```

The regression suite uses native socket operations.
It covers trailers, reuse, queued-request cancellation, source failure, partial-progress errors, explicit close, and virtual deadlines.

## Benchmark client

The [`keepalive_bench`](examples/keepalive_bench.rs) example measures repeated exchanges through both wrapper endpoints on one established native socket pair.
It checks every payload byte, exact exchange counts, connection reuse, and successful shutdown.
The [benchmark contract](../docs/http1-wrapper-lab/BENCHMARK.md) describes its timing scope, comparison runner, and allocation probe.

`bench_client` uses the public client API and native transport.
It supplies no server, connection pool, automatic retry, or performance claim.
Each native worker uses one connection at a time.
The runner reads response frames without collecting the complete body.
Every successful response has status 200, exactly `--size` bytes of ASCII `x`, and no nonempty trailers.

Build an optimized binary with debug information:

```sh
CARGO_PROFILE_RELEASE_DEBUG=2 cargo build -p kimojio-http1 --example bench_client --release
```

For a short functional check against the fixture server, run:

```sh
target/release/examples/bench_client \
  --url http://127.0.0.1:PORT/bytes/4096 --size 4096 \
  --connections 1 --warmup-ms 5 --duration-ms 20 \
  --mode keepalive --timeout-ms 5000 \
  --max-requests-per-connection 1000 --json target/client-smoke.json
```

The URL requires a numeric IP address and an explicit port.
`--connections` accepts 1 through 256 workers.
`--mode fresh` opens a new connection for every request.
The request limit defaults to the core's limit of 1000.
The runner counts normal retirements and reconnects before it admits the next request.
It stops a worker after a failed request instead of replaying that request.
Unannounced peer closure races count as errors, not retries.

Request latency includes connection setup for the first request on each connection.
The request timeout includes that setup and the complete response.
The same timeout configures core head, body, and idle deadlines.
Normal shutdown awaits the driver and native operation settlement.
An emergency settlement watchdog invalidates the run and reports `forced_driver_drops`.
This watchdog expires three timeout intervals after the admission window ends.

The runner writes one JSON summary to stdout and, optionally, `--json`.
Any error or absence of measured successes causes a nonzero exit.
Invalid runs report `valid: false` and `requests_per_second: null`.
The summary contains these fields:

| Field | Meaning |
| --- | --- |
| `schema` | Schema version 1 |
| `valid` | False if any phase or cleanup reports an error |
| `attempts`, `warmup_attempts` | Successful requests plus failed request or connection attempts in each phase |
| `count`, `warmup_count` | Successful requests, classified by request start time |
| `validated_payload_bytes` | `count * size`, excluding incomplete or invalid responses |
| `warmup_elapsed_seconds` | Actual warmup interval, including timer scheduling delay |
| `errors`, `error_details` | Errors from all phases and at most eight bounded diagnostic strings |
| `measured_errors`, `warmup_errors`, `driver_errors` | Request-phase errors and driver failures |
| `elapsed_seconds`, `process_cpu_seconds` | Time from the measurement boundary through request drain and cleanup |
| `requests_per_second` | `count / elapsed_seconds`, or null for an invalid run |
| `rusage` | Explicitly unavailable here; a harness can collect process resource usage with `wait4` |
| `latency_us` | Estimated p50, p95, and p99 for successful measured requests |
| `histogram` | Bucket precision, rounding rule, and overflow count |
| `connections`, `reconnects`, `retired_connections` | Connection counters across warmup and measurement |
| `limit_retirements`, `fresh_closes` | Explicit request-limit and fresh-mode closures |
| `forced_driver_drops` | Emergency watchdog failures |
| `config` | URL, workload, deadlines, validation, and connection policy |

Each worker retains 2048 histogram counters, not an unbounded latency sample list.
The histogram has 32 sub-buckets per power of two in nanoseconds.
Percentiles use nearest ranks and exclusive bucket upper bounds, not exact sample quantiles.
The reported precision is at most 3.125% bucket width.
Input samples have one-nanosecond resolution.
Warmup requests that cross the measurement boundary remain warmup requests.
Their remaining CPU work can overlap the measured interval.
After the warmup timer resumes, the coordinator samples CPU time and publishes one actual measurement window.
All workers use that window for request classification, admission, and the settlement watchdog.
Before publication, requests remain warmup requests even if the nominal warmup deadline passed.

Build all comparison targets before measurement or profiling.
Use the same payload, concurrency, connection policy, and validation in each comparison.
For the shared Go comparison, use separate warmup and measurement invocations with `--warmup-ms 0`.
This gives both measurement clients new connections instead of comparing different warmup pool states.
The JSON configuration records TCP_NODELAY and the native TCP keepalive configuration.
