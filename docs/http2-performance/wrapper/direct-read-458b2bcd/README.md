# Capped direct-read candidate: paired qualification

## Recommendation: reject general integration at this checkpoint

Keep `458b2bcd` separate from the accepted baseline.
The candidate removes the sampled buffered-copy cost and improves the generic large-body group.
However, the broader net benefit remains unresolved, and small-message controls show regressions.
This is a **performance acceptance rejection**, not a workload correctness failure.
No extra trials or full 80-cell refresh followed the nine planned paired groups.
No further optimization was made.

Key results:

- All 4140 primary process runs and 108 sensitivity runs passed their strict workload assertions.
- Generic large-body paired median ratio: **0.98476**, nominal bootstrap interval **[0.97795, 0.98956]**.
  This supports about a **1.52% improvement** for that group.
- All generic steady cells, equally weighted within each trial: **0.99007 [0.98329, 1.00083]**.
  The interval includes no change.
- The all-cell generic/native-control ratio is **0.98574 [0.96904, 1.00264]**.
  This also includes no change.
- Cold generic C128 empty exchanges regressed: **1.02930 [1.01790, 1.04144]**.
- Native steady empty C1 and fixed4k C128 showed nominal regressions of **1.80%** and **1.59%**.
  Their logical read path did not change. These observations do not establish a causal read-path slowdown,
  but they prevent an unconditional acceptance claim.
- Fresh buffered-copy return-site samples fell from **350/8791 to 0/8574**.
- All connection-live, post-driver-close, and post-runtime allocation counters and plateaus matched exactly.

Individual intervals are nominal and have no multiple-comparison correction.
There are only nine independent trial groups per comparison.
The native results can include code-layout effects or host variability; neither cause was established.
No observation was trimmed away.

## Exact scope and freeze

Only the supplied delta was applied, in a separate integration commit:

| Item | Identity |
|---|---|
| Baseline source | `6e00374698d40dedebd33b87d3fd183ebb0e2682` |
| Previous report | `8ba74acb59a607d30fae614f2866c475860ec4dc` |
| Supplied candidate | `458b2bcd2f371b234087f04d2e96e19b61837390` |
| Candidate build source / integration | `72758bb8342c609ecc72cb3ec73ff7308a883b2c` |
| Candidate source-manifest SHA256 | `7980c72754383fc84001fffac8be558d6b05c9810f0db39103ed0bd12a0110f6` |
| Candidate normal binary SHA256 | `7e2b52b61504ed32694ebdbe253aed7c2e1e082aea01f48c5b90e1126db0556d` |
| Candidate allocation binary SHA256 | `7c6bc302860867eed7da3633adef13b0daa825c56a9a20cb1c2af40e359e0b9d` |
| Baseline normal binary SHA256 | `0a362f7029ead27d25b1046df1ae0cdd4838b87f5cfc275c8f042096b7ec00ad` |
| Baseline allocation binary SHA256 | `c62c7f0d2755f40824aa45850e5ffc00e9f1fc21e1ff841def21f400e16602c8` |

The integration changes only:

- `kimojio/src/async_stream.rs`;
- `kimojio/src/async_stream/direct_read_tests.rs`.

Their contents match the supplied candidate exactly.
Wrapper, protocol, harness, configuration bounds, and assertions remain unchanged.
The later evidence commit is not the binary's source revision.
The old binaries and all old reports remain intact.

Candidate binaries and profiles are retained under:

```text
/workspace/kimojio-rs/target/http2-program/build-http2-wrapper-performance/candidate-72758bb8/
```

`freeze.json` records both versions' absolute paths, SHA256 values, compiler, and build flags.
The normal binary uses the default allocator, without allocation tracing.
The allocation executable is separate.
Rust was 1.98.1 with LLVM 22.1.8.
Release debug level was 2; `RUSTFLAGS` and encoded Rust flags were empty.

```sh
CARGO_PROFILE_RELEASE_DEBUG=2 \
CARGO_TARGET_DIR=/workspace/kimojio-rs/target/http2-program/build-http2-wrapper-performance \
taskset -c 8-31 cargo build --release -p kimojio-http2 \
  --bench runtime --bench allocations --message-format=json
```

The candidate retains the **16 KiB offered read size**.
It bypasses the internal buffer only when that buffer is empty and the destination is sufficiently large.
The rejected full-destination read was not imported.
The parent reported that wider reads broke a paused-consumer capacity case.
This experiment therefore targets only the original **190/4356 user-sample buffered copy**, not the combined 7.2% copy estimate.
**Frame assembly reduction is neither expected nor claimed.**

## Measurement design

`paired.py` reuses the prior strict runner and all of its outcome assertions.
The only changed runtime selection is the immutable binary path.
The work uses real UNIX stream sockets, both endpoint drivers, bounded producers, and actual close.
Static 16 KiB producer chunks remain the primary case.
Every received byte is compared.
Both endpoints must report full stream retirement.
Gated duplex must deliver response payload before each upload tail is released.

The primary comparisons cover:

- native and generic;
- empty request / 128-byte response, fixed 4 KiB each direction,
  streamed 1 MiB each direction, and gated duplex 1 MiB each direction;
- concurrency 1, 8, and 128;
- warmed steady for all cases, plus cold construction-through-close controls for the two small cases.

This is **36 paired cells**, with baseline and candidate observations for every cell.
There are nine paired trial groups.
Version order, backend order, and cell traversal alternate.
Cold repeats interleave versions per process instead of running one entire version group first.
The same cohort count applies to both binaries and both backends.

The pilot scales steady windows to at least about one second.
Observed primary steady windows were **0.975–1.493 seconds**.
Warmup is unchanged: at least 2048 small exchanges, and eight large cohorts.
Small bounded tombstone growth can remain in low-concurrency large windows.
This applies equally to both versions.

Timing processes retain the 1 GiB address-space cap and 90/110-second internal/external wall limits.
They do not set `RLIMIT_CPU`, because the prior experiment found distorted CPU clocks with that limit.
All primary process-CPU deltas were positive.
Builds, tests, allocation probes, and analysis used CPUs 8–31.
Timings and profiles used the exclusive CPU2 lease.
The lease was released after collection; no further CPU2 work followed.

The primary matrix completed:

- **7,035,984 measured exchanges**;
- **7,704,720 retirements per endpoint**, including warmup;
- **531,654,520,320 compared payload bytes**;
- **120,672 required overlap witnesses**.

Sensitivity runs added 59,040 measured exchanges and 65,952 overlap witnesses.
No process failed, reconnected, relaxed an assertion, or fabricated a close receipt.

### Statistics and interpretation

For each cell, each trial produces one baseline/candidate ratio.
Cold groups first normalize total window time by total exchanges.
Intervals use 20,000 seeded bootstrap resamples of the nine paired group ratios.
The statistic is the median ratio.
The scripts retain CPU and wall medians, min/max, all paired ratios, and raw commands.

Group summaries first take an equal-weight geometric mean across the predefined cells within each trial,
then summarize the nine resulting trial values.
“Large” comprises stream1m and duplex1m at all three concurrencies.
“Small” comprises empty and fixed4k.
These are transparent benchmark weights, not a claim about an application's traffic mix.

| Steady group | Native candidate/baseline | Generic candidate/baseline | Generic divided by native control |
|---|---|---|---|
| small | 1.00933 [0.99478, 1.02823] | 1.00293 [0.97826, 1.02151] | 0.99473 [0.97192, 1.03512] |
| large | 1.00126 [0.98284, 1.00881] | 0.98476 [0.97795, 0.98956] | 0.98207 [0.95349, 0.99902] |
| all | 1.00311 [0.99638, 1.01759] | 0.99007 [0.98329, 1.00083] | 0.98574 [0.96904, 1.00264] |

The broader interval crossing 1 is not proof of equal performance.
It means this bounded experiment did not resolve a general net win.
The cold generic C128 regression and native controls strengthen the case against unconditional integration.
No extra trials were added to seek a favorable result.

### Generic large-body cells

Times are median wall µs per completed exchange.
The ratio is the **median paired ratio**, not the quotient of the two independent medians.

| Case | C | Baseline | Candidate | Paired ratio [nominal 95% interval] |
|---|---:|---:|---:|---|
| stream1m | 1 | 1832.890 | 1800.103 | 0.97677 [0.88293, 1.00266] |
| stream1m | 8 | 1546.060 | 1524.192 | 0.98304 [0.97536, 1.08352] |
| stream1m | 128 | 1604.294 | 1586.349 | 0.97894 [0.96049, 0.99326] |
| duplex1m | 1 | 1825.442 | 1801.133 | 0.98557 [0.94007, 1.05204] |
| duplex1m | 8 | 1546.805 | 1527.453 | 0.98529 [0.96993, 0.99042] |
| duplex1m | 128 | 1627.823 | 1591.803 | 0.98196 [0.96899, 0.99064] |

At generic C8 duplex, CPU medians were 1546.607 / 1527.430 µs.
Candidate throughput was 654.7 exchanges/s, or about 1309 MiB/s counting both directions.
The candidate average completed-cohort time was about 12.220 ms.
Steady time per exchange is inverse throughput, not individual request latency.

At generic C8 empty, wall medians were 29.232 / 29.083 µs, but the paired interval included no change.
At cold generic C128 empty, wall medians were 34.25 / 35.25 µs per exchange.
The paired CPU ratio for that cold cell was 1.01945.
Cold scope includes one cohort plus construction and close; it is not construction alone.

`matrix-summary.csv` contains all 36 comparisons, including the unfavorable cells.
`matrix-summary.json` retains all nine wall and CPU values per version and all paired ratios.
Large host variability remains visible in several cells.
These results do not establish individual request p95/p99 latency or remote-network capacity.

### Representative producer/chunk sensitivity

C8 gated duplex, nine paired groups per variant/backend:

| Backend / variant | Baseline ms/exchange | Candidate ms/exchange | Paired ratio [nominal 95% interval] |
|---|---:|---:|---|
| generic static16k | 1.554 | 1.513 | 0.97141 [0.96564, 0.98519] |
| generic owned16k | 1.611 | 1.578 | 0.97868 [0.97273, 0.98727] |
| generic static1k | 19.991 | 19.793 | 0.99718 [0.98242, 1.03454] |
| native static16k | 1.290 | 1.294 | 1.00320 [0.99835, 1.01072] |
| native owned16k | 1.344 | 1.348 | 1.00840 [0.99945, 1.20271] |
| native static1k | 16.753 | 16.611 | 0.99153 [0.98372, 1.02747] |

Owned mode allocates one bounded chunk at a time, not a whole body.
The 1 KiB runs use fewer cohorts to bound duration; their full byte and overlap assertions remain active.
The generic 16 KiB sensitivity results support the large-body improvement.
They do not remove the small/cold uncertainty or regression.

## Hottest stacks

Fresh profiles use the **same normal binaries as the paired timings**:
generic C8 gated duplex, 1000 measured cohorts plus eight warmup cohorts.
The event is `cpu-clock:user`, 999 Hz, DWARF capture size 32768.
Both profiles completed every workload assertion, with no reported lost samples.
`profiles.json` records exact commands, outcomes, binary hashes, and data hashes.

| Version | Samples | Client ancestor | Server ancestor | Shared/unattributed |
|---|---:|---:|---:|---:|
| baseline | 8791 | 227 | 198 | 8366 |
| candidate | 8574 | 234 | 182 | 8158 |

The normal builds again have weak DWARF ancestry: about 1.80 / 1.82 physical frames.
Roughly 95% cannot be assigned to a unique endpoint.
The hottest identifiable client path is `TryJoinAll<exchange>::poll`:
53 baseline samples; candidate's corresponding path through `cohort` has 47.
The hottest identifiable server scoped-driver leaf has 23 / 21.
The largest shared/unattributed path is the inlined `IoScopeFuture<run_stream>::poll` leaf, 395 / 401.
That leaf includes driver work; it is not pure scope bookkeeping.

Common helpers remain substantial:
WaitData pointer hashing has 335 / 289 self samples;
`WaitData::retire_from_scopes` has 302 / 269;
`next_input::poll` has 394 / 405.
These shared helpers must not be assigned solely to the client.
This profile cannot supply a precise fresh client/server CPU split.
Old frame-pointer addresses and percentages were not mixed into this comparison.

## How copies were found (MIR vs objdump vs perf)

Both binaries were frozen before recording.
Then `nm`, Rust demangling, `objdump`, relocations, and actual `symbol+offset` samples identified sites.
Matching `addr2line` output is retained with each inventory entry.
Baseline source locations refer to immutable `6e003746`, not the changed working-tree file.
Candidate locations refer to `72758bb8`.
There is no MIR-absence argument and no ASLR/file-address subtraction.

Exact call-return PC samples, including the one-byte unwinder adjustment, determine callee-site evidence.
Nearby leaf samples within ±24 bytes are retained separately.
They are not automatically treated as copy time.
At the candidate copy site, both nearby leaf samples were at `0x9ec2b`, in a separate polling block.
The preceding copy block jumps over it after the call.
`candidate-copy-site.txt.gz` retains those instructions.

## Copy inventory

| Site | Size | Exact return / nearby leaf / parent samples | Kind and necessity |
|---|---|---|---|
| Baseline buffered read `try_read_impl` poll `+0xc3`, `0x9ec73`; `async_stream.rs:174` | variable, at most 16 KiB | **350 / 0 / 398** | explicit copy from internal buffer; 3.98% of fresh user samples |
| Candidate buffered fallback poll `+0xbd`, `0x9ec1d`; `async_stream.rs:190` | variable, at most 16 KiB | **0 / 2 / 46** | fallback still exists and remains necessary for buffered tails; neighboring samples are not copy hits |
| Baseline generic `Connection::next+0x1877`, `0xb86d7`; `engine.rs:2109` | variable partial-frame bytes | **257 / 2 / 714** | explicit frame assembly; source page cannot be recycled before bytes are preserved |
| Candidate generic `Connection::next+0x1877`, `0xb8807`; same core source | same variable length | **341 / 0 / 827** | assembly remains hot; no reduction is claimed |

Frame assembly's sampled share was 2.92% / 3.98%.
Samples are not byte counts or call counts, so this does not quantify an increase in copied bytes.
The absent buffered-copy return samples support removal of its hot cost in this workload,
not a proof of zero fallback invocations in every application.

Inline SIMD moves also remain.
For example, baseline `0xfb7d6` / candidate `0xebdd6` has 62 / 73 self samples on
`movups xmm1,[rbx+0x10]`.
Another staging load at `0xfbc99` / `0xec299` has 51 / 40.
`inline-sites.json` retains the actual sites.
No dominant new zero-init target was established.
Profile percentages cover user CPU only and cannot be equated to wall-time savings.

## Allocation and retained-storage comparison

Both frozen allocation binaries repeated the same capped controls on CPUs 8–31:

- native and generic C8 static 1 MiB duplex through cohort 32;
- native and generic owned 1 MiB duplex through cohort 8;
- native and generic empty/128-byte-response connections through cohort 4096.

That is 12 independent runs.
Each keeps the 512 MiB address-space cap, 100/105-second CPU limit, and 110-second wall limit.
The site ledger retains its 2048-site, 24-address limits.
No counter is reset at a measurement boundary.
All strict payload, overlap where required, retirement, and close assertions passed.

All **42 post-pre-runtime snapshot pairs** match the complete per-origin counters,
including allocations, reallocations, deallocations, requested live bytes, and per-origin peaks.
The six pre-runtime pairs differ by one requested byte because `candidate/` is one byte longer than `baseline/`.
The diagnostic environment pathname is still owned while `retention_enable` takes that snapshot
(`benches/allocations.rs:10–11`, `support/allocation.rs:62–64`).
It is freed before the connection snapshots.
This is not a production retention difference.

Representative cumulative connection-live values are identical between versions:

| Case / endpoint backend | Cohort | Allocations | Reallocations | Deallocations | Requested live bytes |
|---|---:|---:|---:|---:|---:|
| static duplex native | 32 | 767,418 | 37 | 767,240 | 317,820 |
| static duplex generic | 32 | 1,010,075 | 38 | 1,009,886 | 496,193 |
| owned duplex native | 8 | 199,786 | 33 | 199,610 | 276,572 |
| owned duplex generic | 8 | 260,488 | 34 | 260,299 | 487,745 |
| empty native | 4096 | 3,258,761 | 35 | 3,258,594 | 258,314 |
| empty generic | 4096 | 3,813,227 | 35 | 3,813,053 | 338,451 |

For both versions, empty live storage is exactly flat at cohorts **256, 1024, and 4096**.
Post-driver-close values are 37,971 / 37,972 bytes for empty native/generic,
and 38,548 / 38,549 for duplex.
Post-runtime cleanup is **1572 bytes in every run**.
The previously attributed bounded pools and tombstones remain unchanged.
The candidate neither repairs allocation churn nor adds measured retained storage under these controls.
The runtime repair's earlier 38%/85% allocation-count increase remains a separate tradeoff.

`retention-summary.csv`, `retention-comparison.json`, and both sets of full site ledgers preserve all boundaries.
Site live-byte sums reconcile with continuous counters plus unattributed pre-trace storage.
Origin peaks are not added because they need not be simultaneous.
These are requested Rust heap bytes, not allocator usable size or RSS.
Static producer storage, kernel buffers, stacks, and allocator/tracking metadata remain excluded.
Instrumented run times are not performance evidence.

## Controls, limitations, and replay

Passed locally:

- 25 focused release stream tests with no default features;
- 26 focused release stream tests with all features;
- seven release harness controls under each configuration;
- read-only formatting;
- both HTTP/2 package clippy modes, all targets, with warnings denied;
- four report-accounting and artifact-integrity controls.

An additional runtime-wide all-target/all-feature clippy command failed on two existing
`byte_char_slices` warnings in `kimojio/src/pipe.rs:64–65`.
That file has the same blob in baseline and candidate:
`18d28920e6d3b3065f99923d7c4e2e361e782139`.
No unrelated source was changed to hide the failure.
The preceding all-feature stream and harness tests passed.
Both command logs are retained.

Independent review and peer qualification are separate parent-owned gates.
This report does not claim their completion.
The benchmark does not replace paused-consumer or adversarial protocol tests.
It retains the supplied candidate's 16 KiB size restriction and all accepted configuration bounds.

Do not subtract in-memory direct-core timings or infer HTTP/1 comparability.
The same socket/application/byte-comparison costs appear in both sides of this experiment.
Native is a logically unchanged control, not a promise of machine-code-identical placement.

Replay requires a new exclusive CPU2 lease and a new output directory.
The recorded commands are in the raw evidence.
The drivers are:

```sh
PYTHONDONTWRITEBYTECODE=1 taskset -c 8-31 python3 docs/http2-performance/wrapper/direct-read-458b2bcd/paired.py pilot
PYTHONDONTWRITEBYTECODE=1 taskset -c 8-31 python3 docs/http2-performance/wrapper/direct-read-458b2bcd/paired.py matrix
PYTHONDONTWRITEBYTECODE=1 taskset -c 8-31 python3 docs/http2-performance/wrapper/direct-read-458b2bcd/paired.py sensitivity
```

Copy the evidence drivers to a new report directory and retain their relative reference to `../measured`.
Do not overwrite this freeze or its observations.
The allocation runner uses only CPUs 8–31.
Raw large files are losslessly compressed; `compressed-evidence.json` records original and compressed hashes.
`private-artifacts.json` records retained perf data, symbols, and disassembly.

## Proposals in order

1. **Do not integrate this candidate as a general performance win on the current evidence.**
   Preserve the measured large-body improvement and the unfavorable controls together.
## What not to cut first

1. Do not widen the offered read size to seek assembly savings.
   That is the explicitly excluded capacity-changing variant.
2. Do not remove byte comparisons, retirement assertions, or close work to make the result favorable.
3. Do not launch another copy optimization or extend timing until the parent selects a new task.

The candidate and evidence remain separate and immutable.
**CPU2 lease released at this checkpoint.**
