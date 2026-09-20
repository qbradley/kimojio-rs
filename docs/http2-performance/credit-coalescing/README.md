# Credit-coalescing baseline and private forwarding experiment

This report measures immutable integration `beb291573508a7f69b081d4ca5c4e3dbeb95dc2a`.
It does not cover later API changes or establish the final wrapper gate.
The baseline includes core credit correction `c08b733358317351dc4338f951fee239f1a1135a`.
The application configuration, payload assertions, and resource limits remain unchanged.

## Decision and measured results

The baseline completes **945 successful trials across 189 cells**, with no failed or skipped cell.
The two former failure reproducers also pass unchanged.
The isolated forwarding PoC is **not accepted**.
It produces a sampled 232-byte aggregate return copy instead of eliminating the staging cost.
All six paired timing intervals include a ratio of 1.0.
The baseline remains unchanged.

At concurrency 128, empty/full-fragment medians are 2.693µs direct, 2.753µs selected, and 2.693µs auto.
The selected median is 2.2% above direct.
This does not establish a universal small-overhead bound.
The 4096-byte duplex case with 17-byte fragments shows 5.9% selected and 6.8% auto median overhead at concurrency 1.
Those trial ranges do not overlap the direct range.
Routing costs increase with some callback-heavy workloads.

The formerly failing 1MiB duplex case at concurrency 8 now completes in all three modes.
Its medians are 154.705µs direct, 172.966µs selected, and 161.133µs auto.
This cell has wide trial ranges.
Its percentages are observations, not precise overhead limits.

Cold full-fragment construction medians are 7.924µs direct, 7.978µs selected, and 9.856µs auto.
The auto construction median is 24.4% above direct.
The measurement includes both preconstructed protocol children, startup, and one exchange.
It is not detection alone.

[RESULTS.md](RESULTS.md) contains the five-trial medians and ranges.
[COPIES.md](COPIES.md) contains the fresh profiles and the rejected PoC.
[POC-RESULTS.md](POC-RESULTS.md) contains the nine-pair timing results.

## Source and binary identity

The baseline harness revision is `eaac15fa2c52ba9ff86f193784485a7d5e48e035`.
It removes the ignore attribute only after the unchanged large-duplex reproducer passes.
It also adds fail-fast behavior to the timing runner.
No core implementation change forms part of that harness revision.

The baseline binary remains at:

```text
/workspace/kimojio-rs/target/http2-program/build-http2-performance-credit/frozen-eaac15f/composition_bench
```

`evidence/baseline-freeze.txt` records its SHA256, compiler, build command, and source hashes.
The baseline binary SHA256 is `69daef245f511790cca00741884b8cb52db58d5bf2b8a26491dce5b780e90977`.
The release build uses `CARGO_PROFILE_RELEASE_DEBUG=2` and no `RUSTFLAGS`.
Builds and tests use CPUs8–31.
Timings and profiles use the exclusive CPU2 lease.
Profiles run separately from noninstrumented trials.

The historical binaries, profiles, and negative controls remain unchanged in the parent report directories.
Their addresses do not identify sites in this new binary.

## Correctness prerequisites

The unchanged large-duplex reproducer passes on the imported integration.
The normal harness suite then reports **7 passed, 0 failed, 0 ignored**.
Both historical strict reproducers now run as normal tests.
Formatting and both required clippy modes pass.
The release core suites pass 284 tests by default and 289 with all features, including three documentation tests in each total.
The isolated PoC passes the same suites and all seven harness tests.

The complete matrix covers direct, selected, and auto routes.
It retains the five workloads, four concurrency levels, and three transport fragments from the original report.
The separate construction phase still includes startup and the first exchange, but excludes teardown.
Steady-state cells still exclude construction, detection, and eight warmup cohorts.

The runner requires five alternating trials for each cell.
It stops immediately on any assertion or process failure.
It refuses diagnostic instrumentation during timings.
The body comparisons, exact status and field assertions, sent receipts, receive ends, and retirement counts remain active.

## Interpretation limits

The static producer isolates protocol and executor costs from per-buffer producer allocation.
The executor still copies transport bytes into owned read pages and compares every delivered body byte.
Those costs are not exclusively client or server protocol costs.
The result does not measure socket throughput or arbitrary application payload creation.
This experiment does not measure allocation counts or a retained-memory plateau.

CPU affinity does not isolate shared memory bandwidth or package frequency changes.
The paired experiment therefore uses alternating binary order and reports trial ranges.
A small median difference alone is not an accepted improvement.
A profile-supported copy removal also does not prove an elapsed-time improvement by itself.
The PoC has 108 successful timed runs and 54 exact baseline/candidate wire-count comparisons.
Its paired medians range from -0.21% to -1.45%, but every reported interval includes a ratio of 1.0.
The compiler also replaces the original move with a larger sampled return copy.
These facts reject acceptance of this attempt as an optimization.

The broader core gate remains a parent decision after final integration.
No stable-slot rewrite or header-finalization optimization forms part of this experiment.
The CPU2 lease is released.

## Replay

After a new CPU2 grant, run the frozen baseline matrix:

```sh
taskset -c 8-31 python3 kimojio-fsm-http2/examples/composition_bench_support/trials.py \
  /workspace/kimojio-rs/target/http2-program/build-http2-performance-credit/frozen-eaac15f/composition_bench \
  docs/http2-performance/credit-coalescing/evidence \
  --stop-on-failure
```

For a separate replay, use a new evidence directory to retain the original results.
The repository stores compact evidence and hashes.
The original binaries, DWARF data, full demangled stacks, symbol tables, and disassembly remain in their frozen build directories.
