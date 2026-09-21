# HTTP/1 opportunities after header-scratch optimization

## Scope and recommendation

This study measures `7c101976fdba39df35c71ed7cde08fd38debeb46`, after the
header-scratch optimization. It does not implement another optimization.

The three strongest **core** opportunities are:

1. Bulk metadata scanning/accumulation instead of per-byte `Vec` work.
2. Size- and field-count-aware outgoing encoding instead of formatting,
   repeated growth, and rescanning serialized headers.
3. Cheaper common-case transition selection, beginning with local changes
   rather than a cached final transition or boxed executor.

These are rankings of measured opportunity, not measured speedups for proposed
implementations. The first two principally target small exchanges; selection
also matters for large bodies. Different deployments can weight them differently.

## Measurement

The release probe reuses `kimojio-fsm-http1/benches/support/mod.rs`. Each endpoint
receives and sends the stated body size on a reusable connection. It validates
wire/payload bytes with `CHECK=true` before measurement, performs 100 warmup
exchanges, then times `CHECK=false` exchanges. Timed drivers retain their progress,
byte-count, and reuse assertions. Client and server are separate simulated-peer
workloads, not two simultaneously running endpoints.

- Compiler: rustc 1.98.1, release optimization, release debug information level 2.
- Metrics disabled; default no-op log callbacks.
- CPU 2 affinity. No sampler or counting allocator during elapsed-time runs.
- Five one-second elapsed-time runs per cell, shuffled workload order.
- Default timers with caller time advancing 1,000 ns before each drive turn,
  plus a separate disabled-timer build.
- Twenty gprofng userspace clock profiles: both roles, small fixed, large fixed,
  and large chunked with timers on/off; timer-enabled small/chunked yield controls;
  and independent repeats of the four timer-enabled small/chunked continue cases.
- Each profile runs ten seconds and contains approximately 2,500 samples. The
  requested interval is 2 ms, but observed sampling frequency is about 250 Hz.
  Use normalized sample percentages, not the profiler's weighted absolute CPU
  seconds. Separate process resource measurements do not support inferring that
  half the wall time was descheduled from those weighted sample totals.
- `perf` is not executable by this session's user. No branch-miss, cache-miss,
  or hardware-cycle claims are made.
- Sampled PCs are resolved with `nm` and LLVM DWARF inline stacks. Recovered
  caller/callee stacks distinguish protocol work from driver work; some long
  ancestry is incomplete. Tiny percentage differences are not reliable rankings.

This is a shared-host **core microbenchmark**, not network latency, wrapper CPU,
or socket throughput. Benchmark transport copies are included in elapsed times.
No unsupported weighting of these workloads into a production average is used.

### Current elapsed times

Default timers, continue callbacks, microseconds per endpoint round trip:

| Body in each direction | Client median [min, max] | Server median [min, max] |
| --- | ---: | ---: |
| Fixed 128 B | 1.731 [1.728, 1.739] | 1.898 [1.893, 1.911] |
| Fixed 1 MiB | 28.171 [25.789, 30.156] | 31.928 [28.389, 39.753] |
| Chunked 1 MiB, 16 KiB chunks | 42.953 [42.060, 43.282] | 44.733 [43.468, 44.849] |

The large fixed-body spread is material. Min/max are observed ranges, not
confidence intervals. Disabled-timer results and all samples are in the evidence
JSON. Timers-on/off builds are not a controlled ablation of timer instructions:
code layout and workload scheduling also differ.

### Current CPU attribution

Representative timer-enabled continue profiles; percentages of **all sampled
program CPU**, not just time inside `next`:

| Workload | `next` inclusive | Metadata accumulation loop | Selector including helpers |
| --- | ---: | ---: | ---: |
| Small client | 54.7% | 26.7% | 5.2% |
| Small server | 63.4% | 31.8% | 7.6% |
| Large fixed client | 31.5% | 1.8% | 8.5% |
| Large fixed server | 34.5% | 1.7% | 11.5% |
| Large chunked client | 44.3% | 8.7% | 8.5% |
| Large chunked server | 43.9% | 9.0% | 11.3% |

The loop and selector columns are contained in the `next` path; do not add all
columns. `Core::next` and `advance` inline into the probe's `Endpoint::next`.
The loop column uses inline attribution excluding deeper Core helpers. The
selector includes the client's separately tail-called `receive_transition`.

The former `process_metadata` initialization hotspot is no longer dominant:
its inlined work is approximately 0–0.5% in these profiles. The initialized
header loop remains only in the trailers branch.

## 1. Bulk metadata scanning and accumulation

**Evidence.** `receive_metadata` accounts for approximately **27–34%** of total
CPU on small exchanges across the timer-enabled primary/repeat profiles, and
roughly **8–10%** on large chunked exchanges across the controls. Hot PCs are in
the append-and-CRLF-completion loop at `coordinator.rs:619–669`. This remains
larger than the actual `httparse` head parser.

The current loop repeatedly obtains a byte, validates CR/LF adjacency, checks
limits and capacity, pushes one byte, and checks `ends_with`. Metadata batching
already removed per-byte global coordinator dispatch; the remaining opportunity
is inside that local loop. Sampling does not establish branch misprediction or
store-forwarding stalls as the hardware cause.

**Recommended change.** First prototype a section-local scanner over the existing
input slice. Find CR/LF boundaries in bulk, validate them, and append contiguous
runs using `extend_from_slice`, with capacity checks per run. A vectorized search
such as `memchr` is worth comparing against a scalar scanner; do not assume it
wins on tiny chunk-size lines. Keep a small cross-fragment delimiter state or
inspect the retained suffix. Initially retain the accumulator and existing
parser/callback ownership rather than coupling this to zero-copy head parsing.

**Preserve:** rejection of bare LF and malformed CRLF, cumulative byte limits,
chunk delimiter semantics, fragments ending between CR and LF, trailers, head
callback boundaries, and the current first-byte/head-deadline notification
boundary. Never scan into a body or following pipelined message just because it
is buffered.

**Experiment:** compare small heads, larger/many-field heads, one-byte fragments,
16 KiB body chunks, and small chunks. Run the request-smuggling corpus and every
transport-split tests, timer regressions, and both callback modes. A hypothetical
halving of this loop would save about 13–17% of current small-workload CPU if all
other costs stayed fixed; that is a prioritization illustration, not a forecast.

## 2. Outgoing encoding: reserve once, count once, format less

**Evidence.** With timers enabled and continue callbacks, primary/repeat small
profiles put `encode_request` at **26.3–26.4% inclusive** and `encode_response` at
**19.4–20.5% inclusive**. Separately, `header_count` consumes **5.9–6.8%** client
and **4.0–4.6%** server CPU. These counts are outside the encoder subtree.
Formatting, buffer growth, copying, and mandatory validation are included in the
encoder figures; they are not all removable overhead.

`HeadWriter` starts from `Vec::new` (`codec.rs:147`). `write!` produces several
small writes, each checked against the limit, and the buffer grows repeatedly.
`request` also calls `header_count` before checking outgoing budgets and again
when recording accepted output (`connection.rs:456–511`). The function scans
all CRLF pairs in already encoded bytes.

A **separate**, steady-state counting-allocator probe reports the same values
on three successive exchanges after warmup:

| Endpoint/workload | New allocations | Reallocations | Requested allocation/growth sizes |
| --- | ---: | ---: | --- |
| Small fixed client | 1 | 3 | 8, 22, 52, 106 B |
| Small fixed server | 1 | 2 | 16, 34, 110 B |
| Large fixed client | 1 | 3 | 8, 22, 52, 106 B |
| Large fixed server | 1 | 2 | 16, 34, 110 B |
| Large chunked client | 2 | 4 | 8, 22, 52, 106, 214, 5 B |
| Large chunked server | 2 | 2 | 16, 34, 110, 5 B |

These are whole-exchange Rust allocation counts, not call-stack-tagged allocation
counts. Source and CPU stacks identify repeated head construction as the growth
path; the additional 5-byte allocation matches the final chunk terminator.
Requested sizes are capacities requested over time, not live or copied byte totals.
No counting-allocator run contributes elapsed-time results.

**Recommended changes, incrementally:**

1. Reuse a single computed field count in `request` as a small first experiment.
2. Return field count alongside bytes/framing from the encoder, counting actual
   emitted fields instead of rescanning CRLFs. Include automatically generated
   framing, expectation, and connection headers; preserve informational handling.
3. Compute a checked bounded output size while validating the header inputs, then
   reserve once. Start with single-allocation construction before introducing a
   per-connection reuse cache and its retained-memory/ownership tradeoffs.
4. Append known string/byte pieces directly. Compare small stack integer encoders
   against general `write!` formatting for content lengths/statuses.

There is a related streaming target: `prepare_body` is **4.5–6.0% inclusive** in
primary chunked continue profiles, with much of that below `write_fmt`. Compare
fixed-stack hexadecimal chunk-size formatting against the current formatting
machinery (`coordinator.rs:517`). This should be a separate experiment, not
silently combined with the head writer change.

**Preserve:** outgoing byte/field budgets, failure atomicity, canonical wire
format, generated-header counts, suppressed bodies, informational/final head
coalescing, and partial-write ownership. Avoid reserving `max_head_bytes` for
all connections regardless of actual head size.

## 3. Reduce common-case selection work

**Evidence.** Full selection accounts for roughly **4–8%** on small continue
workloads and **8–13%** on large continue workloads across these profiles.
Timer-enabled chunked yield controls are lower: about **4.6% client / 7.9% server**.
That difference reinforces that callback/completion scheduling matters.

On timer-enabled chunked servers, `continue_due` plus its out-of-line
`Transmit::started` call accounts for approximately **2.7–3.8% of all CPU** in
primary/repeat profiles. None of these workloads requests `100-continue`, but
`continue_due` tests `exchange.expect` last, after transmit and other checks.
`Transmit::started` itself compiles to a tag test, `setne`, and `ret`; the call
boundary is significant relative to this tiny operation.

**Recommended changes:**

- Try checking whether an exchange actually expects `100-continue` before the
  remaining predicates, and inspect whether `Transmit::started` should inline.
  Test each separately: moving a condition or adding inlining can change layout.
- Fuse the guaranteed local `PrepareBody → Write` successor rather than entering
  global selection again. The earlier traces found 64 such pairs per 1 MiB send;
  this removes one selector pass per buffer, not the entire streaming cycle.
- Consider a guarded receive-local continuation across chunk delimiters/size
  lines only if profiles still justify it after metadata scanning improves.

Do **not** start with a cached final `Transition`, a complete product-state
machine, or `Box<dyn FnMut>`. The selector depends on many independently changing
ownership/readiness fields. Moving the same computation to setters is not a
saving, and indirect dispatch can cost inlining. Preserve deadline and receipt
priority, buffered early-response precedence, credit, source notifications,
cancellation, and callback boundaries. The full selector budget is an upper
opportunity bound, not the expected saving from these small changes.

## Excluded: simulated transport copies

A libc copy leaf accounts for approximately **40–45%** on chunked workloads and
**49–58%** on fixed large-body workloads. Disassembly identifies the dominant PC
as `rep movsb`. Recovered stacks put most of this under the benchmark session's
read-completion path, which copies the simulated peer's wire bytes into an owned
receive buffer. For example, 94.4% of the chunked-server primary copy leaf's
samples are attributed to `Session::round_trip`.

Removing that driver copy could improve benchmark scores without improving the
actual core. It is not one of the three recommendations. Real adapter/runtime
copy costs require a separate end-to-end profile before recommending zero-copy
transport or a buffer-ownership redesign.

## Recommended experiment order

For largest upside, start with the metadata scanner. For a short low-risk first
patch, reuse the request header count and then measure single-allocation head
construction. Follow with the local selector changes; do not combine them before
obtaining individual paired measurements. Retain all-feature/core/wrapper tests,
timer-enabled and fragmented workloads, and source/binary identities. Reprofile
after each accepted change rather than adding the opportunity percentages.

## Evidence and reproducibility

[Evidence JSON](evidence/next-opportunities.json) contains binary hashes,
per-profile commands/sample counts/selected attribution, all timing samples,
allocation output, and the exact temporary probe/collection/analysis scripts.
The raw experiments, full symbolized PC stacks, disassembly, support copies,
and frozen binaries remain under `/tmp/http1-opportunities-study/` and
`/tmp/http1-transition-study/` in this session.

The probe paths refer to those support copies. The disabled-timer copy is the
repository benchmark support file; the timer-enabled copy follows the existing
[timer harness patch](evidence/timer-harness.patch). Recreate those copies before
rerunning the embedded scripts. Temporary Cargo examples are removed after the
study. Production source and dependency files are unchanged.
