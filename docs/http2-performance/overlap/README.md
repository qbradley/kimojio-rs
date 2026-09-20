# Actual duplex payload overlap

This report covers a correctness regression, not a performance measurement.
The positive source starts at parent integration `2a27281d6d7080572cad36ab73d3b02b435543eb`.
No core source, resource limit, API, or optimization changes form part of this work.
CPU2 remained unleased and unused.

## Result

The same regression fails with `c08b7333` and passes with `18a8f392`.
The four Rust harness files are byte-identical in both controls.
The manifests and lockfile also match.
The only core source difference is the scheduling correction in `src/engine.rs`.

The workload uses direct H2, eight streams, 1MiB per direction, and 65536-byte transport fragments.
Eight warmup cohorts and two further cohorts produce 80 exchanges and 160MiB of payload.
Each cohort requires overlap independently.

| Observation | c08 negative control | 18a8 positive control |
|---|---:|---:|
| Complete cohorts | 10 | 10 |
| Cohorts with actual response-payload overlap | 0 | 10 |
| First response payload per cohort | 16384 bytes | 16384 bytes |
| Request payload accepted at first delivery, cohort 1 | 8388608 / 8388608 bytes | 49152 / 8388608 bytes |
| Request payload accepted at first delivery, cohorts 2–10 | 8388608 / 8388608 bytes | 65536 / 8388608 bytes |
| Native test result | 0 passed, 1 failed | 1 passed, 0 failed |
| Cargo exit code | 101 | 0 |

Both controls complete all original payload, receipt, status, header, END, and retirement assertions.
Both controls accept exactly 83886080 request-payload bytes and deliver exactly 83886080 response-payload bytes.
The negative control fails the added assertion after both endpoints complete shutdown.
No ignore attribute, expected-panic annotation, or relaxed assertion hides this regression failure.

Evidence:

- [Negative control output](evidence/negative-regression.txt)
- [Positive control output](evidence/positive-regression.txt)
- [Source, compiler, binary, and result identities](evidence/freeze.json)

## Observation boundaries

Request acceptance means successful completion of the original client write with `WriteOutcome::Written(length)`.
The observer counts only the accepted prefix of the second write slice.
For this API, that slice contains DATA payload, not a frame header or a control frame.
The count is `length.saturating_sub(write.slices()[0].len())`.
This formula also handles a partial header and a cursor inside the payload.

The executor updates the counter immediately after successful write completion and before peer read completion.
The client `BodyOp` callback records the first nonempty response payload.
That callback reads the current transport counter before the next transport transfer.
The overlap condition is strictly `accepted_request_payload < cohort_request_payload`.

Permits, admissions, delayed `Sent` callbacks, and delayed END callbacks cannot satisfy this condition.
The observer resets the transport counter and first-delivery record at each cohort boundary.
Completion requires exact total transport acceptance for that cohort.
The final assertion requires zero missed cohorts.

The observer requires one positive response-payload delivery before all uploads complete.
It does not establish fairness for every stream or a response-latency bound.
This regression covers the stated direct-H2 workload, not every matrix cell.

## Test and timing scope

All new observer fields, counters, output, and hooks use `#[cfg(test)]`.
The regular release example contains none of this new state or these instructions.
The test binary does include counter operations and diagnostic output.
Its execution costs are not performance evidence.
The existing test runner computes elapsed values internally, but this report makes no timing comparison.

The positive integration passes:

- Default harness: **11 passed, 0 failed, 0 ignored**.
- Default ownership tests: **5 passed, 0 failed, 0 ignored**.
- All-feature harness: **11 passed, 0 failed, 0 ignored**.
- All-feature ownership tests: **5 passed, 0 failed, 0 ignored**.
- `cargo fmt --all`.
- Default clippy with `-D warnings`.
- All-targets, all-features clippy with `-D warnings`.

The observer unit tests cover early delivery, late delivery without notifications, empty delivery, and cohort reset.
All builds and tests used CPUs8–31 and separate build directories.
No new performance matrix, paired timing trial, or profile ran.
The earlier completion-only measurements remain historical evidence.
The final core and wrapper qualification still require the later integrated API source and fresh measurements.

## Frozen identities

| Control | Source commit | Starting integration |
|---|---|---|
| Positive | `15a1d065ec922369b9660d8875f088a31c4a80ab` | `2a27281d6d7080572cad36ab73d3b02b435543eb` |
| Negative | `27838c58208004d1f8800e8e854b5cf5b29672ba` | `eaac15fa2c52ba9ff86f193784485a7d5e48e035` |

The negative source contains only the same harness commit on the earlier c08 baseline.
It does not contain `18a8f392` or the rejected forwarding PoC.
The positive source also excludes that PoC.

The frozen positive test binary is:

```text
/workspace/kimojio-rs/target/http2-program/build-http2-performance-overlap-positive/frozen-15a1d065/composition_bench-tests
SHA256 f98d88232b61fae9558509b0a9535f6eb0188c59500a942a789e36ffac55379a
```

The frozen negative test binary is:

```text
/workspace/kimojio-rs/target/http2-program/build-http2-performance-overlap-negative/frozen-27838c58/composition_bench-tests
SHA256 a0c5b8632ec78a4c7ff5d484641d67e37b22229d923b4193b496e1c5d197465d
```

Both builds used Rust1.98.1, LLVM22.1.8, and `CARGO_PROFILE_RELEASE_DEBUG=2`.
Neither build used `RUSTFLAGS` or `CARGO_ENCODED_RUSTFLAGS`.
The freeze file includes each harness SHA256, the core source tree, and the all-feature test binary.

## Reproduction

Run the positive regression from its frozen worktree:

```sh
cd /workspace/kimojio-rs/target/http2-program/worktrees/http2-performance
CARGO_TARGET_DIR=/workspace/kimojio-rs/target/http2-program/build-http2-performance-overlap-positive \
CARGO_PROFILE_RELEASE_DEBUG=2 taskset -c 8-31 \
cargo test --release -p kimojio-fsm-http2 --example composition_bench \
duplex_payload_overlap_regression -- --nocapture
```

Run the negative regression from its separate worktree:

```sh
cd /workspace/kimojio-rs/target/http2-program/worktrees/http2-overlap-c08-negative
CARGO_TARGET_DIR=/workspace/kimojio-rs/target/http2-program/build-http2-performance-overlap-negative \
CARGO_PROFILE_RELEASE_DEBUG=2 taskset -c 8-31 \
cargo test --release -p kimojio-fsm-http2 --example composition_bench \
duplex_payload_overlap_regression -- --nocapture
```

The negative command returns 101 with `duplex overlap missing in 10/10 batches`.
Its complete payload and shutdown checks precede that failure.
