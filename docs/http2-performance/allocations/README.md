# HTTP/2 allocation evidence

This report adds allocation evidence for frozen integration `2c8d1d48a880c4dec70c8d5feeacbf45dfc8d359`.
It does not add a timing result or an optimization.
The probe source is `9bc026a56e7b9090225e61217f4524ad7082ffb6`.
The production source, resource limits, workspace manifests, and dependencies remain unchanged.
CPU2 remained unleased and unused.

## Acceptance evidence

The separate `allocation_probe` example uses the existing strict in-memory executor.
Only this example installs the counting global allocator.
The timed `composition_bench` example retains its original allocator.
Both examples use the same payload comparisons, headers, status assertions, typed receipts, retirement checks, and shutdown procedure.
The transport still copies directly between owned write slices and read pages.

All **324 runs passed across 108 cells**.
Each cell has three independent process runs with identical allocation counts and workload results.
No cell failed or was skipped.
Every run returned to its requested-live baseline after shutdown and pair destruction.

| Requirement | Evidence |
|---|---|
| Cold cohorts | Construction, scratch storage, protocol startup, and one complete cohort |
| Warmed cohorts | Four complete cohorts after eight warmup cohorts on the same connection |
| Routes | Direct H2, selected composite H2, auto-server detection |
| Workloads | Empty requests with 128-byte responses, 4096-byte duplex, 32KiB duplex, 1MiB duplex |
| Concurrency | 1, 8, 64, 128 for every workload and route |
| Fragmentation | 65536-byte transport fragments for all workloads, plus 17-byte duplex fragments at concurrency 1 and 8 |
| Event counts | Successful allocation, zeroed-allocation subset, successful reallocation, deallocation |
| Requested storage | Live bytes at both boundaries, peak live bytes, peak growth, signed live change |
| Cross-boundary correctness | Four deterministic allocator controls |
| Original workload assertions | All active, including exact payload bytes and typed operation settlement |

The [complete count table](evidence/counts.csv) contains all 108 cells.
The [JSON summary](evidence/summary.json) also contains exact wire and payload totals.
The [individual runs](evidence/trials.jsonl) include each command and binary hash.
The [freeze record](evidence/freeze.json) contains source hashes and build identity.

## Measurement boundaries

The cold window starts before either endpoint or application scratch storage exists.
It ends after one complete cohort retires on both endpoints.
The cold window excludes shutdown and JSON output.

The warmed window starts after eight complete cohorts.
It ends after four additional cohorts retire on both endpoints.
Construction, detection, warmup, shutdown, and JSON output stay outside that window.
Their allocations still contribute to continuous live-byte accounting.

The global allocator records all Rust allocation requests throughout the process lifetime.
The window changes event baselines and resets its peak to current live bytes.
It never resets the live-byte ledger.
Thus, a deallocation inside the window can correctly release storage that predates the window.

An allocation counts only after `System` returns a non-null pointer.
A successful reallocation replaces the old requested size with the new requested size.
A failed reallocation leaves the old live-byte count unchanged.
Deallocation removes the layout size after `System` returns.
Checked ledger arithmetic records an error instead of underflow or overflow.
The probe refuses a result from an invalid ledger.

The allocator serializes calls and ledger updates with an allocation-free mutex.
Each measured process runs one synchronous workload without worker threads.
The counter and its mutex change execution costs.
Those costs are not timing evidence.

## Deterministic controls

The controls use isolated allocator instances, not the global test-runner allocator.
Concurrent test-runner activity cannot change their expected counts.

| Control window | Allocations | Reallocations | Deallocations | Live start → end | Peak live | Peak growth |
|---|---:|---:|---:|---:|---:|---:|
| Zeroed 32 bytes, grow to 80, shrink to 16, free | 1 | 2 | 1 | 0 → 0 | 80 | 80 |
| Free earlier 64 bytes, allocate 16, free | 1 | 0 | 2 | 64 → 0 | 64 | 0 |
| Earlier-window 24 bytes, grow to 48, free | 0 | 1 | 1 | 24 → 0 | 48 | 24 |
| Hold earlier 24 bytes, no operations | 0 | 0 | 0 | 24 → 24 | 24 | 0 |

The second control proves that zero peak growth does not mean zero allocation activity.
The fourth control proves that zero events do not mean zero live storage.
The first control also checks every zeroed byte.

## Observed counts

The cold empty workload at concurrency 1 produced:

| Route | Allocations | Reallocations | Deallocations | Requested live at end | Requested peak live | Peak growth |
|---|---:|---:|---:|---:|---:|---:|
| Direct | 62 | 9 | 14 | 76528 | 76580 | 75744 |
| Selected | 64 | 9 | 14 | 83634 | 83686 | 82848 |
| Auto | 70 | 9 | 20 | 83630 | 114023 | 113189 |

All byte values describe requested storage, not allocator capacity.
Absolute live values include small pre-window process allocations.
The freeze preserves the exact executable path and arguments that produced these values.

The warmed full-fragment workloads produced these counts in all three routes:

| Workload | Concurrency | Exchanges | Allocations | Allocations per exchange | Reallocations | Deallocations | Peak growth |
|---|---:|---:|---:|---:|---:|---:|---:|
| Empty / 128-byte response | 1 | 4 | 16 | 4 | 2 | 16 | 116 |
| Empty / 128-byte response | 8 | 32 | 128 | 4 | 2 | 128 | 616 |
| Empty / 128-byte response | 64 | 256 | 1060 | 4.140625 | 2 | 1060 | 6404 |
| Empty / 128-byte response | 128 | 512 | 2128 | 4.15625 | 2 | 2128 | 13339 |
| 1MiB each direction | 1 | 4 | 520 | 130 | 2 | 520 | 220 |
| 1MiB each direction | 8 | 32 | 4216 | 131.75 | 2 | 4216 | 1396 |
| 1MiB each direction | 64 | 256 | 38500 | 150.390625 | 2 | 38500 | 15316 |
| 1MiB each direction | 128 | 512 | 77032 | 150.453125 | 2 | 77032 | 31732 |

The three routes have identical warmed event counts for matching full-fragment cells.
Their absolute requested-live values differ.
Equal event counts do not establish equal allocator CPU cost or equal storage.
No historical allocation baseline exists for a reduction claim.

### Requested storage still grows within warmed windows

All 54 warmed cells include two reallocations.
Their requested-live increase is exactly `64 * concurrency` bytes between the stated boundaries.
That increase ranges from 64 to 8192 bytes.
The largest requested-live peak across the full matrix is 610551 bytes.

These snapshots do not establish a retained-storage plateau or an unbounded growth rate.
This work does not identify the internal owner of those reallocations.
Complete shutdown and pair destruction return every run to its pre-construction requested-live baseline.
That observation does not remove the live-connection storage question.
The final acceptance review must not replace these observations with a zero-allocation or zero-growth claim.

## Exclusions and interpretation limits

- The meter counts Rust global allocator requests, not all native allocator activity.
- Requested byte counts exclude allocator rounding, metadata, caches, fragmentation, and RSS.
- A reallocation contributes its resulting requested size, not temporary old-plus-new storage inside the system allocator.
- The static 32768-byte producer payload creates no heap allocation.
- Application-owned dynamic payload creation is outside this workload.
- Stack storage, static storage, and code pages are outside the live-byte ledger.
- Endpoint storage, owned receive pages, headers, and application scratch vectors are inside the ledger.
- Counts describe both endpoints and the strict executor together, not the core alone.
- Cold and warmed event counts have different construction and exchange boundaries.
- The separate overlap regression still passes, but the allocation matrix does not add per-stream fairness assertions.

This evidence fills the allocation-measurement requirement for this immutable integration.
It does not qualify later external-failure API changes or establish the final wrapper gate.
No second copy experiment, source optimization, timing matrix, or profile forms part of this work.

## Tests and source identity

Both default and all-feature release checks pass:

- Allocation probe: **11 passed**, including four counter controls and the strict route smoke test.
- Composition harness: **11 passed**.
- Ownership tests: **5 passed**.
- Total per configuration: **27 passed, 0 failed, 0 ignored**.

`cargo fmt --all`, default clippy, and all-targets/all-features clippy pass.
Both clippy commands use `-D warnings`.
The Python runner parses successfully and completes its full matrix.
All builds, tests, and probe runs use CPUs8–31.

```text
Base:   2c8d1d48a880c4dec70c8d5feeacbf45dfc8d359
Source: 9bc026a56e7b9090225e61217f4524ad7082ffb6
Binary: /workspace/kimojio-rs/target/http2-program/build-http2-allocation/frozen-9bc026a5/allocation_probe
SHA256: a3698d6817fc2ee19a4206aff37029ea06cdbe5dc0cfdd0d4ed603cfdb2405ca
```

The compiler is Rust1.98.1 with LLVM22.1.8.
The release build uses `CARGO_PROFILE_RELEASE_DEBUG=2`.
`RUSTFLAGS` and `CARGO_ENCODED_RUSTFLAGS` are unset.

## Reproduction

From the frozen allocation worktree, build the isolated probe:

```sh
cd /workspace/kimojio-rs/target/http2-program/worktrees/http2-allocation
CARGO_TARGET_DIR=/workspace/kimojio-rs/target/http2-program/build-http2-allocation \
CARGO_PROFILE_RELEASE_DEBUG=2 taskset -c 8-31 \
cargo build --release -p kimojio-fsm-http2 --example allocation_probe
```

Run one cold cohort:

```sh
taskset -c 8-31 /workspace/kimojio-rs/target/http2-program/build-http2-allocation/frozen-9bc026a5/allocation_probe \
  direct empty 1 65536 1 cold
```

Run four warmed cohorts after eight warmup cohorts:

```sh
taskset -c 8-31 /workspace/kimojio-rs/target/http2-program/build-http2-allocation/frozen-9bc026a5/allocation_probe \
  direct 1m 8 65536 4 steady
```

For a complete replay, create a separate evidence directory:

```sh
mkdir -p docs/http2-performance/allocations/replay
taskset -c 8-31 python3 \
  kimojio-fsm-http2/examples/composition_bench_support/allocation_trials.py \
  --binary ../../build-http2-allocation/frozen-9bc026a5/allocation_probe \
  --output docs/http2-performance/allocations/replay/trials.jsonl \
  --summary docs/http2-performance/allocations/replay/summary.json
```

The runner stops on a workload failure, residual requested storage, or unequal repeated counts.
It does not collect elapsed-time fields.
