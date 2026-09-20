# Admission integration: common-path measurements

This report measures immutable integration `b3c6484b1cb664dca76b5c35399d2e7d1367ce87`.
Its contemporaneous baseline is `5505efc469c7939c43d6d42a2ecb10b94ab4d9e4`.
The Rust benchmark sources are byte-identical between those revisions.
No core, native harness, configuration, or dependency change forms part of this measurement.
No optimization or PoC ran.

## Scope and decision

**The admission integration has a small measured common-path regression.**
Across 27 representative cells, paired median changes range from **+0.60% to +3.84%**.
The unweighted median of those cell changes is **+2.04%**.
Nineteen empirical bootstrap intervals are entirely positive.
Eight intervals include zero.
These results do not support a zero-cost claim.

All strict workload, overlap, allocation, and retention runs pass.
No case was skipped or relaxed.
Performance acceptance remains a parent decision because no numerical regression budget was specified.
The CPU2 lease is released.

### Metadata-pressure paths remain outside this workload

The integration adds transient `Blocked` results and coalesced `admission_changed` notifications.
The notification becomes armed only after a blocked metadata command.
This benchmark submits successful metadata commands and never takes that blocked-command path.
It therefore does not measure readiness delivery, retry cost, or metadata-pressure latency.

The matrix does exercise DATA flow control and paused body receipts.
Those conditions are not equivalent to blocked metadata admission.
Core tests, independent review, and socket cases provide separate correctness evidence for the new admission paths.
This report does not replace that evidence or qualify a future wrapper.

## Results and evidence

| Measurement | Result |
|---|---|
| Complete new-source timing matrix | **945 passed**, 189 cells × five trials |
| Contemporary baseline/admission comparison | **486 passed**, 243 pairs across 27 cells |
| Matched paired work | Exact workload fields and wire-byte totals match in every pair |
| Default release examples and ownership tests | **39 passed**, zero failed or ignored |
| All-feature release examples and ownership tests | **39 passed**, zero failed or ignored |
| Additional overlap regression | **1 passed**, overlap in all ten cohorts |
| New-source allocation matrix | **324 passed**, 108 cells × three identical repetitions |
| New-source retention runs | **12 passed**, 10000 cohorts per connection |
| Fresh profiles | Four successful `cpu-clock:u` recordings with DWARF stacks |
| Formatting and clippy | Pass, including all targets and all features with `-D warnings` |

The [qualification record](evidence/qualification.json) contains exact counts.
The [full source-specific timing table](RESULTS.md) includes medians and ranges.
The complete [189-cell summary](evidence/summary.json) and [945 trials](evidence/trials.jsonl) retain every result.

## Paired regression estimate

The comparison uses the preserved `5505efc4` binary, not historical timing values as its baseline.
Each cell runs nine baseline/admission pairs.
Binary order alternates between pairs.
Both versions use the same batch count, with approximately 400ms of baseline measured work per run.
The existing eight-cohort warmup precedes every measured window.

Each route covers these cases:

- Empty requests with 128-byte responses at concurrency 1 and 128.
- Empty requests at concurrency 1 with 17-byte fragments.
- 4096-byte duplex and 32KiB duplex at concurrency 8.
- 32KiB duplex at concurrency 64 with 17-byte fragments.
- 1MiB duplex at concurrency 1, 8, and 128.

All other paired cases use 65536-byte fragment bounds.
The [paired table](REGRESSION.md) contains all 27 distributions and uncertainty intervals.
Representative paired median changes are:

| Workload | Direct | Selected | Auto |
|---|---:|---:|---:|
| Empty, C1, full fragment | +3.84% | +1.68% | +1.77% |
| Empty, C128, full fragment | +1.43% | +2.04% | +1.97% |
| 4096-byte duplex, C8, full fragment | +1.96% | +2.71% | +2.63% |
| 1MiB duplex, C1, full fragment | +1.66% | +1.08% | +1.14% |
| 1MiB duplex, C128, full fragment | +2.22% | +2.14% | +0.60% |

The direct empty C1 interval is +1.72% to +5.27%.
The selected 1MiB C128 interval is +1.79% to +3.82%.
The auto 1MiB C128 interval is -0.24% to +1.03%.
Some other intervals are broad because of host variability.
No outlier was removed.

The interval resamples nine paired ratios with a fixed seed and 10000 bootstrap draws.
It describes those observations, not all machines or future traffic.
The 2.04% aggregate is an unweighted summary across cells, not a workload-weighted slowdown.
The profile does not establish which individual instruction caused the regression.
The difference covers the integrated revision, including its composition changes.

The new full matrix remains useful as a source-specific table.
For empty C128/full-fragment exchanges, its medians are 2.682µs direct, 2.700µs selected, and 2.707µs auto.
Cross-revision conclusions use the alternating pairs instead of subtracting older report medians.

## Preserved assertions and overlap

Both binaries retain the same full-payload comparisons, headers, status assertions, typed receipts, receive ends, and retirement counts.
Both endpoints run symmetrically through the direct slice-to-read-page transport.
No socket or per-byte producer allocation enters the transport loop.
The producer uses static payload buffers.

The additional overlap regression completes ten 1MiB duplex cohorts at concurrency 8.
It requires actual response payload delivery before all request payload transport acceptance.
The first response arrives after 49152 accepted request bytes in the first cohort.
Later cohorts observe 65536 accepted request bytes out of 8388608 total.
Admission and delayed `Sent` or END notifications cannot satisfy that assertion.
The [fresh overlap output](evidence/overlap.txt) retains all observations.

## Allocation and retained storage

The separate allocation executable does not instrument the timed binary.
All 108 allocation cells have identical repeats.
Their allocation, reallocation, deallocation, live-change, and peak-growth values match the `5505efc4` evidence.

For the cold empty C1 case:

| Route | Allocations | Reallocations | Deallocations | Requested peak growth |
|---|---:|---:|---:|---:|
| Direct | 62 | 9 | 14 | 75744 bytes |
| Selected | 64 | 9 | 14 | 82848 bytes |
| Auto | 70 | 9 | 20 | 113189 bytes |

The largest absolute requested-live peak is **610562 bytes**.
Absolute process values contain 16 more pre-window bytes than the earlier executable.
The executable path is also 16 bytes longer.
Baseline-relative values match in every cell.
The absolute shift is not evidence of additional connection storage.

All twelve retention runs reach equal live-byte values at cohorts 2000, 5000, and 10000.
Those intervals have zero reallocations and zero net requested-live growth.
Every run returns to its pre-construction baseline after shutdown and pair destruction.
All harness vector snapshots remain unchanged after cohort 8.

Retained requested bytes after subtraction of the process baseline:

| C | Direct | Selected | Auto |
|---|---:|---:|---:|
| 1 | 128836 | 135940 | 135940 |
| 8 | 152028 | 159132 | 159132 |
| 64 | 353180 | 360284 | 360284 |
| 128 | 583068 | 590172 | 590172 |

These values also match the earlier revision.
The bounded tombstone history remains part of the measured cost.
Stable storage does not mean allocation-free operation.
The [allocation data](evidence/allocations/summary.json) and [retention records](evidence/retention-runs.jsonl) contain all boundaries.

**Requested bytes are not RSS.**
The ledger excludes allocator rounding, metadata, caches, stack storage, static payload storage, and internal reallocation overlap.
It includes both endpoints and harness scratch storage.
No total-memory reduction claim follows from these values.

## Fresh profiles

[COPIES.md](COPIES.md) reports current client, server, and shared stacks.
It uses the new binary's symbols and instruction offsets.
Large-body profiles still contain substantial transport-copy and full body-comparison work.
The 144-byte server progress move remains sampled.
No old address or old profile substitutes for new-source evidence.
No copy experiment or source optimization was repeated.

## Source, binaries, and replay

```text
Baseline source: 5505efc469c7939c43d6d42a2ecb10b94ab4d9e4
Admission source: b3c6484b1cb664dca76b5c35399d2e7d1367ce87

Baseline composition_bench SHA256:
861d584ad7c355cbf89eddbb43b74f487e9ee6431ed44d3d7c8135f5f7c2d692

Admission composition_bench SHA256:
80d578b012ab4a5d9d165c6174e7b12fd4e776d20f53a50b0cf72201773de850

Admission allocation_probe SHA256:
b723d7f62743de86f954be3a131ed53f61ac4765a5349210caacfbd8f3479c7c
```

The new frozen directory is:

```text
/workspace/kimojio-rs/target/http2-program/build-http2-admission-performance/frozen-b3c6484b
```

Both revisions use Rust1.98.1, LLVM22.1.8, release builds, and `CARGO_PROFILE_RELEASE_DEBUG=2`.
`RUSTFLAGS` and `CARGO_ENCODED_RUSTFLAGS` are unset.
The [freeze record](evidence/freeze.json) contains exact paths, commands, and source hashes.
The original baseline binary and all earlier evidence remain unchanged.

Builds, tests, memory runs, and report analysis use CPUs8–31.
CPU2 hosted only the sequential timing and profile phases under the exclusive lease.
Affinity does not isolate shared memory bandwidth or package frequency.
The lease is now released.

After a new CPU2 lease, replay the paired comparison into a separate directory:

```sh
taskset -c 8-31 python3 docs/http2-performance/admission/paired_regression.py \
  /workspace/kimojio-rs/target/http2-program/build-http2-final/frozen-5505efc4/composition_bench \
  /workspace/kimojio-rs/target/http2-program/build-http2-admission-performance/frozen-b3c6484b/composition_bench \
  docs/http2-performance/final/evidence/summary.json \
  docs/http2-performance/admission/replay-paired
```

The runner stops on a strict failure or a paired workload mismatch.
The normal workload still does not exercise metadata `Blocked` or `admission_changed`.
