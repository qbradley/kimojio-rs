# Comparative HTTP/1 and WebSocket tools

These tools measure complete loopback implementations, not isolated parser speed.
The initial checkpoint supplied builds and correctness smoke runs only.
The completed comparative results are in [the evidence report](../../docs/http1-fsm-evidence/PERFORMANCE.md).
Do not use smoke rates as performance results.
The parent coordinates timing and profiling periods on the shared host.

## Build and smoke

From the repository root, run:

```sh
python3 -B interop/perf/run.py build
python3 -B -m unittest discover -s interop/perf -p 'test_*.py' -v
python3 -B interop/perf/run.py smoke
python3 -B interop/perf/challenge.py
```

The build uses Go and the pinned `github.com/gorilla/websocket v1.5.3` dependency.
The dependency manifest precedes the download.
No third-party source is vendored.
Caches, scratch files, binaries, fixtures, and generated reports stay under `target/perf/`.
No command uses a system temporary directory.

The build publishes:

- `target/perf/tool-publication.json`: executable hash, source hashes, compiler, and build flags.
- `target/perf/perf-tool-SHA256`: immutable executable.
- `target/perf/reference-manifest.json`: complete reference commands and limits.

The Go flags are `-trimpath -buildvcs=false -pgo=off`.
Default Go optimization and debug symbols remain enabled.
The source hashes identify builds that precede the harness commit.

Smoke runs use 50 ms admission windows, one repetition, and no warmup.
The selected server matrix covers sixteen configurations.
The independent Python reference also checks the Go WebSocket load generator.
Negative tests reject corrupt payloads and fixed-length responses presented as chunked fixtures.

## Load generator commands

Replace `TOOL` with the published immutable executable.
Replace `PORT` with the server readiness port.

```sh
taskset -c 2,4 env GOMAXPROCS=2 TOOL http-load \
  --url http://127.0.0.1:PORT/bytes/65536 \
  --size 65536 --concurrency 16 --duration 2s

taskset -c 2,4 env GOMAXPROCS=2 TOOL http-load \
  --url http://127.0.0.1:PORT/trailers \
  --size 13 --framing trailers --concurrency 1 --duration 2s

taskset -c 2,4 env GOMAXPROCS=2 TOOL ws-load \
  --url ws://127.0.0.1:PORT/chat \
  --size 4096 --fanout 4 --duration 2s
```

Each command emits one JSON object.
Nonzero errors make `valid` false and the process exit nonzero.
An invalid run has `successes_per_second: null`, even if earlier requests succeeded.
The report retains all worker errors, attempts, successes, and actual elapsed time.

HTTP requests use `GET`.
The load generator compares every response byte with the expected payload.
Fixed payloads contain only `x`.
The fixed case requires status 200 and the exact `Content-Length`.
The trailers case requires actual chunked transfer, the exact `first\nsecond\n` body, and `X-Finished: yes`.
It does not label a fixed response as chunked.
The `chunked-echo` case sends a chunked POST and requires an exact chunked response without `Content-Length`.
It uses `/echo`, with ASCII `x` in both upload and validated response.
It requires fresh connections because the native streaming echo explicitly closes each exchange.
`--fresh` disables HTTP connection reuse.
Reports count new and reused connections.
TLS, HTTP/2, redirects, and compression are not selected.

WebSocket payloads are binary.
The first eight bytes hold a big-endian publication sequence.
The remaining bytes contain `0xa5`.
The publisher belongs to the recipient count.
A registration ping/pong precedes publication.
One publication remains outstanding until every recipient supplies the exact payload and expected sequence.
This is a single-publisher, complete-broadcast latency workload, not an unlimited pipeline.
The report counts publications, deliveries, and validated payload bytes separately.
Normal close handshakes follow the admission interval and remain inside the CPU/elapsed measurement window.

The load interval stops new work at the configured deadline.
The CPU baseline precedes the actual admission boundary.
One admission timestamp controls both request classification and request latency.
The actual elapsed denominator includes admitted work, worker completion, and explicit connection cleanup.
Each I/O operation has a five-second deadline by default.
The CLI permits at most 30 seconds of admission and 15 seconds per I/O operation.

## References and comparison limits

`http-serve --mode static --root DIR` uses Go `net/http.FileServer`.
It can use the standard sendfile path.
The native static driver instead reads file chunks through its own operations.
These are equivalent file-service outcomes, not identical data paths.

`http-serve --mode fixture` exposes `/bytes/N` and `/trailers`.
The fixed fixture allocates its complete body once.
The native wrapper supplies 16 KiB producer frames.
The chunked fixture explicitly flushes its headers and first body chunk.
Both fixtures return the same payloads and trailer.
The benchmark-only Go `/echo` route enables full duplex and flushes each copied 16 KiB chunk.
It sends the final response head before reading the upload.
It matches the native route's connection-close policy.

For server comparisons, both HTTP references close after 1000 exchanges on a connection.
This matches the native HTTP default.
`--max-requests-per-connection 0` disables the reference cap.
Client comparisons use that uncapped Go fixture for both clients.
The native client receives a cap of 1000000000.
Its normalization rejects a run if that cap is reached.
The Go client has no proactive request cap.

`ws-serve` uses Gorilla for WebSocket framing and a bounded broadcast hub.
Text and binary messages include the sender.
One hub lock establishes a common publication order.
Outgoing writers use bounded queues and 16 KiB write chunks.
Compression is disabled.
Ordinary runs have one outstanding publication and must not overflow.

The reference counts queued and in-flight messages.
Its byte caps cover payload lengths, including shared payload retention.
They exclude queue metadata, allocator capacity slack, and fixed driver reservations.
The native chat cap includes those resources.
The manifests state this difference.
The byte caps are not comparable RSS limits.
The ordinary cases stay far below both limits.
Slow-consumer cases use the common message-count cap.

Reference socket configuration matches the inspected native examples:

| Family | Server TCP_NODELAY | TCP keepalive | Requested send buffer |
| --- | --- | --- | --- |
| Static | false | disabled | OS default |
| Wrapper fixture | true | 30 s idle, 1 s interval, 30 probes | OS default |
| WebSocket | false | disabled | 65536 bytes |

The load generator sets client TCP_NODELAY to true.
Its Go dialer retains the standard Go keepalive defaults.
The client and server commands record their configuration.
Linux can adjust requested socket buffer sizes.
The table states requested values, not measured kernel capacities.

The wrapper example uses a 32-task pool.
Its `--connections` flag limits the **total lifetime accepts**, not simultaneous connections.
Do not pass `--connections 32` or `64` for a sustained fresh-connection run.
The example exits after that many accepts.
Omit the flag and use the harness process deadline.

## CPU placement and metrics

The harness reads the actual socket, core, and SMT topology before each run.
On the initial Xeon 8370C host, sibling pairs are `0,1`, `2,3`, `4,5`, and subsequent adjacent pairs.
Default placement is:

- Server: CPU 0, with `GOMAXPROCS=1` for Go.
- Load generator: CPUs 2 and 4, with `GOMAXPROCS=2`.
- Python orchestration and sampling: CPU 6.

The harness rejects overlapping physical cores, including SMT siblings.
It does not change governors, sysctls, or global CPU configuration.
CPU affinity does not reserve a shared host.
Exclusive timing still requires parent coordination.

Each measured row records:

- Full server, warmup, and client commands.
- Source/profile declarations and independently checked executable hashes.
- Compiler, kernel, platform, CPU topology, affinity, and initial load average.
- Request counts, errors, actual elapsed time, and successful throughput.
- p50, p95, and p99 from an all-observation logarithmic histogram.
- Client measurement-window CPU, separately labeled process CPU, peak RSS, context switches, final FDs, and Go memory counters.
- Server user/system CPU, RSS/high-water samples, FDs, thread counts, and per-thread context counters.
- Resident-memory samples 200 ms and 500 ms after load completion.
- Server output, client output, and sampler errors.

Histogram buckets have 32 subdivisions per power of two.
Quantiles report the bucket upper bound, with at most about 3.125% relative width.
HTTP latency ends after the complete response.
WebSocket latency ends after every recipient completes the publication.
`cpu_user_seconds` and `cpu_system_seconds` are deltas from the actual measurement baseline through explicit connection cleanup.
`elapsed_seconds` covers that same window.
The measurement timestamps identify the actual boundaries, not nominal warmup deadlines.
`process_cpu_user_seconds` and `process_cpu_system_seconds` are separate process-wide diagnostics.
They include setup and do not supply the saturation denominator.
Whole-process peak RSS and context counters retain their separate scope.
Server CPU covers the client invocation window.
`server_cpu_window_seconds` identifies its own denominator.
Warmup uses a separate client process against the same server.
HTTP measurement includes its initial connection establishment.
WebSocket measurement starts after all registrations complete and includes final connection cleanup.
HTTP cleanup closes idle connections after the load workers finish.
The standard HTTP transport does not expose a join operation for its internal background goroutines.

Server CPU counters have kernel tick granularity.
Per-thread context samples can miss threads that start and exit between samples.
Client `rusage` supplies whole-process context totals.
Post-load RSS is resident retention, not proof of live allocations or a leak.
No forced garbage collection runs before retention samples.
The client saturation warning uses measurement CPU divided by matching elapsed time and allocated cores.
It marks utilization above 85%.
It is a diagnostic warning, not proof that the server reached its limit.

## Comparative runs

After the parent authorizes an exclusive measurement window, run:

```sh
python3 -B interop/perf/run.py measure \
  --manifest target/perf/comparison-manifest.json \
  --suite all --native-client-publication target/perf/native-client-publication.json \
  --server-cpu 0 --client-cpus 2,4,6 --monitor-cpu 8 \
  --repetitions 3 --duration 2 --warmup .5 --seed 953 --archive
```

The final server matrix has 16 configurations:

- Static 128 B and 64 KiB at concurrency 1 and 16.
- Wrapper fixed 128 B and 64 KiB at concurrency 1 and 16.
- Static fresh 128 B at concurrency 16.
- The true 13-byte chunked/trailers case.
- WebSocket 128 B, 4 KiB, and 64 KiB at fanout 1 and 4.

Five additional client configurations compare native and Go clients against the same uncapped Go fixture.
They cover fixed 128 B and 64 KiB at concurrency 1 and 16, plus fresh 128 B at concurrency 16.
The full matrix contains 21 paired configurations and 126 rows across three trials.
`--suite servers` or `--suite clients` selects one part.

The separate `--suite chunked` supplement covers 64 KiB and 1 MiB POST echoes at concurrency 1 and 16.
It adds four paired configurations without changing the earlier results.
Both upload and response use real chunked transfer coding.
Use `chunked_progress.py --manifest MANIFEST` for independent, untimed response-prefix barriers before this supplement.
Those barriers require the final response head before any upload and each echoed prefix before the next upload.
Its varying-byte correctness corpus is separate from the timed ASCII `x` payloads.

The seeded schedule randomizes cases and target order within each repetition.
Each row starts a separate server.
The defaults use three repetitions, 0.5 seconds of warmup, and two seconds of admission.
There is no warmup subtraction or interpolation.
The actual completion interval supplies the throughput denominator.

Copy `reference-manifest.json` to form a comparison manifest.
Add native targets with these fields:

```json
{
  "name": "native-static",
  "kind": "static",
  "binary": "/absolute/frozen/http1-static",
  "sha256": "FULL_BINARY_SHA256",
  "source": {"integration_commit": "FULL_COMMIT"},
  "profile": "release + debuginfo=2; exact parent build flags",
  "socket_options": {"server_tcp_nodelay": false, "send_buffer": "OS default"},
  "limits": {"max_connections": 64},
  "command": [
    "/absolute/frozen/http1-static",
    "--bind", "{bind}", "--root", "{root}", "--max-connections", "64"
  ]
}
```

The outer object has `schema: 1` and a `targets` array.
Kinds are `static`, `fixture`, and `ws`.
The declared binary must match the first command argument.
The executable hash must match before and after each run.
Native release+debuginfo2 publications belong in the final manifest.
Earlier correctness artifacts are suitable for smoke checks only.

`--archive` copies the complete selected report into `docs/http1-fsm-evidence/performance/`.
The copy contains results and metadata, not only a worktree-path reference.
Generated originals remain in UUID directories under `target/perf/`.
The final comparison must use archived reports.
The selected final archives use lossless gzip compression.
Their index records both compressed and uncompressed SHA256 values.
`summarize.py` accepts either JSON or gzip JSON and retains per-trial percentile ranges.

`native_client.py` runs the canonical native executable with warmup zero.
The outer runner supplies separate 500 ms warmup and 2000 ms measurement invocations.
The adapter uses `wait4` for native process user/system CPU, peak RSS, and context switches.
Those counters cover the complete native child process.
The native measurement CPU total retains its separate request-window scope.
No user/system split is invented for that measurement total.
Native FD samples come from `/proc` while the child runs.
The last sample is not a post-exit FD count.
The adapter preserves raw native JSON and normalizes only documented fields.
Native quantile bins are unavailable, so reports use per-trial quantiles without pooling.

Client comparisons measure complete validated workloads, not isolated HTTP library or runtime efficiency.
The native client compares ASCII `x` incrementally in response frames without whole-body collection.
The Go load generator collects the response with `io.ReadAll(io.LimitReader(..., size+1))`.
It then uses `bytes.Equal` against a prebuilt expected buffer.
Go's amd64 equality implementation includes SIMD paths for large buffers.
The parent identifies a scalar native comparison loop in the frozen client profile.

These strategies impose different allocation, collection, and comparison costs.
Equal payloads and equal CPU affinity do not remove those differences.
Client throughput and CPU differences cannot be attributed entirely to the HTTP libraries.
The [performance report](../../docs/http1-fsm-evidence/PERFORMANCE.md#native-versus-go-clients) records the parent-supplied sample counts.
Exact byte comparisons and frozen clients remain unchanged during the matrix.

## Intentional aborts and slow consumers

```sh
python3 -B interop/perf/challenge.py --manifest target/perf/comparison-manifest.json
```

The resource challenge is separate from ordinary timing.
HTTP cases send nine intentional resets across partial headers, partial bodies, and stalled 1 MiB responses.
Each reset precedes an exact healthy exchange with a three-second deadline.
The descriptor bound remains warm baseline plus two.
A three-second settling window accounts for bounded transport cleanup.
Every settling sample remains in the report.

WebSocket cases send six intentional resets and a bounded slow-consumer workload.
Two healthy recipients must receive twelve exact messages.
The slow recipient must receive close 1008.
This case explicitly selects a five-second close deadline.
Its command must appear as `challenge_command` in each WebSocket target.
The reference manifest supplies the Go command.
The native command uses the matching documented limits:

```text
--max-message-bytes 65536 --max-queued-messages 2
--max-client-bytes 262144 --send-buffer-bytes 4096
--close-timeout-ms 5000
```

Reports distinguish intentional resets from unexpected errors.
They record recovery and retention, never error-free throughput.
These socket challenges do not inject deterministic kernel completion ordering.
Native cancellation-unit evidence remains separate.

## Current boundary

The parent owns `perf`, disassembly, and hotspot analysis.
This harness does not infer copy cost from source code or payload size.
The native Rust benchmark-client owner supplies the frozen CLI/JSON contract.
The final matrix uses the parent-selected canonical publication, not an older owner artifact.
Comparative measurements require parent coordination and the final publication manifest.
