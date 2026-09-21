# HTTP/1 state costs, observability, and adapter boxing

## Receive-phase promotion

The explicit receive-phase correction is now integrated with role specialization and metadata batching.
The original study and measurements below describe their separate baseline experiments.
The reused-server head-deadline defect below is fixed.

Core regressions cover enabled and disabled idle timing with buffered and separately received prefixes.
Additional regressions cover native and generic wrappers, plus selected and detected HTTP/1 composition.
They cover partial-head timeouts, exact wire responses, original-operation settlement, actual close, idle EOF, and unchanged initial timing.
The wrapper cases use virtual time.

## Observability implementation

The follow-up implementation adds independent, default-off `metrics` and `diagnostics` features to the HTTP/1 core.
The [core README](../../kimojio-fsm-http1/README.md#optional-observations) defines snapshots, accounting, diagnostic delivery, and disabled costs.
The Kimojio wrapper supplies asynchronous snapshot queries and synchronous typed log forwarding.
The HTTP/1+HTTP/2 composite exposes observations for its selected HTTP/1 child, including prefix replay.
This work does not add HTTP/2 telemetry.

The [disabled-cost check](evidence/observability-disabled.txt) compares the promoted receive-phase baseline with the instrumented core.
Both features remain disabled in that comparison.
Client and server layouts remain 1,040 bytes for the measured buffer types.
The executable text size and emitted connection-symbol sizes are unchanged.
The assembly differs, so these results do not claim identical CPU timing.
The [qualification record](evidence/observability-qualification.txt) covers all feature combinations, wrapper query lifetimes, and diagnostic delivery.

The remaining sections preserve the original study, including its historical change and bookmark locations.
The user subsequently integrated role specialization and metadata batching before these follow-up changes.

## Conclusions and delivered changes

The strongest measured opportunity is repeated global dispatch during metadata parsing, not the success branch in `complete_read`.
Role specialization also helps some workloads.
Its code-size cost depends strongly on whether the executable uses both roles.

This investigation found a correctness problem in the timer-based inference of receive state.
A reused server loses its head deadline when idle timing is disabled.
An explicit receive phase both fixes that problem and simplifies the first-byte decision.

The core experiments remain separate changes, not silently combined production changes.
The production changes remove redundant adapter boxes.
At the time of this study, metrics and logging were design recommendations without a telemetry API.

| Work | jj change | Commit | State |
| --- | --- | --- | --- |
| Const-generic private role | `xtrulptl` | `2fd535dd` | Measured PoC |
| Metadata batching | `xnnuwpqt` | `a064dd4e` | Measured PoC |
| Explicit awaiting-request phase | `ltqnmvrr` | `cef890a7` | Correctness PoC, not performance-measured |
| HTTP/1 box removal | `pwsxoomp` | `d37624a4` | Integrated |
| HTTP/2 fixture watchdog | `qmvymsqt` | `d9b19db5` | Integrated |
| This assessment | `mnnklmzk` | See current change | Documentation |

The baseline is `7c7fe7415029b91c17aea61707656fd620b9c02d`.
The `fsm-composition` bookmark remains there.
No change was pushed.

## Hottest stacks

The initial profiles use the existing `roundtrip` benchmark.
It simulates completions and transport copies, without a runtime or kernel I/O.
Client and server have separate workloads.
Both callback modes, small fixed bodies, and large chunked bodies participate in timing.

The initial benchmark disables deadlines.
A second workload enables the default deadlines and advances caller-supplied time by 1,000 ns before each drive turn.
That workload exercises timer updates without adding clock system calls.
It retains the wire and payload oracles.

| Baseline workload | `Core::next` self samples | `next_transition` self samples | `complete_read` self samples |
| --- | ---: | ---: | ---: |
| Small client, timers disabled | 33.94% | 11.91% | 0.40% |
| Small server, timers disabled | 37.13% | 18.27% | 0.20% |
| Small server, advancing time | 34.88% | 20.79% | 0.26% |
| Large client, advancing time | 25.62% | 11.92% | 0.95% |

The hottest identifiable client path is its benchmark session through `Core::next`.
The server has the corresponding path.
Shared selection and metadata helpers dominate both small-message profiles.
Incomplete stack ancestry prevents a complete ownership classification of every sample.

`receive_metadata` consumes one byte and then returns to the global coordinator.
The next iteration repeats lifecycle, notification, transmit, producer, and receive selection.
Usually none of those decisions can change before another metadata byte arrives.
This repeats for the request line, headers, chunk-size lines, delimiters, and trailers.

The batching PoC stays within one metadata section.
It returns to the coordinator at buffer exhaustion, section completion, failure, or a pending deadline notification.
It does not continue across a header callback, body delivery, or another externally visible boundary.

With advancing time, small-server `next_transition` self samples decrease from 20.79% to 5.62%.
`Core::next` includes the remaining parser work, so its percentage can increase while elapsed cost decreases.
Percentages from separate profiles are not absolute time savings.

## Role specialization: measured tradeoffs

`server`, `id`, and the owned `config` do not change during the connection lifetime.
Only role is an obvious small compile-time domain.
The PoC changes private `Core<B, W>` into `Core<B, W, const SERVER: bool>`.
The public `Client` and `Server` signatures do not change.

Results below use advancing time.
Positive percentages mean lower elapsed cost.
Intervals are nominal 95% paired-bootstrap intervals, without multiple-comparison adjustment.

| Workload | Const role | Metadata batching |
| --- | ---: | ---: |
| Small client, continue | 3.9% [-12.2, 16.1] | 15.5% [2.7, 26.7] |
| Small client, yield | 5.6% [-2.6, 15.1] | 17.8% [8.2, 26.3] |
| Small server, continue | 13.6% [10.5, 16.9] | 30.7% [29.1, 33.2] |
| Small server, yield | 17.0% [5.6, 27.5] | 34.7% [30.8, 40.0] |
| Large client, continue | 13.7% [7.0, 20.4] | 8.4% [-0.3, 17.0] |
| Large client, yield | 13.7% [6.7, 22.6] | 4.4% [-12.4, 18.7] |
| Large server, continue | 4.0% [-4.1, 10.1] | 6.2% [-0.1, 12.0] |
| Large server, yield | 7.7% [-2.6, 17.8] | 12.4% [5.5, 22.1] |

The disabled-timer study also supports a small-message batching improvement.
Several large-body and role results remain uncertain.
The host was noisy, and the raw samples retain that noise.
Neither experiment establishes a universal speedup.
Their gains must not be added because the combined implementation was not measured.

Code-size results use matched compiler settings:

| Executable or object | Runtime role | Const role | Difference |
| --- | ---: | ---: | ---: |
| Both-role benchmark text | 2,487,900 B | 2,521,760 B | +1.36% |
| HTTP/1-named text in that benchmark | 114,513 B | 147,248 B | +28.59% |
| Client-only probe text | 438,345 B | 431,653 B | -1.53% |
| Server-only probe text | 437,497 B | 430,929 B | -1.50% |
| `Client<Vec<u8>, &[u8]>` size | 1,048 B | 1,040 B | -8 B |
| `Server<Vec<u8>, &[u8]>` size | 1,048 B | 1,040 B | -8 B |

HTTP/1-named text excludes code inlined into differently named callers.
It is a useful subset, not an exact protocol footprint.
The full benchmark includes Criterion and standard-library code.
The single-role probes retain real exchanges but exclude Criterion.
The benchmark shares its port type across roles.
Real adapters can already produce separate `next` instantiations through different port types.
The increase therefore depends on buffer types, port types, linker elimination, and compiler settings.

Thus, neither "it doubles the executable" nor "it is only one predicted branch" describes the result.
Single-role programs can become smaller.
Both-role programs retain more specialized code, but shared helpers and linker elimination limit the increase.
The hardware counters reported `not supported`.
There is no measured branch-miss or instruction-cache-miss claim.
Correct prediction still costs instructions and loads, but total text size does not establish the active instruction-cache footprint.

## `complete_read` and explicit receive state

The positive-read path must still establish completion ownership, operation identity, and a valid byte count.
The immutable role does not establish those properties.
Foreign, stale, readiness, zero-byte, failed, and late-cancelled completions remain distinct.

The baseline disassembly does contain the server/head/exchange/idle-timer tests.
Short-circuit evaluation means body reads do not execute the complete chain.
Clients stop at the role test.
Servers receiving bodies stop at the receive-phase test.
Only a reused connection at its next head boundary attempts to install that head deadline.

`SequenceExhausted` is not a counter scanned on every successful body read.
That branch handles failure while changing a deadline.
The current mapping also includes deadline arithmetic failure, not just exhausted sequence IDs.
Moving that cold failure into a helper can change layout, but it does not remove the necessary state decision.

There is a more important representation defect.
`timers.phase` is an `Option<(TimerPhase, Tick)>`.
It represents an armed timer, but the receive path also uses it to infer the logical idle phase.
With `idle_timeout_ns = None`, retirement removes that evidence.
The next nonempty read consequently fails to start an enabled head timeout.

The [public-API probe](deadline_probe.rs) fails on the baseline:

```text
expected: [Some(Tick(110))]
actual:   []
```

PoC `cef890a7` adds `Rx::AwaitingRequest` for a reusable server awaiting the next request.
The first nonempty read, or already buffered request prefix, changes it to `Rx::Head`.
That transition starts the head policy independently of whether idle timing was enabled.
An empty read does not start the head timer.

The resulting decision is one authoritative receive-phase test, not a cached Boolean derived from four other fields.
The initial connection still starts its head deadline at construction.
The PoC covers enabled and disabled idle timers, with buffered and separately received next-request prefixes.
Its 114 core tests pass.
This candidate remains separate, so the baseline defect is not fixed in the integrated boxing changes.

## Other fields and the state hierarchy

| Field or group | Assessment |
| --- | --- |
| Connection identity | Immutable, but runtime-specific. Preserve ownership discrimination rather than create a type per connection. |
| Configuration limits and durations | Immutable. Generic parameters for arbitrary limits create many combinations and reduce deployment flexibility. |
| Optional deadlines | Potential compile-time policy, but do not use timer presence as protocol state. Measure before adding a policy axis. |
| Method, framing, persistence, expectation | Stable over parts of an exchange, not the connection. Runtime semantic enums remain appropriate. |
| `no_content` | A candidate for incorporation into the relevant receive variant. No measured reason to prioritize it over metadata dispatch. |
| `close_after` | Accumulates several protocol decisions. It is not equivalent to closing or to a pending transport close. |
| Storage, I/O, credit, receipts | Independent ownership obligations. They cannot safely disappear behind one protocol-state tag. |
| Lifecycle and `Boundary` | Already useful authoritative state. Preserve their explicit transition and notification boundaries. |

A single control variable is theoretically possible for a finite product of states.
It does not remove the product's complexity.
Next actions still depend on the incoming event, byte counts, time, credit, and token identity.
An enormous enum either enumerates combinations or hides the same predicates inside its handlers.
A function pointer per state adds indirect calls and can prevent useful inlining.

The useful hierarchy is lifecycle, local direction or operation state, then event-specific conditions.
Completion ownership precedes policy suppression.
Closing does not permit the machine to discard an original read or write completion.

There is no universal ordering in which receive always dominates transmit, or the reverse.
They can progress independently.
At a callback or completion boundary, new work can require a return to higher-priority coordination.
Inside a metadata section, there is no such external boundary, so a local loop is appropriate.

Readiness bits can help if the machine updates them at authoritative transition sites.
Duplicating every predicate into cached flags creates another consistency problem.
The existing semantic `Boundary` record is a better model than a general cache of arbitrary conditions.

## Metrics design

Use synchronous snapshots owned by the FSM.
The adapter chooses the polling interval and exports differences.
The FSM needs no metrics runtime, clock read, background task, atomic counter, or formatting allocation.

Separate cheap gauges from lifetime counters.
Current buffered bytes, leased storage, outstanding operations, current phase, and admitted work can often come from existing authoritative state.
Lifetime counters need dedicated storage because the existing exchange counters reset or have different meanings.
Generic buffers expose slices, not allocator capacity.
Gauges must distinguish visible bytes, known retained capacity, and unknown backing storage.
Configured limits are not measurements of current storage.

Counters must distinguish:

- Transport bytes received from body bytes delivered and body bytes consumed.
- Accepted producer bytes from confirmed transport progress.
- Exact write progress from a lower bound after an uncertain generic write.
- Admitted exchanges from retired, failed, or reusable exchanges.
- Cancellation requests from settled originals and cancellation acknowledgments.

Use fixed-size saturating counters with documented units and a saturation indicator.
Statistics overflow must not fail a connection.
Snapshots must not reset the counters or drive the machine.
The adapter takes a final snapshot before it discards the connection.
Connection identity supplies the aggregation boundary.

A conventional client handle does not own the FSM.
Its snapshot request can pass through the driver's existing command channel, with a reply from a coherent drive boundary.
That adds control-plane work per poll, not shared atomic updates on every I/O operation.
A periodically published snapshot needs an explicit timestamp and stale-data contract.

Existing HTTP/2 flow and HPACK diagnostics provide prior art:
`server/h2/flow.rs` has saturating lifetime counters and snapshot gauges.
`server/h2/wire.rs` has content-free directional HPACK counters.
Expose those meanings through the owned engine rather than invent contradictory wrapper counters.

Start with computed gauges.
A `metrics` feature can add fixed lifetime counters without another generic dimension across every connection type.
The counter API must be feature-gated too, rather than return misleading zeros when disabled.
Measure enabled and disabled builds before making counters a default requirement.
Large histograms, labels, and per-stream aggregation belong outside the core.

## Logging design

Use a typed event enum and an optional port callback.
Events contain IDs, phases, reason enums, counts, and caller-supplied timestamps.
They contain no formatted strings, body copies, credentials, or automatic header dumps.
The adapter chooses severity, formatting, filtering, and destination.

An illustrative callback is:

```rust,ignore
fn log(&mut self, _event: LogEvent) {}
```

Unlike operation callbacks, this observational callback does not suspend protocol progress.
It cannot reenter the machine.
It must return promptly rather than perform blocking log I/O on the drive path.
Allowing it to yield would require another resumable step between logging and the required protocol callback.
A no-op implementation gives the optimizer an opportunity to remove event construction.
Filtering must precede expensive optional diagnostics.

The event vocabulary includes primary failure, shutdown, deadline expiration, admission rejection, exchange retirement, handoff, and actual closure.
Additional I/O and framing events can form an explicitly enabled trace level.
One enum variant describes one semantic event, rather than a string template with untyped arguments.
The feature still needs a complete event inventory and public compatibility decision before implementation.

There is an important API constraint: `complete_read`, `request`, and `shutdown` do not receive ports.
They cannot synchronously call a port that exists only during `next`.
This constraint rules out silently adding immediate logging to every API call.

Use `next` for logs associated with committed drive transitions.
For primary failure, retain one diagnostic obligation and emit it in the relevant error lifecycle.
Do not add an unconditional per-byte scan of pending logs.
The adapter can log rejected commands directly from their typed returned errors.
That path can use the same event vocabulary.

If every API attempt needs exact immediate ordering, each entry point needs an explicit observer or ports argument.
That is an API design change, not a free callback addition.
An unbounded diagnostic queue is not an acceptable substitute.
A bounded queue needs explicit loss counters and does not satisfy a lossless-log contract.

## Boxing removed and retained

The [complete audit](boxing-audit.txt) names the sites, ownership arguments, and rejected alternatives.
The main distinctions are address stability, type erasure, and layout bounds.
Not every retained box is required by Rust's type system.

| Removed allocation | Frequency | Why removal is sound |
| --- | --- | --- |
| HTTP/1 generic read-half box | Once per non-zero-sized half | The existing pinned worker already owns and stabilizes it |
| HTTP/1 generic write-half box | Once per non-zero-sized half | The same worker ownership persists through settlement and close |
| HTTP/1 boxed `SleepFuture` | Each changed nonempty deadline | The concrete future is `Unpin` and can reside directly in state |
| HTTP/2 fixture watchdog box | Once per watchdog | The concrete sleep future can enter `select` directly |

The HTTP/1 timer field grows by 8 bytes without virtual time and 48 bytes with virtual time on this target.
The public future-size guards still pass.
Runtime timer resources still allocate.
These allocation reductions follow from removed constructors, not an allocator-counter or CPU-speedup measurement.

Retained boxes have several different justifications:

- Native reusable operation slots stabilize futures that borrow their own buffers or I/O vectors.
- Owned dynamic bodies stabilize arbitrary `!Unpin` streams and preserve the non-generic public body API.
- Optional HTTP/2 observers and informational callbacks have heterogeneous types across requests.
- The retirement waiter must survive cancellation of one polling future and resume on a later call.
- Generic transport, driver, and split boxes bound caller-future size for arbitrary large transport implementations.
- HTTP/2 half boxes avoid moving arbitrary transport buffers into and out of each operation.
- Handler boxes stabilize arbitrary futures and bound moves through exchange or runtime-task construction.
- Static-server operation boxes preserve pointers to inline syscall fields until the original completion.
- Composite and chat child boxes bound enum and connection-slot layouts during protocol transitions.
- Boxed payload slices still need owned variable-size storage. Substituting `Vec` does not remove the allocation.

The native setup descriptor box is not intrinsically necessary.
It remains part of the shared generic representation.
A specialized native setup representation can remove this connection-time allocation.
Likewise, typed handler storage can remove internal erasure, but needs large-future and move-cost evidence.
These are explicit layout tradeoffs, not claims that all async futures require heap pinning.
Cold heterogeneous error boxes and test-only convenience boxes were not broadly rewritten.

## How copies were found

The sequence was frozen binary, profile, symbol sizes, matching disassembly, then source.
MIR string absence was not used as evidence.
`nm`, `objdump`, and `perf script` refer to the exact baseline executable.

### Copy inventory

| Site | Size | Samples | Kind | Necessary here |
| --- | --- | --- | --- | --- |
| Server benchmark `Session::round_trip`, call at `+0x560`, return at `+0x566` | Up to 16 KiB | 419 stacks include the server input copy, including preflight | Explicit copy | Yes, for this simulated owned-read workload |
| `complete_read` rejection return at `+0x161` onward | Returned completion fields | Not established as a hot site | Inline move | Preserves rejected ownership without a new allocation |
| Metadata `head.push` and bounded growth | One byte plus occasional growth | In the hot `next` path | Append and possible allocation | Contiguous parser storage remains necessary in this PoC |

The copy site passes its bounded count through `rdx` and calls `memcpy`.
The large-server profile attributes 27.22% of self samples to the corresponding libc leaf.
That is benchmark transport simulation, not a redundant wrapper copy.
The study does not remove it or the payload oracle to manufacture a speedup.
Metadata batching removes repeated selection, not the requirement to retain fragmented metadata.

## Proposals in order

1. Promote the explicit receive-phase correction after affected wrapper and composition coverage passes.
2. Combine metadata batching with that corrected state model, preserving callback and deadline boundaries.
3. Measure role specialization against the combined baseline, including actual one-role applications and a both-role consumer.
4. Add snapshot metrics and typed diagnostics with an explicit disabled-cost and event-delivery contract.

The independent prototypes need a combined source and test review before promotion.
Their measured improvements are not additive.

## What not to cut first

Ownership, token, and byte-count validation are required correctness boundaries.
Large rejected-result moves are not established hot sites.
The simulated input copy is part of the workload contract.
Pinned-operation boxes remain necessary in their current movable containers.
Arbitrary configuration values do not justify a generic parameter for every setting.

## Evidence and replay

The compiler is rustc 1.98.1, LLVM 22.1.8, on x86-64 Linux.
Release benchmark builds use `CARGO_PROFILE_BENCH_DEBUG=1`.
Each source has a separate `CARGO_TARGET_DIR`.
Timing uses CPU2, with builds on CPUs8-31.
No governor or system profiling policy changed.

Each study has seven interleaved groups, three binaries, and eight workload cells.
Each cell uses 30 Criterion samples, 0.1 seconds of warmup, and 0.5 seconds of measurement.
The outer analysis resamples paired group log ratios 10,000 times.
Its random seed is 619.
The estimate uses the geometric mean of paired ratios, not the difference between separately reported medians.
Short windows and host variation limit precision.
All 336 cell results remain in the evidence, including unfavorable ratios.

The baseline normal benchmark SHA-256 is:

```text
289eb94b361ab0225a862f8e1c0fba3fd8d0a56b74472d9f12c131332f344ef5
```

The manifests record all candidate sources and binary hashes.
The advancing-time sources are `f0474bf5`, `aec91145`, and `15a062ec`, respectively.
The same harness patch applies to baseline, role specialization, and metadata batching.

The [runner](measure.py) expects isolated source directories and Cargo JSON build records under a fresh study directory.
It refuses to overwrite existing timing evidence.
The [size probe](size_probe.rs) supplies a separate one-role linking experiment.
The probes expect a copy in the study directory, beside its `base` worktree.
The private working directories and raw profiles remain under `target/http1-core-study/`.

The role PoC passed 113 core tests.
The metadata and receive-phase PoCs each passed 114.
All three passed all-target, all-feature core Clippy.
The integrated wrappers passed 163 tests and doctests, plus 18 interoperability-fixture tests.
Formatting and both required workspace Clippy configurations passed with only the two existing `pipe.rs` warnings.
Address-stability, cancellation, timer replacement, virtual time, reuse, and watchdog-settlement regressions cover the box removals.

The experiments do not establish end-to-end network gains.
No combined core candidate, broad hardware matrix, or telemetry implementation was measured.
