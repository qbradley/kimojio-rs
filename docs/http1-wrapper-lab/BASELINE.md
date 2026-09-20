# Original wrapper baseline

## Frozen subject

The benchmark source is `58e304d811335952167809914e85dce6015fadf6` (`ynznrvsu`).
Its wrapper and runtime implementations match the starting revision `05545524`.
The benchmark, comparison runner, and allocation probe are new.

The compiler is Rust 1.98.1.
The release build includes debug information through `CARGO_PROFILE_RELEASE_DEBUG=2`.
The binary is `target/wrapper-lab/frozen/original/keepalive_bench`.
Its SHA-256 is `993602a3ba20bd884e7c77ad23ef3fa05fca1afe1d17e5afa3dbd2ca92d8eade`.

The local manifest records the full compiler output and exact build command:
`target/wrapper-lab/original-manifest.json`.
The comparison contract is in [BENCHMARK.md](BENCHMARK.md).

## Initial timings

These are three sequential trials on CPU 2, in randomized workload order.
Other program builds and tests used CPUs 8-31.
The host is shared. Affinity does not reserve a CPU.

All 18 rows passed complete payload, count, reuse, and shutdown checks.
Each row uses one established socket pair for every warmup and measured exchange.
Both wrapper endpoints share one runtime.
The units are microseconds per complete client/server exchange, not server-only throughput.

| Workload | Median | Trial range |
| --- | ---: | ---: |
| Empty request and response | 30.680 | 28.483-32.407 |
| Empty request, 128-byte response | 44.414 | 42.768-54.102 |
| 128-byte request and response | 57.372 | 55.903-57.626 |
| 64-KiB request and response | 164.528 | 137.732-168.488 |
| Chunked 1-MiB request and response | 2322.966 | 2205.019-2807.651 |
| Chunked 8-KiB bodies, 512-byte source frames | 390.441 | 379.692-393.334 |

The complete local result is `target/wrapper-lab/original-comparison.json`.
The ranges are substantial in some cases.
These initial results establish a baseline, not a final statistical comparison.
Final comparisons must rerun this same frozen binary alongside the candidates.

## Allocation result

The probe uses the original benchmark entry point and the existing counting allocator.
The two runs use 100 warmup exchanges and either 1,000 or 3,000 measured exchanges.
Both use an empty request and a 128-byte response.

| Measured exchanges | Allocations | Zeroed allocations | Reallocations | Peak live requested bytes |
| --- | ---: | ---: | ---: | ---: |
| 1,000 | 357,376 | 2 | 5,565 | 130,803 |
| 3,000 | 1,006,787 | 2 | 15,669 | 130,803 |

The resulting slope is **329.7575 allocator calls per exchange**.
This includes both endpoints and the benchmark application, not only one wrapper.
The counters include startup, warmup, shutdown, and reporting before the final allocator record.
The two-point slope is not a universal constant.
Neither run reported an allocation failure.
Instrumented elapsed time is not a performance comparison.

Raw records are `target/wrapper-lab/original-alloc-{1000,3000}.{json,stdout,stderr}`.

## Profile observations

The profile uses the exact uninstrumented frozen binary.
It records userspace CPU samples at 999 Hz with DWARF stacks.
Recording starts after a 300-ms delay.
The application performs 5,000 warmup and 100,000 measured exchanges.
The benchmark result is valid and the profile reports no lost samples.

| Leaf | Self samples |
| --- | ---: |
| `next_input` polling closure | 10.74% |
| `WaitAsyncEventFuture::poll` | 10.12% |
| HTTP FSM `next` | 7.71% |
| `malloc` | 4.25% |
| `next_input` async state | 4.23% |
| Wait unregistration | 2.82% |

Approximately 2.98% of samples identify allocation of `WaitData`.
That figure is part of the `malloc` total, not an additional category.
Shared leaves do not establish which endpoint owns their cost.
The profile-guided PoC performs the detailed stack and instruction analysis.

The first hypothesis is excessive creation and removal of wait registrations around immediately runnable protocol work.
The evidence does not put large payload copies first for this small-response workload.
A proposed change still needs failure-path coverage and measured improvement.

Profile artifacts:

- `target/wrapper-lab/original-small.perf.data`
- `target/wrapper-lab/original-small.perf-report.txt`
- `target/wrapper-lab/original-profile-small.json`

## Shared prerequisite baseline

Change `upnvvwuq` combines the benchmark with five independent prerequisite commits:

| Prerequisite | Source commit |
| --- | --- |
| Wrapped-waker cancellation | `eb7bb6c5bb5193a037be591588b87d2164cf4218` |
| Core duplex policy | `af49a1a00da31fb92b2d31576d467cc87a85ca5f` |
| Lease-preserving output | `6216d9d42073182f0d69849a9826f613f1ff270e` |
| Exact native transport | `8507a00f419d55b9cca9ed524988a3c396a49cee` |
| Explicit wrapper duplex policy | `238c439242576917524cc3aff1ae6d237cef117e` |

The shared benchmark supports generic and native backends.
It also supports reusable duplex echo with either receive leases or vector copies.
The original six workloads retain their payload patterns, source bounds, comparison, and timing boundaries.
The new entry point boxes both connection futures during setup.
That differs from the frozen original entry point, which does not box those two futures.
The matched common-baseline PoCs all use the same entry point.

The integrated baseline passed 162 core/wrapper tests and doctests with all features.
It also passed 13 scope tests, four benchmark tests, and six comparison-runner tests.
The benchmark tests exercise both backends, both request framing modes, and duplex lease/copy paths.
`cargo fmt`, `cargo clippy`, and `cargo clippy --all-targets --all-features` completed.
Clippy reports only existing warnings in the static-file example and `kimojio/src/pipe.rs`.

No scheduling or eager-body optimization forms part of this baseline.
The four PoCs must compare their deltas against this common behavior.

## Infrastructure lesson

Git worktree commits need a reachable Git ref before jj imports them.
Removing temporary import tags with automatic abandonment enabled can abandon their imported ancestry.
The initial cleanup attempt caused this effect.
Restoring the exact preceding jj operation recovered the complete graph without a source change.

Import-tag cleanup must use `jj --config 'git.abandon-unreachable-commits=false' git import`.
This is a per-command safeguard, not a global repository configuration change.
Existing bookmarks and unrelated worktrees remain untouched.
