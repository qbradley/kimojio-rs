# Runtime retention candidate a4af94fd

## Result

**The measured retention gate passes on this candidate.**
The runtime registries no longer retain completed operation history in these workloads.
Native and generic connections reach stable requested-live storage.
The remaining growth comes from bounded core storage, not the repaired registries.

The candidate also increases allocation activity.
At duplex cohort 32, allocation calls increase from 556,667 to 767,418 for native.
They increase from 546,854 to 1,010,075 for generic.
Those are observed increases of approximately 38% and 85%.
This report does not establish a speed improvement or acceptable CPU cost.

Independent safety review remains a separate gate.
The result applies only to runtime candidate `a4af94fd97c986877f1ba23188d923dbe31fae2d`.
A changed candidate requires fresh evidence.
No production optimization or additional production change occurred.
All builds and runs used CPUs 8–31. CPU2 was not used or requested.

## Exact integration and freezes

Commit `752381ee456e5f7f366c011bd2d19fbe7ca434c1` applies only the authorized runtime commit onto `598dad51`.
Every imported runtime file matches the corresponding file in `a4af94fd` exactly.
The HTTP/2 wrapper remains `d8ca94b6c6ba8439085609b289ff1ddbd6698357`.
Wrapper and core source files remain unchanged.

The existing allocation harness supplies the duplex and runtime-control measurements.
Commit `d7df50032e0f757c31c6884d55308eed21209a62` adds only later diagnostic checkpoint positions.
It supplies the cheap empty-body measurements.
The original `598dad51` evidence and all earlier frozen binaries remain unchanged.

| Measurement | Built source | Binary SHA256 |
| --- | --- | --- |
| Duplex and runtime controls | `752381ee456e5f7f366c011bd2d19fbe7ca434c1` | `ba2579a516e53c8c43e9ef438b15bd4b7eb00883a6d05cbc5d149e4f0c1dcc07` |
| Extended empty-body checkpoints | `d7df50032e0f757c31c6884d55308eed21209a62` | `c62c7f0d2755f40824aa45850e5ffc00e9f1fc21e1ff841def21f400e16602c8` |

The source manifest SHA256 values are:

- Duplex: `a1eaa602072363a4ce7a3138fa2339660ca2c2478793731af2ad05ed2d553990`.
- Empty: `67e6a7f31d52e8d7e62db319089d114ac6ecb83d66fbe586503278dc8be497a8`.

`freeze.json` records exact paths, compiler identity, source trees, and the build command.
Both binaries use release mode with debug level 2 and no extra `RUSTFLAGS`.
The baseline socket binary came from `3a7362e8`, as recorded in the original retention report.
The candidate uses the later diagnostic entry point that also supports runtime-only controls.
Its entry-future layout differs slightly. The workload and lifecycle boundaries remain the same.

## Bounds and unchanged correctness conditions

Each run had a 512MiB address-space limit and a 100-second CPU limit.
The hard CPU limit was 105 seconds. The external wall limit was 110 seconds.
Socket runs also used the existing 90-second watchdog and bounded abort settlement.
No run reached a limit.

The trace table remains fixed at 2048 sites with 24 addresses per site.
The largest candidate run used 363 sites.
The allocator preserves continuous live accounting and original allocation sites.
Reallocation remains attributed to the original allocation site.
The prefix, alignment padding, allocator rounding, libc unwinder storage, kernel storage, and RSS remain outside requested-byte counts.
These instrumented durations are not timing evidence.

All socket cases preserve:

- Every-byte payload comparison and exact lengths.
- Method, status, version, content length, and stream identity conditions.
- Actual retirement at both endpoints.
- Actual completion of both connection drivers.
- Actual response-payload delivery before the withheld upload tail in every duplex stream.

The 18 bounded runs include ten duplex cases, four runtime controls, and four empty-body cases.
All passed.
The socket totals are 70,448 retirements per endpoint, 28 driver closes, and 1,720,188,928 compared payload bytes.
All 816 required duplex overlap witnesses passed.

## Same C8 duplex workload

Each stream sends 1MiB in each direction in bounded 16KiB chunks.
The static cases repeat total cohort counts 1, 2, 8, and 32.
Runs with multiple cohorts use one warmup cohort on the same connection.
Owned-chunk controls repeat eight cohorts on both backends.

The rows at 1, 2, 8, and 32 also come from the same 32-cohort connection.
The shorter processes reproduce the corresponding live-byte boundaries.

| Boundary | Native before | Native candidate | Generic before | Generic candidate |
| --- | ---: | ---: | ---: | ---: |
| Cohort 1 | 1,691,032 | 273,596 | 1,296,813 | 419,137 |
| Cohort 2 | 3,161,712 | 273,948 | 2,126,669 | 485,185 |
| Cohort 8 | 11,690,656 | 276,572 | 6,846,285 | 487,745 |
| Cohort 32 | 46,063,760 | 317,820 | 25,991,877 | 496,193 |
| After both drivers close | 627,572 | 38,548 | 38,493 | 38,549 |
| After runtime cleanup | 1,572 | 1,572 | 1,572 | 1,572 |

Values are requested live bytes, not RSS.
The final 1572 bytes contain 548 pre-trace bytes and the 1024-byte standard-output buffer.
No traced wrapper, protocol, or runtime allocation survives that last boundary.

The owned producer adds exactly 8192 allocations and frees at cohort 8 in each backend.
Its requested-live boundaries remain identical to the static producer.
Thus the fixed registry behavior does not depend on omission of producer allocation.

### Remaining growth is attributable

| Allocation-site owner | Native cohort 1 → 8 → 32 | Generic cohort 1 → 8 → 32 |
| --- | ---: | ---: |
| Runtime | 38,836 → 39,348 → 39,348 | 39,060 → 39,572 → 39,572 |
| Core/protocol | 185,440 → 187,904 → 229,152 | 251,040 → 319,136 → 327,584 |
| Wrapper | 45,020 → 45,020 → 45,020 | 124,736 → 124,736 → 124,736 |
| Harness | 3,752 → 3,752 → 3,752 | 3,753 → 3,753 → 3,753 |
| Pre-trace bytes | 548 | 548 |

The runtime allocation sites stop increasing by cohort 8.
The small earlier runtime increases are bounded storage, not waiter or completion history.
The 512-byte increase in each backend contains 64 bytes from the task-ready queue and 448 bytes from the driver-event channel.
Their stacks resolve to `TaskState::schedule_io_internal` and `AsyncChannelUnbounded<driver::Event>::push_back`.
The corresponding source is `kimojio/src/task.rs:678` and `kimojio/src/async_channel.rs:197`.
Core/protocol allocation-site bytes match the prior run at each corresponding duplex boundary.
They include receive pages and closed-stream records.
The next experiment establishes their plateau rather than assuming one.

## Cheap same-connection plateau

Thirty-two C8 cohorts retire only 256 streams per endpoint.
That does not cross the configured 1024-entry tombstone limit.
The extension therefore uses empty requests and 128-byte responses, not more gigabytes of duplex payload.

The first extension stops at 256 cohorts.
Only after its bounded memory result does the next process continue through 4096 cohorts.
Each process still uses one connection and C8.
The longer run retires 32,768 streams per endpoint.
Its checkpoints at 256, 1024, and 4096 belong to that same connection.

An empty upload cannot support the withheld-tail overlap condition.
This case retains full payload, identity, retirement, and driver-close conditions, but does not claim duplex overlap.

| Cohort | Native live bytes | Generic live bytes |
| ---: | ---: | ---: |
| 1 | 205,322 | 281,667 |
| 2 | 205,674 | 282,051 |
| 8 | 207,882 | 284,163 |
| 32 | 216,330 | 292,675 |
| 128 | 250,122 | 330,259 |
| 256 | 258,314 | 338,451 |
| 1024 | 258,314 | 338,451 |
| 4096 | 258,314 | 338,451 |
| After drivers close | 37,971 | 37,972 |
| After runtime cleanup | 1,572 | 1,572 |

Live allocation counts also remain identical at 256, 1024, and 4096.
There are no further reallocations at those boundaries.
Each process has 35 cumulative reallocations at all three points.

The plateau allocation-site owners are:

| Owner | Native bytes | Generic bytes |
| --- | ---: | ---: |
| Runtime | 37,876 | 38,296 |
| Core/protocol | 171,769 | 171,769 |
| Wrapper | 44,380 | 124,096 |
| Harness | 3,741 | 3,742 |
| Pre-trace bytes | 548 | 548 |

### Actual pool and tombstone sites

The frozen empty-body stacks identify these retained core allocations:

- Two tombstone hash tables of 18,448 bytes each, through `H2Endpoint::remember_tombstone` at `server/h2/endpoint.rs:571`.
- Two FIFO queues of 8192 bytes each, at `endpoint.rs:573`.
- Three receive-page buffers of 32,768 bytes each, through `engine.rs:1321`.

These paths are under `kimojio-fsm-http2/src/`.
The default tombstone bound is 1024 at `server/h2/wire.rs:676`.
The endpoint inserts before eviction at `endpoint.rs:570–580`.
That extra temporary entry requires FIFO capacity 2048, or 8192 bytes per endpoint.
The observed increase from cohort 128 to 256 is exactly 8192 bytes across both queues.
No later history-dependent increase occurs in the measured sequence.

The receive-page allocator stops at `max_receive_capacity`, at `engine.rs:1312–1322`.
The default bound is 8MiB per endpoint, at `engine.rs:43`.
The empty workload retains only three page buffers across the pair.
This is an observed workload plateau, not a claim that every permitted workload uses that same small amount.

The runtime completion pool retains its separate 4096-record cap at `kimojio/src/task.rs:39,859`.
The candidate duplex boundary contains seven 144-byte completion objects, rather than 131,144 native objects.
This workload does not fill the pool cap.
Runtime destruction releases the remaining pool storage.

## Actual registry allocations and costs

All ownership conclusions use allocation backtraces and matching binary addresses.
`static-32-sites.json` and `empty-4096-sites.json` contain the complete inline chains.
`registry-sites.json` groups the named runtime structures.
The site ledger reconciles exactly with the continuous ledger at every noninitial boundary.
The differences are three pre-trace allocations, one pre-trace free, and 548 live pre-trace bytes.
Reallocation counts reconcile without an offset.

At duplex cohort 32, both backends retain:

- Seven `Rc<Completion>` objects: 1008 bytes.
- Seven `Rc<WaitData>` objects: 672 bytes.
- Seven reverse-membership vectors: 224 bytes.
- Two completion maps: 168 bytes.
- Two waiter maps: 236 bytes.

Native retains two registry objects, or 224 bytes.
Generic retains four registry objects, or 448 bytes.
Those counts represent live work and small map capacity, not all earlier operations.
Registry maps shrink when capacity exceeds both 64 slots and four times their live length.
The target is the greater of 32 slots and twice the live length, at `kimojio/src/task/io_scope.rs:53–57`.
This rule bounds spare capacity relative to live work. It is not a universal 64-entry limit.

The allocation cost changed:

| Backend / source at cohort 32 | Allocations | Reallocations | Deallocations |
| --- | ---: | ---: | ---: |
| Native before | 556,667 | 3,169 | 117,789 |
| Native candidate | 767,418 | 37 | 767,240 |
| Generic before | 546,854 | 3,142 | 250,809 |
| Generic candidate | 1,010,075 | 38 | 1,009,886 |

These are process-cumulative counters at aligned workload boundaries.
They are not per-window or timing estimates.

The new reverse-membership vector allocates through `IoScopeRegistry::register_wait`, at `kimojio/src/task/io_scope.rs:90`.
That site accounts for 341,115 allocations in native and 329,414 in generic.
Only seven vectors remain live at each boundary.
Each new waiter contains a vector and now requests 96 bytes instead of 72 bytes.

Generic uses a short inner scope for each transport operation.
It records 133,806 registry-object allocations and 133,036 completion-map allocations.
Native records 772 and four, respectively.
Registry objects originate at `IoScopeCompletions::registry`, at `task/io_scope.rs:167`.
Completion-map insertion occurs at `task/io_scope.rs:65–68`.
Only four generic registry objects and two completion maps remain live at the boundary.

The candidate removes the retention defect but does not remove allocation churn.
The observed counts are a cost for the later CPU qualification, not proof of a CPU regression.
No optimization proposal or source change follows from this allocation-only checkpoint.

## Same runtime-only controls

All four original controls repeat 4096 operations.
The waiter controls poll an event wait to pending, then drop the future and event.
The NOP controls await genuine runtime I/O completions.

| Control | Candidate live bytes, operation 1 through 4096 | Before, at 4096 | Candidate after work | After runtime |
| --- | ---: | ---: | ---: | ---: |
| Scoped wait | 33,451, exactly flat | 360,999 | 33,255 | 548 |
| Unscoped wait | 33,257, exactly flat | 33,321 | 33,257 | 548 |
| Scoped NOP | 33,834, exactly flat | 623,342 | 33,638 | 548 |
| Unscoped NOP | 33,640, exactly flat | 33,688 | 33,640 | 548 |

Every checkpoint at 1, 2, 8, 32, 128, 512, and 4096 has the same live bytes within each candidate control.
The scoped controls now behave like bounded live-work registries.
Their small constant overhead remains explicit.

## Tests, replay, and readiness

The focused runtime `io_scope` suite passed 22 release tests.
The harness passed seven controls with default features and seven with all features.
Both required clippy modes passed with `-D warnings`.
Formatting, exact source integration, ledger reconciliation, and plateau assertions passed.

Build and run the focused tests from this worktree:

```sh
export CARGO_TARGET_DIR=/workspace/kimojio-rs/target/http2-program/build-http2-wrapper-performance
export CARGO_PROFILE_RELEASE_DEBUG=2
taskset -c 8-31 cargo build --release -p kimojio-http2 --bench allocations
taskset -c 8-31 cargo test --release -p kimojio --lib --no-default-features -- io_scope
taskset -c 8-31 cargo test --release -p kimojio-http2 --bench harness_tests
taskset -c 8-31 cargo test --release -p kimojio-http2 --bench harness_tests --all-features
taskset -c 8-31 cargo clippy -p kimojio-http2 -- -D warnings
taskset -c 8-31 cargo clippy -p kimojio-http2 --all-targets --all-features -- -D warnings
```

The retention runner remains unchanged in the earlier report directory.
Use a new output directory for replay:

```sh
taskset -c 8-31 python3 docs/http2-performance/wrapper/retention/run.py \
  --binary /workspace/kimojio-rs/target/http2-program/build-http2-wrapper-performance/retention-candidate-752381ee/allocations \
  --backend native --cohorts 32 \
  --output-directory docs/http2-performance/wrapper/retention-candidate-a4af94fd/replay
```

Run the cheap extension separately:

```sh
taskset -c 8-31 python3 docs/http2-performance/wrapper/retention-candidate-a4af94fd/empty.py \
  --binary /workspace/kimojio-rs/target/http2-program/build-http2-wrapper-performance/retention-empty-candidate/allocations \
  --backend generic --cohorts 4096 \
  --output-directory docs/http2-performance/wrapper/retention-candidate-a4af94fd/replay
```

Every `*-run.json` contains its command, limits, binary hash, and exact result.
Runtime-control records also contain both diagnostic environment variables.
`all-boundaries.csv` supplies all cumulative counts and live-object totals.
Large trace and symbol files use gzip compression with hashes in `evidence-sha256.json`.

This candidate is ready for the next retention gate decision.
Independent safety review and the final integrated source freeze still precede timing authorization.
The higher allocation counts require honest common-path CPU measurement after that authorization.
There is no CPU2 lease request, statistical result, RSS claim, or universal memory bound in this report.
