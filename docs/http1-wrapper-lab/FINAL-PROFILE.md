# Final HTTP/1 wrapper profile

## Result and scope

The frozen final wrapper still spends more sampled CPU time on protocol and driver work than on individual copy sites.
The largest sampled wrapper copies are moves of operation completions and selected inputs.
They are not payload clones.

One bounded trial adds `#[inline(always)]` to the private native `Slot::poll` method.
The separate completion-return move disappears from the compiled caller path.
Two five-trial comparisons show lower medians in every ordinary workload and both native modes.
Focused duplex repeats also support the change after an initially noisy result.

**Recommendation: retain the one-line trial for parent integration consideration.**
The production worktree and frozen production binary remain unchanged.
No channel redesign, runtime change, unsafe code, API restriction, deadline change, or struct redesign was attempted.

The trial commit is `5068d6582b18db490192a16041e6a3d465b9c5d6`.
Its only code change is in `kimojio-http1/src/io_driver.rs`.
The report worktree remains based on the unmodified final source.

## Frozen identities

| Item | Identity |
| --- | --- |
| Final source | `da081a89349ef1cc5619496e74556a898dcb98c6` |
| Final binary | `/workspace/kimojio-rs/target/wrapper-lab/frozen/final/keepalive_bench` |
| Final SHA-256 | `604778177b4c2b03b969297fd811542c419bfe38582667f470ec3990af316b19` |
| Report worktree | `/workspace/kimojio-rs/target/wrapper-lab/worktrees/final-profile` |
| Trial worktree | `/workspace/kimojio-rs/target/wrapper-lab/worktrees/final-profile-trial` |
| Trial binary | `/workspace/kimojio-rs/target/wrapper-lab/worktrees/final-profile-trial/target/final-profile-trial-artifacts/keepalive_bench` |
| Trial SHA-256 | `ee27800c7b4293ee5b0229795885d7fd0c8a9be695e491e116cd319150f208f9` |

The supplied final build command was:

```sh
taskset -c 8-31 env CARGO_PROFILE_RELEASE_DEBUG=2 \
  CARGO_TARGET_DIR=/workspace/kimojio-rs/target/wrapper-lab/build-final \
  cargo build --manifest-path target/wrapper-lab/worktrees/implementation/Cargo.toml \
  --release -p kimojio-http1 --examples --offline
```

The trial uses the same package selection, release profile, and debug level:

```sh
cd /workspace/kimojio-rs/target/wrapper-lab/worktrees/final-profile-trial
CARGO_PROFILE_RELEASE_DEBUG=2 \
  CARGO_TARGET_DIR=/workspace/kimojio-rs/target/wrapper-lab/build-final-profile-trial \
  taskset -c 8-31 cargo build --release -p kimojio-http1 --examples --offline
```

The compiler is Rust 1.98.1 (`48a229cea`, LLVM 22.1.8).
The profiler is `perf` 6.6.139.1.
No allocator-package build was combined with either benchmark build.

The report source and final binary match.
The binary's debug paths name the original `implementation` worktree.
All baseline addresses below belong to that frozen binary, not a later build.
Trial addresses appear separately.

## Hottest stacks

### Fresh profiles

Both modes used 5,000 warmup exchanges and 150,000 measured exchanges.
Requests have empty bodies, and responses contain 128 bytes.
Each profile reports the correct full payload, one connection, zero reconnects, and 155,000 server exchanges.
The profiles use `cpu-clock:u`, 999 Hz, 16,384-byte DWARF stacks, and a 300 ms recording delay.
Both final profiles and both trial profiles have zero lost samples.

Profile-instrumented elapsed times are not used as performance evidence.
The later matched comparisons run without `perf`.

| Final shared leaf | Native | Native with full-body coalescing |
| --- | ---: | ---: |
| FSM `Core::next` | 10.48% | 11.01% |
| `next_input` poll closure | 8.89% | 9.61% |
| `next_input` async body | 5.43% | 4.59% |
| `WaitAsyncEventFuture::poll` | 4.22% | 3.56% |
| `malloc` | 3.05% | 3.08% |
| Read `Slot::poll` | 1.52% | 1.10% |

These shared leaves serve both endpoints.
Their leaf symbols alone do not identify a client or server owner.

### Client, server, and shared attribution

The classifier examines every recorded ancestor.
A client stack requires the client `NativeConnection` specialization.
A server stack requires a driver specialization with the benchmark's server handler.
Mixed, truncated, and unresolved stacks remain shared.

| Profile | Client | Server | Shared or unresolved | Total |
| --- | ---: | ---: | ---: | ---: |
| Final native | 247 | 267 | 2634 | 3148 |
| Final coalesced | 194 | 234 | 2297 | 2725 |
| Trial native | 227 | 242 | 2486 | 2955 |
| Trial coalesced | 175 | 195 | 2015 | 2385 |

These are conservative attribution counts, not complete endpoint CPU shares.
Sample totals also depend on recording duration.
They are not independent timing results.

The hottest identified client stack reaches `Core::receive_metadata` through the client driver.
It has 40 native and 39 coalesced inline-leaf samples.
The hottest identified server stack reaches the same method through the server driver.
It has 66 native and 75 coalesced inline-leaf samples.

The shared inline input-poll leaf has 234 native and 223 coalesced samples.
It is hotter than either identified endpoint stack.
The source path for metadata work is `kimojio-fsm-http1/src/coordinator.rs:558`.

### Allocation churn is not a payload copy

Among `malloc` stacks, `WaitData` appears in 36 native and 37 coalesced samples.
`SleepFuture` appears in seven native and five coalesced samples.
Slice `to_vec` appears in ten native and nine coalesced samples.
Header-map capacity allocation appears in eight native and three coalesced samples.
These are sampled allocation stacks, not allocator call counts.

The coordinator supplied these independent small-workload allocator slopes:

| Implementation | Allocator calls per complete exchange |
| --- | ---: |
| Frozen original | 329.7575 |
| Frozen final native | 104.0555 |
| Frozen final native, coalesced | 87.718 |
| Narrower buffered API | 65.0 |

This assignment did not repeat allocator instrumentation.
It does not claim a new allocator-count improvement from inlining.
The narrower buffered API remains a different supported surface.

## How copies were found (MIR vs objdump vs perf)

The analysis started with the profiles, not the source.
`nm -C -S` supplied each hot symbol's address and size.
`objdump -d -C` exposed direct and register-indirect `memcpy` calls.
The disassembly also contains `movaps`, `movups`, `movdqa`, and `movdqu`.
The selected ranges contain no `rep movs` instructions.

Copy lengths come from the last immediate assignment to `edx`.
Register-indirect calls require a preceding load of the `memcpy` address into that register.
An unknown or overwritten length does not become an inferred constant.

`perf script` supplies `symbol+offset` frames.
The analysis normalizes `.llvm` symbol suffixes while retaining the symbol-relative offsets.
It never subtracts a runtime address from a file address.
Each event counts at most once per parent symbol.

No MIR result is used to claim that a copy is absent.
The binary gives direct evidence of these sites.
Unresolved libc leaves can contain additional copy work.
This inventory is not a claim that all copies were identified.

| Final symbol | Address | Size | Native/coalesced parent samples |
| --- | --- | --- | ---: |
| Native input poll | `0x75ff0` | `0x7c7` | 318/281 |
| Native write `Slot::poll` | `0x7ed40` | `0x899` | 51/31 |
| Native read `Slot::poll` | `0x7f5e0` | `0x55f` | 51/31 |
| `Core::next` | `0x6cd60` | `0x3637` | 358/314 |
| Server `State::event` | `0x4b3c0` | `0x136a` | 58/36 |
| Client `State::event` | `0x4daa0` | `0x1382` | 32/27 |

## Copy inventory

The hit window is `0x18` bytes before or after each call.
Nearby hits include surrounding instructions and can overlap.
They do not measure total time inside libc.
The counts are native/coalesced.

| Site | Size | Hit | Kind | Necessary |
| --- | ---: | ---: | --- | --- |
| Input poll `+0x6ed` | 296 | 25/14, hit | move of selected `Input` | Owned result required. Separate staging not inherently required |
| Write slot `+0x695` | 288 | 16/9, hit | move of completion result | Owned completion required. Separate call boundary not inherently required |
| Input poll `+0x2fc` | 294 | 12/5, hit | move into selected input | Same |
| `Core::next +0xfb0` | 264 | 8/1, hit | move of write operation | Ownership must transfer to the I/O owner |
| `Core::next +0x213b` | 272 | 6/9, hit | move of metadata callback result | Owned callback result must survive return |
| Input poll `+0x3f8` | 294 | 5/6, hit | move into selected input | Separate staging is not inherently required |
| `Core::next +0xff7` | 272 | 5/4, hit | move of callback result | Owned callback result required |
| Server event `+0x75b` | 672 | 1/0, cold in coalesced profile | move of handler future into its box | Future must survive the callback |
| `Core::next +0x91d` | 264 | 0/0, cold | move of operation value | Not a priority in this workload |

The selected-input return maps to `kimojio-http1/src/driver.rs:1310`.
The slot return maps to `kimojio-http1/src/io_driver.rs:174`.
The core write site maps to `kimojio-fsm-http1/src/coordinator.rs:430`.
The metadata site maps through `kimojio-fsm-http1/src/connection.rs:1326` and `coordinator.rs:610`.
The large handler-future move maps to `kimojio-http1/src/driver.rs:1121`.

All these rows are moves of owned Rust values.
None establishes a redundant payload clone.
The 672-byte move has fewer samples than the 288-byte slot return, so size alone does not give it priority.

## Proposals in order

### 1. Selected-input staging: higher-ranked remaining work, not changed

The final input-return site has the largest nearby copy count.
Removing this boundary directly requires changes to how `next_input` returns an owned input to the driver.
That can affect borrow scopes and the point where the driver observes time.
It is not a small, clearly isolated source change under this assignment.

The relevant lifetime is from selection through `State::input`.
The owner must retain the completion and its payload until that consumer receives it.
Future work must preserve source-capacity revocation, rotation, body credit, and deadline observation.
No input-enum or coordinator redesign was attempted.

### 2. Completion-return staging: the one trial

The write slot's 288-byte return move has 16/9 nearby hits.
`Slot::poll` polls a pinned operation future and clears the slot only after the future returns a ready result.
The result already owns everything that must survive the future's removal.

Inlining this private method gives LLVM visibility across the completion-return boundary.
It changes no lifetime or control-flow rule in the Rust source.
The hint applies to both native slots.
The generic transport backend does not use these native slots.

```diff
+    #[inline(always)]
     fn poll(&mut self, cx: &mut Context<'_>) -> Poll<F::Output> {
```

The source still clears completed futures at the same point.
It retains the same `Poll::Pending` and ready-result behavior.
The trial introduces no allocation, buffering rule, cancellation gate, public API change, or unsafe block.

### 3. Metadata and notification costs: remaining work, not changed

`Core::receive_metadata` is the hottest identified endpoint stack.
The profile also retains event-wait and allocation costs.
Their safe reduction requires separate protocol, ownership, or wake-registration analysis.
The earlier scheduler experiments already showed that fewer allocations do not automatically improve timing.
This bounded trial does not revisit persistent channel waits or runtime registration internals.

### Deferred hypothesis: native-only blocked probing

The earlier two-pass rejection used worker-channel completions.
It does not establish the result for the final native slots.
An already-ready native completion can potentially avoid unrelated temporary wait registrations.

The fresh profiles show event-wait costs of 4.22%/3.56%, but they do not identify the blocked selector's readiness state.
They therefore cannot separate necessary suspension registrations from discarded registrations before an already-ready native completion.
That missing distinction prevents a specific gain estimate.

A future experiment must preserve the ten-way rotation and every eligible handler/source poll.
An unconditional native-first probe can change fairness even if it never returns `Pending`.
A two-phase scan also needs clear rules against duplicate cooperative-source polls.
Every actual suspension must still register the applicable channel and cancellation waits.

The possible benefit is fewer temporary registrations on blocked turns.
The costs are another selection phase, more readiness state, and additional fairness/wake tests.
This hypothesis is neither disproved by the older worker result nor supported strongly enough by these samples for another trial.
The one permitted implementation iteration was used for the sampled completion-return move.
No native-only blocked-probe change or additional measurement was attempted.

## Trial machine-code result

The separate native `Slot::poll` symbols disappear from the trial binary.
The native input poll grows from 1,991 to 5,412 bytes because it now includes the slot bodies.
The complete ELF `.text` section grows only 32 bytes.
The GNU `size` text aggregate, which also includes read-only data, decreases by 200 bytes.

The original write-slot return copy no longer exists as a separate call boundary.
The original write-completion path's intermediate 288/294-byte staging calls also disappear from that caller path.
The request path still contains its own 288/294-byte moves.
The final selected-input return remains 296 bytes.

The trial input poll starts at `0x76070`.
Its retained selected-input return is `+0x9fc`, with 21 native and 23 coalesced nearby samples.
Its parent appears in 396/328 sample stacks.
These counts come from new trial profiles, not the original addresses.

The trial input-poll self percentages rise to 12.39%/12.24%.
That symbol now includes work previously attributed to separate slot functions.
This change in symbol attribution is not evidence of a performance regression.
The independent matched timings determine the result.

## Controlled timing results

All comparisons use the unchanged shared runner, CPU2, and the frozen final and trial binaries.
The two ordinary series each contain five trials for five workloads and both native modes.
Each series therefore contains 100 runs.
Every run passed the existing payload, connection-count, exchange-count, backend, and coalescing assertions.

Medians are microseconds per complete socketpair exchange.
Both endpoints and runtime cleanup are included.
These are not external server QPS results.

| Mode and workload | Final, seed 20260920 | Trial, seed 20260920 | Final, seed 20260921 | Trial, seed 20260921 |
| --- | ---: | ---: | ---: | ---: |
| Native small | 30.643 | 30.013 | 30.666 | 29.984 |
| Native POST small | 37.327 | 36.275 | 37.412 | 36.831 |
| Native large | 97.042 | 96.393 | 97.577 | 95.248 |
| Native chunked | 1527.443 | 1497.730 | 1529.743 | 1499.015 |
| Native fragmented | 240.750 | 235.310 | 240.348 | 235.765 |
| Coalesced small | 24.973 | 24.370 | 25.188 | 24.690 |
| Coalesced POST small | 26.938 | 26.581 | 27.332 | 26.500 |
| Coalesced large | 98.062 | 95.928 | 98.324 | 97.075 |
| Coalesced chunked | 1522.997 | 1514.025 | 1531.611 | 1505.734 |
| Coalesced fragmented | 240.745 | 237.574 | 242.366 | 239.886 |

The ten workload/mode cells improve in both series.
Median reductions range from 0.59% to 3.04%.
Native small improves by 2.06% and 2.22%.
Coalesced small improves by 2.42% and 1.98%.
These are modest gains, not a new order of performance.

Some trials contain substantial outliers.
The second native-small series gives narrower evidence: final 30.405–30.896, trial 29.734–30.333 microseconds.
The complete JSON reports preserve all ranges and individual observations.
No confidence interval is claimed.

### Duplex follow-up and noisy results

The first duplex series used three trials per workload/mode, for 36 runs.
It showed worse fixed-duplex medians despite better low-end values.
That result was not omitted or treated as a proven regression.
A second series used five trials, for 60 runs.

| Mode and workload | Final, first series | Trial, first series | Final, repeat | Trial, repeat |
| --- | ---: | ---: | ---: | ---: |
| Native duplex fixed | 110.449 | 126.194 | 109.536 | 108.224 |
| Native duplex chunked | 2010.517 | 2005.497 | 1807.168 | 1778.784 |
| Native duplex copy | 1863.527 | 1747.388 | 1769.299 | 1805.167 |
| Coalesced duplex fixed | 110.131 | 121.326 | 109.951 | 108.097 |
| Coalesced duplex chunked | 1802.704 | 1786.664 | 1798.582 | 1766.759 |
| Coalesced duplex copy | 1775.224 | 1772.147 | 1764.381 | 1742.746 |

The repeat resolved the fixed-duplex concern but left native copy-forwarding ambiguous.
A focused five-trial native copy-forwarding comparison then ran with seed `20260924`.
Its final median was 1781.426 microseconds, and its trial median was 1753.865.
The ranges were 1776.648–1812.659 and 1740.994–1776.427 respectively.
This supports retention without a blanket claim about every noisy duplex trial.

All 106 duplex follow-up runs passed.
Together, the ordinary and duplex comparisons contain 306 passing runs.
No streaming or duplex profile was necessary to justify a payload-copy claim because no such claim is made.

## Regression and review evidence

Commands ran in the isolated trial worktree with:

```sh
export CARGO_TARGET_DIR=/workspace/kimojio-rs/target/wrapper-lab/build-final-profile-trial
taskset -c 8-31 cargo fmt
taskset -c 8-31 cargo test --offline -p kimojio-http1 -p kimojio-fsm-http1
taskset -c 8-31 cargo test --offline -p kimojio-http1 -p kimojio-fsm-http1 --all-features
taskset -c 8-31 cargo clippy --offline
taskset -c 8-31 cargo clippy --offline --all-targets --all-features
CARGO_PROFILE_RELEASE_DEBUG=2 taskset -c 8-31 cargo test --release --offline \
  -p kimojio-http1 -p kimojio-fsm-http1 --all-features
taskset -c 8-31 cargo test --offline -p kimojio --lib io_scope
```

Results:

- Default wrapper/core suites: 190 passing tests.
- All-feature wrapper/core suites: 193 passing tests.
- Release all-feature wrapper/core suites: 193 passing tests.
- I/O-scope cancellation suite: 12 passing tests.
- Formatting and both clippy commands: successful.

The existing lint warnings are `question_mark` at `examples/http1-static/src/app.rs:413` and `byte_char_slices` at `kimojio/src/pipe.rs:64-65`.
The trial introduced no warning.

Relevant regressions cover native slot reuse, cancellation, partial writes, unknown write progress, body credit, lease return, duplex reuse, and custom transports.
Review of the complete source diff shows exactly one attribute on a private method.
There is no change to the benchmark, operation constructors, future removal, completion handling, or shutdown ordering.

The coordinator's broader final evidence predates this trial.
It includes 330 matched timing runs, 120 duplex runs, 780 workspace tests, and 180 independent peer cases.
Those counts are supplied context, not tests rerun by this assignment.

## What not to cut first

- The 672-byte handler-future move: only one nearby native sample and none in the coalesced profile.
- Payload leases or ownership: the hottest identified copies do not justify changing those contracts.
- Deadline observations: clock cost is visible, but deadline changes are outside this assignment.
- Runtime wait registration: allocation samples exist, but a runtime redesign is not a bounded copy cut.
- Metadata storage or input enums based only on their size.
- Error-path or unused copies with no nearby samples.
- Full-body coalescing policy: its existing independent speedup is not caused by this one-line trial.

The coordinator's supplied matched small medians were 41.343 microseconds original, 41.760 common-native, 30.568 final-native, and 25.046 final-coalesced.
The fresh trial is a small additional improvement over the frozen final binary.
The narrower buffered API remains faster on its narrower supported surface.

## Replay and artifacts

The report artifact root, `P`, is:

`/workspace/kimojio-rs/target/wrapper-lab/worktrees/final-profile/target/final-profile-artifacts`

The trial artifact root, `T`, is:

`/workspace/kimojio-rs/target/wrapper-lab/worktrees/final-profile-trial/target/final-profile-trial-artifacts`

Each root contains `native.perf.data`, `coalesced.perf.data`, decoded stacks, assembly ranges, analysis JSON, and workload output.
`T` also contains the test/build/lint logs and frozen trial executable.
`P` contains all comparison manifests and results.

The file `P/artifact-manifest.json` records artifact paths, SHA-256 hashes, compiler information, and both source revisions.
Its SHA-256 is `1d951bebd21caf03efb19f62d4471377a9701c27f6a1f9773215852a7710252e`.

The profile command shape was:

```sh
perf record -q -o ROOT/MODE.perf.data \
  -e cpu-clock:u -F 999 --call-graph dwarf,16384 --delay 300 -- \
  taskset -c 2 BINARY --native COALESCING_FLAG \
  --iterations 150000 --warmup 5000 --response-bytes 128 \
  --json ROOT/MODE.json
```

`COALESCING_FLAG` is absent for `native` and is `--coalesce-full-bodies` for `coalesced`.
`ROOT` and `BINARY` select the final or trial identity stated above.

The ordinary comparison command shape was:

```sh
python3 perf/wrapper-lab/compare.py \
  --manifest target/final-profile-artifacts/trial-manifest.json \
  --output OUTPUT --trials 5 --cpu 2 --seed SEED \
  --case small --case post-small --case large --case chunked --case fragmented
```

The first output is `P/trial-comparison.json` with seed `20260920`.
The second is `P/trial-repeat.json` with seed `20260921`.
The duplex outputs are `P/trial-duplex.json` and `P/trial-duplex-repeat.json`, with seeds `20260922` and `20260923`.
They select `duplex-fixed`, `duplex-chunked`, and `duplex-copy`, with three and five trials respectively.
The focused output is `P/trial-copy-focus.json`, with seed `20260924`, five trials, and only `duplex-copy`.
Its manifest contains only the two native candidates.

## Final decision and limits

The one-line inlining trial has repeatable timing support and passes the relevant debug and optimized regressions.
It remains isolated for parent integration.
The recommendation does not select any broader wrapper architecture.
It does not attribute the entire gain exclusively to memcpy removal: call overhead, optimizer visibility, and instruction layout also changed.

The code-footprint result and copy inventory are compiler/build-specific.
Future compilers or different workload mixes can change the tradeoff.
Remaining larger changes need separate evidence and are not part of this assignment.
The bounded iteration is complete.
CPU2 is released after this report, and no further measurement is scheduled.
