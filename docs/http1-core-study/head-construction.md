# Request field-count reuse and single-allocation heads

## Changes

Two changes were built and measured separately against the accepted bulk metadata
scanner (`cd4899b8d4f17e56db5e355df709a13dd72900d9`):

1. **Count reuse:** `Core::request` computes `header_count(&head)` once, uses it
   for the outgoing budget check, and stores the same value after acceptance.
   This does not change generated-header accounting or failure atomicity.
2. **Single-allocation construction:** retain the count change and precompute
   each request/response head's exact wire length, then allocate its vector once.

The existing `valid_headers` pass already sums serialized field lengths; it now
returns that sum instead of discarding it. Checked sizing adds the start line,
framing, optional generated expectation/connection fields, and final CRLF. A
response's generated connection field is selected once and reused for sizing
and writing. Oversized/overflowing sizes fail before allocation. The existing
per-write bound checks remain, and debug assertions check the predicted length
against the actual serialized length.

This change intentionally retains existing formatting and one serialized-header
scan per request. It does not return field counts from the encoder, cache buffers
on connections, specialize integer formatting, or change trailer construction.
No new dependency, unsafe code, or public API is introduced. Single allocation
refers to each encoded head, not every exchange: chunk terminators and queued
informational/final-head coalescing remain separate work.

## Correctness and allocation checks

New tests cover:

- Exact byte limits, one byte below, and one byte above actual encoded sizes.
- Decimal length boundaries through `u64::MAX`, including zero/empty bodies.
- HTTP/1.0 and HTTP/1.1, generated Expect headers, connection close/keep-alive,
  informational statuses, HEAD, 204/205/304, and tunnel/body suppression.
- Checked size arithmetic and validation-error precedence.
- Request rejection without state mutation, followed by acceptance at the exact
  generated-field count, with the stored count matching the accepted budget.

Existing wire, short-write, informational coalescing, protocol, and benchmark
validation tests pass. Validation commands:

- `cargo test -p kimojio-fsm-http1`
- `cargo test -p kimojio-fsm-http1 --all-features`
- `cargo test -p kimojio-http1 --lib` (37 tests)
- Matching-toolchain rustfmt and `git diff --check`

A separate counting-allocator probe, after warmup, reports identical results on
three successive exchanges:

| Workload | Client allocation sizes | Server allocation sizes | Reallocations |
| --- | --- | --- | ---: |
| Fixed 128 B | 102 B | 80 B | 0 |
| Fixed 1 MiB | 106 B | 84 B | 0 |
| Chunked 1 MiB | 109 B + 5 B terminator | 87 B + 5 B terminator | 0 |

The preceding allocation study observed one allocation plus three reallocations
for fixed client heads, and one allocation plus two reallocations for fixed
server heads. Outgoing construction had not changed between that study and this
baseline. The fresh candidate probe confirms that growth reallocations are gone.
These are whole-exchange Rust allocator observations, not production transport
allocation counts. No allocator instrumentation is present in timed binaries.

## Timing method

- Rustc 1.98.1, release optimization, release debug information level 2.
- Same validated reusable-session probe as the preceding studies, with 100 warmup
  exchanges before `CHECK=false` timing. A `CHECK=true` exchange validates exact
  outgoing wire and incoming payload before timing.
- Both roles; fixed 128 B, fixed 1 MiB, and chunked 1 MiB with 16 KiB chunks;
  continue/yield callbacks; timers disabled and default timers advancing caller
  time 1,000 ns per drive turn.
- Six paired one-second trials per cell, CPU 2 affinity, shuffled workload order,
  balanced ordering of baseline/count-only/combined binaries.
- Geometric means of paired elapsed-time ratios. Intervals are nominal 95%
  paired-bootstrap intervals (10,000 resamples), without multiple-test adjustment.
- Shared-host core microbenchmarks with simulated transport copies, not network
  latency or end-to-end throughput. No profiling or trace logging during timing.

### Default timers: small fixed exchanges

Positive percentages mean lower elapsed cost. Combined is compared directly with
the original baseline; the columns must not be added.

| Endpoint / callback | Count reuse only | Count reuse + single allocation | Single allocation increment over count-only |
| --- | ---: | ---: | ---: |
| Client / continue | 3.4% [3.0, 3.9] | 9.4% [9.2, 9.5] | 6.2% [5.7, 6.6] |
| Client / yield | 3.1% [2.4, 3.6] | 12.7% [11.6, 13.7] | 9.9% [8.6, 11.3] |
| Server / continue | -0.1% [-0.9, 0.6] | 10.2% [10.0, 10.3] | 10.3% [9.7, 10.9] |
| Server / yield | 0.2% [-0.1, 0.5] | 10.3% [10.0, 10.8] | 10.2% [9.8, 10.5] |

The count-only server rows are useful unchanged-path controls. Median combined
round-trip times are 1.180/1.126 microseconds for client continue/yield and
1.264/1.218 microseconds for server continue/yield.

With timers disabled, combined small-message improvements are 9.7%, 8.8%, 9.8%,
and 13.4%, respectively. Count-only client/continue improves 2.8%; client/yield is
inconclusive in that run. Both changes are retained based on the small-message
results and the verified allocation reduction.

### Large-body qualification

With default timers, combined-versus-baseline intervals include zero for all
large fixed and chunked cells. Point estimates range from -1.1% to +2.2%.
Disabled-timer results also vary; some fixed-body cells improve, but unchanged-
path count-only server controls fluctuate substantially too. Do not attribute
all isolated large-body deltas to the head encoder.

There is one nominally significant incremental regression: timer-enabled
chunked-server/yield is 1.9% slower than count-only [-3.6, -0.6 as reductions],
while its combined-versus-original comparison remains inconclusive. The data do
not establish a uniform large-body benefit or a universal absence of regressions.
These changes save once-per-exchange work; payload processing dominates large
transfers. Long/many-field head performance remains a useful follow-up workload.

## Evidence

[Evidence JSON](evidence/head-construction.json) records all raw timing cells,
all comparisons, binary hashes, the probe/comparison scripts, and allocation
output. Frozen binaries, source snapshots, patches, and logs remain under
`/tmp/http1-head-construction-study/`. The support copies are those used by the
preceding studies (`/tmp/http1-transition-study/`); the disabled-timer copy is the
repository benchmark support, and the enabled copy follows the existing timer
harness patch. Temporary examples are removed after measurement.
