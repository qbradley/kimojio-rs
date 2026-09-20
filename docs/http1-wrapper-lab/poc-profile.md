# Profile-guided wrapper experiment

## Current result

The final-base PoC retains synchronous channel probes only on runnable driver turns.
Its production source is `12e58e137245628c11c92bd05e7cad8462fcff71`, based on common prerequisite `0bd3e950f4b0aedb552fbfd8fab96139ef399bb9`.
All public APIs and prerequisite ownership rules remain unchanged.
Two further scheduler candidates were measured and rejected.
The final comparison passed 180 runs across generic/native backends and nine workloads.
Detailed final-base evidence appears at the end of this report.
Earlier sections describe separate experiments on older prerequisites.

## Frozen baseline

The original binary and this worktree have different source revisions.
All original addresses below refer to `58e304d811335952167809914e85dce6015fadf6`.
The initial worktree revision is `5b2bd2c198a908e3e4676b13bfdb30ae6c3dbbe6`.
Production changes wait for the final common base.

- Binary: `/workspace/kimojio-rs/target/wrapper-lab/frozen/original/keepalive_bench`
- SHA-256: `993602a3ba20bd884e7c77ad23ef3fa05fca1afe1d17e5afa3dbd2ca92d8eade`
- Build: `CARGO_PROFILE_RELEASE_DEBUG=2 CARGO_TARGET_DIR=target/wrapper-lab/build-original cargo build --release -p kimojio-http1 --example keepalive_bench --offline`
- Profile: `/workspace/kimojio-rs/target/wrapper-lab/original-small.perf.data`
- Record: `perf record -q -e cpu-clock:u -F999 --call-graph dwarf,16384 --delay300 -- taskset -c2 BINARY --iterations100000 --warmup5000 --response-bytes128`

The benchmark measures complete client/server exchanges over a socketpair.
Both endpoints share one runtime and core.
It includes cleanup, full byte equality, and exact same-connection exchange counts.
These results are not external server QPS.

The original three-trial results, in microseconds per exchange:

| Workload | Original |
| --- | ---: |
| Small | 44.414 |
| Empty | 30.680 |
| POST small | 57.372 |
| Large | 164.528 |
| Chunked, 1 MiB each direction | 2322.966 |
| Fragmented, 512 bytes | 390.441 |

## Hottest stacks

The existing profile contains 4,188 samples and no lost samples.
Shared leaves dominate the result:

| Leaf or stack | Self time |
| --- | ---: |
| `next_input` poll closure | 10.74% |
| `WaitAsyncEventFuture::poll` | 10.12% |
| FSM `Core::next` | 7.71% |
| `malloc` | 4.25% |
| `next_input` async body | 4.23% |
| `AsyncEventSource::unregister` | 2.82% |
| `cfree` | 2.65% |

The `WaitData` allocation stack accounts for approximately 2.98% of all samples.
Its destructor accounts for approximately 1.79% through `cfree`.
These are allocation and wait-registration costs, not payload copies.
The supplied allocator experiment measured approximately 329.76 Rust allocator calls per complete exchange.

Conservative ancestor attribution identifies 221 client samples, 301 server samples, and 3,666 shared or unresolved samples.
The rule requires a client `Connection` ancestor or a server `driver` specialization with a `run_pair` ancestor.
Truncated or ambiguous stacks remain shared.
Thus these counts are lower bounds for endpoint attribution, not endpoint CPU shares.

The hottest identified server stack reaches `Core::receive_metadata` through `Server::next` and the server `drive` specialization (45 leaf samples).
The hottest identified client leaf is the client `drive` specialization (25 samples).
The client also reaches `receive_metadata` (22 leaf samples).
The shared `next_input` closure alone has 257 inline-leaf samples.
There is insufficient ancestry to charge that shared work to only one endpoint.

## How copies were found (MIR vs objdump vs perf)

`nm -C -S` locates the input poll at file address `0x6bc80`, size `0x1425`.
`objdump -d -C` shows explicit `memcpy` calls in that range.
Lengths come from `edx`, not Rust type-size assumptions.
`perf script -F comm,ip,sym,symoff,dso` supplies symbol-relative offsets.
No runtime address subtraction or current-tree line attribution is necessary.
MIR is unnecessary for this inventory because the binary exposes the calls directly.

## Copy inventory

All sites below belong to `driver::next_input`'s `PollFn::poll` in `kimojio-http1/src/driver.rs`.
The parent appears in 503 sample stacks.
The hit column counts samples within `0x18` bytes of each call.
Nearby counts can overlap and do not measure time inside libc.

| Site offset | Size | Hit | Kind | Necessary |
| --- | ---: | ---: | --- | --- |
| `+0x13b5` | 272 | 20 | move of selected input | Ownership transfer required, staging not inherently required |
| `+0x74b` | 183 | 14 | move of selected input | Same |
| `+0xd9d` | 183 | 6 | move of selected input | Same |
| `+0x1392` | 263 | 6 | move of selected input | Same |
| `+0xdc2` | 183 | 2 | move of selected input | Same |
| `+0x98b`, `+0xd6b`, `+0x1225`, `+0x127c`, `+0x1353` | 184–264 | 1 each | move of selected input | Same |
| `+0x1257` | 263 | 0, cold | move of selected input | Same |

Only one sample has a `memcpy` leaf.
Its ancestors include `realloc` and `RawVecInner::finish_grow`, without a reliable endpoint owner.
This profile does not support a payload-copy redesign as the first experiment.

## Proposals in order

1. Avoid wait registration while the FSM can advance synchronously.
   The original `drive` calls `next_input` after each callback.
   The selector constructs receive and cancellation futures before it knows whether suspension is necessary.
   Polling those futures registers waits that the next callback immediately discards.
   A synchronous channel probe can consume ready input without a wait allocation.
   A separate pending path must register every applicable wake source before suspension.
2. Preserve receive waits across callbacks if the first change leaves registration costs dominant.
   Channel lifetimes extend across the entire driver loop.
   Active cancellation needs a separate lifetime because exchanges replace its token.
   Extra state and completed-future handling are costs of this alternative.
3. Reduce intermediate input moves only if the next profile still attributes material time to those sites.
   Owned inputs must remain alive through `State::input`.
   The original profile gives this lower priority than wait churn.

Required tests cover cancellation, source lifecycle, input credit, early responses, leases, write errors, reuse, and custom transports.
The common base also requires lease forwarding and explicit duplex opt-in.
Benchmark fixtures and correctness assertions remain unchanged.

## What not to cut first

- Payload ownership and leases: these are correctness constraints.
- Protocol callbacks or error handling based only on struct size.
- Metadata copies without stronger sample evidence.
- Time observations without deadline and fairness evidence.

## Experiment plan

The first implementation will retain the public API and the existing FSM interface.
It will use the final common base and a private build directory.
CPU affinity for compilation and tests is `8-31`.
Timing and new profiles require an exclusive slot from the experiment coordinator.
No new timing run occurred during this initial analysis.

## Candidate 1: synchronous receive probes on runnable turns

The first code candidate uses prerequisite base `90b7ff60`.
`poll_receive` calls `try_recv` during a runnable turn.
It polls the original receive future only on a turn that can suspend.
Cancellation waits follow the same rule.
Already-cancelled tokens remain immediately observable.

The selector retains all ten input classes and its rotating priority.
Handler and timer polling remain unchanged.
Source polling still requires a non-runnable core.
Thus protocol notifications can revoke source capacity before the next source poll.
Receive futures still exist on the stack, but runnable probes do not allocate their wait registrations.

The change does not modify body storage, lease settlement, transport constructors, or I/O workers.
Two new tests cover the synchronous probe and the transition to a registered wait.
The latter test sends after a cooperative yield and requires the waiting receive to resume.

### Functional and lint results

All commands ran from the isolated profile worktree.
Cargo used `CARGO_TARGET_DIR=/workspace/kimojio-rs/target/wrapper-lab/build-profile-poc`.

```sh
taskset -c 8-31 cargo fmt
taskset -c 8-31 cargo test --offline -p kimojio-http1 -p kimojio-fsm-http1
taskset -c 8-31 cargo clippy --offline
taskset -c 8-31 cargo clippy --offline --all-targets --all-features
```

The wrapper/core suites passed 144 tests, including four documentation tests.
Both clippy commands succeeded.
The pre-existing warnings are `question_mark` at `examples/http1-static/src/app.rs:413` and two `byte_char_slices` warnings at `kimojio/src/pipe.rs:64-65`.
The candidate introduced no clippy warning.

Additional commands passed:

```sh
taskset -c 8-31 cargo test --offline -p kimojio-http1 -p kimojio-fsm-http1 --all-features
taskset -c 8-31 cargo test --offline -p kimojio cancellation_token
taskset -c 8-31 cargo test --offline -p kimojio --lib io_scope
```

These commands passed 146, two, and 12 tests respectively.
The I/O-scope suite includes wrapped-waker settlement and sibling cancellation isolation.
Local logs and allocator output reside in `target/profile-artifacts/` inside this worktree.

### Allocation experiment

The existing `alloc-http1-keepalive` binary includes the unchanged benchmark application.
The following commands are allocation experiments, not timing evidence:

```sh
CARGO_PROFILE_RELEASE_DEBUG=2 taskset -c 8-31 cargo build --release --offline \
  -p fsm-allocation-probes --bin alloc-http1-keepalive
taskset -c 8-31 BUILD/release/alloc-http1-keepalive \
  --iterations 1000 --warmup 100 --response-bytes 128
taskset -c 8-31 BUILD/release/alloc-http1-keepalive \
  --iterations 3000 --warmup 100 --response-bytes 128
```

`BUILD` is the Cargo target directory stated above.
An initial invocation used unsupported `--output` and exited before the workload.
The corrected commands use standard output and completed all benchmark assertions.

| Iterations | Allocations | Zeroed allocations | Reallocations |
| --- | ---: | ---: | ---: |
| 1,000 | 162,828 | 2 | 5,564 |
| 3,000 | 458,593 | 2 | 15,668 |

The slope is 152.9345 allocator calls per exchange.
The frozen original slope is approximately 329.76.
That comparison spans prerequisite changes, so it does not isolate this scheduler delta.

The same commands also ran against exact prerequisite `90b7ff60`.
Only this worktree changed revisions for the control run.

| Iterations | Control allocations | Control zeroed allocations | Control reallocations |
| --- | ---: | ---: | ---: |
| 1,000 | 357,373 | 2 | 5,564 |
| 3,000 | 1,006,784 | 2 | 15,668 |

The same-base control slope is 329.7575 calls per exchange.
The candidate removes 176.823 calls per exchange, approximately 53.62%.
Requested allocation bytes decrease from 22,837.204 to 10,105.948 per exchange.
These figures include both endpoints and runtime work.
They are not payload-copy counts.
Final-common-base timing and profiling remain necessary.

### Review and next decision

A runnable turn returns immediately even after an empty probe.
It cannot sleep without a registered wake source because it cannot sleep at all.
A subsequent non-runnable turn uses the original receive futures, which probe their channels before registration.
No await occurs between that channel probe and its registration.

This candidate deliberately does not retain completed receive futures across inputs.
It avoids the lifetime and terminal-state complexity of persistent waits.
The next profile will determine whether remaining registration costs justify that additional state.

The prepared candidate benchmark uses the required debug-enabled release build.
Its source is candidate commit `781d5808`, before this results-only documentation update.
Binary: `/workspace/kimojio-rs/target/wrapper-lab/build-profile-poc/release/examples/keepalive_bench`.
SHA-256: `1de1bd29352289abf197365cae10d9e739664d9e25d57e9ff78b56a0d5efff53`.
The final common base will require a new build and new binary identity.

## Exclusive-slot measurements on prerequisite `90b7ff60`

The coordinator granted exclusive CPU2 use for this series.
All three binaries use release optimization and `CARGO_PROFILE_RELEASE_DEBUG=2`.
Frozen binaries reside in `target/profile-artifacts/frozen/`.

| Binary | Revision | SHA-256 |
| --- | --- | --- |
| `control90b` | `90b7ff60` | `2ba8e6eb91ad714e33d84dacfd069f2aa699d0ae6827557122c6487dda4b349d` |
| `candidate1` | `781d5808` | `1de1bd29352289abf197365cae10d9e739664d9e25d57e9ff78b56a0d5efff53` |
| Frozen original | `58e304d8` | `993602a3ba20bd884e7c77ad23ef3fa05fca1afe1d17e5afa3dbd2ca92d8eade` |

The shared runner command was:

```sh
python3 perf/wrapper-lab/compare.py \
  --manifest target/profile-artifacts/manifest1.json \
  --output target/profile-artifacts/comparison1.json --trials 3 --cpu 2
```

All 54 runs passed the unchanged benchmark assertions and runner requirements.
The order uses the default seed `20260920`.
The table reports medians in microseconds per complete exchange.

| Workload | Original58e | Same-base control90b | Candidate 1 |
| --- | ---: | ---: | ---: |
| Empty | 32.190 | 34.174 | 28.219 |
| Small | 47.821 | 46.465 | 40.073 |
| POST small | 55.042 | 57.564 | 48.155 |
| Large | 160.162 | 169.142 | 125.070 |
| Chunked | 2307.615 | 2458.588 | 2029.504 |
| Fragmented | 392.476 | 421.923 | 349.281 |

Trial ranges are material.
For example, candidate fragmented results span 319.245–474.860 microseconds, and original fragmented results span 358.711–610.615.
These medians support another scheduler experiment, not a precise causal speedup claim.
The same-base control isolates the code delta better than the historical original.
Neither comparison includes the upcoming raw transport and duplex prerequisites.

### Candidate 1 profile

```sh
perf record -q -o target/profile-artifacts/candidate1.perf.data \
  -e cpu-clock:u -F 999 --call-graph dwarf,16384 --delay 300 -- \
  taskset -c 2 target/profile-artifacts/frozen/candidate1 \
  --iterations 100000 --warmup 5000 --response-bytes 128 \
  --json target/profile-artifacts/candidate1-profile.json
```

There were no lost samples.
The next-input poll remains the hottest leaf at 10.10%.
Event waits remain at 5.94%, `malloc` at 2.49%, and unregister at 1.09%.
These are shared leaves unless a stack supplies an endpoint ancestor.
This profile supports a second bounded iteration before persistent wait storage.

The second hypothesis concerns a blocked core with an already-populated completion channel.
Candidate 1 can register unrelated waits before its rotation reaches that ready channel.
A ready-probe pass before the blocking registration pass can avoid those short-lived registrations.
Handler, source, and timer futures must not receive duplicate polls in the second pass.

## Candidate 2: ready probes before blocked registration

**The first timing series in this section is superseded by the isolated-build series below.**
The combined allocator/benchmark build enabled an extra runtime feature.

Commit `831c1513` implements the two-pass hypothesis.
The binary SHA-256 is `3893743afb5fe167b9a9ecc7b021577c705bd424bdd12b3977d85a4058c7da62`.
The allocation slope decreased from 152.9345 to 151.6985 calls per exchange.
That is less than one percent.

The shared runner compared candidates 1 and 2 across five trials:

```sh
python3 perf/wrapper-lab/compare.py \
  --manifest target/profile-artifacts/manifest2.json \
  --output target/profile-artifacts/comparison2.json --trials 5 --cpu 2
```

All 60 runs passed.
Medians in microseconds:

| Workload | Candidate 1 | Candidate 2 |
| --- | ---: | ---: |
| Empty | 24.767 | 25.280 |
| Small | 37.904 | 37.150 |
| POST small | 45.299 | 46.888 |
| Large | 120.683 | 121.889 |
| Chunked | 1887.895 | 1853.326 |
| Fragmented | 315.827 | 316.888 |

The second pass does not produce a consistent timing improvement.
Some trial ranges overlap substantially.
Candidate 2 adds a second scan on blocked turns for very little allocation reduction.
The next candidate removes the two-pass scan.
Its regression test remains useful: a ready probe can consume a message after a registered wait without losing channel credit.

Candidate 2 passed formatting, both required clippy commands, and all-feature wrapper/core tests.
Only the previously listed lint warnings occurred.

## Candidate 3: connection-long shutdown waits

The candidate 1 profile still attributes 5.94% to event waits.
The shutdown tokens live for the complete connection, unlike the active-exchange token.
Candidate 3 retains their futures across `next_input` calls on the driver's stack.
This introduces no heap allocation and no changes to active bodies or transport workers.
The futures use `fuse` to make repeated polls after completion safe.
The original single-pass rotation remains.

A preliminary allocation run, before the final `fuse` addition, measured 135.1695 calls per exchange.
The previous candidate 1 slope was 152.9345.
The final fused binary requires its own allocation and timing evidence.

Persistent receive streams remain a separate option.
Safe `unfold` streams can own each receive future without per-message boxes.
However, their lifetimes require separation of receiver ownership from mutable `State`.
That change is larger than retaining the two stable shutdown waits.
This experiment first measures the smaller lifetime change.

## Build-feature audit and corrected comparisons

Building the benchmark and allocator package in one Cargo command unified their runtime features.
The allocator package enables the default `tls` feature, unlike the standalone benchmark package.
The first candidate 2 and candidate 3 timing binaries therefore differed from the specified isolated build.
Those timing series do not isolate scheduling changes.
Their results remain in the local artifacts for audit, not as selection evidence.
Allocator comparisons remain consistent because all allocation runs used the same allocator package.

The corrected binaries each used exactly:

```sh
CARGO_PROFILE_RELEASE_DEBUG=2 \
CARGO_TARGET_DIR=/workspace/kimojio-rs/target/wrapper-lab/build-profile-poc \
taskset -c 8-31 cargo build --release --offline \
  -p kimojio-http1 --example keepalive_bench
```

| Binary | Revision | SHA-256 |
| --- | --- | --- |
| `candidate2-isolated` | `831c1513` | `e9e883a12461d2fbc48538e29ee33b9d09edfe8887cec573506c1cc43549a96c` |
| `candidate3-isolated` | `24872b17` | `f743448c3f4d22f00d8a14186e4e02a3fdfc7f1f311ddde6b846e7ec66504faf` |

The isolated control and candidate 1 hashes remain unchanged.
The corrected shared-runner command was:

```sh
python3 perf/wrapper-lab/compare.py \
  --manifest target/profile-artifacts/manifest-isolated.json \
  --output target/profile-artifacts/comparison-isolated.json --trials 5 --cpu 2
```

All 120 runs passed.
Medians in microseconds:

| Workload | Same-base control90b | Candidate 1 | Candidate 2 | Candidate 3 |
| --- | ---: | ---: | ---: | ---: |
| Empty | 29.049 | 25.408 | 25.014 | 27.263 |
| Small | 43.493 | 36.616 | 37.105 | 37.103 |
| POST small | 53.214 | 45.531 | 52.270 | 48.579 |
| Large | 140.233 | 120.546 | 121.594 | 120.671 |
| Chunked | 2169.500 | 1862.877 | 1857.244 | 1834.795 |
| Fragmented | 408.831 | 316.759 | 340.799 | 369.143 |

The candidate 1 medians improve on the same-base control in every workload.
Neither further iteration improves consistently on candidate 1.
Candidate 3 removes allocations but has worse medians for five of six workloads.
Its fragmented trial range is wide (311.568–465.999 microseconds).
These observations do not prove a regression mechanism, but they do not support its added lifetime state.

The final fused candidate 3 allocation slope remains 135.1695 calls per exchange.
The 1,000/3,000-iteration allocation counts are 143,285/403,520.
The zeroed counts are two in both runs.
The reallocation counts are 5,564/15,668.
Allocation reduction alone does not meet the performance-first selection criterion.

### Corrected candidate 3 profile

The first isolated recording lost one data chunk.
A larger-buffer retry exited with status 255 without a diagnostic.
A 499 Hz retry completed without lost samples:

```sh
perf record -q -o target/profile-artifacts/candidate3-isolated-repeat.perf.data \
  -e cpu-clock:u -F 499 --call-graph dwarf,16384 --delay 300 -- \
  taskset -c 2 target/profile-artifacts/frozen/candidate3-isolated \
  --iterations 100000 --warmup 5000 --response-bytes 128 \
  --json target/profile-artifacts/candidate3-isolated-repeat-profile.json
```

The shared input poll remains 9.99%, and event waits remain 5.59%.
Persistent waits still execute event polling and task registration.
Lower allocation counts do not remove those costs.
Skipping event polls solely because a token remains unset is unsafe: an I/O-scope cancellation can complete the wait independently.

## Retained PoC and rejected iterations

The retained PoC restores candidate 1 scheduling.
It also retains the additional ready-probe regression test.
The public API, runtime channels, core interface, payload ownership, transport constructors, and I/O workers remain unchanged.

Rejected iterations:

- Two-pass ready probes: less than one percent allocation reduction and no consistent timing improvement.
- Persistent shutdown waits: lower allocation counts, but no consistent timing improvement.

Persistent receive streams remain unimplemented.
They require receiver/state lifetime separation and careful cancellation-settlement coverage.
The retained bounded change has stronger timing evidence and fewer ownership changes.
The upcoming final common base still requires a rebase, regression runs, and a new comparison.
This experiment does not select a production wrapper.

The retained tree passed 145 default wrapper/core tests and 147 all-feature tests.
Formatting and both required clippy commands succeeded with only the previously listed warnings.
A final wrapper-library run also passed after restoration of the exact candidate 1 production source.

The rebuilt retained binary SHA-256 is `d8099993cc1846f350f0b2c597017db07488e811e73f4a09e2e43722099b6ee8`.
Only an additional test distinguishes its driver source from candidate 1.
The two ELF `.text` sections are byte-identical.
Their shared `.text` SHA-256 is `5164a680859b46f777ff032d237c8e5c5db9b43c4be680c6cc8f71741b55c52a`.
The full-file hashes differ because the binaries also contain source/debug information.

The exclusive measurement slot is released after this series.
All binary manifests, comparisons, profiles, and functional logs remain under this worktree's `target/profile-artifacts/`.

## Final-common-base integration

The coordinator renewed the exclusive CPU2 slot for the final-base series.
The exact common base is `0bd3e950f4b0aedb552fbfd8fab96139ef399bb9`.
The rebased candidate source is `12e58e137245628c11c92bd05e7cad8462fcff71`.
The rebase preserves the full experiment history.

The only conflicts involved the input selector.
The resolution retains `state.read_send.is_some()` before any read-channel probe or poll.
Thus the raw backend's stopped read channel remains disabled.
The net production diff does not touch `State::observe`, source-abandonment gates, or lease-return ordering.
It also leaves native/generic constructors and I/O workers unchanged.

The net driver diff is 94 added lines and nine removed lines.
It includes three tests, a synchronous receive helper, and changes to the existing selector.
The common benchmark and shared runner have no PoC modifications.

### Final build identities

Final builds use a fresh target directory and isolated package selection:

```sh
CARGO_PROFILE_RELEASE_DEBUG=2 \
CARGO_TARGET_DIR=/workspace/kimojio-rs/target/wrapper-lab/build-profile-final \
taskset -c 8-31 cargo build --release --offline \
  -p kimojio-http1 --example keepalive_bench
```

Both binaries were built from this worktree at their exact revisions.
The control build temporarily selected the common-base commit, then the worktree returned to the candidate.
Each binary supports both `--native` and the default generic backend.

The artifact root is `/workspace/kimojio-rs/target/wrapper-lab/worktrees/profile/target/profile-final-artifacts`.

| Relative binary path | Revision | SHA-256 |
| --- | --- | --- |
| `frozen/common-final` | `0bd3e950` | `698f451234fbdc6865fbaa06e3b630329f0cbb8044c2fb86f0eb0d6f362539af` |
| `frozen/profile-final` | `12e58e13` | `09c2a547c03f44d84f40b926c822f45e66e0baeba0730ba146c933de835273c7` |

### Final functional evidence

The final target directory applies to every Cargo command:

```sh
taskset -c 8-31 cargo fmt
taskset -c 8-31 cargo test --offline -p kimojio-http1 -p kimojio-fsm-http1
taskset -c 8-31 cargo test --offline -p kimojio-http1 -p kimojio-fsm-http1 --all-features
taskset -c 8-31 cargo clippy --offline
taskset -c 8-31 cargo clippy --offline --all-targets --all-features
taskset -c 8-31 cargo test --offline -p kimojio --lib io_scope
python3 -m unittest discover -s perf/wrapper-lab -p 'test_compare.py'
```

The default suites passed 163 tests.
The all-feature suites passed 165 tests.
The runner suite passed six tests.
The I/O-scope suite passed 12 tests, including wrapped-waker settlement and sibling cancellation isolation.
Formatting and both clippy commands succeeded.
Only the previously listed `question_mark` and two `byte_char_slices` warnings remain.

The wrapper suites include duplex, forwarding, native transport, reusable connections, cancellation, source settlement, and custom transports.
They preserve the common base's body lease and unknown-progress write guarantees.
The added tests cover empty/ready/closed probes, wake registration before suspension, and credit after a registered wait.

### Final timing results

```sh
python3 perf/wrapper-lab/compare.py \
  --manifest target/profile-final-artifacts/manifest.json \
  --output target/profile-final-artifacts/comparison.json --trials 5 --cpu 2 \
  --case empty --case small --case post-small --case large \
  --case chunked --case fragmented --case duplex-fixed \
  --case duplex-chunked --case duplex-copy
```

The manifest compares both binaries in both backend modes.
All 180 runs passed the unchanged byte, connection-count, backend, and exchange-count requirements.
The default seed is `20260920`.
The table gives medians in microseconds per complete exchange.

| Workload | Common generic | PoC generic | Common native | PoC native |
| --- | ---: | ---: | ---: | ---: |
| Empty | 32.904 | 25.489 | 28.302 | 24.928 |
| Small | 43.240 | 37.719 | 41.553 | 36.386 |
| POST small | 57.449 | 46.008 | 52.117 | 47.309 |
| Large | 139.922 | 123.626 | 130.975 | 114.446 |
| Chunked | 2131.441 | 1870.602 | 1999.802 | 1763.227 |
| Fragmented | 376.359 | 322.052 | 357.513 | 311.367 |
| Duplex fixed | 159.024 | 141.872 | 148.882 | 132.879 |
| Duplex chunked, lease | 2640.826 | 2623.591 | 2462.493 | 2143.162 |
| Duplex chunked, copy | 2506.202 | 2182.398 | 2393.694 | 2143.450 |

Each ordinary workload improves in both backend median comparisons.
The generic duplex-chunked result is effectively inconclusive: its approximately 0.65% median difference is smaller than trial variation.
Its PoC range spans 2318.653–3013.681 microseconds.
Other wide ranges include PoC native POST (44.076–63.736) and PoC generic small (37.029–50.510).
The JSON report retains every trial, command, range, and population standard deviation.
No confidence interval or external server QPS claim follows from these results.

The final benchmark equally boxes connection futures at setup.
Thus earlier `90b7ff60` and original `58e304d8` timings are distinct experiments, not direct final-base controls.

### Final allocation results

Allocation builds select only `-p fsm-allocation-probes --bin alloc-http1-keepalive`.
Both revisions therefore use the same allocator-package features.
Each backend runs 1,000 and 3,000 iterations with warmup 100 and response size 128 on CPUs8-31.
These are functional allocation runs, not timing evidence.

| Backend | Common calls/exchange | PoC calls/exchange | Common requested bytes/exchange | PoC requested bytes/exchange |
| --- | ---: | ---: | ---: | ---: |
| Generic | 329.7575 | 152.9345 | 22837.204 | 10105.948 |
| Native | 317.7055 | 140.8825 | 22449.876 | 9718.620 |

Both backends remove 176.823 allocator calls per complete exchange.
The count includes client, server, and runtime allocations.
It measures wait-registration churn separately from payload copies.

### Final profiles and copy inventory

Both final profiles use the frozen candidate binary and this command shape:

```sh
perf record -q -o ARTIFACT_ROOT/BACKEND.perf.data \
  -e cpu-clock:u -F 999 --call-graph dwarf,16384 --delay 300 -- \
  taskset -c 2 ARTIFACT_ROOT/frozen/profile-final BACKEND_FLAG \
  --iterations 100000 --warmup 5000 --response-bytes 128 \
  --json ARTIFACT_ROOT/BACKEND-profile.json
```

`BACKEND_FLAG` is absent for `stream` and is `--native` for `native`.
The profiles contain 2,639 generic samples and 2,479 native samples, with zero lost samples.

| Shared leaf | Generic self time | Native self time |
| --- | ---: | ---: |
| Input poll | 9.32% | 11.62% |
| `Core::next` | 7.54% | 7.14% |
| Event wait poll | 6.71% | 7.10% |
| `next_input` async body | 5.99% | 5.45% |
| `malloc` | 3.18% | 2.46% |

Conservative ancestor attribution gives generic client/server/shared counts of 165/194/2280.
The native counts are 147/189/2143.
Ambiguous, truncated, or mixed stacks remain shared.
The hottest identified endpoint leaf is `Core::receive_metadata`.
Its generic client/server counts are 27/43, and native counts are 16/45.
The shared inline input-poll leaf has 195 generic samples and 235 native samples.
No shared leaf is charged to one endpoint without ancestors.

`nm -C -S` locates the final input poll at `0x791c0`, size `0x88f`.
`objdump -d -C --start-address=0x791c0 --stop-address=0x79a4f` exposes three directly annotated `memcpy` calls.
The `edx` values give the copy sizes.
Symbol-relative sample offsets identify nearby hits without an ASLR assumption.
The parent appears in 256 generic and 301 native stacks.

| Input-poll site | Size | Generic/native hits within `0x18` | Kind | Necessary |
| --- | ---: | ---: | --- | --- |
| `+0x874` | 288 | 6/12 | move of selected input | Ownership transfer required, staging not inherently required |
| `+0x60d` | 280 | 6/5 | move of selected input | Same |
| `+0x2ae` | 280 | 0/1 | move of selected input | Same, cold in generic profile |

These sites refer to `next_input` in `kimojio-http1/src/driver.rs` at revision `12e58e13`.
Nearby hits are not a measurement of total time inside libc.
Unresolved libc leaves can also contain copies.
The inventory therefore does not claim that all copies are identified.
No payload-copy change was necessary for the retained scheduling result.

### Final limits and decision

This PoC is ready for comparison with the other wrapper experiments.
It is not selected for production.
Remaining wait costs are visible, but two measured extensions did not justify retention.
Persistent receive streams require a separate experiment if future profiles justify their larger lifetime changes.
The final-base series is complete, and the exclusive CPU2 slot is released.
