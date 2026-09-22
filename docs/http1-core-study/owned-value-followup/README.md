# Owned-value HTTP/1 follow-up

Starting tree: `ce96b903`, whose production source is `cfb7aa75`.
Keep the owned-operation API, generic buffers, callback semantics, and replay
segmentation. No boxing, stable slots, owned arena leases, or new dependencies.

## Measurement

Xeon Platinum 8370C VM, rustc 1.98.1. Initial full Criterion runs on CPU 8 were
noisy (`before.txt`, `direct.txt`). Therefore the direct-issuance comparison also
uses CPU 13 in before/direct/direct/before order, with 1-second warmup, 2-second
measurement, 50 samples, and identical optimized + debug-info build settings.
Neither CPU was exclusively reserved; these are local measurements, not production
speedup guarantees. Each run includes all eight `http1_replay` cases, no timed
receive-payload copying. The benchmark and qualification driver are unchanged.

```sh
# Full checkpoints
 taskset -c 8 cargo bench -p kimojio-fsm-http1 --bench roundtrip \
   --features bench-internals -- http1_replay --noplot --save-baseline NAME
# Debug-info binary for profiles and matched comparisons (no frame pointers)
 CARGO_PROFILE_BENCH_DEBUG=2 CARGO_TARGET_DIR=target/http1-owned-value \
   cargo bench -p kimojio-fsm-http1 --bench roundtrip --features bench-internals --no-run
# Run frozen executables sequentially for matched short comparisons
 taskset -c 13 BINARY --bench http1_replay --noplot --warm-up-time 1 \
   --measurement-time 2 --sample-size 50 --save-baseline NAME
```

For the first paired comparison, the pre-change executable is the unchanged
`target/release/deps/roundtrip-f8b1e690de642ca9`, verified against the previous
perf report's hash. The candidate uses the new target directory.

## Change 1: direct owned-body issuance (`normlzqw`)

`prepare_body` now returns its operation instead of writing it into Core.output.
The hot path constructs the operation for the port directly. Small issuance and
construction helpers are inline, allowing the compiler to eliminate ABI
intermediates. No representation or ownership contract changes.

Queued writes, short-write retries, and readiness still use the output slot.
If operation identity allocation fails, the newly constructed operation is put
back into that slot for normal settlement, never dropped. The test reference
explicitly takes the old prepare→queue→issue route and compares core state,
callbacks, operation IDs, and logs with the direct path, including both readiness
and sequence exhaustion. The 474-schedule ownership model also retains the queued
reference. All HTTP/1 all-feature tests pass in debug and release (139 each).

Mean of the two point estimates per build, microseconds (raw intervals in
`before-a.txt`, `before-b.txt`, `direct-a.txt`, `direct-b.txt`):

| Case | Before | Direct |
| --- | ---: | ---: |
| fixed/client/continue | 0.88901 | 0.85937 |
| fixed/client/yield | 0.87114 | 0.85159 |
| fixed/server/continue | 0.94422 | 0.91529 |
| fixed/server/yield | 0.88179 | 0.85148 |
| chunked/client/continue | 20.728 | 19.310 |
| chunked/client/yield | 19.959 | 18.623 |
| chunked/server/continue | 21.259 | 20.446 |
| chunked/server/yield | 20.246 | 19.208 |

The paired estimates improve about 2–3% for fixed and 4–7% for chunked cases.
A short cpu-clock perf profile confirms that prepare_body/issue_write are now
inlined; it does not establish that every compiler-generated operation copy has
vanished. Reassess remaining copies after all three changes before considering
stable storage or changing the public API. Temporary profiles, binaries, and
full test output are in `/tmp/http1-owned-value/`.

## Change 2: contiguous metadata and tiny-line fast paths (`ozqvxqxk`)

Complete metadata with no buffered prefix is parsed directly from receive storage.
Strict CRLF scanning, section boundaries, limits, and first-byte deadline behavior
are unchanged. Fragmented input uses the previous accumulation path. For borrowed
head/trailer callbacks the buffer is owned locally by the synchronous parser and
restored before returning or processing an error; no read/body lease is issued.
The transient Parsing state cannot survive a normal drive/yield boundary.

Chunk size/CRLF metadata with no prefix has a separate in-place path that does
not even move the buffer handle. Chunk digits are classified and accumulated in
one pass; extension syntax uses the existing parser. Token validation uses a
256-entry ASCII lookup table, tested against the old grammar for every byte.
We did not consolidate all HTTP semantic header checks: preserving their error
precedence warrants a separate investigation, not a broad rewrite here.

A test-only buffering reference remains available through a compile-time
parameter. Differential tests cover both roles, every split of valid/invalid
heads, informational responses, chunk sizes/extensions/overflow, chunk CRLFs,
and trailers, at four byte limits and three callback policies. They compare
full protocol state, cursor, unread suffix, callbacks, and logs. Another test
checks that a borrowed target points into the original receive allocation and
that contiguous heads need no metadata scratch allocation. Drop/address tests
cover temporary parser ownership. HTTP/1 all-feature debug/release tests pass
(143 tests each).

Final paired runs are `direct-control-c/d.txt` and `metadata-c/d.txt`; a/b files
record an earlier trial before the tiny-line specialization and token table.
Mean central estimates, microseconds:

| Case | Direct control | Metadata |
| --- | ---: | ---: |
| fixed/client/continue | 0.86427 | 0.85955 |
| fixed/client/yield | 0.85357 | 0.81912 |
| fixed/server/continue | 0.91998 | 0.88606 |
| fixed/server/yield | 0.84944 | 0.82378 |
| chunked/client/continue | 19.471 | 19.259 |
| chunked/client/yield | 19.552 | 18.570 |
| chunked/server/continue | 20.096 | 20.075 |
| chunked/server/yield | 19.796 | 19.121 |

The fixed cases improve about 0.5–4%. Chunked runs vary considerably between
trials; the paired means range from essentially unchanged to 5% lower. Do not
claim the whole metadata CPU share was eliminated: delimiter scanning and HTTP
semantic validation still occur.

## Change 3: local chunk continuations (`uwoyuprr`)

After accepting a chunk CRLF or size, buffered input can continue directly to
Size/Trailers metadata or body delivery (with credit), instead of traversing the
full selector again. No continuation is cached in the core. This optimization
is limited to internal actions with no callback, timer update, or cross-direction
policy change; it returns at a body/head/trailer callback and at any error or
input/credit boundary. Debug builds compare every chosen successor with the full
selector. Read sizes, body delivery segmentation, and separate release/credit
commands are unchanged. No new public batching API was added.

Differential streaming tests use the old full-selection, buffering-only driver
as the reference for both roles, all callback policies, credit suspension after
zero consumption, advancing timers, chunk metadata limits, and abort with an
outstanding body lease. They compare consumed payload, complete core state,
callback traces, and logs. Existing fragmentation, reuse, short-write, early-
response, cancellation, and handoff tests continue to pass.

For `metadata-control-a/b.txt` versus `continuations-a/b.txt`, paired mean
chunked times improve by 6–8%. Fixed-message results are mixed: client/continue
and server/continue means are about 2% higher, client/yield about 1% lower, and
server/yield about 0.5% higher. Retain this small-message tradeoff explicitly;
do not infer that every scheduler optimization helps every workload.

### Final baseline-to-stack comparison

Fresh before/final/final/before runs, same optimized + debug-info build settings
on CPU 13 (`before-final-a/b.txt`, `final-a/b.txt`). Means of point estimates in us:

| Case | Before stack | Final stack | Time change |
| --- | ---: | ---: | ---: |
| fixed/client/continue | 0.88669 | 0.85850 | -3.2% |
| fixed/client/yield | 0.85605 | 0.82692 | -3.4% |
| fixed/server/continue | 0.93716 | 0.89199 | -4.8% |
| fixed/server/yield | 0.88191 | 0.84158 | -4.6% |
| chunked/client/continue | 20.7725 | 18.0040 | -13.3% |
| chunked/client/yield | 19.9075 | 17.3620 | -12.8% |
| chunked/server/continue | 21.0665 | 18.4930 | -12.2% |
| chunked/server/yield | 20.1890 | 17.8100 | -11.8% |

These are matched end-to-end comparisons, not sums of per-change percentages.
The shared host still creates run-to-run variation, particularly in large cases.

### Validation and perf reassessment

- HTTP/1 all features: 144 tests passed in debug and 144 in release.
- HTTP/1 default features: 136 tests passed.
- HTTP/2 all features, including composition: 316 tests passed.
- Clippy, all HTTP/1 targets/features, warnings denied: passed.
- New/changed Rust files pass rustfmt checks with toolchain 1.98.1.

`before-perf.self.txt` and `final-perf.self.txt` are fresh server/continue 1 MiB
profiles. Both use `cpu-clock:u`, 997 Hz, DWARF 16 KiB stacks, a one-second recording
delay, eight seconds of replay, CPU 13, and the same two debug-info binaries as
the final comparison. No samples were reported lost. Hardware counters remain
unavailable, and normal-build DWARF stacks have the same limitations documented
in the previous perf report; use self-PC attribution rather than deep inclusive
stacks.

Summing all sampled PCs in this host's resolved libc copy implementation gives
approximately **10% before versus 5% after**. The retained self reports show only
symbols at least 0.5%; the total was computed with percent-limit zero, so cannot
be recovered by adding just those displayed rows. The selector remains important
(about 15% before versus 14% after), while chunk_size falls from about 2.8% to
1.6%. Inlining changes symbol attribution, so disappearing prepare_body/issue_write
symbols do not mean their entire work vanished.

Operation sizes and public ownership contracts remain unchanged. Completion and
port-side movement still exist; not all residual libc-copy samples belong to the
core. This is the evidence for the next stable-slot/owned-lease decision—not a
claim that such a redesign is now necessary. This stack deliberately does not
implement stable storage, pointer-owning operations, or per-chunk allocation.
