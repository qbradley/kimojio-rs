# Inline reverse membership: final candidate qualification

## Recommendation

**Accept the performance candidate `cd81114e` on this evidence, subject to the parent-owned independent review and peer gates.**
The decision uses measured CPU/wall cost and correctness outcomes, not allocation counts alone.
Both native and generic paths are affected. Neither is an unchanged control.
No additional candidate or production optimization follows this report.

- Nine paired groups support steady improvements of **2.59% native** and **1.93% generic**.
- Equal-weight combined improvement: **2.24%**, nominal interval **1.94–3.00%**.
- No primary or sensitivity comparison has an interval wholly on the regression side.
  This is not proof of universal non-regression.
- All primary, sensitivity, full-refresh, retention, and successful debugger workloads passed their strict assertions.
- The authorized **80-cell × five-trial refresh passed on the same candidate binary**.
- Actual reverse-vector allocations at C8 duplex cohort 32 fell from **341,115 / 329,414 to zero**, native/generic.
- Normal release `WaitData` remains 80 bytes. Its requested `Rc` allocation remains 96 bytes.
- Actual benchmark retirement observations preserve the zero/one membership distribution.
  Multi-scope behavior is separately covered by real migration, cancellation, and 16-scope tests.
- Requested-live storage plateaus remain bounded. Post-runtime cleanup is unchanged.

## Isolation and immutable identities

The new worktree was created at accepted report `8ba74acb59a607d30fae614f2866c475860ec4dc`:

```text
/workspace/kimojio-rs/target/http2-program/worktrees/http2-membership-performance
```

Only the supplied `cd81114e` delta was imported, as separate commit
**`a2bbb0666c5a3adcf740569243b20efa875a9223`**.
The four changed source/test files are `async_event.rs`, `task.rs`, `task/io_scope.rs`,
and `task/io_scope/tests.rs`, all under `kimojio/src/`.
No benchmark or wrapper source changed.

`async_stream.rs` is byte-identical to accepted `6e003746`:
blob **`97c2eb6367dc06d02e65293e9f314b92a09ec3ec`**.
The rejected direct-read source and its tests are absent.
Only immutable measurement-driver code is reused from the previous experiment.

| Item | SHA256 / revision |
|---|---|
| Baseline build source | `6e00374698d40dedebd33b87d3fd183ebb0e2682` |
| Baseline normal binary | `0a362f7029ead27d25b1046df1ae0cdd4838b87f5cfc275c8f042096b7ec00ad` |
| Baseline allocation binary | `c62c7f0d2755f40824aa45850e5ffc00e9f1fc21e1ff841def21f400e16602c8` |
| Supplied candidate | `cd81114e8dd8596ca709106271bfbd21bb542e74` |
| Candidate normal binary | `670dba3077c072839cf2d45a9afbb2936c86e98f9d0166cc87607b0f9eb42d84` |
| Candidate allocation binary | `92f26fbe1ec0acdb5963359a5fa0b67cb26f9b3693294c73fce9df7d7012d8f0` |
| Candidate source manifest | `787fd8ecda4a81862b6e1b7dd457a8f8532f0445ee3018c297d54e153c7aee81` |

The normal executable uses the default allocator without counting instrumentation.
The allocation executable is separate.
Both are retained under:

```text
/workspace/kimojio-rs/target/http2-program/build-http2-membership-performance/frozen-a2bbb066/
```

The baseline binaries remain untouched in the old `measured-6e003746` directory.
`freeze.json` records all paths, source identities, and flags.
The report commit is not the revision that produced the executable.

```sh
CARGO_TARGET_DIR=/workspace/kimojio-rs/target/http2-program/build-http2-membership-performance \
CARGO_PROFILE_RELEASE_DEBUG=2 taskset -c 8-31 \
cargo build --release -p kimojio-http2 --bench runtime --bench allocations --message-format=json
```

Rust was 1.98.1, LLVM 22.1.8, x86_64 Linux.
Rust flags and encoded Rust flags were empty.
No candidate rebuild occurred between the paired runs and full refresh.
All builds, tests, allocations, debugger work, and analysis used CPUs 8–31.
Timings and profiles used the exclusive CPU2 lease, released when collection finished.

## Paired experiment

The workload and assertions are unchanged.
They include real UNIX stream sockets, both drivers, complete byte comparisons, exact stream retirement, and actual close.
Gated duplex also requires response payload delivery before upload-tail release.
Static producer chunks are 16 KiB. The producer does not buffer a whole large body.
Application tasks, assertions, framing, and runtime/kernel I/O remain included costs.

The primary experiment has **36 paired cells**:

- native and generic.
- empty/128-byte response, fixed 4 KiB each way, stream1m each way, and gated duplex1m each way.
- concurrency 1, 8, and 128.
- warmed steady for every case, plus cold construction-through-close for the small cases.

There are nine alternating paired groups.
Versions, backend order, and traversal order alternate.
Cold processes interleave versions within each group.
The pilot chooses common cohort counts for both versions/backends, targeting approximately one-second steady windows.
Observed primary steady windows were **0.924–1.365 seconds**.

Warmup is unchanged: at least 2048 small exchanges and eight large cohorts.
The large, low-concurrency cases can still include small bounded tombstone growth.
This applies equally to baseline and candidate.
Bounds remain unchanged: 128 active/wrapper streams, queue count equal to concurrency,
the accepted send/receive/metadata capacities, and the original buffered stream read policy.

The primary matrix passed **4140 process runs**:

- 6,971,760 measured exchanges.
- 7,640,496 retirements per endpoint including warmup.
- 536,228,361,216 compared payload bytes.
- 121,500 required overlap witnesses.

The sensitivity matrix passed 108 runs, with 60,192 measured exchanges and 67,104 overlap witnesses.
No failing cell was omitted or rerun.

### Distributions and uncertainty

Each trial produces one candidate/baseline ratio per cell.
Cold groups first normalize total window time by completed exchanges.
Intervals use 20,000 fixed-seed bootstrap resamples of the nine paired group ratios.
The statistic is the median.
Intervals are nominal, with no family-wise correction.
Raw per-trial values, CPU/wall min/max, medians, and ratios are retained.

Group summaries first take an equal-weight geometric mean across their cells within each trial,
then summarize the nine trial values.
Both backends are affected. No “unaffected native control” adjustment is made.
The equal weighting is a benchmark summary, not an assumed production traffic mix.

| Steady group | Native ratio [interval] | Generic ratio [interval] | Both affected backends |
|---|---|---|---|
| small | 0.97695 [0.94529, 0.98597] | 0.97734 [0.96635, 0.98817] | 0.97786 [0.95916, 0.98123] |
| large | 0.97724 [0.95771, 0.98260] | 0.98160 [0.97650, 0.98716] | 0.97944 [0.96845, 0.98349] |
| all | 0.97409 [0.95331, 0.98390] | 0.98073 [0.97482, 0.98425] | 0.97764 [0.97000, 0.98061] |

All 36 primary comparisons and six sensitivity comparisons are present in the summary files.
Some individual intervals include 1. The improvement is not resolved separately for every cell.
No comparison has a nominal lower interval bound above 1.
Twenty-one of the 36 primary intervals support an improvement.
The worst primary point estimate is cold generic empty C8: **1.01772 [0.97510, 1.02745]**.
That cell has a 1.77% slower point estimate, but the interval does not resolve a regression.
No extra paired groups were added to seek a favorable outcome.

Representative paired C8 wall results, µs per completed exchange:

| Case | Backend | Baseline median | Candidate median | Median paired ratio |
|---|---|---:|---:|---:|
| empty | native | 25.479 | 24.843 | 0.98128 |
| empty | generic | 29.052 | 28.780 | 0.98518 |
| fixed4k | native | 35.645 | 35.009 | 0.98086 |
| fixed4k | generic | 40.816 | 39.966 | 0.98546 |
| stream1m | native | 1286.116 | 1262.576 | 0.97669 |
| stream1m | generic | 1551.183 | 1529.251 | 0.97941 |
| duplex1m | native | 1290.793 | 1263.188 | 0.98580 |
| duplex1m | generic | 1544.722 | 1523.710 | 0.98709 |

The ratio is not the quotient of independent medians.
Process CPU results are separately retained in `matrix-summary.csv/json`.
Steady time per exchange is inverse throughput, not individual request latency.
Cold scope includes construction, one cohort, and close. It is not construction alone.

### Sensitivity and variability

Nine paired C8 gated-duplex groups also compare static16k, owned16k, and static1k producers.
Owned mode allocates only bounded chunks.
The 1 KiB variant uses fewer cohorts to bound run duration, without changing payload/retirement assertions.

| Backend / variant | Paired ratio [interval] |
|---|---|
| native static16k | 0.98279 [0.86511, 1.00058] |
| native owned16k | 0.93212 [0.85215, 1.00472] |
| native static1k | 0.95042 [0.86014, 1.00967] |
| generic static16k | 0.89089 [0.88165, 1.08991] |
| generic owned16k | 0.97136 [0.88458, 1.10058] |
| generic static1k | 0.99917 [0.97788, 1.08637] |

These sensitivity intervals are wide. Their large point improvements are not established wins.
They show no resolved regression, but do not establish equivalence.
Host variability was retained, not trimmed.

## Full 80-cell refresh on the same binary

The clear paired group benefit and lack of a resolved regression triggered the authorized refresh.
It adds C32 and stream32k and covers both phases/backends, with five alternating groups per cell.
All **80 cells / 400 groups / 2000 processes** passed.
It completed 820,250 measured exchanges, 1,154,490 retirements per endpoint including warmup,
119,074,448,640 compared bytes, and 23,840 required overlap witnesses.
Steady windows ranged from 214.4 to 534.9 ms.

Fresh C8 warmed candidate values:

| Case | Backend | Wall µs/exchange | CPU µs/exchange | Bidirectional payload MiB/s |
|---|---|---:|---:|---:|
| empty | native | 24.903 | 24.903 | 4.90 |
| empty | generic | 28.516 | 28.515 | 4.28 |
| fixed4k | native | 35.235 | 35.235 | 221.72 |
| fixed4k | generic | 39.993 | 39.993 | 195.35 |
| stream32k | native | 71.130 | 71.128 | 878.67 |
| stream32k | generic | 82.199 | 82.199 | 760.35 |
| stream1m | native | 1263.128 | 1263.116 | 1583.37 |
| stream1m | generic | 1565.573 | 1565.398 | 1277.49 |
| duplex1m | native | 1254.993 | 1254.919 | 1593.63 |
| duplex1m | generic | 1531.983 | 1531.971 | 1305.50 |

The complete 80-cell distributions and cohort-completion values are in `full-refresh/`.
These independently collected numbers must not be subtracted from the old full matrix to infer a causal gain.
The nine-group paired experiment supplies the comparison.

## Actual allocation sites and object size

Both frozen allocation binaries ran the same capped C8 controls for native and generic:

- static 1 MiB duplex through cohort 32.
- owned duplex through cohort 8.
- empty through cohort 4096.

There were 12 socket runs plus eight scoped/unscoped runtime controls.
Allocation traces have a 512 MiB address-space cap, 100/105-second CPU limit, and 110-second wall limit.
The ledger is fixed at 2048 sites and 24 stack addresses.
Instrumented run times are not timing evidence.

`alloc_sites.py` resolves each stack against its matching allocation executable.
The original reverse-vector sites are identified by the actual `Vec<Weak<IoScopeRegistry>>` allocation frames.
The candidate's `WaitScopes::push` fallback is identified separately.
This is source/site attribution, not poll-context totals alone.

At static duplex cohort 32:

| Backend | Total successful allocations, baseline → candidate | Removed reverse-vector allocations | WaitData allocations, both | Requested live bytes, baseline → candidate |
|---|---:|---:|---:|---:|
| native | 767,418 → 426,303 | 341,115 | 374,923 | 317,820 → 317,596 |
| generic | 1,010,075 → 680,661 | 329,414 | 363,210 | 496,193 → 495,969 |

The total allocation reductions are about **44.45% native / 32.61% generic**.
They exactly equal the removed vector-site counts.
WaitData allocation counts are unchanged.
Candidate reverse-membership fallback allocations are zero in these traced socket workloads.
The old runtime repair's extra allocation churn is reduced without restoring history retention.

The frozen DWARF layout gives `WaitData = 80 B`, `WaitScopes = 24 B`.
The old vector also occupies 24 bytes.
Seven live WaitData allocations occupy 672 requested bytes in both versions: **96 B each**, including the `Rc` header.
Their seven separate old vectors occupied 224 bytes: **32 B each**.
That explains the exact 224-byte live-storage reduction.
The candidate does not enlarge the requested WaitData allocation size on this compiler/target.
This compiler niche layout is not a portable ABI promise.

The multi-scope fallback starts with two weak slots, requesting 16 bytes.
It can grow and allocate for real multi-scope workloads.
It is not an allocation-free general-purpose collection.
The old first allocation had four slots. A third membership can therefore trigger earlier growth in the candidate.
No multi-scope CPU improvement is claimed.

## Membership distribution on the actual benchmark

Bounded software-breakpoint observations ran the **same normal binaries**, with strict socket oracles intact.
They observed membership length at entry to `WaitData::retire_from_scopes`.
The exact frozen disassembly establishes the `&Rc` entry ABI and field offsets.
The independently read DWARF layout establishes the 80/24-byte sizes.
These are target-specific diagnostics, not production instrumentation.

Baseline and candidate counts matched exactly:

| Actual C8 workload | Zero memberships | One membership | Two or more |
|---|---:|---:|---:|
| native empty, 32 cohorts | 768 | 4,973 | 0 |
| generic empty, 32 cohorts | 768 | 5,143 | 0 |
| native duplex1m, one cohort | 1,057 | 10,273 | 0 |
| generic duplex1m, one cohort | 1,057 | 11,448 | 0 |

These count retirement observations, excluding waits that complete without allocating WaitData.
The debugger changes scheduling, so the distribution is not a production traffic estimate.
The longer allocation traces independently show the common-case vector sites disappearing.
Multi-scope absence in this benchmark does not establish multi-scope correctness by itself.
The 35-test scope/wait suite includes 16-scope capture, deduplication, expired weak owners,
real cross-task migration, cancellation, fresh generations, and reentrant clone/wake/drop.

The first two debugger attempts failed inside GDB at the fixed 1 GiB address-space limit.
GDB reported 32 default worker threads.
The successful runs retained that memory limit, disabled DWARF loading, used one debugger worker,
and limited allocator arenas to two.
The original failures remain preserved.
No production source, normal timing environment, or on-disk binary changed.

## Retention and cancellation

Both versions keep exactly flat empty-connection plateaus at cohorts 256, 1024, and 4096:

| Boundary | Baseline native / generic | Candidate native / generic |
|---|---:|---:|
| connection live, empty plateau | 258,314 / 338,451 | 258,090 / 338,227 |
| after drivers close, empty | 37,971 / 37,972 | 37,971 / 37,972 |
| after runtime cleanup, socket cases | 1,572 / 1,572 | 1,572 / 1,572 |

The 224-byte live difference is seven removed 32-byte vectors.
Tombstones, receive pages, registry maps, and completion-pool bounds are unchanged.
Site live-byte sums reconcile with continuous counters plus pre-trace storage.
Counters are not reset at window boundaries.
Application and driver origins return to zero after runtime cleanup.

Runtime-only controls repeat 4096 operations:

| Control | Live bytes at operation 1 and 4096, both versions | Baseline → candidate allocations at 4096 |
|---|---:|---:|
| scoped wait | 33,451 | 8,204 → 4,108 |
| unscoped wait | 33,257 | 4,106 → 4,106 |
| scoped NOP | 33,834 | 17 → 17 |
| unscoped NOP | 33,640 | 15 → 15 |

All four finish at 548 requested bytes after runtime cleanup.
The scoped wait takes snapshots after dropping each waiter, so its live-byte boundary need not decrease.
Its 4096 removed allocations are churn, not retained storage.
NOP and original/ACK handling are unchanged.
The genuine-CQE ownership and both ACK-owner-order tests remain in the passing scope suite.

These counters measure requested Rust heap bytes, not RSS or allocator usable size.
They exclude allocator/tracking metadata, rounding, stacks, static producer storage, and kernel buffers.
Per-origin peaks are not simultaneous and are not added.

## Hottest stacks

Fresh normal-binary profiles use C8 gated duplex, 1000 measured cohorts plus eight warmup cohorts,
`cpu-clock:user` at 999 Hz, and 32768-byte DWARF stack capture.
All profile workloads passed.
Exact commands, data hashes, binary hashes, and outcomes are in `profiles-native.json` and `profiles-generic.json`.
Profile durations are not non-instrumented timing results.

| Profile | Samples | Client ancestor | Server ancestor | Shared/unattributed |
|---|---:|---:|---:|---:|
| native baseline | 7063 | 243 | 181 | 6639 |
| native candidate | 6529 | 234 | 156 | 6139 |
| generic baseline | 8754 | 224 | 192 | 8338 |
| generic candidate | 8804 | 209 | 240 | 8355 |

Normal DWARF unwinding again gives only about 1.75–1.89 physical frames.
About 94–95% of samples cannot be assigned uniquely to an endpoint.
No precise endpoint CPU split is claimed.
Identical-function aliases and join types naming both endpoints remain conservative.

Hottest identifiable client paths include `TryJoinAll<exchange>::poll → cohort`,
with 47 baseline-native and 52 baseline-generic samples.
Candidate client leaves have 51 native and 39 generic samples.
The hottest identifiable candidate server paths are `H2Server::accept_driver_event_bytes_ref` (27 native)
and the scoped-server `MaybeDone` future (43 generic).
These are observed paths, not all endpoint work.

The hottest shared native `next_input` path has 340 / 263 samples, baseline/candidate.
The shared generic buffered-read libc path has 401 / 369.
The buffered read remains: this is not the rejected direct-read candidate.

Pointer-key hashing has 304 / 272 native self samples and 317 / 315 generic self samples.
Identical-code aliases can include both WaitData and Completion keys.
Registration has 180 / 183 native and 182 / 185 generic self samples.
WaitFuture construction/polling also remains substantial.
Retirement work partly moved to a separate `IoScopeRegistry::retire_wait` function
(166 candidate-native and 183 candidate-generic self samples).
The old and new `retire_from_scopes` self counts therefore do not measure the same work.

## How copies were found (MIR vs objdump vs perf)

The procedure was immutable binary, profile, stack split, `nm`/`objdump`, matching source, then site attribution.
No MIR string absence is used as evidence.
Addresses come from `symbol+offset`, not subtraction of ASLR addresses.
Exact call-return PC samples, with the one-byte unwinder adjustment, are separate from ±24-byte leaf neighborhoods.
Nearby branch/return instructions are not automatically allocator costs.

The primary cut is heap allocation/free for metadata, not a payload-copy change.
The actual enum move is still 24 bytes.
The candidate entry moves its fields, writes the eight-byte Empty discriminant, then selects Empty/One/Many.
It does not allocate a vector for the One branch.
The disassembly search also covered `rep movs` and the SIMD move instructions.
`inline-sites.json` retains sampled inline moves from all four profiles.
Those moves remain outside this allocation cut.

## Copy / allocation inventory

`E/S/P` means exact return-site samples / nearby leaf samples / parent-function samples.
E and S can overlap. Do not add them.
Poor ancestry makes these conservative caller-site observations.

| Site | Size / kind | Native E/S/P | Generic E/S/P | Necessity |
|---|---|---:|---:|---|
| Baseline `IoScopeRegistry::register_wait+0x162`, `0x18bfb2`, `io_scope.rs:90` | first reverse-vector growth, 32 B for four weak slots | 18/19/197 | 3/26/183 | Avoidable for one membership |
| Baseline `WaitData::retire_from_scopes+0x2b1`, `0x18c4c1` | reverse-vector deallocation | 60/6/299 | 38/7/300 | Removed with the common-case allocation |
| Candidate `register_wait+0x1b0`, `0x18d220`, `WaitScopes::push` | 16 B initial multi-scope vector | 0/0/189 | 0/0/196 | Required for the general fallback |
| Candidate `register_wait+0x24c`, `0x18d2bc` | later fallback growth | 0/8/189 | 0/16/196 | Required when the fallback fills. Nearby leaf hits do not prove growth. |
| Candidate fallback iterator free `+0xb7`, `0x195127` | vector deallocation | 0/0/0 | 0/0/0 | Required for multi-scope ownership |
| WaitData `Rc` construction, baseline `0x18d46d`, candidate `0x18da01` | unchanged 96 B requested allocation | 102 → 83 exact hits | 84 → 91 exact hits | Waiter ownership remains necessary |

`membership-profile-sites.json`, the assembly excerpts, and `membership-site-source.txt.gz`
preserve instructions, offsets, and matching source.
The normal profiles show the common vector free cost disappearing, while map/hash/wait costs remain.
They do not imply that all waiter allocation or all scope overhead disappeared.
The paired timings, not sample-count reduction alone, support acceptance.

## Correctness, evidence limits, and replay

Passed locally:

- 35 scope/wait tests in debug, release/no-default-feature, and release/all-feature configurations.
- seven strict harness controls in release/default and release/all-feature configurations.
- read-only formatting and both HTTP/2 package all-target clippy modes with warnings denied.
- all benchmark, retention, runtime-control, and successful debugger oracles.
- five outcome, accounting, distribution, and artifact-integrity controls.

Runtime-inclusive default clippy passed with warnings denied.
Runtime-inclusive all-target/all-feature clippy reported only the two unchanged
`byte_char_slices` warnings in `kimojio/src/pipe.rs:64–65`.
The package-specific HTTP/2 modes were warning-free.
No unrelated source was changed to remove existing warnings.

A utility-only indentation error prevented the first sensitivity launch after all nine primary groups completed.
It was repaired before any sensitivity workload ran.
No primary group was repeated. The two executables and workload assertions did not change.
`runner-repair.json` records the interruption.

The measurement processes retain bounded address space and wall watchdogs.
Timing does not impose `RLIMIT_CPU`, due to the previously demonstrated process-clock distortion.
Allocation and debugger runs retain CPU limits and are not timing evidence.
Default-DWARF endpoint attribution remains limited.
User-CPU profile percentages do not include kernel CPU and are not predicted wall-time gains.
No RSS, HTTP/1 comparability, or in-memory-core subtraction claim is made.

Drivers reuse immutable measurement code from report `2a65d8e4a690975564104f3a78c697448eaee8e0`.
`run.py` makes sure that reused code matches its Git object and replaces only report/binary paths.
No rejected production source is loaded or imported.
The `stats` action removes the prior experiment's unaffected-native-control interpretation.
Both backends are summarized as affected.

Before a replay, obtain a new CPU2 lease.
Use a new report directory:

```sh
PYTHONDONTWRITEBYTECODE=1 taskset -c 8-31 python3 docs/http2-performance/wrapper/membership-cd81114e/run.py pilot
PYTHONDONTWRITEBYTECODE=1 taskset -c 8-31 python3 docs/http2-performance/wrapper/membership-cd81114e/run.py matrix
PYTHONDONTWRITEBYTECODE=1 taskset -c 8-31 python3 docs/http2-performance/wrapper/membership-cd81114e/run.py sensitivity
```

Keep the recorded relative references to the accepted `measured/` and retention drivers.
Do not overwrite this freeze or old worktrees.
Raw large evidence is losslessly compressed with original/compressed hashes.
Private perf data, binaries, symbols, and disassembly remain retained at their recorded paths.

## Proposals in order

1. Accept this exact membership candidate after the remaining parent-owned gates.
2. Preserve the accepted buffered-read implementation, all caps, and the multi-scope fallback.
3. Finalize the workstream. Do not start a third optimization experiment.

## What not to cut first

Do not remove weak membership ownership, live registry entries, or original/ACK settlement.
Do not change pointer hashing merely because it remains hot.
Do not widen reads or revive the rejected forwarding-layout/direct-read experiments.
Do not infer a zero-allocation runtime from zero common-case reverse-vector allocations.

**CPU2 lease released.**
