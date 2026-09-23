# One-request Go / Kimojio HTTP/1 comparison

This is intentionally one workload, not a general web-server benchmark suite.
Two servers accept the same fixed REST-shaped POST over persistent TCP. A separate
Go process sends prebuilt HTTP/1.1 wire bytes and validates each whole response.

## Contract

- `POST /v1/quotes`, 860-byte JSON request, 3,342-byte JSON response.
- Request head: 591 bytes including request line, 11 fixture fields,
  Content-Length, and delimiters. Response head: approximately 294 bytes including
  status, eight fixture fields, Content-Length, and delimiters. Header ordering
  and capitalization are library-owned.
- Both handlers collect and byte-validate the request, then return the same
  immutable, prebuilt response. **No JSON parsing/serialization, authentication,
  database work, routing framework, compression, TLS, or HTTP/2 is timed.**
- All authorization/cookie values are non-secret dummy fixture data.
- Go: standard `net/http`, `GOMAXPROCS=1`, normal GC (`GOGC=100`, no memory limit).
- Kimojio: one default runtime thread, native HTTP/1 transport, no busy polling,
  shared immutable response body and explicit `coalesce_full_bodies=true`.
  Coalescing is not the wrapper's default; it corresponds to this known full-body
  workload and Go's ordinary buffered response write. No default-policy claim.
- Both: TCP_NODELAY on; TCP keepalive off; uncapped requests per connection;
  configured 16 KiB header-byte bound, 5-second header timeout, 30-second idle
  timeout. Body/write timers are disabled to avoid conflating different absolute
  versus progress-timeout policies. This is not a hardened production configuration.
- Kimojio's 128-connection task pool is non-binding at the tested 1/16 connections.
  Go's accept loop is not admission-capped. This test says nothing about overload
  admission or equivalence of malformed-request/resource-limit behavior.

The shared `fixtures/` files are embedded by both servers. `make_fixture.py`
regenerates them. The Date header is fixed explicitly in both implementations to
avoid an unmatched automatically generated header.

The load generator dials exactly one connection per worker before measurement.
Warmup uses those same connections. Each worker has one request outstanding; no
pipeline, retry, reconnect, or connection pool exists. Request bytes and receive
scratch space are prepared once; responses are parsed with `http.ReadResponse`
and checked for status, required headers, framing, reuse, exact length and bytes.
Reads and writes have a five-second deadline. Any failure invalidates the row and
suppresses its throughput claim. Measurement includes admitted work draining and
explicit connection cleanup. Warmup and initial connection establishment are out.

## Build and run

Only the Go standard library is required. Run from the repository root:

```sh
python3 interop/rest-bench/make_fixture.py
go -C interop/rest-bench test ./...
go -C interop/rest-bench build -trimpath -buildvcs=false -pgo=off \
  -o "$PWD/target/rest-bench/go-rest" .
CARGO_PROFILE_RELEASE_DEBUG=1 CARGO_TARGET_DIR=target/rest-bench/rust \
  cargo build --release -p kimojio-http1 --example rest_bench
cargo test -p kimojio-http1 --example rest_bench
python3 -B -m unittest discover -s interop/rest-bench -p 'test_*.py'

python3 -B interop/rest-bench/run.py \
  --output target/rest-bench/new-run \
  --server-cpu 13 --client-cpus 16,18,20,22 \
  --concurrency 1,16 --trials 3 --warmup 2 --duration 10
```

Each server is pinned to the same logical CPU, one at a time. GOMAXPROCS=1 limits
Go goroutine execution, not its OS thread count; all its threads share that CPU.
The harness rejects server/client SMT-sibling overlap. Affinity does **not** reserve
the machine or guarantee that the server's sibling is otherwise idle.

Runs alternate server order, restart the server per trial, freeze/check hashes,
and retain stdout/stderr and /proc samples. CPU counters are interpolated at the
client measurement timestamps from 50 ms samples; counters have kernel tick
precision. RSS is peak *sampled process RSS*, not live object bytes or a leak test.
Server CPU is user+system charged to that process, not all system CPU or uncharged
io_uring/softirq work. The runtime does not enable SQPOLL for this workload.

The report includes throughput, complete-exchange latency, CPU/request, sampled
RSS/threads, connection counts, and client CPU use. Latency histogram quantiles
are bucket upper bounds with about 3.125% maximum bucket width. These are
**closed-loop** results, not open-loop SLO or coordinated-omission-corrected claims.

Use a new output directory for every run. Raw histograms and samples stay under
`target/rest-bench/`; a compact publication and limitations are in
[`docs/http1-rest-comparison/README.md`](../../docs/http1-rest-comparison/README.md).
