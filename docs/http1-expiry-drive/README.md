# Clock-aware HTTP/1 driving and wrapper expiry orchestration

Starting production tree: `9c18bf71` (`vtvytykq`). Implementation child: `xyvykpzu`.
This follows the rejected deadline-yield PoC in
[the wrapper architecture report](../http1-wrapper-design/README.md).

## API design: additive, no new driver object or event type

Both Client and Server now offer:

```text
advance_time(now: Tick) -> Result<(), CommandError>
next_at(now: Tick, ports: &mut P) -> Result<Option<P::Output>, CommandError>
```

`next_at` is the normal driving operation with automatic expiry. The existing
Ports traits and owned-operation/completion types are unchanged. Callers still
supply time; the FSM reads no wall clock, owns no timer future, and allocates no
new scheduler state. There is no runtime mode bit or implicit permanent opt-in.

- On entry, validate time and apply all currently due phases.
- Keep the supplied Tick fixed for the synchronous call.
- Before selecting another transition, check any newly changed earliest timer.
  Deadline mutation already marks a pending notification, so no additional dirty
  state is needed. Unchanged timers cannot become due while this Tick stays fixed.
- Continue fallback can expose another due head/upload deadline; consume that
  chain before allowing further work. An error-response flush deadline can also
  be immediately due. The existing failure and settlement paths handle it.
- Normal deadline callbacks still occur, but may return None without allowing
  work past a deadline already due at the supplied Tick. Obsolete due hints may
  be superseded before notification; logs remain drive-boundary observations.
- A backwards Tick returns TimeRegression without mutations or callbacks.
  Sequence exhaustion during automatic expiry becomes Failure::SequenceExhausted,
  preserving outstanding resources and the reserved close identity.

The original `observe_time`, explicit `expire`, and `next` remain manual and keep
their semantics. New APIs can be used without changing old compositions.

`advance_time` makes input precedence explicit rather than baking a universal
race policy into completion methods:

1. Expiry first: advance_time(now), apply completion/command, drive with next_at.
2. Progress first: observe_time(now), apply completion, then next_at. A legitimate
   progress completion can refresh its deadline before expiry is considered.

Neither permits discarding a live operation. A late successful write still has
its accepted payload counted even when timeout already stopped the exchange.

This is transition-boundary expiry, not asynchronous interruption of callbacks.
Lengthy or asynchronous application work must yield and resume with a fresh Tick.

## Wrapper integration

The wrapper applies advance_time before shutdown/cancellation at drive entry,
retaining the old priority when expiry and abort coincide. It then uses next_at
with that accepted sample. After input selection it deliberately continues to
use observe_time before applying the input: this preserves the existing
completion-before-the-next-expiry-check behavior.

Deadline callbacks can now update a local wake hint and return None. The latest
hint is applied before handling the next ordinary callback/I/O event. This avoids
returning a large deadline Event and scanning the wrapper input lanes solely for
an internal timer refresh. Every such callback counts against the existing turn
budget. When the remaining budget is exhausted, the callback yields normally.
Transport/application callbacks continue to yield; no producer polling is moved
ahead of revocation notifications, and no owned I/O is silently accepted/dropped.

The wrapper no longer expires a cached deadline identity. Its existing physical
DeadlineTimer still schedules/reuses wakes, but the FSM decides whether the latest
logical deadline is due. An old wake cannot expire a superseded token.

Two guards keep the integration conservative:

- If the connection epoch cannot represent every possible u64-nanosecond Tick,
  deadline callbacks retain their old yielding path. Conversion failures therefore
  occur before issuing a subsequent I/O capability. An extreme-Instant test covers
  this fallback.
- When all four protocol timeouts are disabled, the wrapper uses legacy driving.
  If there is also no observation handle, it skips unobservable clock reads.
  Observed connections still update timestamps even without policy timers.

The no-timer clock fast path is a separate improvement. Do not attribute the
no-deadline control's gain to deadline callback coalescing.

## Assumptions checked and revised

The first correct implementation checked the full current deadline before every
internal transition. It passed the deadline regressions but added approximately
3–4% to the no-deadline controls. The final implementation:

- checks existing expiry unconditionally at entry;
- uses the existing pending-notification fact for intra-call changes;
- reads the armed time by reference on the fast path, copying the deadline only
  when actually due;
- keeps expiration/failure processing in a cold helper;
- avoids automatic-expiry/timekeeping work on the wrapper's unobserved no-timer path.

No additional FSM state, allocation, or public policy enum was required. The
initial and intermediate results remain in `clocked-trial.txt` and
`tuned-trial.txt`, rather than being represented as successful final results.

The unsafe assumption from the previous wrapper-only PoC is specifically tested:
checking only the deadline visible at drive entry is insufficient. A reused or
pipelined head can create an already-due deadline *inside* the drive. The new core
handles that case before dispatching a second request or issuing another read.

## Correctness evidence

`kimojio-fsm-http1/tests/clocked_drive.rs` covers non-yielding deadline callbacks:

- initial due head closes without issuing a read;
- legacy next remains manual after observe_time;
- time regression is transactional;
- reused/pipelined zero-time head deadlines preempt request dispatch;
- continue fallback preserves another future deadline and cannot hide another
  already-due phase;
- zero continue timeout releases the gate rather than failing the connection;
- upload expiry retains original read/write operations and accounts late positive
  write progress exactly;
- zero body/error-flush deadlines settle without delivery or an invalid error write;
- callers can explicitly choose progress-first or expiry-first body completion;
- idle expiry can reject new admission; handoff revokes all clocked HTTP authority.

Unit coverage includes continue-expiry sequence exhaustion with either a head
phase or upload-only fallback, and reserved terminal-close identity. The existing
474-schedule ownership model now compares seven modes: step reference, legacy
continue/yield/mixed, and clocked continue/yield/mixed, including callbacks/logs.

Wrapper architecture tests cover initial expiry before I/O and before concurrent
abort, a deadline born while consuming pipelined input, budgeted hint coalescing,
unrepresentable wake epochs, and observation timestamps with policy timers off.
The full existing timeout, duplex, coalescing, source revocation, cancellation,
late-result, body lease, reuse, observer, and composition suites also pass.

Final counts (including doctests):

- HTTP/1 core all features: 156 passed in debug and release; default: 148 passed.
- Wrapper all features: 126 passed in debug and release; default: 107 passed.
- HTTP/2 composition all features: 316 passed.
- Clippy, both crates/all targets/all features, warnings denied: passed.

No production changes to the runtime, HTTP/2 engine, buffer representation, or
public wrapper configuration were made. No new unsafe code or dependency.

## Combined performance

Frozen optimized + debug=2 executables, rustc 1.98.1, Xeon Platinum 8370C VM.
CPU13, no exclusive reservation. Same named public-wrapper/socket-pair workloads,
features, source modes, and instrumentation exclusions as the preceding report.
Order: baseline, final, final, baseline. Each run uses 30 samples, one-second
warmup, two-second target measurement. Numbers are means of central estimates,
not pooled confidence intervals; raw intervals/outliers are retained.

| Case | Baseline us | Final us | Time change |
| --- | ---: | ---: | ---: |
| empty/native | 15.078 | 14.224 | -5.7% |
| empty/stream | 20.855 | 20.120 | -3.5% |
| fixed 128 B/native | 26.739 | 25.521 | -4.6% |
| fixed/stream | 38.862 | 38.347 | -1.3% |
| fixed/native_shared | 26.572 | 25.392 | -4.4% |
| fixed/native_shared_coalesced | 18.717 | 18.072 | -3.4% |
| fixed/native_coalesced | 18.868 | 18.105 | -4.0% |
| fixed/native_no_deadlines | 23.848 | 22.489 | -5.7% |
| chunked 1 MiB/native | 1062.300 | 1022.300 | -3.8% |
| chunked/stream | 1524.850 | 1471.400 | -3.5% |
| chunked/native_shared | 1004.900 | 968.970 | -3.6% |
| chunked/native_no_deadlines | 962.170 | 917.240 | -4.7% |
| chunked/native_forward | 1237.600 | 1184.800 | -4.3% |
| chunked/native_copy_forward | 1244.850 | 1182.650 | -5.0% |
| fragmented 8 KiB/native | 171.170 | 166.795 | -2.6% |
| fragmented/stream | 271.095 | 260.835 | -3.8% |

The small stream change is modest and should not be over-interpreted on a shared
host. No default coalescing, timeout, transport, payload-generation, or forwarding
policy was changed to obtain these comparisons. `allocations.txt` is a separate
instrumented run; its durations are not timing evidence. This change targets
orchestration, not elimination of all channel or body allocations.

## Use and reproduce

```sh
cargo test -p kimojio-fsm-http1 --all-features
cargo test -p kimojio-fsm-http1 --all-features --release
cargo test -p kimojio-http1 --all-features
cargo test -p kimojio-http1 --all-features --release
cargo test -p kimojio-fsm-http2 --all-features

CARGO_PROFILE_BENCH_DEBUG=2 CARGO_TARGET_DIR=target/http1-expiry-drive \
  cargo bench -p kimojio-http1 --bench roundtrip --no-run
# Freeze binaries, then run in alternating order.
taskset -c 13 BINARY --bench http1_wrapper --noplot \
  --warm-up-time 1 --measurement-time 2 --sample-size 30 --save-baseline UNIQUE
```

The public core README describes the two new methods and ordering examples.
Frozen binaries and full test output are under `/tmp/http1-expiry-drive/`;
source/binary hashes, raw measurements, and the comparison table are retained here.
The design intentionally stops short of a new scheduler object, clock abstraction,
command-batch API, or change to manual driving semantics.
