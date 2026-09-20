# Runtime HTTP/2 wrapper measurements

## Decision

The measured baseline passed all 80 cells and all five trial groups per cell.
There were **no workload failures**. No production source changed during measurement.
Native and generic wrappers both completed full payload comparisons, both endpoint retirements, and real driver close.
The gated duplex cases also required actual response payload delivery before the upload tail could be produced.

At concurrency 8, native/generic medians were:

- Empty request and 128-byte response: **25.55 / 28.93 µs per exchange**.
- Gated 1 MiB in each direction: **1287.24 / 1556.01 µs per exchange**.
- Gated payload throughput: **1554 / 1285 MiB/s**, counting both directions.

These are real local-socket workload costs, **not pure wrapper overhead**.
The normal allocator binary has no allocation instrumentation.
The separate allocation executable reproduced bounded retention on the same source.

The first proposed copy experiment is an empty-buffer, large-destination fast path in `OwnedFdStreamRead`.
It could avoid the generic buffered-read copy and reduce frame assembly copies.
Those two sites account for 313/4356 user-CPU samples in the normal generic duplex profile.
This is a measured target, not a promised improvement.
The next candidate is inline storage for the common single reverse scope membership.
See [profiles.md](profiles.md) for stacks, sites, ownership requirements, and tests.
**No optimization is implemented or accepted in this report.**

## Frozen source and binaries

| Item | Identity |
|---|---|
| Build source | `6e00374698d40dedebd33b87d3fd183ebb0e2682` |
| Runtime repair | `a4af94fd97c986877f1ba23188d923dbe31fae2d`, applied byte-for-byte by `752381ee` |
| Wrapper source | `d8ca94b6c6ba8439085609b289ff1ddbd6698357` |
| Source manifest SHA256 | `67e6a7f31d52e8d7e62db319089d114ac6ecb83d66fbe586503278dc8be497a8` |
| Normal `runtime` SHA256 | `0a362f7029ead27d25b1046df1ae0cdd4838b87f5cfc275c8f042096b7ec00ad` |
| Separate `allocations` SHA256 | `c62c7f0d2755f40824aa45850e5ffc00e9f1fc21e1ff841def21f400e16602c8` |

The binaries are retained at:

```text
/workspace/kimojio-rs/target/http2-program/build-http2-wrapper-performance/measured-6e003746/
```

`freeze.json` records build flags, paths, compiler, and source identity.
`source-sha256.txt` records the source files; `source-match.txt` records the final match.
The later report commit contains only this new evidence directory.
Its revision is not the revision that produced the binary.

Build command, from the isolated wrapper worktree:

```sh
CARGO_TARGET_DIR=/workspace/kimojio-rs/target/http2-program/build-http2-wrapper-performance \
CARGO_PROFILE_RELEASE_DEBUG=2 taskset -c 8-31 \
cargo build --release -p kimojio-http2 --bench runtime --bench allocations
```

The compiler was Rust 1.98.1, LLVM 22.1.8, x86_64 Linux.
Normal `RUSTFLAGS` and `CARGO_ENCODED_RUSTFLAGS` were empty.
Timing and profiles used the exclusive CPU2 lease.
Builds, allocation probes, and analysis used CPUs 8–31.
The host reports CPU2's SMT sibling as CPU3, not CPU18.
No governor, turbo, or host policy changed.
Machine details are in `cpu.json`, `cpu2-siblings.txt`, and the `perf-*` metadata.

## Workload and normalization

Both backends use one real UNIX `SOCK_STREAM` socketpair per process.
Native uses the native wrapper; generic uses the generic wrapper with `OwnedFdStream`.
Both endpoints share one runtime thread and the same application workload.
This excludes TCP connect/listen, DNS, TLS, a NIC, and a remote peer.

The 80 cells are:

| Dimension | Values |
|---|---|
| Backend | native, generic |
| Workload | empty/128-byte response, fixed 4 KiB each way, streamed 32 KiB each way, streamed 1 MiB each way, gated duplex 1 MiB each way |
| Concurrency | 1, 8, 32, 128 |
| Phase | cold, warmed steady |

Cold time starts before socket construction and ends after both drivers close.
It excludes runtime initialization, argument parsing, and JSON output.
Steady time includes completed cohorts and both endpoint retirements.
It excludes initial connection setup, warmup, and close; successful close still gates the result.

Each exchange includes full byte comparisons and status/stream/outcome assertions.
Streaming includes bounded producers, application drain tasks, and task fan-in.
The static producer repeats a 16 KiB pattern; it does not allocate a whole large body.
These application and assertion costs are deliberately included.
URI paths vary by concurrency slot, so this is not the old in-memory core HPACK workload.

The gated case parks each upload after its first chunk.
Only a nonempty response delivery releases its tail.
This proves actual bidirectional progress, but it is not a test of ungated scheduler fairness.
The ordinary streamed case has no artificial overlap gate.

`max_queued_requests` equals concurrency and `max_active_streams` is 128.
Other protocol/runtime limits remain unchanged.
In particular, the wrapper retains its 128-stream bound, 8 MiB queued-storage bound,
128 KiB pending-trailer bound, 256 KiB pending-response bound, and 64-step turn budget.
Core defaults include 8 MiB receive/send capacities, 256 KiB receive capacity per stream,
128 fragments per stream, 64 KiB per send buffer, 512 outbound items, and 2 MiB outbound capacity.
The initial stream/connection receive windows are 65,535 bytes / 1 MiB.
The core turn budget is 128. Closed-stream tombstones have a logical bound of 1024.
This matrix does not replace malformed-peer, hard-abort, alarm-failure, or admission-pressure tests.

**Steady wall time per exchange is inverse throughput, not individual request latency.**
Average cohort completion time is also provided.
Cold rows additionally provide individual connection/cohort p50 and p95 observations.
Large cold cells have only five observations; they cannot establish reliable tail latency.

## Trials and uncertainty

`run.py` retains pilot selection, commands, results, and failures.
The same cohort counts apply to native and generic.
Five trial groups alternate backend order; alternate groups also reverse cell order.
The pilot targets at least 250 ms per steady window.
Final steady windows ranged from **243.8 to 444.5 ms**.

Small and 32 KiB cases use `max(32, ceil(2048/concurrency))` warmup cohorts.
Large cases use eight.
Large, low-concurrency warmup does not fill all 1024 tombstones.
Small bounded pool/tombstone growth can therefore remain inside those steady windows.
No claim of complete allocator equilibrium is made for every cell.

Cold trial groups aggregate independent fresh processes, with 1–31 processes per group.
This targets about 10 ms of measured cold work per group.
Group results use total measured time divided by total completed exchanges.
The primary matrix contains:

- 400 trial groups and 2030 successful process runs;
- 828,110 measured exchanges;
- 1,162,350 retirements per endpoint, including warmup;
- 118,846,475,520 compared payload bytes, including warmup;
- 23,800 required overlap witnesses.

`matrix-summary.csv` contains all 80 cells: median, min/max, quartiles, CPU cost, throughput, and cohort completion.
`paired-ratios.json` contains all 40 paired backend comparisons.
Intervals use the exact `5^5` percentile bootstrap of trial-group medians.
With only five groups, these intervals are descriptive, not strong guarantees.

Paired generic/native wall medians range from **1.090 to 1.305** across all cells.
Thirty-four of the 40 bootstrap intervals are wholly above 1.
The other six include 1; a win is not established for each individual cell.
The largest steady max/min spread was 1.49×.
VM variability is visible and was not trimmed away.

### Representative warmed results, concurrency 8

Times are µs per completed exchange. The range is the five trial-group min/max.
Throughput counts request plus response payload, not framing bytes.

| Workload | Backend | Wall median [range] | CPU median | Exchanges/s | Payload MiB/s |
|---|---|---:|---:|---:|---:|
| empty | native | 25.55 [25.32, 35.22] | 25.54 | 39,146 | 4.78 |
| empty | generic | 28.93 [28.69, 40.50] | 28.93 | 34,562 | 4.22 |
| fixed4k | native | 35.45 [35.26, 38.08] | 35.45 | 28,207 | 220 |
| fixed4k | generic | 40.96 [40.42, 43.99] | 40.96 | 24,414 | 191 |
| stream32k | native | 72.43 [72.03, 95.69] | 72.43 | 13,806 | 863 |
| stream32k | generic | 84.76 [84.26, 89.44] | 84.76 | 11,798 | 737 |
| stream1m | native | 1297.77 [1284.72, 1693.93] | 1297.75 | 771 | 1541 |
| stream1m | generic | 1567.41 [1547.08, 1652.53] | 1567.31 | 638 | 1276 |
| duplex1m | native | 1287.24 [1272.28, 1402.04] | 1287.22 | 777 | 1554 |
| duplex1m | generic | 1556.01 [1543.85, 1670.58] | 1555.95 | 643 | 1285 |

At C1/C128, empty native costs were 32.82/26.85 µs and generic costs were 35.77/28.75 µs.
For gated duplex, native costs were 1427.83/1325.51 µs and generic costs were 1821.55/1644.53 µs.
Concurrency improves small-message amortization; it does not provide 128 independent CPU cores.

At C8, cold empty connection/cohort p50 was 0.550/0.599 ms native/generic.
Cold gated-duplex p50 was 10.785/13.382 ms for eight exchanges.
Steady gated-duplex average cohort completion was 10.298/12.448 ms.
These differently scoped numbers must not be subtracted to invent construction-only costs.

### Measurement correction

The first runner set `RLIMIT_CPU=(100,105)`.
On this host, 1288/1920 initial process runs then reported zero process-CPU deltas.
All zero values were in cold cells.
Ten capped/uncapped controls reproduced zero versus sensible clock deltas.
`cpu-clock-control.jsonl` preserves this evidence; no kernel root cause is asserted.

The entire pilot and matrix were repeated without a CPU-time limit.
The final timing runner requires an unlimited inherited CPU limit.
It retains a 1 GiB address-space cap, a 90-second workload timeout, and a 110-second external wall timeout.
Every primary process-CPU delta is positive.
The original `*-cpu-limit.*` data remains preserved but is **not primary timing evidence**.
Allocation runs retain their CPU caps because their timings are not evidence.

### Sensitivities

Thirty additional C8 gated-duplex runs used five alternating trials per variant/backend.
`sensitivity-summary.json` contains all values and paired intervals.

| Variant | Native median ms/exchange | Generic median ms/exchange | Paired ratio to each backend's static16k |
|---|---:|---:|---|
| static16k | 1.349 | 1.625 | 1.000 / 1.000 |
| owned16k | 1.439 | 1.671 | 1.076 / 1.030 |
| static1k | 17.342 | 21.298 | 12.655 / 12.769 |

Owned mode allocates only one bounded producer chunk at a time.
Its paired bootstrap intervals are [1.031, 1.253] native and [0.756, 1.050] generic.
The generic allocation penalty is not resolved by these noisy trials.
The 1 KiB result is a clear frame/operation-rate cost, not proof that copies alone dominate.
Those runs use 32 measured exchanges per trial rather than 200.
They still retain the same payload, retirement, and overlap assertions.

Half/default/double warmup controls also showed VM variability without a clean monotonic benefit.
Their raw results are preserved; no outlier filtering was used.

## Allocation and retained-storage results

All 80 allocation cells passed in separate processes on the same frozen source.
`allocation-summary.csv` contains successful allocation/reallocation/deallocation counts and requested-byte peaks.
Each allocation retains its original origin across realloc/free.
The live counters are never reset at a measurement boundary.
Freeing a pre-window allocation can correctly produce a negative live-byte change.

Origin labels describe **poll context**, not exact component ownership.
Application-facing polls can call wrapper code; detached wrapper workers can appear in the runtime/unattributed bucket.
Exact ownership comes from the earlier stack-ledger report and fresh profile/site evidence, not origin totals alone.
Per-origin peaks are not simultaneous and must not be added.

The counter excludes allocator rounding and allocator metadata, tracking prefixes, alignment padding, static producer arrays,
kernel/socket storage, stacks, and untracked system allocations.
It measures requested Rust heap bytes, **not RSS**.
Instrumented timing values are not used.

### C8 allocation observations

Steady values cover four measured cohorts for small/32 KiB cases and two for 1 MiB cases.
Cold values include one cohort and actual driver close.

| Workload | Cold allocations/exchange N/G | Steady allocations/exchange N/G | Steady reallocations N/G |
|---|---:|---:|---:|
| empty | 130.625 / 148.125 | 99.344 / 116.125 | 0 / 0 |
| fixed4k | 157.375 / 180.375 | 126.688 / 147.656 | 0 / 0 |
| stream32k | 231.625 / 283.000 | 206.625 / 252.563 | 0 / 0 |
| stream1m | 3099.375 / 4334.375 | 3049.500 / 3828.875 | 2 / 2 |
| duplex1m | 2926.000 / 4263.000 | 3051.438 / 3844.625 | 2 / 2 |

The two large-case reallocations add 512 requested bytes at C8.
They agree with bounded tombstone queue growth after only eight warmup cohorts.
They are not evidence of renewed operation-history retention.

C8 empty steady requested peaks were 294,868 / 374,684 bytes.
C8 gated-duplex steady peaks were 341,136 / 552,701 bytes.
The largest requested peak among all 80 probes was **2,323,215 bytes**: generic stream1m, C128, steady.
This is an observation under these fixed bounds, not a universal process memory bound.

The prior matched repair experiment found allocation counts at 32 duplex cohorts changed:

- native: 556,667 → 767,418, about **38% more**;
- generic: 546,854 → 1,010,075, about **85% more**.

The repair exchanges unbounded connection-history storage for live registries and reverse membership bookkeeping.
The current profiles measure hashing, waiter allocation, membership vector growth, and retirement costs.
No before/after CPU slowdown percentage is claimed: the old unbounded implementation is not a valid steady baseline.

### Fresh same-source retention sanity

Both backends repeated empty C8 through 4096 cohorts: 32,768 retired streams per endpoint.
These cheaper cohorts establish long-lived storage behavior; they are not 4096 large-body cohorts.

| Stage | Native requested live bytes | Generic requested live bytes |
|---|---:|---:|
| connection live, cohort 256 | 258,314 | 338,451 |
| connection live, cohort 1024 | 258,314 | 338,451 |
| connection live, cohort 4096 | 258,314 | 338,451 |
| after both drivers close | 37,971 | 37,972 |
| after runtime cleanup | 1,572 | 1,572 |

The last 1572 bytes are the previously attributed 548 pre-trace process bytes plus the 1024-byte stdout buffer.
Application and driver origin live counts are zero after runtime cleanup.
The fresh data reproduces the accepted plateau exactly.
See `retention-sanity/` and the immutable [repair report](../retention-candidate-a4af94fd/README.md).
That report attributes the two 18,448-byte tombstone maps, two 8192-byte FIFO allocations,
receive pages, live registries, and bounded completion pool.
The runtime completion pool's configured bound remains 4096 records; this workload does not fill that bound.

## Evidence, replay, and limits

`compressed-evidence.json` records compressed and original SHA256 values for large evidence files and selected raw tool logs.
The summary readers accept either plain files or their `.gz` versions.
Private perf data and binaries remain intact; `private-profile-artifacts.json` records their paths and hashes.
Do not overwrite an existing freeze when replaying.

Representative direct replay, with a new exclusive timing lease:

```sh
taskset -c 2 /workspace/kimojio-rs/target/http2-program/build-http2-wrapper-performance/measured-6e003746/runtime \
  --backend native --case duplex --bytes 1048576 --concurrency 8 \
  --phase steady --cohorts 25 --warmup 8 --timeout-seconds 90
```

Use `generic` for the paired backend.
The exact full-matrix commands and selected counts are in `matrix.jsonl.gz` and `plan.json`.
For offline evidence controls:

```sh
PYTHONDONTWRITEBYTECODE=1 taskset -c 8-31 python3 docs/http2-performance/wrapper/measured/analysis_tests.py
PYTHONDONTWRITEBYTECODE=1 taskset -c 8-31 python3 docs/http2-performance/wrapper/measured/summarize.py
PYTHONDONTWRITEBYTECODE=1 taskset -c 8-31 python3 docs/http2-performance/wrapper/measured/extra_summaries.py
```

Final controls passed: four report-accounting tests, seven release/all-feature harness tests,
read-only workspace formatting, and package clippy in both all-target/default-feature and all-target/all-feature modes with warnings denied.
`rust-validation.txt.gz` records the commands' results.

The profile report explains the failed default DWARF ancestry, separate frame-pointer diagnostic,
one lossy capture, and the successful lower-frequency replacement.
Those are measurement limitations, not hidden workload failures.
The profile percentages cover user CPU only; whole-process controls found roughly 64–80% user CPU.
Kernel work is therefore substantial and is not attributed by these profiles.

Do not subtract the earlier in-memory direct-core timings or claim an HTTP/1 comparison.
Transport, headers, scheduling, and application contracts differ.
This is a source-specific, single-host baseline, not a remote-network capacity claim.
The CPU2 lease ends with this checkpoint; further experiments require a new lease and parent approval.
