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
