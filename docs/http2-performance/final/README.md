# Source-specific integrated HTTP/2 measurements

This report measures immutable integration `5505efc469c7939c43d6d42a2ecb10b94ab4d9e4`.
It includes the corrected core APIs, composition handling, overlap regression, allocation probe, and bounded-retention probe.
All requested measured gates pass on this source.
The report does not establish a universal overhead bound or qualify a socket executor or Kimojio wrapper.

## Later API findings: qualification remains open

Subsequent wrapper research identified admission gaps outside these measured workloads:

- `Capacity` conflates transient pressure with a permanently oversized request.
- A zero peer-concurrency limit reports `InvalidFrame`.
- The API lacks a metadata-admission readiness signal.

The pending correction introduces distinct `Blocked` handling and coalesced `admission_changed` notifications.
That correction is not part of this freeze.
These results are **source-specific measurements, not final API qualification**.
They do not establish the cost or behavior of the future admission paths.
The parent will supply another immutable integration for affected-path measurements after the correction and regressions.

The `5505efc4` binaries, raw measurements, profiles, and memory evidence remain unchanged.
No candidate replaced the measured source.
The CPU2 lease was already released at the measurement checkpoint.

## Completion

| Gate | Final result |
|---|---|
| Strict timing matrix | **945 passed**, 189 cells × five trials, zero failures or skipped cells |
| Default release examples and ownership tests | **39 passed**, zero failures or ignored tests |
| All-feature release examples and ownership tests | **39 passed**, zero failures or ignored tests |
| Additional explicit overlap run | **1 passed**, actual payload overlap in all ten cohorts |
| Allocation matrix | **324 passed**, 108 cells × three identical repetitions |
| Same-connection retention | **12 passed**, 10000 cohorts per connection, 6030000 completed exchanges |
| Fresh profiles | Four successful `cpu-clock:u` profiles with DWARF stacks |
| Formatting and clippy | Pass, including all targets and all features with `-D warnings` |

The [machine-readable gate record](evidence/qualification.json) contains exact counts.
No core, harness, configuration, or dependency change forms part of this final measurement.
No PoC or optimization ran.
Earlier evidence remains unchanged.
**The CPU2 lease is released.**

## Timing distributions

All values are microseconds per completed request/response exchange.
Each value is the median of five trials.
The [results table](RESULTS.md) includes the minimum–maximum ranges.
The [complete CSV](evidence/timing-distributions.csv) contains all 189 distributions.

| Representative case | Direct | Selected H2 | Auto server |
|---|---:|---:|---:|
| Empty request / 128-byte response, C1, full fragment | 2.881 | 2.901 | 2.893 |
| Empty request / 128-byte response, C128, full fragment | 2.662 | 2.679 | 2.658 |
| 4096-byte duplex, C128, full fragment | 3.567 | 3.604 | 3.593 |
| 1MiB duplex, C8, full fragment | 159.534 | 160.913 | 161.342 |
| 1MiB duplex, C128, full fragment | 162.027 | 168.272 | 164.460 |
| New connection plus first empty exchange, full fragment | 8.034 | 8.015 | 9.994 |

Full-fragment empty workloads show approximately stable cost per exchange across the tested concurrency range.
Selected overhead at C128 is 0.7% by median, with overlapping trial ranges.
Auto overhead there is -0.1%, also with overlapping ranges.
Neither observation establishes a speed advantage or a universal small-overhead guarantee.

### Fragmentation costs are visible

Routing overhead is larger in some callback-heavy cells.
For 32KiB duplex at C64 with 17-byte fragments, direct costs 294.291µs and auto costs 335.452µs.
The auto median is 14.0% greater.
Their ranges do not overlap: 288.782–300.212µs direct and 316.148–431.926µs auto.

The largest selected median difference is 14.7% for 1MiB duplex at C64 with 17-byte fragments.
That cell has overlapping ranges: 9221.208–12515.358µs direct and 10084.075–14932.680µs selected.
The percentage is an observation, not a precise overhead bound.
No trial was discarded because its latency was large.

Cold auto construction costs 24.4% more by median with full fragments.
It costs 57.6% more with 17-byte fragments.
The cold workload includes both protocol children, detection, startup, and the first exchange.
It does not isolate detection alone.
Detection does not recur in warmed exchanges.

The report makes no historical speedup claim.
Earlier measurements use different core revisions or different progress guarantees.
The routes within this final matrix share the same frozen binary and workload assertions.

## Workload and normalization

The matrix covers direct H2, selected composite H2, and auto-server detection.
Its warmed cases use concurrency 1, 8, 64, and 128 with fragment bounds 17, 1024, and 65536.
The five workloads are:

- Empty requests with 128-byte responses.
- 4096-byte bodies in both directions.
- 32KiB bodies in both directions.
- 1MiB bodies in both directions, from 32KiB producer chunks.
- Paused body receipts with a sibling that must complete before the held receipts are released.

Each warmed connection completes eight cohorts before its measured window.
The existing runner chooses the number of measured cohorts for each case.
The three construction cases use concurrency 1 and fresh connections.
Construction excludes shutdown and pair destruction.

The executor copies directly from pending write slices into the peer read page.
It uses no socket, transport byte queue, or per-byte producer allocation.
Both endpoints run symmetrically.
All body bytes, headers, status codes, returned buffers, accepted-byte receipts, receive ends, and retirements retain their assertions.
The complete JSON records contain exchanges, wire bytes, payload bytes, batch counts, and commands.

The static producer isolates protocol and executor work from dynamic payload creation.
Requests and responses reuse fixed header fields.
The profiles do not represent a diverse HPACK corpus.
The executor still performs real transport copies and complete body comparisons.
Those costs are not exclusively protocol costs.
The simulated protocol clock remains `Duration::ZERO`.
These results do not measure timer-heavy traffic, network latency, TLS, or application business logic.

## Current-source overlap

The explicit regression also runs on this final source.
The first 16384 response bytes reach the client after 49152 accepted request bytes in cohort 1.
They arrive after 65536 accepted request bytes in each later cohort.
Each cohort has 8388608 request-payload bytes in total.
All ten cohorts satisfy actual payload overlap.

The assertion uses immediate transport acceptance and the client body callback.
It does not use admission, delayed `Sent`, or delayed END notifications as evidence.
The [complete output](evidence/overlap.txt) includes exact observations and shutdown settlement.

## Current-source allocation and retention

The separate allocation executable closes the earlier revision gap.
The timed executable does not install the counting allocator.
The final allocation matrix repeats all 108 cells three times with identical results.

For the cold empty C1 workload:

| Route | Allocations | Reallocations | Deallocations | Requested peak growth |
|---|---:|---:|---:|---:|
| Direct | 62 | 9 | 14 | 75744 bytes |
| Selected | 64 | 9 | 14 | 82848 bytes |
| Auto | 70 | 9 | 20 | 113189 bytes |

For four warmed cohorts with full fragments:

| Workload | C | Exchanges | Allocations | Reallocations | Deallocations |
|---|---:|---:|---:|---:|---:|
| Empty / 128-byte response | 1 | 4 | 16 | 2 | 16 |
| Empty / 128-byte response | 128 | 512 | 2128 | 2 | 2128 |
| 1MiB duplex | 1 | 4 | 520 | 2 | 520 |
| 1MiB duplex | 8 | 32 | 4216 | 2 | 4216 |
| 1MiB duplex | 128 | 512 | 77032 | 2 | 77032 |

The warmed event counts match across the three routes for corresponding full-fragment cases.
The two reallocations still reflect bounded tombstone-queue growth.
They do not imply growth without a bound.
The maximum requested-live peak across the allocation matrix is **610546 bytes**.

The fresh retention runs use the same connection through 10000 empty cohorts.
All routes plateau at cohorts 2000, 5000, and 10000.
The following table subtracts the pre-construction process baseline from requested live bytes:

| C | Direct retained bytes | Selected retained bytes | Auto retained bytes |
|---|---:|---:|---:|
| 1 | 128836 | 135940 | 135940 |
| 8 | 152028 | 159132 | 159132 |
| 64 | 353180 | 360284 | 360284 |
| 128 | 583068 | 590172 | 590172 |

Harness vector lengths and capacities remain unchanged after cohort 8.
All later plateau intervals have zero reallocations and zero net requested-live growth.
Every run returns to its pre-construction baseline after shutdown and pair destruction.
Allocations and deallocations still occur during each exchange.
Bounded retained storage does not mean allocation-free operation.

The [allocation data](evidence/allocations/summary.json) includes every count and boundary.
The [retention data](evidence/retention-runs.jsonl) includes every interval and harness snapshot.
The earlier [source and debugger attribution](../retention/README.md) identifies the bounded tombstone queues and map.
This final run reproduces the plateau on the integrated API source.

**These values are not RSS.**
They exclude allocator rounding, metadata, caches, stack storage, static payload storage, code pages, and internal reallocation overlap.
They include both endpoints and application scratch storage.
Differences in requested heap bytes do not establish differences in total process memory.
No allocation-reduction or RSS claim follows from this report.

## Fresh profiles and copies

[COPIES.md](COPIES.md) reports current client, server, and shared stacks.
It also records current-binary copy addresses, sizes, and near-site samples.
Large-body profiles contain substantial transport-copy and body-comparison work.
The 144-byte server progress move remains sampled.
The historical rejected forwarding PoC was not repeated.

No old profile address supplies evidence for this binary.
The original binaries, profile data, demangled stacks, symbol table, and disassembly remain in the private frozen build directory.
The repository retains compact summaries and hashes.

## Identity and replay

```text
Source: 5505efc469c7939c43d6d42a2ecb10b94ab4d9e4
Build directory: /workspace/kimojio-rs/target/http2-program/build-http2-final
Frozen directory: /workspace/kimojio-rs/target/http2-program/build-http2-final/frozen-5505efc4

composition_bench SHA256:
861d584ad7c355cbf89eddbb43b74f487e9ee6431ed44d3d7c8135f5f7c2d692

allocation_probe SHA256:
ca2c891fe504309b73f01196abede2e98a5b3482c266b128485516098c723b30
```

The compiler is Rust1.98.1 with LLVM22.1.8.
The release build uses `CARGO_PROFILE_RELEASE_DEBUG=2`.
`RUSTFLAGS` and `CARGO_ENCODED_RUSTFLAGS` are unset.
The [freeze record](evidence/freeze.json) contains exact commands, source hashes, and CPU topology.

Builds, tests, allocation runs, retention runs, and report analysis use CPUs8–31.
Timings and profiles used the exclusive CPU2 lease, in separate runs.
CPU affinity does not isolate package frequency, memory bandwidth, or unrelated host activity.
Trial ranges retain that uncertainty.

After a new CPU2 lease, replay the timing matrix into a separate directory:

```sh
taskset -c 8-31 python3 \
  kimojio-fsm-http2/examples/composition_bench_support/trials.py \
  /workspace/kimojio-rs/target/http2-program/build-http2-final/frozen-5505efc4/composition_bench \
  docs/http2-performance/final/replay-timings --stop-on-failure
```

The existing CPU2 lease is no longer active.
