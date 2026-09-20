# HTTP/1 wrapper results and assessment

## Result

The selected wrapper combines reusable native operations, runnable input probes, and unboxed complete bodies.
It preserves the streaming API, generic transports, explicit duplex responses, and original-operation settlement.
Full-body coalescing requires an explicit opt-in because it changes observable timeout progress.
The buffered minimum-code API remains a separate experiment.

The measured production source is `da081a89349ef1cc5619496e74556a898dcb98c6`.
The [comparison](COMPARISON.md) records the four experiments, selection, rejected alternatives, and compatibility review.
The [benchmark contract](BENCHMARK.md) defines payloads, timing boundaries, failure gates, and reproduction.
The fresh final profile and its bounded follow-up remain the last active assessment step.

## Measured performance

These are median microseconds per complete client/server exchange.
Each configuration has five trials per workload.
Both endpoints run on one CPU through one established Unix socket pair.
Every run requires exact payloads, exchange counts, connection reuse, and successful shutdown.

| Workload | Original wrapper | Final generic | Final native | Final native, coalesced |
| --- | ---: | ---: | ---: | ---: |
| Empty request and response | 28.065 | 28.825 | 21.679 | 21.522 |
| Empty request, 128-byte response | 41.343 | 39.051 | 30.568 | 25.046 |
| 128 bytes in both directions | 54.457 | 47.496 | 37.297 | 27.074 |
| 64 KiB in both directions | 133.525 | 127.668 | 97.168 | 97.763 |
| 1 MiB chunked in both directions | 2090.884 | 1892.483 | 1530.762 | 1533.253 |
| 8 KiB with 512-byte source frames | 391.074 | 332.904 | 241.907 | 243.492 |

The native default reduces these medians by approximately 23-38% against the original wrapper.
Coalescing reduces the bidirectional small-body median by approximately 50%.
It does not improve streaming workloads generally.
The generic path improves several workloads but retains its transport workers and write-all behavior.
The generic empty median does not improve in this matrix.

These figures are not TCP throughput claims.
CPU affinity does not reserve the CPU on this shared host.
Some trial ranges are wide, and the raw results retain that variability.
The final matrix contains 330 successful executions, including common-baseline and individual-PoC controls.

### Reusable duplex

The final duplex matrix contains 120 successful executions.
It covers fixed-length and chunked uploads, receive-lease forwarding, and a vector-copy control.

| Workload | Common native | Final native |
| --- | ---: | ---: |
| 64 KiB fixed request with lease echo | 151.601 | 110.667 |
| 1 MiB chunked request with lease echo | 2515.287 | 1857.093 |
| 1 MiB chunked request with copy echo | 2417.644 | 1988.154 |

Copy and lease rankings vary across runs and configurations.
Earlier lease return can permit more receive progress even when it requires a payload copy.
The API exposes both choices.
The evidence does not support an unconditional claim that lease forwarding is faster.

### Allocation calls

The small-response probe uses 1,000 and 3,000 measured exchanges with identical 100-exchange warmup.
The difference gives an empirical allocator-call slope across both endpoints and the fixture.
All three repetitions produced the same slope.

| Configuration | Allocator calls per exchange |
| --- | ---: |
| Original wrapper | 329.7575 |
| Common native backend | 317.7055 |
| Final generic | 151.9345 |
| Final generic, coalesced | 121.2500 |
| Final native | 104.0555 |
| Final native, coalesced | 87.7180 |
| Buffered minimum-code experiment | 65.0000 |

The native default removes approximately 68% of the original allocator calls.
The coalesced native mode removes approximately 73%.
The counts include allocation, zeroed allocation, and reallocation calls.
They are not allocations isolated inside the timed interval.
Instrumented elapsed times do not enter the performance table.

The small-workload peak requested allocation was about 128 KiB for the original and 59 KiB for final native.
Those process-wide counters include startup, fixture arguments, and reporting.
They do not measure RSS, allocator workspace, or kernel memory.
The final live requested bytes were identical across measured counts and configurations.
That observation is not a general leak proof.

## What changed

### Runtime cancellation

Foreign or wrapped wakers no longer run while cancellation holds the runtime state borrow.
Native wake scheduling retains its existing fast path.
Scope regressions cover explicit cancellation, normal scope exit, and dropped scopes.

### Exact native transport

`connect_native` and the native server APIs consume established owned descriptors.
Reads fill FSM receive storage directly.
Each write reports the exact count from one native vectored operation.
The FSM retains the partial-write cursor.
Cancellation waits for the original operation and preserves a late successful count.

The final native driver retains two reusable pinned operation slots.
It does not allocate an operation future, transport channel, or cancellation token for each frame.
Connection-lifetime cancellation flags control the slots.
Close waits for operation settlement and awaits actual descriptor close.
No new unsafe code implements these slots.

### Application scheduling

Runnable turns inspect application channels and cancellation state without temporary wait registration.
Blocking turns still register the wake sources that suspension requires.
Native slots poll their retained operation futures directly.
Input rotation, source revocation, deadline polling, and progress budgets remain active.

### Storage and duplex reuse

`OutgoingFrame::Forward` transfers an incoming lease instead of requiring a vector copy.
Its receipt retains that lease until the original write settles.
Capacity limits account for the complete retained receive allocation, not only the visible slice.

`OutgoingBody::continue_request_body()` explicitly retains request consumption after response start.
Reuse requires complete input, complete output, and ownership settlement.
Dropping an unfinished duplex consumer cancels the exchange.
An outstanding final forwarded lease can return before that abandonment decision.
The core still owns framing, deadlines, Expect policy, and reuse.

### Complete-body coalescing

Complete bodies no longer require a boxed stream source.
With `Config::coalesce_full_bodies = true`, eligible metadata and payload can share one scatter write.
Rejected eager admission returns the original body for ordinary demand.
Custom and forwarded streams remain demand-driven.

The default is false for both client and server.
Client fusion can start the upload deadline before metadata completion.
Server fusion can remove the head completion that previously refreshed its body deadline.
The opt-in accepts these combined-write boundaries.
It is not merely an allocation switch.

## Correctness and review

The complete workspace passed 780 tests and doctests with all features.
Four pre-existing macro doctests remain ignored.
Targeted core/wrapper matrices also passed in release and with both feature configurations.
The benchmark has six tests, and the comparison runner has eight.
Formatting and both required Clippy modes completed with existing unrelated warnings only.

Independent Python and Go peers passed 180 cases across four configurations:
generic and native transport, each with default or explicitly coalesced output.
These cases retain conservative `/echo` and `/early` behavior.
They also require three gated `/duplex` exchanges on one socket.
Each response prefix must arrive before the peer sends its next upload fragment.

The independent final source review recommends adoption without a high-confidence correctness finding.
It examined slot ownership, conditional wake registration, fairness, close ordering, and duplex ownership joins.
Earlier review found client and server timeout compatibility changes.
The explicit opt-in and deterministic regressions address both findings.
Additional tests cover late positive completions, cancellation before issuance, and independently held duplex leases.

The [peer record](INDEPENDENT-PEERS.md) explains the initial acceptance contract.
The final [peer publication](evidence/final-independent-peers.json) records all final outcomes and binary hashes.

## Assessment

The family-wide FSM pattern remains intact.
The protocol core still uses synchronous commands, callback ports, and explicit completions.
It does not acquire a runtime, async operations, system calls, or a clock dependency.
The largest improvements came from adapter scheduling and transport ownership, not protocol-policy duplication.

The independent experiments exposed different costs.
Direct slots improved native operation overhead.
Runnable probes reduced temporary waits on both backends.
Eager owned bodies reduced small-message boundaries.
The minimum-code experiment exposed the cost of the broader streaming contract.

Fewer allocations did not reliably mean faster execution.
The profile experiment rejected two further scheduler changes after measurement.
Fewer lines did not establish an equivalent API.
The 660-line buffered experiment lacks streaming delivery, duplex handlers, outgoing Expect support, independent shutdown, and observable close errors.

Review improved the design rather than only approving the implementation.
A faster combined write initially changed timeout behavior silently.
The final API makes that choice explicit and preserves existing defaults.
Ownership tests also distinguish cancellation requests from original completions.

The process needed better early control of build configuration.
Feature unification contaminated one preliminary profile comparison, which the owner repeated.
Some supplied release binaries also used different debug settings.
The parent rebuilt comparison subjects with explicit settings and retained their hashes.
Freezing the complete common baseline before all experiments would also reduce rebase work.

## Remaining limits

The wrapper still allocates application channels, metadata, and dynamic handler or stream sources.
The narrower buffered API remains faster in several workloads.
The final wrapper is not allocation-free or the proven minimum-overhead implementation.

The native backend supplies neither TLS nor a readiness retry loop.
Unexpected native `EAGAIN` is terminal.
Generic transports must cooperate with cancellation.
Applications must continue to poll the driver through normal shutdown.

This work does not add upgrades, pools, redirects, retries, DNS, or TLS configuration.
It does not establish external TCP or TLS performance.
Bounded models and independent peers do not cover every kernel completion schedule.
Future work can compare a direct-call streaming facade, bounded receive overlap, and broader external-peer workloads.
Those experiments need explicit progress and ownership contracts rather than API narrowing hidden inside benchmark results.

## Change map

| Work | jj change | Commit |
| --- | --- | --- |
| Shared prerequisite baseline | `upnvvwuq` | `0bd3e950` |
| Independent native/duplex examples | `mnnlnwyt` | `612a0949` |
| Minimum operation overhead | `llplqkps` | `4eeb643a` |
| Minimum code | `mvpmxsqz` | `06eb49be` |
| Eager wrapper interface | `utxknnpr` | `451f61b5` |
| Profile-guided scheduling | `zsvowumt` | `a75433ce` |
| Explicit coalescing contract | `ylmqyzum` | `4129a197` |
| Measured selection and plan | `npymkptm` | `031a63f3` |
| Example and benchmark policy wiring | `nolsnwvp` | `61b7bdcb` |
| Combined library implementation | `lmqqvroz` | `53d0a0a9` |
| Measured complete integration | `twyqmwoy` | `da081a89` |

The isolated worktrees and frozen executables remain under `target/wrapper-lab`.
The evidence directory retains ordinary comparisons, allocation counters, commands, source identities, and peer outcomes.
The large profile files and executable files remain outside version control.
