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
