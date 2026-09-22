# Public HTTP/1 wrapper benchmark and initial profile

## Scope

Developed a Criterion benchmark for `kimojio-http1`, using the production client,
server, bodies, driver, and transports. Starting production tree: `06ffb418`
(`uwoyuprr`), with the preceding owned-value core optimizations. The experimental
stable-write-op change is **not in this checkout**. No production wrapper, core,
or runtime logic was changed for this benchmark.

The older `examples/keepalive_bench.rs` remains unchanged. It is a useful native
application check but includes every payload comparison in timing. The new
benchmark provides Criterion distributions and explicit controls with full byte
comparison outside timing.

Files:

- `kimojio-http1/benches/roundtrip.rs`: 13-case Criterion matrix.
- `kimojio-http1/benches/support/mod.rs`: shared public-API workload driver.
- `kimojio-http1/tests/benchmark_workloads.rs`: six qualification tests.
- `kimojio-http1/tests/benchmark_allocations.rs`: separate allocation probe.

## Benchmark contract

Each Criterion batch creates **one Unix stream socket pair**, one wrapper client,
and one wrapper server in one Kimojio runtime. Both endpoints stay on that pair
for eight warmup exchanges and all measured exchanges. No connection pool, retry,
reconnect, TCP setup, TLS, external load generator, or simulated completion exists.
The core request-count limit is lifted to avoid ordinary default retirement.

The application uses POST /bench with Host and Content-Type fields. Request and
response bodies are independently generated deterministic binary patterns.
Payload sizes below are **per direction**, not total round-trip payload.

- Empty: zero request/response bytes.
- Fixed: 128 request bytes and 128 response bytes, known length.
- Chunked: 1 MiB each direction, 16 KiB producer frames.
- Fragmented: 8 KiB each direction, 512 B producer frames, chunked framing.
  This controls producer framing, not an assertion about kernel read boundaries.
- Forward controls: chunked duplex echo with either receive-lease forwarding or
  explicit vector copying. These two share the same echo policy; do not compare
  them to request-then-response as if their scheduling were identical.

The receive allocation is 16 KiB; the wrapper turn budget remains its default 64.
Default protocol timeouts are enabled (30 s head/body, 60 s idle, 1 s continue),
except in the explicitly named no_deadlines controls. Full-body coalescing is
off except in the named control. Metrics and virtual-clock features are disabled
in timing/profile builds; optional observation logging is qualification-only.

### Timing boundaries

Runtime/socket/driver setup, fixture creation, and eight warmup exchanges are
excluded using `iter_custom`. The interval starts immediately before the first
measured public Client::send and ends after the application, both drivers, and
transport shutdown complete successfully. Thus one shutdown is amortized across
the batch, not charged once per exchange. Criterion iter_custom receives that
exact duration; Elements(1) means exchanges/s, not a claimed memory-copy rate.

Timed work includes request/response object construction, owned Vec body creation
and copying, body-source polling, frame consumption, metadata conversion, channels,
scheduling, timeouts, and **real kernel I/O**. Fixed bodies use OutgoingBody::full;
streaming sources create Vec frames. No whole-body collect occurs on receive.

The selected case first runs through the same driver with full payload equality
and diagnostic logging enabled, outside Criterion timing. Every timed exchange
still checks head identity/status, exact byte lengths, absence of unexpected
trailers, and successful framing/termination. End-of-batch counters must match
both directions. Qualification also checks two successful core retirements per
exchange. Filtered-out cases do not execute validation I/O.

A 120-second batch watchdog turns stalls/incomplete settlement into failure.
Runtime panic/shutdown without a result is also a failure, never a timing sample.
The profile includes small per-batch setup/warmup portions even though Criterion
excludes them from timing; the initial profiling delay excludes startup validation.

## Reproduction and environment

Intel Xeon Platinum 8370C VM, rustc 1.98.1 (48a229cea), perf 6.6.139.1. Sequential
runs pinned to logical CPU 13, **without exclusive CPU reservation**. Treat these
as local single-connection optimization results, not service latency or scaling
claims. Do not subtract the one-endpoint core replay timings from this two-endpoint
real-I/O benchmark to infer wrapper-only overhead.

```sh
cargo test -p kimojio-http1 --test benchmark_workloads
cargo test -p kimojio-http1 --release --test benchmark_workloads -- --nocapture

taskset -c 13 cargo bench -p kimojio-http1 --bench roundtrip -- \
  http1_wrapper --noplot --save-baseline wrapper-initial
# Repeat used 50 samples, 1s warmup, 3s target measurement.
taskset -c 13 cargo bench -p kimojio-http1 --bench roundtrip -- \
  http1_wrapper --noplot --warm-up-time 1 --measurement-time 3 \
  --sample-size 50 --save-baseline wrapper-repeat

# Exact case filters should end in $, so native does not select its controls.
cargo bench -p kimojio-http1 --bench roundtrip -- 'fixed_128b/native$' --noplot
```

The initial run used Criterion defaults (100 samples, 3s warmup, 5s target
measurement); some large cases ran longer to satisfy sampling. Before the repeat,
qualification was moved inside the selected benchmark closure and augmented with
diagnostic counts; the timed exchange algorithm is unchanged. Both logs remain
in `benchmarks.txt` and `benchmarks-repeat.txt`.

## Timing results

Central estimates in microseconds per exchange:

| Workload/control | Initial | Repeat |
| --- | ---: | ---: |
| empty/native | 20.070 | 20.262 |
| empty/stream | 25.759 | 26.001 |
| fixed 128 B/native | 36.847 | 37.655 |
| fixed 128 B/stream | 48.986 | 49.317 |
| fixed/native_coalesced | 25.607 | 26.031 |
| fixed/native_no_deadlines | 24.473 | 25.438 |
| chunked 1 MiB/native | 1448.5 | 1467.9 |
| chunked 1 MiB/stream | 1917.1 | 1934.2 |
| chunked/native_no_deadlines | 1042.2 | 1015.4 |
| chunked/native_forward | 1719.7 | 1778.8 |
| chunked/native_copy_forward | 1678.9 | 1681.9 |
| fragmented 8 KiB/native | 237.88 | 236.65 |
| fragmented 8 KiB/stream | 343.64 | 344.02 |

Raw confidence intervals/outliers are in the logs. The native path is roughly
22–31% lower latency than stream across the baseline cases. Full-body coalescing
reduces small-body time about 31%; disabling deadlines reduces fixed time about
32–34% and chunked time about 28–31%. These controls change policy or transport
semantics, so they bound/locate costs rather than prove equivalent optimizations.
Lease forwarding did not win on this single-connection echo workload: fewer copied
bytes can be outweighed by tighter receive-storage backpressure and more scheduling.

## Perf methodology and evidence

```sh
CARGO_PROFILE_BENCH_DEBUG=2 RUSTFLAGS='-C force-frame-pointers=yes' \
  CARGO_TARGET_DIR=target/http1-wrapper-profile \
  cargo bench -p kimojio-http1 --bench roundtrip --no-run

perf record -e cpu-clock:u -F 997 --call-graph dwarf,16384 --delay 1500 \
  -o /tmp/http1-wrapper-bench/CASE.data -- taskset -c 13 \
  target/http1-wrapper-profile/release/deps/roundtrip-a30f13157333a1b3 \
  --bench 'http1_wrapper/fixed_128b/native$' --profile-time 12

perf report --stdio --no-children --sort symbol --call-graph none \
  --percent-limit 0.5 -i /tmp/http1-wrapper-bench/CASE.data | rustfilt
perf script -i /tmp/http1-wrapper-bench/CASE.data \
  -F time,period,ip,sym,symoff,dso --inline | rustfilt
```

Eight representative cases were profiled separately. Each produced 5,666–7,942
userspace samples, with zero reported lost samples. Frame pointers improve stack
unwinding, but register/code-layout effects mean these are attribution builds,
not the binaries used for latency results. Hardware cycles/instructions are not
available. Percentages are weighted cpu-clock **userspace** shares, not elapsed
latency fractions or guaranteed savings. Self symbols with inlined work must not
be interpreted as pure branch/dispatch overhead.

| Profile | next_input self incl. inlines | Async-event self symbols | Clock/time self symbols | libc copy | Named allocator self symbols |
| --- | ---: | ---: | ---: | ---: | ---: |
| empty/native | 17.21% | 7.52% | 7.30% | 9.07% | 4.99% |
| fixed/native | 18.52% | 8.19% | 8.10% | 9.40% | 4.61% |
| fixed/stream | 14.87% | 10.26% | 6.43% | 9.13% | 5.29% |
| fixed/coalesced | 16.74% | 7.94% | 8.82% | 9.98% | 4.70% |
| fixed/no_deadlines | 16.82% | 10.16% | 8.08% | 10.77% | 4.96% |
| chunked/native | 19.10% | 8.82% | 8.90% | 16.42% | 3.59% |
| chunked/no_deadlines | 18.27% | 10.66% | 9.21% | 20.53% | 2.96% |
| fragmented/native | 21.69% | 8.05% | 9.78% | 10.89% | 4.91% |

Raw demangled self reports and inline/copy-caller summaries accompany this file.
The HTTP/1 core physical self symbols account for about 8% in chunked/native and
11% in fixed/native, excluding called libc work; the rest must not all be called
wrapper overhead because it includes runtime, application, libc, and I/O work.
The libc copy range was resolved in preceding experiments as memcpy/memmove's
IFUNC implementation on this host, not DNS/NSS lookup work.

An additional cpu-clock (user + kernel) recording cannot resolve restricted kernel
symbols. A separate perf stat run for fixed/native reports ~1.94 user seconds and
~1.14 system seconds over ~3.09 seconds: kernel work is significant and absent
from the userspace percentages. No claim is made that all wall time can be fixed
by userspace optimizations. Raw perf data and scripts are under
`/tmp/http1-wrapper-bench/`; binary/source hashes are recorded here.

## Allocation and operation counts

The test-only System-forwarding allocator records runs with N and 2N measured
exchanges, identical setup/warmup, and pre-initialized runtime state. It uses the
same CHECK=false driver as timing (no logger or byte comparison). Differences
estimate per-exchange slopes; actual read/wake scheduling can vary, so noninteger
slopes are expected. Counts include both endpoints, body production, and runtime,
not just wrapper structs. Instrumented durations are not performance results.

```sh
taskset -c 13 cargo test -p kimojio-http1 --release \
  --test benchmark_allocations -- --nocapture
```

| Case | Estimated allocations/exchange | Requested bytes/exchange |
| --- | ---: | ---: |
| empty/native | 67.4 | 6,839 |
| fixed/native | 110.8 | 10,642 |
| fixed/stream | 182.1 | 16,982 |
| fixed/coalesced | 85.1 | 8,421 |
| fixed/no_deadlines | 99.2 | 10,489 |
| chunked/native | 2964.8 | 2,333,468 |
| chunked/no_deadlines | 2496.8 | 2,325,220 |
| chunked/forward | 3395.0 | 1,320,588 |
| chunked/copy_forward | 3254.8 | 2,346,460 |

Reallocation slope was zero. Requested bytes are cumulative allocation volume,
not peak live memory. Native chunked body production itself allocates/copies
128 x 16 KiB frames across both directions, a known workload cost.

Qualification logger counts (not timings):

- Over 40 fixed exchanges: separate writes issue 160 writes / 161 reads and 483
  deadline notifications; coalescing issues 80 writes / 81 reads and 361 deadline
  notifications. Counts include the final shutdown read/notifications.
- Over 11 native 1 MiB exchanges: 1452 writes (132/exchange), 1459 reads, 2816 body
  deliveries (256/exchange), 1408 receipts (128/exchange), and 5047 deadline
  notifications (~459/exchange).

These are core operation/callback counts, not syscall counts. Kernel read
segmentation and observation overhead can change totals. The diagnostic pass
also validates actual retired exchange counts and complete byte equality.

## Top three optimization opportunities

### 1. Avoid rearming the physical timeout on every progress refresh

Strongest controlled signal: disabling protocol deadlines removes roughly one
third of fixed-message time and 28–31% of chunked time. A normal small exchange
produces about 12 deadline notifications across the pair; a 1 MiB exchange about
459. `State::event(Event::Deadline)` replaces its SleepFuture when the logical
deadline changes, and polling/dropping those futures submits/cancels runtime I/O.
`State::observe` and observe_time also read/convert the clock repeatedly; clock/
time symbols consume 8–10% of native userspace samples.

**Candidate:** separate the core's authoritative logical deadline from the
currently armed physical wake. If progress pushes the deadline later, keep the
existing earlier wake and recheck/rearm when it fires; immediately rearm for an
earlier deadline. Coalesce superseded timer updates before arming where callback
and command boundaries permit. Avoid redundant clock reads within a single
non-suspending turn, but refresh time after actual asynchronous progress.

This must preserve deadline generation checks, continue/head/body/upload policy,
stale wake handling, and exact expiry/cancellation settlement. Disabling timeouts
is only a diagnostic control, not the proposed optimization. Do not loosen timeout
semantics simply to improve the benchmark.

### 2. Reduce fair-input polling and async wait registration churn

`next_input` and its poll closure account for 17–22% of native userspace samples;
async-event self symbols add about 8–9%. The driver constructs channel and
cancellation wait futures on each call, scans ten lanes, and drops unused waits
when any input wins. Runtime WaitFuture allocates Rc<WaitData> when first polled
pending and unregisters on drop. Allocator stacks under async waits account for
about 2.0% of all fixed/native samples and 2.5% of chunked/native samples—not the
whole allocator cost, but direct evidence of this mechanism.

**Candidates:** retain reusable wait state for an active exchange/connection;
probe already-ready native completions and channels before constructing/registering
losing waits; reduce large temporary Input/Event movement while polling. Preserve
the rotating fairness order and avoid double-polling custom handlers/body sources.
Register every eligible wait before genuine suspension, and never leave a stale
cancellation registration attached to a later operation/exchange. The existing
runnable-path try_recv optimization already exists; merely adding it again is
not new work. A broader two-pass/persistent-wait change needs fresh correctness
and performance tests—previous experiments were not uniformly successful.

The stream backend also adds worker request/completion channels and per-operation
Rc<CancellationToken> allocations. It is 28–45% slower than native in these cases.
Reuse the native approach where appropriate, or reduce worker-specific overhead
without pretending a generic write-all transport reports exact partial progress.

### 3. Reduce I/O and buffer handoffs, not just payload-copy bytes

Existing opt-in full-body coalescing reduces fixed exchange time about 31%. The
logger confirms it halves the pair's writes (4→2/exchange) and reads (about 4→2),
while reducing timer and scheduler work. This is a demonstrated useful option
for eligible workloads, not a newly implemented optimization.

**Candidates:** make the eligible full-body path easier to select explicitly;
evaluate batching adjacent streaming metadata/data when ownership, producer
backpressure, receipt, and deadline boundaries permit. The native driver already
uses pinned reusable I/O-future slots, so adding per-I/O boxes would move backward.
Investigate remaining Input/Event/WriteResult materialization before another
blanket stable-operation-slot rewrite.

For large bodies, 16.4% of native userspace samples are in libc copying, but **9.1%
of all samples are copies under the benchmark's Vec-producing body source**.
Do not mislabel that as a protocol-core copy. An immutable shared outbound-buffer
API or producer buffer-return/reuse facility could address application copies,
but changes the public ownership surface and must be measured separately.
Existing lease forwarding cuts allocation bytes substantially yet is slower than
the vector-copy echo control here. Zero-copy must be evaluated together with
receive-buffer retention/backpressure, not assumed to win from memcpy counts.

Coalescing changes timeout progress granularity (already documented by the
wrapper); retain its opt-in nature unless a separate semantic review supports
changing defaults. Kernel I/O, scheduler work, timers, and allocation savings
from these ideas overlap; do not add their percentages as independent speedups.

## Qualification and limitations

The new workload tests cover the full timed matrix, both backends, bytewise/uneven
producer frames, empty chunked bodies, duplex lease/copy modes, deadlines on/off,
coalescing, more than the default 1000 requests without reconnect, corrupted and
oversized chunks, and failure on a zero-iteration batch. Both debug and release
qualification runs passed. The full all-feature wrapper suite and Clippy also
passed; detailed counts are in validation.txt.

This establishes a repeatable single-connection pair baseline. It does not isolate
client/server independently, include network/TLS effects, or characterize many-
connection scheduling. Those should be separate workloads rather than silent
changes to this one. No production optimization was attempted in this change.
