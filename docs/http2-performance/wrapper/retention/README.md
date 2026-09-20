# Retention attribution: connection-lifetime runtime registries

## Result and blocking gate

The large growth is a production retention defect in the runtime's `io_scope` registries.
It is not retained HTTP payload, harness history, or the old core tombstone plateau.
Both wrapper backends keep an `io_scope` open for the complete connection.
Its registries retain completed I/O objects and obsolete event waiters until scope exit.
Storage grows with operation history, not the number of live streams.

There was no connection-live plateau through 32 cohorts.
Native requested live bytes reached **46,063,760**. Generic requested live bytes reached **25,991,877**.
All payload, overlap, retirement, and close assertions passed.
Runtime cleanup released all traced allocations except the 1024-byte standard-output buffer.
Thus this is connection-lifetime retention, not evidence of a permanent post-runtime leak.

The next gate is a common-runtime correction from the owner.
No production source changed during this investigation.
CPU2 was not used. All builds and runs used CPUs 8–31.
There were no statistical timing runs, CPU profiles, or optimization candidates.

## Frozen sources and method

The production integration remains `c8f6014aa3128279690834f38a82373d3d31e4c2`.
The wrapper remains `d8ca94b6c6ba8439085609b289ff1ddbd6698357`.
The original harness binaries and evidence remain unchanged.

| Diagnostic | Source commit | Binary SHA256 |
| --- | --- | --- |
| Socket cohorts | `3a7362e8b1198739f5ad21e11fdaddef1d7ebb30` | `e02f48e5d54da1b1b99b3ac83cbfb17f12fba34c7bb7e027e941b55acebd2b48` |
| Runtime controls | `15a3ebed4e5420fd0a7b66b9ee714a709f0ef289` | `a5bc0260cdd341889fea6b3673c76d5a75c391d423203a193eb21f84acf46039` |

`freeze.json` records exact binary paths, source manifests, compiler identity, and build commands.
The binaries are in the private build directory under `retention-frozen/`.
Both use the release profile with debug level 2.
The control commit adds only a runtime-only diagnostic branch.
Socket attribution uses the earlier retained binary, not the control binary.

The allocator prefix preserves the original allocation site through reallocation and free.
All successful allocations use full backtraces, not sampling.
The trace stores 24 addresses per site in a fixed table of 2048 sites.
It aborts rather than silently exceeding that table.
The largest run used 341 sites.
The table contains addresses and counters, not references to application objects.

The ledger continues across every boundary.
It never resets requested live bytes.
Snapshots write directly to a preopened file without heap-backed snapshot copies.
The snapshots occur after successful cohort retirement, after driver close, and after `kimojio::run` returns.
The last boundary follows runtime destruction.

The initial diagnostic labels the post-close `cohort` field with the exchange count.
For example, `256` at that boundary means 32 cohorts × C8.
Connection-live labels contain actual cohort counts.
This report uses the correct cohort counts.

The site ledger and continuous ledger reconcile exactly after every socket boundary.
Their difference is always 548 bytes of pre-trace process storage.
The prefix and alignment padding are excluded from requested bytes.
The fixed trace table, libc unwinder storage, allocator rounding, kernel memory, and RSS are also outside these counts.
Instrumentation changes layouts and execution cost. Its durations are not performance results.

## Safe escalation and exact results

Each process had these limits:

- 512MiB address-space limit.
- 100 seconds of CPU time, with a 105-second hard limit.
- 110-second external wall limit.
- 90-second harness watchdog, followed by bounded abort settlement.
- No core dump.

The sequence started at one cohort, then increased to two and eight.
Only after those runs stayed below the limits did the sequence increase to 32.
It stopped there because the source and controls established the cause.

Every socket run used C8, 1MiB in each direction, 16KiB chunks, and the strict duplex gate.
Runs with multiple cohorts used one warmup cohort on the same connection.
The 32-cohort run therefore used one warmup plus 31 measured cohorts.
Snapshots at 1, 2, 8, and 32 belong to that same connection.
Separate shorter runs reproduced the corresponding boundaries.

| Cohorts | Native live bytes | Generic live bytes |
| ---: | ---: | ---: |
| 1 | 1,691,032 | 1,296,813 |
| 2 | 3,161,712 | 2,126,669 |
| 8 | 11,690,656 | 6,846,285 |
| 32 | 46,063,760 | 25,991,877 |
| After both drivers close | 627,572 | 38,493 |
| After runtime cleanup | 1,572 | 1,572 |

The exact cumulative event counts in the 32-cohort processes are:

| Backend / boundary | Allocations | Reallocations | Deallocations |
| --- | ---: | ---: | ---: |
| Native cohort 1 | 17,239 | 162 | 3,754 |
| Native cohort 2 | 35,441 | 263 | 7,437 |
| Native cohort 8 | 139,087 | 853 | 29,509 |
| Native cohort 32 | 556,667 | 3,169 | 117,789 |
| Native after close | 556,752 | 3,182 | 552,596 |
| Native after runtime | 556,834 | 3,182 | 556,831 |
| Generic cohort 1 | 18,478 | 143 | 7,920 |
| Generic cohort 2 | 35,091 | 245 | 15,741 |
| Generic cohort 8 | 136,690 | 830 | 62,748 |
| Generic cohort 32 | 546,854 | 3,142 | 250,809 |
| Generic after close | 546,939 | 3,145 | 546,874 |
| Generic after runtime | 547,021 | 3,145 | 547,018 |

These are process-cumulative counters, not window deltas.
`cohort-counts.csv` contains the same numbers.
Ten socket runs passed 816 retirements per endpoint and 816 required overlap witnesses.
They compared 1,711,276,032 payload bytes and completed 20 driver closes.

The owned-chunk runs reached identical live-byte boundaries through eight cohorts.
They added exactly 8192 allocations and 8192 frees in each backend.
That matches eight cohorts × eight streams × two directions × 64 chunks.
The retained growth therefore does not come from those payload allocations.

## Allocation-site ownership

`addr2line -a -f -C -i` resolved actual allocation return addresses against the frozen socket binary.
The analysis subtracts the recorded executable base and one byte from each return address.
It does not mix runtime addresses with unrelated file addresses.
`site-symbols.json` contains the full inline chains.
`native-sites.txt` and `generic-sites.txt` contain the largest surviving stacks.

At cohort 32:

| Structure or allocation owner | Native bytes / objects | Generic bytes / objects |
| --- | ---: | ---: |
| `Rc<Completion>` | 17,835,584 / 131,144 | 952 / 7 |
| `Rc<WaitData>` | 22,145,688 / 307,579 | 21,302,928 / 295,874 |
| Scope completion vectors | 1,572,864 / 2 | 64 / 2 |
| Scope waiter vectors | 4,194,304 / 2 | 4,194,304 / 2 |
| Runtime completion-pool vector | 32 / 1 | 64 / 1 |
| Other runtime allocation sites | 36,752 / 17 | 36,752 / 17 |
| Core/protocol allocation sites | 229,152 / 70 | 327,584 / 76 |
| Wrapper allocation sites | 45,084 / 26 | 124,928 / 29 |
| Harness allocation sites | 3,752 / 35 | 3,753 / 35 |
| Pre-trace process storage | 548 | 548 |

The named runtime registries account for the large growth.
Harness bytes stayed constant from cohort 1 to cohort 32.
Wrapper-site bytes also stayed constant.
Core/protocol bytes increased from 185,440 to 229,152 for native.
They increased from 251,040 to 327,584 for generic.
Those amounts cannot explain the tens of megabytes in the runtime registries.
The remaining source categories identify allocation sites, not exclusive ownership of every field inside an object.

### Exact chains and retention mechanism

All production references in this section match the frozen integration.

1. `kimojio-http2/src/driver.rs:502–514` wraps the whole native connection in `operations::io_scope`.
   The generic connection does the same at `driver.rs:522–542`.
2. `kimojio/src/task.rs:183–186` defines `IoScopeCompletions`.
   It contains `Vec<Rc<Completion>>` and `Vec<Rc<WaitData>>`.
3. `Task::register_io`, at `task.rs:199–203`, appends a strong completion reference.
   `RingFuture::with_polled`, at `ring_future.rs:111`, calls it.
4. `Task::register_wait`, at `task.rs:228–232`, appends a strong waiter reference.
   Both branches of `WaitFuture::poll` call it at `async_event.rs:467,480`.
   A repeated pending poll can also append another reference to the same waiter.
5. `WaitFuture::drop`, at `async_event.rs:498–505`, unregisters from the event's intrusive list.
   It does not remove the scope's strong references.
6. `IoScopeFuture::poll`, at `operations.rs:1959–2000`, preserves the registry on `Pending`.
   It does not prune completed operations or obsolete waiters there.
   `retain_incomplete` runs during scope termination at `operations.rs:1871,1888`.

These representative file addresses come from the frozen socket binary:

| Address | Site and chain |
| --- | --- |
| `0x19a51b` | `TaskState::new_completion`, `task.rs:886`, allocates a 136-byte `Rc<Completion>` |
| `0x10f1c5` | Native `write_once`, `kimojio-http2/src/io.rs:246`, through `writev_with_timeout` |
| `0x19ad88` | `Task::register_io`, `task.rs:202`, grows the completion registry |
| `0x1a0c91` | `WaitFuture::poll`, `async_event.rs:471`, allocates a 72-byte `Rc<WaitData>` |
| `0x1a0d99` | `Task::register_wait`, `task.rs:231`, grows the waiter registry |
| `0xf6ec4` | Native `driver::next_input`, `driver.rs:1621`, waits for a driver event |
| `0xf74a5` | Generic `driver::next_input`, `driver.rs:1621`, follows the corresponding waiter path |

`symbol-ranges.txt` records the matching symbol starts.
The completion allocation is at `TaskState::new_completion+0x13b`.
The waiter allocation is at `WaitAsyncEventFuture::poll+0x221`.
The registry sites are `Task::register_io+0x38` and `WaitAsyncEventFuture::poll+0x329`.

Generic I/O uses shorter nested scopes in `kimojio-http2/src/io/stream.rs:122,144`.
That explains its small completion count.
The long outer connection scope still retains its driver waiters.

After close, native retains 4098 completion objects.
The runtime pool permits 4096 entries at `kimojio/src/task.rs:40,865–875`.
Those pooled objects account for 557,056 bytes, plus a 32,768-byte vector.
Two additional completion objects remain at this immediate boundary.
All disappear by runtime cleanup.

The final traced 1024 bytes belong to the standard-output `LineWriter` buffer.
Its stack resolves through `std::io::stdio::stdout` and `BufWriter::with_capacity`.
The other 548 bytes precede tracing.
The runtime-only controls do not print a report and finish at exactly 548 bytes.

## Runtime-only positive and negative controls

The control binary removes HTTP, sockets, wrapper objects, and payload producers from the experiment.
Each control performs 4096 operations, either inside one `io_scope` or without that scope.

The waiter control creates an event, polls its wait future to `Pending`, then drops the future and event.
No waiter remains active at each snapshot.
The I/O control awaits a real `operations::nop()` completion each iteration.

| Control | After operation 1 | After operation 4096 | After scope/work ends | After runtime |
| --- | ---: | ---: | ---: | ---: |
| Wait, scoped | 33,423 | 360,999 | 33,351 | 548 |
| Wait, unscoped | 33,321 | 33,321 | 33,321 | 548 |
| NOP, scoped | 33,686 | 623,342 | 623,342 | 548 |
| NOP, unscoped | 33,688 | 33,688 | 33,688 | 548 |

Both unscoped controls remain exactly flat at all seven boundaries.
The scoped waiter control accumulates 4096 obsolete 72-byte objects and its registry.
The scoped NOP control accumulates 4096 completion objects and its registry.
After scope exit, the bounded completion pool accounts for the NOP retention.
Runtime destruction releases that pool.

These controls reproduce the ownership mechanism without the benchmark's application state.
They support the source analysis rather than an inference from polling-context totals.

## Minimal correction proposal for the owner

The common runtime must retire obsolete scope registrations during a long-lived scope.
A focused correction can preserve the current cancellation design:

1. Deduplicate waiter registration for repeated polls of the same live waiter.
2. Retire waiter entries after the future and event list release their ownership.
   With deduplicated entries, a scope-only strong reference identifies an obsolete waiter.
3. Prune completed I/O registrations during scope operation, not only at termination.
   The existing completion-state test provides a starting point.
4. Keep all genuinely pending I/O and active waiters registered.
5. Perform potentially reentrant waiter or waker destruction outside active `TaskState` borrows.

This proposal is not an implemented or measured fix.
Removing the outer scope, clearing its registry indiscriminately, or increasing caps does not preserve the required safety contract.

The correction needs regressions for:

- Long-lived scope retention under repeated NOP and event-wait generations.
- Repeated pending polls of one waiter.
- Active waiter cancellation and wrapped wakers.
- Pending borrowed I/O during scope drop and panic.
- Nested scopes and sibling `join!` or `select!` futures.
- Partial I/O, cancellation, original settlement, and actual transport close.
- The unchanged C8 duplex socket workload and its allocation boundaries.

The registry must stay bounded by live work, not completed history.
Only after that gate passes can final-source wrapper timing resume.

## Replay and evidence

Run one bounded socket case with the retained binary:

```sh
taskset -c 8-31 python3 docs/http2-performance/wrapper/retention/run.py \
  --binary /workspace/kimojio-rs/target/http2-program/build-http2-wrapper-performance/retention-frozen/allocations \
  --backend native --cohorts 32 \
  --output-directory docs/http2-performance/wrapper/retention/replay
```

The runner refuses existing output.
Its `*-run.json` records contain exact commands, hashes, limits, and results.
The runtime-control records also contain both diagnostic environment variables.
Their commands require the same external limits as the socket runner.

Run a runtime-only control with explicit limits and a new output path:

```sh
(
  ulimit -v 524288
  ulimit -t 100
  ulimit -c 0
  export KIMOJIO_RETENTION_CONTROL=wait-scoped
  export KIMOJIO_RETENTION_OUTPUT=docs/http2-performance/wrapper/retention/control-replay.jsonl
  timeout 110s taskset -c 8-31 \
    /workspace/kimojio-rs/target/http2-program/build-http2-wrapper-performance/retention-frozen/controls
)
```

The alternatives are `wait-unscoped`, `nop-scoped`, and `nop-unscoped`.
Large raw trace and symbol files use gzip compression.
Their decompressed SHA256 values are in `evidence-sha256.json`.

Seven existing harness controls passed under default and all features after the instrumentation.
Both required clippy modes passed with `-D warnings`.
Formatting and the exact ledger/site reconciliation also passed.
The original payload, retirement, and overlap assertions remain unchanged.
