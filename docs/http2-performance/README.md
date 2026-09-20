# HTTP/2 CPU qualification

This report measures the direct HTTP/2 core and its protocol selector.
It does not qualify a Kimojio wrapper or a socket executor.
The source starts at `5491242b53e60051178870d7a82fb89b29cda463`.
The final Rust harness commit is `d4bc8694c24225be5c0a6a7ddbdf3baaa387a8fd`.
The final binary SHA256 is `205f19afeaa93d7d6161a3f32dd3359098bf2805d2b0bca08c5cd8a7c077c527`.
The source hashes identify this historical binary, not later corrected-core work.
[The corrected-core follow-up](halfclosure/README.md) has separate source identity and evidence.
The historical results and profiles below remain unchanged.

## Decision

The core-to-wrapper gate remains **blocked**.
The final matrix contains 126 successful cells and 63 failed cells.
Each successful cell has five trials.
Concurrent fragmented bodies and concurrent 1MiB bodies fail in all three routes.
Two strict reproducers remain explicit ignored tests.

The successful empty-body cases show approximately constant CPU cost per exchange across concurrency.
At concurrency 128, the full-fragment medians are 2.434µs direct, 2.470µs selected, and 2.377µs auto.
Their trial ranges overlap.
These measurements do not establish a reliable small routing-overhead bound.

Cold connections cost more with detection.
The full-fragment construction medians are 7.446µs direct, 7.609µs selected, and 9.311µs auto.
The auto measurement includes both preconstructed protocol children.

[RESULTS.md](RESULTS.md) contains the medians, ranges, and blocked cells.
[COPIES.md](COPIES.md) contains the sample-supported proposals and attribution limits.
No core optimization or wrapper implementation forms part of this work.

## Workload contract

The harness compares three routes in one release binary:

| Mode | Client | Server |
|---|---|---|
| `direct` | `kimojio_fsm_http2::Client` | `kimojio_fsm_http2::Server` |
| `selected` | `http::Client::http2` | `http::Server::http2` |
| `auto` | `http::Client::http2` | `http::Server::detect` |

The executor copies pending write slices directly into the pending peer read page.
Each direction receives one transport opportunity per turn.
A pending write cannot prevent a read in the other direction.
There is no byte queue, socket, or per-transfer allocation in the executor.
The core can allocate its own storage.

The application compares every payload byte.
It also requires the exact header fields, status, payload lengths, sent receipts, receive ends, and stream retirements.
Application slots and scratch vectors remain bounded by concurrency.
Headers remain borrowed during each callback.
The application does not retain header clones or a trace of every callback.

The producer uses slices of a static 32KiB buffer.
Each permit supplies its actual byte limit.
The static buffer retains no heap capacity.
This isolates protocol CPU costs from producer allocation costs.
It does not represent an application that creates a new payload for each frame.

Most callbacks continue without an action enum.
The body callback yields so the application can release each receipt promptly.
The `paused` case retains receipts for all but one stream until the ready sibling retires.
This case measures paused receive receipts, not a general scheduler with arbitrary blocked sources.

Each connection uses the default core configuration, except `max_active_streams=128`.
The virtual time remains zero.
This excludes timer progression costs and does not bypass protocol resource limits.
The executor settles read EOF, cancellation originals, cancellation acknowledgments, alarms, and close operations after each measurement.
The teardown is outside the measured interval.

| Case | Request body | Response body |
|---|---:|---:|
| `empty` | 0 | 128 bytes |
| `duplex` | 4096 bytes | 4096 bytes |
| `32k` | 32768 bytes | 32768 bytes |
| `1m` | 1048576 bytes | 1048576 bytes |
| `paused` | 0 | 128 bytes |

The matrix covers concurrency 1, 8, 64, and 128.
Transport fragments contain at most 17, 1024, or 65536 bytes.
The largest fragment also remains bounded by the read page and the pending write.
Each batch starts a full cohort and waits for all retirements.
Thus concurrency describes a cohort, not a continuously replenished arrival process.

## Measurement method

Steady-state results exclude construction and eight warmup batches.
The `auto` route finishes detection during warmup.
Detection does not recur for every exchange.

The separate `construct` phase includes connection construction, scratch allocation, protocol startup, and the first exchange.
It excludes teardown.
It measures a cold connection, not detection alone.
The difference between `auto` and `selected` includes the preconstructed HTTP/1 child and its buffer.

The runner uses five alternating mode orders on CPU2.
All builds and harness tests use CPUs8–31.
Each successful cell has five independent process trials.
The same frozen binary supplies all three modes.
`summary.json` records the median, minimum, and maximum nanoseconds per exchange.
`trials.jsonl` retains exact arguments, counts, elapsed times, and binary hashes.
Failed cells retain the assertion and do not produce timing claims.

The release build uses `CARGO_PROFILE_RELEASE_DEBUG=2`.
`evidence/freeze.txt` records the compiler, build command, source hashes, binary hash, and machine description.
The binary remains at:

```text
/workspace/kimojio-rs/target/http2-program/build-http2-performance/frozen-d4bc869/composition_bench
```

## Gates and limits

The frozen core fails strict concurrent-body cases.
Two ignored harness tests retain bounded reproducers with all assertions intact.
They are explicit blocked gates, not passing tests.
The matrix retains further affected combinations in its error rows.
These failures are protocol/resource failures, not copy costs.

After this qualification, the core owner identified an early-response upload defect in the frozen core.
The client removed both stream halves at response END_STREAM, even while its request upload remained active.
The owner supplied correction `0d1416641d88578e02fd0ebce9f1c67f459afa66` and separate regression evidence.
This qualification did not import or measure that correction.
The failing duplex cells need another run on the corrected core.
The fixed virtual clock does not establish a control-budget defect.
No budget increase is justified by these failures.

The harness requires static payload contents and a single in-memory connection pair.
Payload comparison, transport copy, application assertions, and cohort management contribute to its CPU time.
The result is not socket throughput.
No reliable allocation probe was available in this qualification.
No allocation count or retained-memory plateau claim follows from these results.

The first harness used a scalar loop for exact byte comparison.
Its large-body profile mostly measured that loop.
The final harness compares complete slices instead.
Both versions compare every payload byte.
The final matrix and all final profiles use the new binary.
`evidence/scalar-comparison/` retains the earlier results, not a core-performance baseline.

HTTP/1 and the historical HTTP/2 codec use different workload and ownership contracts.
This report makes no performance claim against those benchmarks.
A short function or a selector branch is not evidence of low overhead.
Only equivalent successful matrix cells support route comparisons.

## Replay

From the frozen worktree, run the build:

```sh
taskset -c 8-31 env \
  CARGO_TARGET_DIR=/workspace/kimojio-rs/target/http2-program/build-http2-performance \
  CARGO_PROFILE_RELEASE_DEBUG=2 \
  cargo build -p kimojio-fsm-http2 --example composition_bench --release
```

After CPU2 becomes available, run a single cell:

```sh
taskset -c 2 /workspace/kimojio-rs/target/http2-program/build-http2-performance/frozen-d4bc869/composition_bench \
  direct empty 128 65536 512 steady
```

Run the complete matrix:

```sh
taskset -c 8-31 python3 kimojio-fsm-http2/examples/composition_bench_support/trials.py \
  /workspace/kimojio-rs/target/http2-program/build-http2-performance/frozen-d4bc869/composition_bench \
  docs/http2-performance/evidence
```

Run the harness tests and the blocked reproducers separately:

```sh
taskset -c 8-31 env CARGO_TARGET_DIR=/workspace/kimojio-rs/target/http2-program/build-http2-performance \
  cargo test -p kimojio-fsm-http2 --example composition_bench
taskset -c 8-31 env CARGO_TARGET_DIR=/workspace/kimojio-rs/target/http2-program/build-http2-performance \
  cargo test -p kimojio-fsm-http2 --example composition_bench -- --ignored
```
