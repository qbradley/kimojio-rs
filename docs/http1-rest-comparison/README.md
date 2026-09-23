# Simple Go / kimojio-http1 TCP result

## What was compared

One fixed REST-shaped exchange: `POST /v1/quotes`, **860 B request JSON and
3,342 B response JSON**, with about **591 B request headers and 294 B response
headers/status lines**. The handler collects and checks the request and returns
an immutable prebuilt response. This measures HTTP serving and body handling,
not JSON serialization, authentication, business logic, or database performance.

Production Kimojio source starts at `c7bc7c0a`; this comparison adds only fixtures,
a server example, the independent load process, tests, and orchestration. The
Go server is standard-library net/http with GOMAXPROCS=1 and normal GC. Kimojio
uses one runtime thread and the native HTTP/1 wrapper. Both are pinned to CPU13,
one server at a time. Go had five sampled OS threads, Kimojio one; every Go thread
was restricted to the same server CPU, so this does not imply five-way execution.

The [benchmark contract and commands](../../interop/rest-bench/README.md) specify
socket settings, timeout choices, shared response storage, and validation. In
particular, Kimojio explicitly enables full-body coalescing for this workload;
these are **not** results for its default separate-head/body configuration.

## Method

- Linux 6.6.137.mshv2-2.azl3, Xeon Platinum 8370C VM.
- Go 1.27.1; Rust 1.98.1. Optimized builds, no profiler or allocator instrumentation.
- Same fixed request bytes, same response validation, and same generator for both.
- One owned TCP connection per load worker, created and warmed before measurement.
- Two operating points for the *same workload*: 1 and 16 connections, one request
  outstanding per connection. No retries, reconnects, or pipelining.
- Two-second warmup; ten-second admission interval plus drain/cleanup; three trials
  per server/connection level. Server order alternates between trials.
- Client pinned to four separate physical cores (16,18,20,22), GOMAXPROCS=4.
- Shared host, not exclusively reserved. CPU affinity does not eliminate sibling,
  hypervisor, or other host interference. No remote NIC, TLS, or overload test.

Every measured row completed with valid status, headers, framing, body equality,
and exactly the original warmed connections. Errors and reconnections were zero.
Raw outputs/samples/histograms are retained in `target/rest-bench/wire-measured/`.
The compact `results.json` keeps each result/command/hash and histogram observation
count; it omits the large all-bucket arrays, which remain in the raw reports.

## Results

Medians of three runs. Ranges are trial min/max, not confidence intervals.

| Connections | Server | Requests/s | Trial RPS range | p50 | p99 | Server CPU us/request | Peak sampled RSS |
| ---: | --- | ---: | --- | ---: | ---: | ---: | ---: |
| 1 | Go | 15,247 | 14,594–15,266 | 63.5 us | 112.6 us | 45.5 | 21.3 MiB |
| 1 | Kimojio | 16,343 | 15,669–16,448 | 62.5 us | 86.0 us | 32.7 | 2.5 MiB |
| 16 | Go | 35,635 | 35,481–35,757 | 442.4 us | 1,114.1 us | 28.0 | 21.4 MiB |
| 16 | Kimojio | 63,781 | 63,210–64,353 | 237.6 us | 442.4 us | 15.6 | 3.0 MiB |

At 16 connections, Kimojio delivered about **79% higher throughput** and used
about **44% less measured server-process CPU per completed request**. Both servers
were near 100% of their assigned CPU. These are stack/workload results—not proof
that io_uring alone is 79% faster than Go's networking runtime.

At one connection, neither server was saturated: approximately 69% server CPU
for Go and 54% for Kimojio. Throughput was about 7% higher for Kimojio; this point
is dominated more by complete request/response latency than maximum capacity.

RSS includes each runtime, allocator, code, and caches; it is not a comparison of
live application payload memory. Server CPU includes user and system CPU charged
to the process; it does not capture all kernel offload or client-side network work.
CPU estimates use 50 ms /proc samples interpolated to the client measurement
boundaries, with kernel tick granularity. Quantiles are histogram upper bounds,
not exact order statistics.

## Load-generator check and a rejected initial setup

An initial net/http Transport-based generator constructed a request per operation
and read responses into fresh buffers. It reached about 34k Go / 45k Kimojio RPS
at 16 connections, but did not saturate the Kimojio server. Increasing generator
cores did not fix it. Those exploratory numbers are **superseded**, not selected
as the final comparison. Their artifacts remain under `target/rest-bench/measured/`
and `target/rest-bench/headroom/` with the original executable hash.

The retained generator is simpler: prebuilt fixed wire request, one TCP connection
per worker, bounded reusable read storage, standard response parser, full byte
validation. It cannot retry or reconnect. Server implementations and fixture
semantics did not change for that correction.

In the primary final run the client consumed about 18% (Go server) / 36% (Kimojio
server) of its four allocated CPUs. An additional eight-core generator run gave
36,375 Go / 63,092 Kimojio RPS with both servers still near full CPU. That is close
to the primary result and supports adequate generator headroom at 16 connections.
It is a single diagnostic repeat, not another statistically independent headline.
See `headroom.json` and `target/rest-bench/wire-headroom/`.

## Checks and limits

- Go handler tests reject wrong method, headers, length, and payload.
- Go client tests verify reuse and suppress throughput for corrupt responses;
  `go test -race` and `go vet` passed.
- Rust fixture tests exercise valid/corrupt requests and the complete shared
  response through both endpoints of a real native socket pair.
- Python tests reject errors, reconnects, invalid rate denominators/nonfinite
  values, and unbracketed CPU samples.
- Rust example Clippy with warnings denied passed.

This is a useful first real-TCP result for a medium REST-like message, not a
universal server ranking. It deliberately omits JSON/application computation,
TLS, file I/O, remote-network effects, high connection counts, open-loop tail
latency, and hostile/slow-client behavior. Header and body timeout policies are
matched only for this success-path fixture; body/write timers are off in both
implementations. Reproduce on an isolated machine before using the percentages
as a product performance claim.
