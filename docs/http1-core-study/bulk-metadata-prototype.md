# Bulk metadata scanning prototype

## Implementation retained

`receive_metadata` now scans a bounded input span before appending it to the
existing metadata accumulator in one `extend_from_slice` call. The scanner
updates CR/LF framing state at delimiter positions rather than doing `Vec`
length/capacity updates and completion checks for every byte.

This is a **scalar delimiter scanner with bulk accumulation**, not SIMD or
zero-copy parsing. It adds no dependency, unsafe code, persistent parser state,
or public API change. `httparse` and the existing metadata processing callbacks
remain unchanged. Metadata buffer growth is bounded as before, but the capacity
chosen when appending a whole span can differ from byte-at-a-time growth.

Preserved boundaries and errors:

- Head: stop at CRLFCRLF; trailers also permit the initial empty CRLF section.
- Chunk size and chunk delimiter: stop at the first CRLF.
- Carry a trailing CR or empty-line prefix across receive-buffer fragments by
  inspecting the existing accumulator's suffix.
- Reject bare LF, repeated CR, and CR followed by an ordinary byte.
- Inspect no more than one byte beyond the remaining metadata budget. A CRLF
  error on that byte still precedes `Failure::Limit`.
- Append only accepted bytes and consume exactly the original error position.
- With a pending deadline notification, consume at most one byte, preserving
  the reused-server first-byte/head-deadline boundary.
- Do not consume the body or a following pipelined message.

## Correctness

Added a test-only byte-at-a-time oracle and differential tests for:

- All short sequences over ordinary-byte/CR/LF inputs through length seven,
  across valid retained prefixes and all four metadata phases.
- Long runs around 16/32/64/128-byte boundaries and through 1,024 bytes,
  including NUL/non-ASCII bytes, malformed delimiters, and every split point.
- Limit arithmetic, retained-byte saturation, exact consumed/retained prefixes,
  error precedence, and pending deadline notifications in the actual core.

The existing deadline/section-boundary, every-transport-split, request-smuggling,
chunk/trailer, large duplex, and benchmark validation tests also pass.

Validation:

- `cargo test -p kimojio-fsm-http1`: passed for the retained scalar implementation.
- `cargo test -p kimojio-fsm-http1 --all-features`: passed after selecting scalar.
- `cargo test -p kimojio-http1 --lib`: 37 passed.
- Matching-toolchain rustfmt and `git diff --check`: passed.

## Performance experiment

Baseline production source is the post-header-scratch implementation at
`7c101976fdba39df35c71ed7cde08fd38debeb46`; intervening changes were documentation.
Frozen binaries come from the [next-opportunities study](next-opportunities.md).
The same release probe, rustc 1.98.1, debug-information setting, CPU 2 affinity,
wire/payload validation, and 100-exchange warmup are used for both prototypes.
No sampler, trace logging, or counting allocator is active during timing.

Three variants were tested:

1. Existing byte-at-a-time accumulator.
2. Scalar delimiter scan plus bulk append (retained).
3. The same bulk algorithm using `memchr::memchr2_iter` (not retained).

Six paired trials per workload, one-second runs, shuffled workload order, and
balanced variant execution order. Results below are elapsed-time reductions
from geometric means of paired ratios; positive is faster. Intervals are
nominal 95% paired-bootstrap intervals (10,000 resamples), without multiple-test
adjustment. These are shared-host core benchmarks with simulated transport,
not end-to-end network throughput claims.

### Retained scalar prototype versus baseline

| Workload | Default timers, advancing time | Timers disabled |
| --- | ---: | ---: |
| Fixed 128 B client / continue | 23.7% [23.4, 24.0] | 25.7% [25.5, 26.0] |
| Fixed 128 B client / yield | 22.1% [21.8, 22.5] | 26.5% [26.1, 27.0] |
| Fixed 128 B server / continue | 26.2% [23.1, 28.0] | 28.7% [28.0, 29.2] |
| Fixed 128 B server / yield | 28.5% [28.1, 28.9] | 30.8% [30.6, 31.1] |
| Chunked 1 MiB client / continue | -0.6% [-4.6, 3.1] | 0.1% [-1.2, 1.6] |
| Chunked 1 MiB client / yield | 2.1% [-0.2, 4.0] | -0.6% [-4.4, 3.1] |
| Chunked 1 MiB server / continue | 0.8% [-1.1, 2.8] | -0.7% [-2.8, 1.6] |
| Chunked 1 MiB server / yield | -0.6% [-3.2, 1.6] | 2.4% [0.0, 4.6] |

The small-message improvement is clear in these workloads. Large chunked
results are mixed and mostly indistinguishable from unchanged; no universal
large-body improvement is claimed.

`memchr` improved small-message results slightly further, but several chunked
cases regressed approximately 2–3% relative to baseline. It was not retained;
the workspace manifests and lockfile are unchanged. A future length-dependent
scanner could revisit vectorization for larger metadata sections, but that is
not part of this prototype.

## Limits of qualification

Fragmentation, binary input, and long sections have correctness coverage here,
not comparative performance measurements. Before treating this as a generally
qualified replacement, extend timing to one-byte reads, large/many-field heads,
small producer chunks, and actual adapters. The current measurements use fixed
128-byte bodies and 1 MiB bodies with 16 KiB chunks/buffers in each direction.
Do not extrapolate the small-message speedup to arbitrary traffic.

## Evidence

[Raw timing evidence](evidence/bulk-metadata-prototype.json) includes all variant
samples, binary hashes, the comparison script, and its printed summaries.
Frozen binaries, both prototype source snapshots/patches, logs, and the probe
remain under `/tmp/http1-bulk-metadata-study/`. The probe/support setup is the
same as in the preceding study. Temporary Cargo examples are removed afterward.
