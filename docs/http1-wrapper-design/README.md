# HTTP/1 wrapper architecture: experiments, selection, and next interfaces

Starting source: `478820c8` (`nwzoukkr`). Implemented in the child change `vtvytykq`.
See [PLAN.md](PLAN.md) for the initial hypotheses and investigation.

## Result and scope

Retained **logical/physical timer separation**, **persistent receive/cancellation
waits**, and **additive immutable shared-body sources**. No production changes to
the HTTP FSM or Kimojio runtime remain. Original constructors, owned operation
semantics, default timeout policy, fairness rotation, and the opt-in coalescing
policy remain. No new unsafe code or dependency was introduced.

For the unchanged Vec-producing workloads, matched baseline/final runs improve
native latency about **26–28%**, and stream latency about **20–22%**. The timer
change produces most of that result. Wait reuse adds a modest body-workload gain
but slightly regresses the empty cases versus timer-only. Optional shared frames
save another ~4–6% in the 1 MiB native workload, not in the small-body workload.

This was not a sequence of assumptions presented as wins. Seven designs were
prototyped or refined; multiple alternatives were rejected, including one faster
candidate that failed a new correctness counterexample. PoC patches and raw
results are retained. They are experiments, **not supported alternative builds**.

## What the runtime actually does

### Native I/O

`io_driver.rs` already owns read and write futures in reusable pinned slots. It
performs one raw read/writev, retains the original operation while pending, and
uses connection-lifetime cancellation flags. Replacing it with new worker tasks,
channels, or per-operation boxes would reintroduce costs already removed.

`RingFuture::with_polled` rents/allocates an Rc<Completion> and registers it in
the current I/O scope; first poll submits the SQE. Pending polls refresh wakers.
Original CQEs retire registrations. Cancellation submits an ACK with a separate
strong target owner; that owner prevents address reuse/ABA before the ACK arrives.
Dropping borrowed-resource I/O can synchronously drain its original completion;
owned timer resources can survive a nonblocking drop. These lifetime rules must
not be bypassed to eliminate copies or cancellation operations.

### Timers

`operations::sleep` owns a boxed Timespec and a native timeout completion.
Originally, every changed logical HTTP deadline replaced SleepFuture. That can
mean a new timeout allocation/SQE plus cancellation and CQEs for the old timer.
The completion pool does not make all of this work free. A 1 MiB exchange in the
qualification pass produced about 459 logical deadline notifications across the
endpoint pair, although successful streaming normally only postpones expiry.

### Channels and waits

AsyncChannel is a single-item Rc-owned local slot, not a kernel channel. A pending
WaitFuture allocates Rc<WaitData>, registers its intrusive event-list link and
scope memberships, and stores a waker. Completion/cancellation/drop unregisters
memberships. A wake alone does not retire them. Canceled retries use a fresh
waiter; scope snapshots retain strong owners to prevent stale-pointer reuse.

The wrapper rebuilt release/demand/cancellation waits whenever another input won.
Those allocations were only part of the cost: runtime task lookup, waker updates,
scope membership, registration/removal, and fair scanning still cost CPU after
allocation removal. This distinction was validated by the rejected waiter pool.

### Wrapper representation and scheduling

The wrapper materializes Event and Input enums around owned FSM operations. Those
moves are wrapper choices, not a requirement of Ports. However, bypassing two
Event variants alone did not materially improve this workload. The native driver
already reuses operation futures; the callback/input boundaries and wake work are
more important here than a blanket boxing strategy.

The ten-lane rotation and `!runnable` source gate are correctness-relevant. A
custom handler/source can have effects on poll, and a buffered response can revoke
upload capacity. A second scan must not poll those futures twice or advertise
stale capacity. Likewise, a deadline notification cannot simply be ignored until
a convenient later point: the initial zero-deadline counterexample below proves it.

## Alternatives tested and disposition

All PoCs used production wrapper/core paths and the same benchmark for common
cases. Timing used no metrics, allocator instrumentation, or profiling. Trial
patches are relative to the baseline except `runtime-wait-pool.patch`, which is
the isolated runtime delta applied atop the compact wrapper candidate.

| Candidate | Hypothesis | Evidence | Disposition |
| --- | --- | --- | --- |
| A: lazy/reused physical timer | Logical refresh does not require a new kernel timeout | Native fixed 37.084→27.761 us; chunked 1450.8→1116.5 us; no-deadline controls largely unchanged | Retained |
| B: ready-only scan before registration | Losing waits are avoidable with a second rotated pass | Fixed native ~1% lower, chunked native/stream slightly worse; no consistent win | Rejected |
| D: direct read/write acceptance through Ports | Removing the operation-containing Event is a major win | At most ~1% changes, no large-message improvement | Rejected |
| C1: retained release/demand streams | Keep pending registrations through unrelated work | Lower allocation slope and modest streaming improvement | Retained with C2 |
| C2: retained cancellation waits | Connection/exchange lifetime is a better lifetime than next_input call | Further allocation reduction and modest latency gains; cancellation-generation tests pass | Retained |
| E: shared immutable sources | Application Vec production is a real source of large copies | Same payload/framing, ~4–6% large-body gain; no small-body win | Additive API/control retained |
| F: defer deadline notifications inside core drive | Deadline yields are just bookkeeping | ~6% additional large-body gain, **but a due initial deadline issues I/O** | Rejected on correctness |
| G: bounded runtime WaitData pool | Most remaining allocation cost can be eliminated by reuse | Native chunked slope ~169 allocations/exchange, but latency not better and several cases worse | Rejected |

B, D, and C/G were tested with A already applied, so their changes must be compared
with the timer candidate, not attributed the full timer gain. Preliminary trial
logs are not confidence-controlled causal results; final selection includes the
balanced comparisons below.

### Falsified deadline-yield assumption

The deferred-notification PoC made `Ports::deadline_changed` record the latest
value and return None, allowing Core::next to continue to another event. The old
wrapper tests passed. A new test with `head_timeout_ns = Some(0)` failed:

```text
expired deadline issued a read
```

The core creates its initial deadline before the wrapper has observed the callback.
`observe_time` does not itself expire it. Continuing past that callback issued a
read before the wrapper applied the already-due timeout. The final code preserves
the yield and passes the test. See `defer-counterexample.txt`,
`defer-counterexample-fixed.txt`, and `src/architecture_tests.rs`.

The PoC also needs a careful contract for input/clock priority between coalesced
notifications. Guarding one zero-timeout case alone would not justify silently
changing those boundaries. This motivates an explicit FSM API proposal rather
than retaining a fast but incompletely specified shortcut.

### Falsified allocation assumption

The runtime pool recycled only unique, unlinked WaitData records with retired
scope memberships, cleared wakers before caching, bounded its TLS cache, and added
no unsafe code. Existing event/scope/wrapper tests passed. It reduced native fixed
allocation slope to 39/exchange and native chunked to 169/exchange. Nevertheless,
several latencies were worse than the non-pooling compact candidate. The pool does
not remove polling, registration, waker, scope, or I/O work, and adds reuse checks.
No claim about a specific hardware-stall cause is made. It is not in production.

## Implemented design

### One authoritative deadline, one physical wake

`timer.rs::DeadlineTimer` stores desired time separately from the armed wake:

- Set only records a desired deadline; allocate the sleep when actually polled.
- Keep an armed wake if it is no later than a new desired deadline.
- Replace a later wake immediately when the desired deadline moves earlier.
- Clear removes the wake and desired time.
- When an earlier wake fires, recheck the clock. Rearm for the desired time if it
  is still in the future; otherwise report ready without a redundant zero sleep.
- Propagate cancellation/error results even when that old wake is obsolete.

The wrapper still records the latest core Deadline, including generation. Only
State::observe calls core expiry, using that current identity. Physical wakes
never expire old tokens directly. Virtual tests cover dozens of postponements
with one timer, earlier deadlines, already-fired replacement, clear, late early
wakes, and scope cancellation. Real timer movement/cancellation is also tested.

### Retained wait lanes

ReceiveLane owns its Receiver through Rc and an `unfold` stream boxed once per
connection lane. The stream retains the same pending recv future across unrelated
inputs. Fresh runnable probes still use try_recv; an already-pending wait is
polled so scope cancellation cannot be silently bypassed. A canceled receive
schedules a fresh pass, avoiding stranded queued data after a canceled registration
is consumed. The queue Drop path still rejects/drains pending demand promises.

CancelLane lazily boxes one pending wait for each connection shutdown token and
active-exchange cancel token. Completion or cancellation discards that wait;
retirement explicitly clears it. No canceled registration is rebound to a later
exchange. Tests include channel closure, repeated unrelated polls, wrapped wakers,
queued data concurrent with scope cancellation, and token owner release.

This deliberately retains existing runtime cancellation semantics rather than
substituting a raw AtomicWaker or a channel that I/O scopes cannot cancel. There
is still dynamic dispatch for persistent stream lanes; the slight empty-workload
regression versus timer-only is recorded, not hidden.

### Shared immutable bodies, without breaking OutgoingFrame matches

New additive constructors:

```rust
OutgoingBody::shared(bytes: Rc<[u8]>)
OutgoingBody::from_shared_stream(length: Option<u64>, stream)
// stream: Stream<Item = Result<Rc<[u8]>, Error>> + 'static
```

Every full allocation must fit the configured frame/buffer limit. An Rc slice
cannot hide a much larger backing allocation via a small public subview. Ownership
is retained through partial writes, late cancellation results, and the body
receipt, not merely source completion. Shared full bodies can use existing
opt-in coalescing. Custom shared streams remain demand-driven and do not emit
trailers; the existing stream API supports trailers/receive-lease forwarding.

No variant was added to public OutgoingFrame. A private SourceFrame normalizes
owned, forwarded, and shared data. An early version put the large OutgoingData
union inside Ready; this was corrected to separate compact Vec/Rc ready variants.
A layout regression test prevents inflation of the existing OutgoingSource size.
Address, refcount, source-drop, partial-write, cancellation, rejection, and both-
transport payload tests cover the new path.

The benchmark's original Vec-producing cases are unchanged. New `native_shared`
controls explicitly prepare bounded immutable frames outside timing. Their gains
are **not** counted as transparent improvements to the old Vec workloads.

## Matched results

Xeon Platinum 8370C VM, rustc 1.98.1. Optimized + debug=2 frozen binaries; no extra
runtime features. CPU13, shared host (not exclusive). Each g/h run uses 30 samples,
1-second warmup, 2-second target measurement. Order: baseline, timer, final, final,
timer, baseline. All common cases retain default deadlines except named controls.
Raw intervals/outliers are in the logs; numbers below are means of central estimates
in us, not pooled confidence intervals or sums of incremental speedups.

| Case | Baseline | Timer only | Final | Final vs baseline |
| --- | ---: | ---: | ---: | ---: |
| empty/native | 20.426 | 14.864 | 15.151 | -25.8% |
| empty/stream | 26.175 | 20.328 | 20.867 | -20.3% |
| fixed/native | 36.942 | 27.464 | 26.712 | -27.7% |
| fixed/stream | 49.927 | 38.863 | 38.785 | -22.3% |
| fixed/native_coalesced | 25.706 | 18.892 | 18.939 | -26.3% |
| fixed/native_no_deadlines | 25.346 | 25.132 | 23.903 | -5.7% |
| chunked/native | 1459.350 | 1106.050 | 1068.600 | -26.8% |
| chunked/stream | 1950.900 | 1538.550 | 1525.700 | -21.8% |
| chunked/native_no_deadlines | 1001.625 | 1005.520 | 965.055 | -3.7% |
| chunked/native_forward | 1721.850 | 1284.400 | 1246.750 | -27.6% |
| chunked/native_copy_forward | 1697.500 | 1295.700 | 1246.900 | -26.5% |
| fragmented/native | 237.880 | 181.280 | 172.055 | -27.7% |
| fragmented/stream | 350.985 | 271.710 | 272.380 | -22.4% |

Against timer-only, native body workloads improve about 3–5%, while empty native
and stream regress about 2–3%. Stream body improvements are modest/noisy. This is
a consciously retained tradeoff, not a claim that every element is universally
faster. The unchanged opt-in coalescing path is still useful: final fixed native
26.712→18.939 us, but its documented timeout/progress granularity remains opt-in.

Separate shared-source controls (`final-shared.txt`, 50 samples/3s): fixed native
26.714 us, shared+coalesced 18.815 us, chunked native shared 1023.9 us. The latter
is ~4% below the final Vec case; an earlier PoC showed ~6%. Small shared bodies
show no meaningful speedup beyond coalescing.

### Allocation slopes

The existing separate allocator-instrumented test driver was used. These are
both endpoints plus runtime/application allocations, not core-only counts or
latency measurements. Baseline values come from the immediately preceding report;
final values are fresh in `final-allocations.txt`. Setup/warmup is differenced with
N/2N runs. Read segmentation can make slopes nonintegral.

| Case | Baseline allocations/exchange | Final |
| --- | ---: | ---: |
| empty/native | 67.4 | 43.0 |
| fixed/native | 110.8 | 52.9 |
| fixed/stream | 182.1 | 123.6 |
| chunked/native | 2964.8 | 626.0 |
| chunked/no_deadlines | 2496.8 | 624.0 |
| chunked/forward | 3395.0 | 678.3 |
| chunked/copy_forward | 3254.8 | 749.3 |

The final shared large-body control is 498 allocations/exchange and ~48 KiB of
requested allocation volume, versus ~2.15 MiB with Vec production. It removes 128
per-frame payload allocations/copies across both directions, but not all control
allocations. Source-owned fixture storage is outside these timed/steady-state
allocation slopes and still consumes memory.

### Reprofiled remaining costs

Final profiles use separate debug=2 + force-frame-pointers builds, cpu-clock:u,
997 Hz, 12 seconds, 1.5-second startup delay, CPU13; zero reported lost samples.
Only userspace self-PC shares are reported; hardware cycles/instructions are not
available. Frame-pointer/code-layout effects prevent using profiled timing as the
latency comparison. Baseline profiles remain in `../http1-wrapper-bench/`.

- Final fixed/native next_input self symbols: ~17%; WaitAsyncEventFuture::poll ~6%.
- Final large/native next_input self symbols: ~18%; wait polling ~6%.
- With shared payloads, the large-copy hot PC (~10% in Vec-producing large bodies)
  disappears from the >=0.5% list. Poll/wait/clock costs remain prominent.
- Persistent receive stream/lane polling itself consumes several percent. Fewer
  allocations did not eliminate task/waker/scope work.

Percentages can rise when total time falls. These profiles do not establish that
all remaining overhead is in the FSM, and cannot be added as independent savings.

## Validation

- Final wrapper: 122 tests passed with all features, in both debug and release.
- Final default-feature wrapper: 103 passed.
- HTTP/1 core: 144 passed; HTTP/2 composition: 316 passed.
- Both transport paths, virtual deadlines, source revocation, coalescing, forwarding,
  native short writes, cancellation/late success, observers, and reuse are covered.
- Clippy on wrapper/all targets/all features with warnings denied passed.
- Runtime event/scope tests were run for the rejected pool PoC (9/26 passed).
  The runtime source was then restored byte-for-byte; no runtime optimization is
  silently retained without wider qualification.

Not a proof of all kernel races, workload shapes, or many-connection performance.
No new unsafe code, sanitizer/Miri claim, or independent-peer requalification is
asserted. The unchanged public paths retain their existing independent-peer tests.

## Remaining architecture choices for review

See [FSM-ALTERNATIVES.md](FSM-ALTERNATIVES.md). In short: most measured overhead was
avoidable adapter/runtime work, not an unavoidable cost of owning operations.
Nevertheless, external expiry orchestration and per-fragment command/notification
granularity make some optimizations awkward. A clock-aware bounded drive API,
unified readiness inbox, transactional credit return, and compact receive/forward
leases are promising *separate* interfaces to consider. None is claimed implemented
or faster from these results.

## Reproduction / evidence

```sh
CARGO_PROFILE_BENCH_DEBUG=2 CARGO_TARGET_DIR=target/http1-wrapper-design \
  cargo bench -p kimojio-http1 --bench roundtrip --no-run
# Freeze each binary before the next build. Use the same feature graph.
taskset -c 13 BINARY --bench 'http1_wrapper/...$' --noplot \
  --warm-up-time 1 --measurement-time 2 --sample-size 30 --save-baseline UNIQUE
cargo test -p kimojio-http1 --all-features
cargo test -p kimojio-http1 --all-features --release
cargo test -p kimojio-http1
cargo test -p kimojio-http1 --release --test benchmark_allocations -- --nocapture
cargo test -p kimojio-fsm-http1 --all-features
cargo test -p kimojio-fsm-http2 --all-features
```

PoC patches, raw timing/allocator logs, the counterexample, final comparison JSON,
profiles, and hashes are retained here. Large binaries, raw perf recordings, and
complete test output are under `/tmp/http1-wrapper-design/`. Public benchmark
cases are additive; the original named workloads were not replaced with faster
source/transport policies during baseline-to-final comparison.
