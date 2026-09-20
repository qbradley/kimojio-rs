# Corrected-half-closure qualification: resource-progress blocker

This follow-up measures the corrected core only after its strict prerequisites pass.
It is **not final core qualification**.
The large-body prerequisite still fails, so no new timing matrix, profile, or optimization PoC occurred.
The overall core-to-wrapper gate remains blocked.

[The later producer-permission probe](PRODUCER.md) isolates the first unreached public progress boundary.
It has a separate binary, counters, and source freeze.

## Frozen identity

- Integration: `7d19cd8551191be92af945a452dd472fdf7d8440`.
- Core checkpoint: `34ee4b71604520f9ae0d1de2c2500b6d268b5cce`.
- Included corrections: `0d141664`, `3b48d765`, and the planner-error reset.
- Diagnostic source: `8172604f7e5bb90d747e9a75e8176f79f4b43e8d`.
- Compiler: Rust 1.98.1, release, `CARGO_PROFILE_RELEASE_DEBUG=2`.
- Build directory: `target/http2-program/build-http2-performance-halfclosure`.
- Diagnostic binary SHA256: `ce256d33a97d05e65f93e4a0dbeb0bca01b3e11715892483fdf0a736cb23bb80`.
- Test binary SHA256: `5296fd277cd2e1052546e5ceedc4a99c05906665d9fe005f970147af4daa65db`.

The binaries remain under `build-http2-performance-halfclosure/frozen-diagnostic/`.
`evidence/freeze.txt` contains full paths, build commands, and source hashes.
No core source change forms part of this diagnostic work.
The older `5491242b` measurements remain separate historical negative controls.

## Exact results

The unchanged ignored reproducers ran first on the imported integration:

| Reproducer | Result |
|---|---|
| Fragmented 4096-byte duplex, concurrency 8, fragment 1024 | Pass |
| 1MiB duplex, concurrency 8, full fragment | Fail: `Sent.result = Err(ConnectionFailed)` |

Only the passing fragmented reproducer lost its ignore attribute.
The large-body reproducer remains explicit and strict.
The final normal harness suite reports **5 passed, 0 failed, 1 ignored**.
Two passing tests exercise the bounded diagnostic ledger.
The explicit large-body test still fails.
Both frozen CLI modes also exit with code 101: strict mode and diagnostic mode.

`cargo fmt`, default crate clippy, and all-targets/all-features crate clippy passed.
All builds, tests, and diagnostic runs used CPUs8–31.
There are **zero new timing trials and zero new profiles**.
The CPU2 lease is released.

## Workload and causal ledger

The workload sends 1MiB in each direction on eight concurrent streams.
The producer supplies static slices of at most 32768 bytes.
The server sends response headers as soon as it receives request headers.
The transport fragment limit is 65536 bytes.
The core configuration remains default except `max_active_streams=128`.
No budget, receive window, timer progression, or assertion changed to obtain success.

The executor gives each direction one transport opportunity per turn.
It copies pending write slices directly into the pending peer read page.
It promptly releases body receipts.
A transport-read fragment is not necessarily a DATA frame or a body receipt.

The diagnostic counts complete emitted frames:

| Direction | DATA frames | One-byte DATA frames | HEADERS | WINDOW_UPDATE | Increment-one WINDOW_UPDATE | GOAWAY |
|---|---:|---:|---:|---:|---:|---:|
| Client to server | 2206 | 1697 | 8 | 1 | 0 | 0 |
| Server to client | 0 | 0 | 8 | 4412 | 3393 | 1 |

The server admitted one 32768-byte response buffer per stream but emitted no response DATA.
The first server failure occurred at turn 6117.
Its failed sent receipts reported zero accepted response bytes.
The server then emitted the remaining control frames.

At turn 6629, the server emitted:

```text
kind=7 flags=0 stream=0 length=8
payload=00 00 00 0f 00 00 00 0b
```

This is GOAWAY with last-stream ID 15 and error code 11.
At turn 6631, the server reported `ConnectionResult::ResourceExhausted`.
At turn 6632, the client reported `ConnectionResult::PeerClosed`.
Client sent receipts retained exact partial acceptance counts.
Several final buffers reported 32753 accepted bytes out of 32768.

This establishes resource-exhaustion termination and response-DATA starvation in the corrected-core workload.
It is different from the old lost-upload-half failure.
The exact private capacity predicate still needs core-owner analysis.
The ledger does not establish that larger budgets are a correct repair.
It also does not classify each short transport read as a separate refund.

## Owned-operation settlement and diagnostic limits

`BENCH_DIAGNOSTICS=1` activates a separate bounded failure ledger.
Each endpoint retains 256 recent events and one snapshot before its first failure.
It also retains at most 32 important events and fixed-size frame-parser state.
The ledger allocates strings, so it is not a performance workload.
The trial runner refuses to run with diagnostic instrumentation enabled.

Diagnostic mode defers the first error assertion.
It continues the ordinary typed read, write, wake, cancellation, body-release, and close paths.
It does not replace an original operation with a fabricated completion.
After the bounded settlement loop, it prints the causal ledger and fails unconditionally.
Ordinary mode keeps the original immediate assertions.

Both endpoints closed in the recorded failure.
Both had zero pending reads, writes, alarms, cancellations, body receipts, and send permits.
The diagnostic asserts this settlement before its final strict failure.
No original owned operation disappears to produce an apparent success.

## Evidence and replay

- `strict-reproducers-first.txt`: unchanged reproducers before diagnostic edits.
- `harness-tests.txt`: final five passing tests and one explicit ignored reproducer.
- `large-duplex-diagnostic.txt`: diagnostic test with the complete bounded ledger.
- `frozen-cli-diagnostic.txt`: the same ledger from the frozen CLI binary.
- `frozen-cli-strict.txt`: the frozen CLI with its original immediate assertion.
- `lints.txt`: both required clippy modes.
- `freeze.txt`: immutable source and binary identities.

Run the strict frozen reproducer:

```sh
taskset -c 8 \
  /workspace/kimojio-rs/target/http2-program/build-http2-performance-halfclosure/frozen-diagnostic/composition_bench \
  direct 1m 8 65536 2 steady
```

Run the causal diagnostic separately:

```sh
taskset -c 8 env BENCH_DIAGNOSTICS=1 \
  /workspace/kimojio-rs/target/http2-program/build-http2-performance-halfclosure/frozen-diagnostic/composition_bench \
  direct 1m 8 65536 2 steady
```

Both commands must fail on this freeze.
The diagnostic command must first report GOAWAY11, `ResourceExhausted`, and complete owned-operation settlement.

## Deferred decisions

The full matrix and fresh profiles remain blocked on corrected resource progress.
The 144-byte server progress move has no fresh profile on this integration.
Thus the proposed private forwarding PoC did not start.
There is no evidence here to accept or reject that PoC.
The stable-slot and header-finalization proposals also remain untouched.

After the next core correction, the strict reproducers need another run before timings.
A new timing lease and source freeze are necessary.
The parent still requires essential measurements against the final review-repair integration before the wrapper gate.
