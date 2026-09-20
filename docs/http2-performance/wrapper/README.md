# HTTP/2 runtime wrapper benchmark

## Status

This checkpoint supplies a working harness and bounded smoke evidence.
It does not qualify wrapper performance.
All commands used CPUs 8–31. CPU2 was not used, and no timing lease was taken.
There were no profiles, optimization changes, or statistical trials.

The frozen integration is `c8f6014aa3128279690834f38a82373d3d31e4c2`.
It contains wrapper revision `d8ca94b6c6ba8439085609b289ff1ddbd6698357`.
The final harness source is `257c12eea0d4b75e7c74a779c3d4c94f1ffe361e`.
External boundary tests, review, and independent peer qualification remain separate gates.

The code commits are:

| Commit | Change |
| --- | --- |
| `98fd6f7f` | Runtime/socket harness, allocation probe, and controls |
| `47522ee4` | Separate minimal manifest registration |
| `257c12ee` | Cargo `--bench` and `--test` compatibility |

No production code or dependency changed.
The worktree is `target/http2-program/worktrees/http2-wrapper-performance`.
The private build directory is `target/http2-program/build-http2-wrapper-performance`.

## Actual workload

Both backends use one real UNIX stream socketpair and the actual runtime.
Native mode uses `connect_native` and `serve_connection_native_with_shutdown`.
Generic mode uses `connect`, `OwnedFdStream`, and `serve_connection_with_shutdown`.
There is no simulated framing, transfer completion, or receipt counter.

Each cohort contains 1, 8, 32, or 128 concurrent requests.
All cohorts reuse the same connection.
Each request uses POST, HTTP/2, an explicit content length, and a slot-specific URI.
Each response uses status 200, HTTP/2, and an explicit content length.

| Case | Request | Response | Application behavior |
| --- | --- | --- | --- |
| `empty` | 0 bytes | 128 bytes | Ready bodies |
| `fixed` | 4096 bytes | 4096 bytes | Ready bodies |
| `stream` | 1–1,048,576 bytes | Same size | Independent, bounded producers |
| `duplex` | More than one chunk | Same size | Independent producers plus the causal overlap gate |

Static payloads reuse two 16KiB patterns.
Each owned-producer poll allocates at most one 16KiB chunk.
The wrapper can retain multiple chunks under its configured bounds.
The harness never collects a large message into one application buffer.
Every received byte must match the expected pattern, offset, and length.
The patterns repeat across streams.

For streaming cases, each server handler starts one request-drain task and returns an independent response.
The harness joins every drain task.
These application tasks are part of the workload cost.
Concurrency greater than one also includes bounded `try_join_all` allocations.

The slot vector and pending-drain vector have capacity proportional to concurrency.
The harness reuses them between cohorts.
It does not retain a history of response headers or payloads.
This does not establish bounded retention inside the wrapper or runtime.

### Completion and close

Response headers do not establish successful upload completion.
Receive EOF does not establish full retirement.
The harness waits for both endpoints' actual `IncomingBody::retirement()` reports.

Every successful report must contain:

- The matching stream ID.
- `StreamOutcome::Complete`.
- Receive outcome `Some(StreamOutcome::Complete)`.
- No send failure.
- No contextual error.

The harness also requires a strictly newer stream ID for each slot in the next cohort.
It releases all `BodyChunk` values before retirement.
It does not await server retirement before it returns the response.
That ordering can deadlock because retirement includes the response half.

After the final cohort, the client requests graceful shutdown.
The harness continues both drivers until their actual `run` futures return successfully.
The peer closure path completes the server.
Only then does the harness emit successful JSON.
The public API does not expose successful per-buffer write receipts.
The harness makes no separate receipt-count claim.

On application failure, the harness requests both aborts and releases parked producers.
It continues both drivers and joins pending drain tasks.
The watchdog also requests aborts and permits five seconds for driver settlement.
A timeout always invalidates the run, including successful abort settlement.
The smoke runner adds a separate 75-second process limit.
It records failure and stops at the first failed cell.

### Causal overlap

In `duplex`, each upload producer supplies one chunk, then waits.
Only actual response payload delivery to the client opens that stream's gate.
At that point, the remaining request bytes do not yet exist in the producer output.
Those bytes therefore cannot already have reached the socket.

Every duplex stream must satisfy this condition.
Permits, source admission, response headers, and delayed completion notices cannot satisfy the condition.
This deliberately gated workload does not measure natural scheduler fairness.
The ungated `stream` case also records early-delivery witnesses.
An absent witness does not prove absent wire overlap.

### Configuration

The harness sets `max_queued_requests` to the selected concurrency.
It sets the protocol active-stream bound to 128.
All other bounds remain unchanged.
In particular, it does not increase the default receive-retention bound.
The configuration supports C128 without overflow of the default 64-item application queue.
It does not exercise metadata-admission pressure or a peer concurrency limit of zero.

## Measurement boundaries

`cold` includes socket creation, connection construction, one complete cohort, and actual driver close.
It includes the HTTP/2 preface and settings exchange.
It does not include TCP connection establishment, DNS, or TLS.
It excludes runtime initialization, argument parsing, and final JSON output.
The options `Rc` remains live for report construction.
The process runtime and its retained storage also remain live at the cold boundary.
Cold live bytes therefore do not represent a post-runtime-shutdown leak test.

`steady` excludes connection construction, warmup, and close.
Its end boundary follows both endpoint retirements and all application drain-task joins.
Close must still succeed before the result becomes valid.
The smoke matrix uses one warmup cohort and two measured cohorts.
The word `steady` names this boundary, not an established memory or latency plateau.

The runtime executable records wall time and process CPU time.
Process CPU time does not include all external kernel-worker costs.
The raw smoke durations are incidental and are not performance results.

## Separate allocation probe

Only `allocations` installs the counting allocator.
The runtime executable uses the normal allocator.
The probe counts successful allocations, successful reallocations, deallocations, and requested live/peak bytes.

Allocation origin has three polling contexts:

1. Runtime and unattributed wrapper workers.
2. Application-facing polls.
3. Connection-driver polls.

These contexts are not exact source-code ownership.
Application-facing polls include wrapper APIs and channels that the application calls.
Separate wrapper worker tasks can appear in the first context.
The first context is not a pure runtime baseline.

A private allocation prefix preserves the original context through reallocation and free.
The accounting runs continuously across measurement boundaries.
Freeing a pre-window allocation reduces its original context's live bytes.
The probe never resets live bytes at the start of a window.
It reports simultaneous aggregate peak bytes separately from per-context peaks.

Requested bytes exclude the tracking header, alignment padding, allocator rounding, kernel socket buffers, and RSS.
The prefix changes allocator layouts and can change size classes.
Probe durations are therefore not timing evidence.
The static producer case excludes application payload allocation by construction.

Three deterministic controls cover:

- Aligned zero allocation, preserved bytes, grow, shrink, failed realloc, and original-context retention.
- A pre-window free followed by allocation and free inside the window.
- Disjoint context peaks that must not become a false simultaneous peak.

## Evidence

The final smoke matrix passed **64/64** runs: 40 runtime and 24 allocation runs.
Both backends passed small messages and 1MiB streaming at C1/C8/C32/C128.
The matrix also covers cold duplex, owned chunks, and 32KiB streaming.

The successful totals are:

| Item | Count |
| --- | ---: |
| Retirements at each endpoint | 6,602 |
| Actual successful driver closes | 128 |
| Compared payload bytes | 5,183,356,672 |
| Required actual-delivery overlap witnesses | 1,448 |

Seven harness controls passed in release mode, both with default features and all features.
They include corrupt-payload negative controls on both socket backends.
Both required clippy modes passed with `-D warnings`.
`cargo fmt --all` passed.
Standard Cargo bench and bench-test entry points also passed.
An initial Cargo entry-point failure exposed its extra `--bench` argument.
Commit `257c12ee` corrected that parser error.

The files in `evidence/` contain the exact commands, reports, compiler identity, and source hashes.
`smoke.jsonl` contains each command and its complete JSON result.
`allocations.csv` contains allocation totals and the three origin counts.
`summary.json` contains aggregate correctness totals.
`initial-47522ee4/` preserves the first successful smoke freeze before the Cargo parser correction.
Neither smoke freeze is a statistical comparison.

### Open retention question

Requested live bytes increased during the short warmed windows.
The probe does not establish a plateau or attribute the retained allocations.
Successful stream retirement and driver close do not prove release of all runtime storage.

These rows use duplex 1MiB messages, C8, and static producers:

| Backend / boundary | Alloc / realloc / free | Start bytes | End bytes | Peak bytes |
| --- | --- | ---: | ---: | ---: |
| Native cold, one cohort | 17,218 / 175 / 13,113 | 33,292 | 624,252 | 1,773,878 |
| Generic cold, one cohort | 18,457 / 146 / 18,443 | 33,293 | 35,173 | 1,430,725 |
| Native warmed, two cohorts | 35,230 / 202 / 7,366 | 1,691,064 | 4,549,080 | 4,659,516 |
| Generic warmed, two cohorts | 33,244 / 202 / 15,639 | 1,296,845 | 2,892,885 | 3,018,969 |

In the warmed native row, origin allocation counts are 2,224 / 2,484 / 30,522.
In the warmed generic row, they are 2,224 / 2,482 / 28,538.
The order matches the three polling contexts.
This identifies a context, not the responsible retained data structure.

Owned chunks add exactly 2,048 application-origin allocations and frees in each corresponding warmed row.
That count matches 16 exchanges × two directions × 64 chunks.
The owned and static rows have the same requested live bytes at both window boundaries.

The retention observation went to the wrapper owner.
A future bounded investigation must separate harness, runtime, wrapper, and protocol retention.
No leak, bounded-memory result, cap increase, or optimization follows from this smoke evidence.
Long statistical runs must wait for this question and the external correctness gates.

## Comparison limits and next matrix

The HTTP/1 `keepalive_bench.rs` supplied prior art for real socketpairs and runtime entry.
Its buffering, body lifetime, and retirement boundaries differ.
These results do not establish an HTTP/1 comparison.

The earlier direct-core benchmark has no runtime, sockets, or kernel transport.
This harness also adds slot-specific URIs, conventional HTTP objects, task scheduling, and different HPACK work.
Subtracting direct-core time from this workload cannot isolate pure wrapper overhead.
Native versus generic comparisons can use the same harness semantics after the final source freeze.

After the parent accepts the final source and grants CPU2, the proposed matrix is:

| Axis | Values |
| --- | --- |
| Backend | Native, generic |
| Body case | Empty, fixed 4KiB, stream 32KiB, stream 1MiB, gated duplex 1MiB |
| Concurrency | 1, 8, 32, 128 |
| Boundary | Cold, warmed cohorts |
| Primary producer | Static, 16KiB chunks |
| Sensitivity rows | Owned chunks and 1KiB chunks for representative large-body cells |

The primary table has 80 cells.
Each cell needs at least five alternating backend trials.
Warmup and cohort counts need a stable-duration pilot on the final source.
Allocation windows need separate cold and warmed evidence on that same source.
No failed cell can disappear from the table.

Profiles require the retained release binary, debug level 2, and separate `cpu-clock` runs.
The report must split client, server, and shared stacks.
Any copy proposal needs samples near the actual machine-code site.
There is no copy proposal in this checkpoint.
There is no extra runtime baseline yet.

## Replay

From this worktree, build the two executables:

```sh
export CARGO_TARGET_DIR=/workspace/kimojio-rs/target/http2-program/build-http2-wrapper-performance
export CARGO_PROFILE_RELEASE_DEBUG=2
taskset -c 8-31 cargo build --release -p kimojio-http2 --bench runtime --bench allocations
```

Run the harness controls:

```sh
taskset -c 8-31 cargo test --release -p kimojio-http2 --bench harness_tests
taskset -c 8-31 cargo test --release -p kimojio-http2 --bench harness_tests --all-features
taskset -c 8-31 cargo clippy -p kimojio-http2 -- -D warnings
taskset -c 8-31 cargo clippy -p kimojio-http2 --all-targets --all-features -- -D warnings
```

Run one bounded Cargo smoke case:

```sh
taskset -c 8-31 cargo bench -p kimojio-http2 --bench runtime -- \
  --backend generic --case duplex --bytes 1048576 --chunk 16384 \
  --concurrency 8 --warmup 1 --cohorts 2
```

Replay the frozen matrix to a new evidence path:

```sh
taskset -c 8-31 python3 docs/http2-performance/wrapper/smoke.py \
  --binaries "$CARGO_TARGET_DIR/frozen-257c12ee" \
  --output docs/http2-performance/wrapper/evidence/smoke-replay.jsonl
```

The runner refuses to overwrite an existing output.
The frozen binary paths and SHA256 values are in `evidence/freeze.json`.

| Artifact | SHA256 |
| --- | --- |
| Runtime binary | `e1bc934da530eb6084d028d64aa7bbd1b336c1fb08389d2c83a52d0455d4b005` |
| Allocation binary | `d997467683864b8220dc1b8d07c82a595cddafaddd2a2feb1119aabb452b48d8` |
| Source manifest | `0ae1436d1d5a9c1bee3c92e1648f3331abfa4e909613b21f2cd274de92e809e5` |
