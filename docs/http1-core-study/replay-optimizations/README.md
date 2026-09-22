# Follow-up replay optimizations

Three sequential changes, in implementation order: opportunity 3 (serialization),
opportunity 1 (metadata scans), opportunity 2 (transition selection).
The starting revision is `5cde80a7` (`ttvpssox`, perf improvement docs).

## Measurement

Fresh runs, not copied historical results. Intel Xeon Platinum 8370C, rustc 1.98.1
(48a229cea), default optimized bench profile, `bench-internals` enabled, metrics
disabled. Criterion defaults: 3-second warmup, 5-second measurement, 100 samples.
The host is shared and these runs are unpinned, sequential rather than balanced
A/B trials; treat differences as local evidence, not production speedup estimates.

```sh
cargo bench -p kimojio-fsm-http1 --bench roundtrip --features bench-internals -- \
  http1_replay --noplot --save-baseline replay-before
# Subsequent checkpoints use replay-serialization, replay-metadata, replay-transitions.
cargo test -p kimojio-fsm-http1 --all-features
```

All timed workloads retain the existing no-copy receive replay, ownership,
segmentation, scatter/gather output, callback policies, and validation driver.
Reported sizes are per direction; a round trip sends and receives that size.
This is simulated I/O, not network or memory-copy throughput.

Criterion central time estimates (microseconds per round trip):

| Workload | Before | Serialization | Metadata |
| --- | ---: | ---: | ---: |
| fixed/client/continue | 1.1456 | 1.0410 | 0.88682 |
| fixed/client/yield | 1.1042 | 1.0013 | 0.85325 |
| fixed/server/continue | 1.2064 | 1.1247 | 0.95661 |
| fixed/server/yield | 1.1442 | 1.0547 | 0.88075 |
| chunked/client/continue | 24.516 | 22.030 | 20.826 |
| chunked/client/yield | 23.698 | 21.731 | 20.254 |
| chunked/server/continue | 26.281 | 23.870 | 23.082 |
| chunked/server/yield | 24.767 | 23.175 | 21.330 |

Raw benchmark output, including confidence intervals, is in `before.txt` and
`serialization.txt`. Qualification output is in `serialization-tests.txt`.

## Opportunity 3: specialized serialization

- Replace general `write!` formatting with bounded byte appends for start lines,
  field names, separators, and trailers. Keep exact-size preflight and defensive
  checked writer limits, including trailer growth limits.
- Encode content lengths with a 20-byte decimal scratch buffer and validated
  three-digit status codes directly.
- Encode hexadecimal chunk sizes directly in the existing 24-byte operation
  prefix. No extra allocation or payload copying; partial scatter/gather write
  handling is unchanged.
- Do not pool outgoing head buffers or change operation layout/ownership.

Added decimal/hex digit-boundary tests (including zero and maximum integers),
all 100..599 status lines, exact request wire bytes, and undersized numeric
writer checks. Existing exact-head-limit, malformed-head, fragmentation,
short-write, identity, cancellation, and replay-equivalence tests all pass with
all features. The eight local replay estimates improved by roughly 6–10%.

## Opportunity 1: reuse metadata validation and accounting

- Encoders return an `EncodedHead` containing bytes, framing, and the emitted
  field count. Include generated framing, Expect, and Connection fields and
  exclude suppressed framing. Core admission and accounting use that count,
  including informational and automatic error responses; remove `header_count`.
- The only caller of `process_metadata` is the bulk receiver after
  `metadata_span` returns Complete. Every accepted CR/LF has already been
  validated, including fragment joins. Replace the duplicate `strict_lines`
  scan with a debug-only assertion, retaining strict input rejection in release.
- Keep the scalar delimiter scanner, fragmented accumulation, limits, and
  callbacks unchanged. No new SIMD dependency or borrowed-buffer shortcut.

The existing large encoder matrix now independently recounts wire CRLFs and
checks predicted fields. The exhaustive short CR/LF and long/binary scanner
oracles additionally verify that every completed section satisfies strict CRLF
validation. All-feature tests pass in both debug and release (the latter without
relying on the debug assertion). See `metadata-tests.txt` and
`metadata-release-tests.txt`. Replay results are in `metadata.txt`: roughly
15–16% lower fixed-message time and 3–8% lower chunked-message time than the
serialization checkpoint, subject to the shared-host measurement caveat.
