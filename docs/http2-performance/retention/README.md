# Retained-storage attribution and plateau

The earlier `64 * concurrency` growth comes from **bounded closed-stream tombstone queues**, not harness history, HPACK, or receive-page pools.
Both endpoints grow their `closed_stream_order: VecDeque<u32>` at the observed boundary.
No core edit, capacity increase, or optimization forms part of this investigation.

The extended probe completes **12 runs, each with 10000 cohorts on one connection**.
All strict outcomes pass across **6030000 exchanges**.
Requested live bytes are equal at cohorts 2000, 5000, and 10000 in every run.
All corresponding intervals have zero reallocations and zero requested-live growth.
Shutdown returns every run to its pre-construction requested-live baseline.

## Frozen scope

The execution base remains `2c8d1d48a880c4dec70c8d5feeacbf45dfc8d359`.
The original probe and allocation report remain unchanged at `9bc026a5` and `05c093b9`.
The extended probe source is `974a48d2ad760455e541638592eef7b69f81af44`.

The newer API integration `b724e43cff9dd75409afbece6ffda1f984065be0` received a source comparison, not execution here.
Its entire `src/server/h2` tree matches the measured source:

```text
460916a1e650693d280003103acf7c8624c3617b
```

The API and engine changes outside that tree still need final integrated qualification.
This result resolves the earlier retained-growth question for the stated workload and configured bounds.
It does not establish behavior for every workload or later source.

## Exact attribution in the original binary

The debugger ran the unchanged original probe:

```text
Source: 9bc026a56e7b9090225e61217f4524ad7082ffb6
Binary: /workspace/kimojio-rs/target/http2-program/build-http2-allocation/frozen-9bc026a5/allocation_probe
SHA256: a3698d6817fc2ee19a4206aff37029ea06cdbe5dc0cfdd0d4ed603cfdb2405ca
```

The concurrency-1, four-cohort warmed window contains exactly two reallocations:

| Endpoint caller | Requested old size | Alignment | Requested new size | Responsible storage |
|---|---:|---:|---:|---|
| Server `commit_data_frame → retire_if_complete → forget_stream` | 32 | 4 | 64 | `closed_stream_order` |
| Client `finish_client_stream → retire_if_complete → forget_stream` | 32 | 4 | 64 | `closed_stream_order` |

Both stacks reach `VecDeque<u32>::grow` through `H2Endpoint::remember_tombstone` at `endpoint.rs:573`.
Both queues have eight entries and capacity eight before growth.
The debugger also reads the actual limit of 1024 from each endpoint.
The net increase is exactly `2 * (64 - 32) = 64` requested bytes.

At concurrency `C`, eight complete cohorts leave `8*C` closed streams per endpoint.
The next cohort crosses a queue-capacity boundary.
For the tested concurrency values, each queue doubles from `8*C` to `16*C` entries.
Thus, the combined increase is `2 * (16*C - 8*C) * 4 = 64*C` bytes.
This accounts for the original two reallocations and complete requested-live increase.

LLVM merges the identical `remember_tombstone` implementations into one physical function.
The distinct caller frames identify the client and server.
The [original reallocation trace](evidence/original-realloc.txt) preserves both stacks and queue state.
The [debugger commands](evidence/original-realloc.gdb) target only that original binary.

## Configured bound and later growth

Source references under `kimojio-fsm-http2/src/`:

- `server/h2/endpoint.rs:230–231` owns the tombstone map and order queue.
- `server/h2/endpoint.rs:551–553` removes active stream state and remembers its tombstone.
- `server/h2/endpoint.rs:570–575` inserts the map entry, appends its ID, and then evicts old entries.
- `server/h2/endpoint.rs:582–587` evicts until the map contains at most the configured entry limit.
- `server/h2/wire.rs:676` sets the default closed-stream limit to 1024.
- `server/h2/wire.rs:683–690` preserves that default while it applies shared HTTP limits.

Insertion precedes eviction.
The 1025th closed stream therefore needs a temporary 1025th queue slot.
The observed queue capacity grows from 1024 to 2048 slots.
After eviction, its length returns to 1024, but its allocated capacity remains 2048.
That allocation is 8192 requested bytes per endpoint.
The logical limit remains 1024 entries.

The tombstone map also grows during the earlier cohorts.
The [bound trace](evidence/original-bound.txt) captures its actual allocation call at `endpoint.rs:571`.
Each map requests 18448 bytes with alignment 16.
The debugger reads 2048 buckets and 1024 entries after eviction.
Together, the two queues and two maps retain `2 * (8192 + 18448) = 53280` requested bytes.

The default logical bound does not require zero spare capacity.
Existing tests for logical limits zero and one pass unchanged.
The probe does not expose a new limit or change the private default.
Its boundary marker uses the source-audited value 1024, also observed by the debugger.

## Same-connection measurements

Each run uses empty requests, 128-byte responses, and 65536-byte transport fragments.
The routes are direct H2, selected composite H2, and auto-server detection.
Each route runs concurrency 1, 8, 64, and 128.
Every run retains the same connection for all 10000 cohorts.
The total response payload is 771840000 bytes across all twelve runs.

Direct-H2 requested live bytes at selected boundaries:

| Concurrency | Cohort 8 | Cohort 12 | Cohort 100 | Cohort 1000 | Cohort 2000 | Cohort 5000 | Cohort 10000 |
|---|---:|---:|---:|---:|---:|---:|---:|
| 1 | 76784 | 76848 | 79760 | 121488 | 129680 | 129680 | 129680 |
| 8 | 102440 | 102952 | 126248 | 152872 | 152872 | 152872 | 152872 |
| 64 | 323305 | 327401 | 354025 | 354025 | 354025 | 354025 | 354025 |
| 128 | 575722 | 583914 | 583914 | 583914 | 583914 | 583914 | 583914 |

Additional checkpoints capture the default-limit transition:

| Concurrency | Cohort at 1024 closed streams | Next cohort | Requested increase between those checkpoints |
|---|---:|---:|---:|
| 1 | 1024 | 1025 | 8192 |
| 8 | 128 | 129 | 8192 |
| 64 | 16 | 17 | 8192 |
| 128 | 8 | 9 | 8192 |

That final 8192-byte increase represents two queue expansions of 4096 bytes each.
No later interval increases requested live storage in these runs.
Allocations and deallocations continue as messages pass through the connection.
Stable retained storage does not mean zero allocation activity.

The [complete boundary table](evidence/boundaries.csv) includes all routes, event counts, peaks, and signed changes.
The [raw runs](evidence/long-runs.jsonl) retain every command and harness snapshot.
The [summary](evidence/long-summary.json) contains exact success and workload totals.

## Harness exclusion without weaker assertions

The probe records the length, capacity, and requested bytes of every harness vector:

- Per-stream slots.
- Body receipts.
- Send permits.
- Queued request IDs.
- Alarm originals.
- Cancellation originals.

Both endpoints retain exactly the same vector snapshots from cohort 8 through cohort 10000.
Their combined vector storage is 3768, 15808, 112128, and 222208 bytes at concurrency 1, 8, 64, and 128.
No harness history grows and no harness storage correction is necessary.

Measurement records occupy a fixed stack array.
JSON output occurs only after shutdown and pair destruction.
Adjacent windows have identical ending and starting live-byte values.
The global ledger remains continuous across all windows.
The full payload, status, header, receipt, END, and retirement assertions remain active for every exchange.

## Tests and limits

- Default release checks: **27 passed, 0 failed, 0 ignored**.
- All-feature release checks: **27 passed, 0 failed, 0 ignored**.
- Existing tombstone-bound tests: **5 passed, 0 failed, 0 ignored**.
- Formatting and both required clippy modes pass.
- Both clippy modes use `-D warnings`.
- All twelve long runs pass, with no skipped cell.

All probes, debugger runs, builds, and tests use CPUs8–31.
CPU2 remains unleased and unused.
No timing or sampling profile ran.
The debugger observations are storage diagnostics, not performance evidence.
The earlier allocation exclusions still apply, including static producer storage and allocator overhead outside requested bytes.

No core fix follows from this bounded growth.
No leak claim or capacity increase is justified by this evidence.
The final API source and final timings remain a separate parent-controlled gate.

## Reproduction

The extended binary is:

```text
/workspace/kimojio-rs/target/http2-program/build-http2-retention/frozen-974a48d2/allocation_probe
SHA256 18707e28efa492624a1fda298cede06773b52cfdace6a2522aba401cab972b41
```

The [freeze record](evidence/freeze.json) includes all source hashes, compiler flags, and debugger commands.

From the allocation worktree, run the concurrency-1 sequence:

```sh
taskset -c 8-31 /workspace/kimojio-rs/target/http2-program/build-http2-retention/frozen-974a48d2/allocation_probe \
  direct empty 1 65536 10000 retention
```

For a separate replay, run all route and concurrency pairs:

```sh
mkdir -p docs/http2-performance/retention/replay
for mode in direct selected auto; do
  for concurrency in 1 8 64 128; do
    taskset -c 8-31 /workspace/kimojio-rs/target/http2-program/build-http2-retention/frozen-974a48d2/allocation_probe \
      "$mode" empty "$concurrency" 65536 10000 retention
  done
done > docs/http2-performance/retention/replay/long-runs.jsonl
```

For the original allocation callsites, run the commands from the freeze record.
Those commands use the preserved original binary, not the extended probe or new API binary.
