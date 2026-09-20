# Producer-permission probe

## Result

The application does **not** wait for upload END before it starts the response.
All eight non-END response-head commands succeed before the first request-body callback.
All eight response permits arrive, and all eight initial buffers enter the core.
The first public boundary not reached is **server DATA `WriteOp` issuance**.

The server reaches these application boundaries:

| Observation | Count or value |
|---|---:|
| Successful response-head commands | 8 |
| Response-head commands with END_STREAM | 0 |
| Send permits received | 8 |
| Successful send admissions | 8 |
| Admitted response bytes | 262144 |
| Successful `Sent` receipts | 0 |
| Failed `Sent` receipts | 8 |
| Response bytes accepted by transport | 0 |
| Total write-operation callbacks | 4421 |
| DATA write-operation callbacks | 0 |
| Request-body bytes received | 8339338 |
| Request-body bytes released | 8339338 |
| Peak application-held body receipts | 1 |
| Peak application-held body bytes | 16384 |
| Final held body receipts and bytes | 0 |
| Successful request receive-END callbacks | 0 |

The lease peak counts application-held `BodyOp` receipts.
It is not a measurement of all internal pages, allocator capacity, or protocol stream records.

## First permission points

| Turn | Event |
|---:|---|
| 3 | Stream 1 response-head command succeeds with `end_stream=false` |
| 4 | Stream 1 receives a send permit |
| 4 | Stream 1 admits a static 32768-byte response buffer with `end_stream=false` |
| 4–10 | Streams 3 through 15 accept their non-END response-head commands |
| 5–11 | Streams 3 through 15 receive permits and admit their first response buffers |
| 12 | The first request-body receipt arrives and returns in the same turn |
| Never | The server issues a DATA write operation |

Every response-head milestone records `incoming_ended=false` and `received=0`.
Every permit allows 65536 visible bytes and 65536 retained-capacity bytes.
The application admits only 32768 visible bytes.
The static payload retains no heap allocation.
Thus the application reaches admission without a permit-size or retained-capacity rejection.
A send permit reserves storage, not wire credit.
Admission alone does not prove that DATA is eligible under flow control.

The server issues control write operations, so the executor continues to service that direction.
The frozen API exposes nonempty DATA payloads in the second `WriteOp::slices()` entry.
The diagnostic counts those callbacks separately from control write callbacks.
This callback count is zero, independently of the wire-frame DATA count.

These observations exclude an application wait for upload END and an unserviced send permit.
They locate the missing progress between successful admission and core-to-executor DATA issuance.
They do not identify the private scheduler branch or capacity predicate responsible for that missing progress.

## Terminal result and remaining question

The probe retains the same strict failure and complete original-operation settlement.
The server emits GOAWAY11 and reports `ResourceExhausted`.
The client reports `PeerClosed`.
Both frozen CLI modes exit with code 101.
Diagnostic mode defers the first error assertion but still fails after the causal report.
Ordinary mode retains its immediate assertion.

The exact `ResourceExhausted` budget is a blocking question sent to the core owner.
The answer was pending when this report was recorded.
No larger budget, window change, timer change, or scheduler change forms part of this probe.
The frame totals alone do not justify a budget repair.

## Immutable identity and evidence

- Integration: `7d19cd8551191be92af945a452dd472fdf7d8440`, core `34ee4b71`.
- Probe source: `565c08ec498b22af8e7f89153c8264c588ca2bd9`.
- Binary SHA256: `e59aa8241252645c6f03b1a0120f9b174ec698bd553f33129850f3625841ca79`.
- Build: release, Rust 1.98.1, `CARGO_PROFILE_RELEASE_DEBUG=2`.
- Normal harness suite: **6 passed, 0 failed, 1 explicit ignored failure**.
- Formatting and both required clippy modes passed.
- CPU2 did not participate. Its lease remains released.

The earlier diagnostic binary and evidence remain unchanged.
The new files are:

| Evidence | Content |
|---|---|
| `evidence/producer-freeze.txt` | Build command, full binary path, source hashes |
| `evidence/producer-observations.json` | Exact counters and all eight stream milestones |
| `evidence/producer-frozen-cli.txt` | Frozen diagnostic output and final failure |
| `evidence/producer-strict-cli.txt` | Frozen ordinary-mode failure |
| `evidence/producer-diagnostic.txt` | Explicit failing test with the new counters |
| `evidence/producer-tests.txt` | Six passing tests and one ignored reproducer |
| `evidence/producer-lints.txt` | Required lint modes |

Run the bounded frozen probe on CPU8:

```sh
taskset -c 8 env BENCH_DIAGNOSTICS=1 \
  /workspace/kimojio-rs/target/http2-program/build-http2-performance-halfclosure/frozen-producer/composition_bench \
  direct 1m 8 65536 2 steady
```

The command must fail.
It must first report eight response admissions, zero server DATA write callbacks, and complete owned-operation settlement.
