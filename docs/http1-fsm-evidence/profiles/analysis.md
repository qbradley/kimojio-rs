# Hottest stacks

These are four separate profiles, not a timing comparison.
The [publication](publication.json) identifies each frozen binary and its exact build command.
The [sample ledger](analysis.json) records attribution, sites, and coverage limits.

| Profile | Samples | Lost | Client | Server | Shared or unknown owner |
| --- | ---: | ---: | ---: | ---: | ---: |
| Static HTTP server | 204 | 0 | 0 | 99 | 105 |
| HTTP wrapper server | 554 | 0 | 0 | 21 | 533 |
| WebSocket server | 184 | 0 | 0 | 131 | 53 |
| HTTP wrapper client | 953 | 0 | 505 | 0 | 448 |

Attribution uses recovered client/server ancestors, including DWARF inline ancestors.
The shared category includes samples without a recoverable owner.
It does not mean that both sides performed every operation in that category.

The hottest client stack is the benchmark's payload comparison.
`FutureExt::poll_unpin` has 490 self samples.
Its inline ancestry includes `bench_client::validate_chunk`.
The scalar ASCII `x` loop accounts for 483 samples, or 50.7% of the client recording.
This loop is not transport execution or a copy.
Client comparisons include this workload cost.

The WebSocket `Server::next` function has 70 self samples.
Its scalar masking loop accounts for 40 samples.
The hottest individual server stack has 34 samples at `+0xee3` through `parse_payload`.
Masking is required protocol work, not a redundant memcpy.

The static HTTP `Core::next` function has 43 self samples.
Its header-terminator comparison at `+0x18c0` accounts for 12 samples.
The hottest recovered server stack through `Service::drive_http` has 16 samples.

Shared helpers dominate the wrapper server's recovered leaves.
`WaitAsyncEventFuture::poll` has 50 self samples, and `MaybeDone::poll` has 48.
HTTP `Core::next` has 43, and `malloc` has 40.
The hottest explicitly server-owned stacks initialize fixture payloads, with 10 allocation samples and nine initialization samples.

## Coverage

Each recording uses `cpu-clock:u`, 499 Hz, and a nominal three-second interval.
DWARF stack capture stops at 16,384 bytes.
Kernel work and off-CPU waits are outside this coverage.
No registers were recorded, so dynamic memcpy lengths cannot be recovered per sample.

| Profile | Raw stacks with `[unknown]` | Raw single-frame stacks |
| --- | ---: | ---: |
| Static | 99 | 66 |
| Wrapper | 177 | 250 |
| WebSocket | 122 | 54 |
| Client | 195 | 228 |

Some libc leaves are recoverable through callers and disassembly.
Other ancestor chains are incomplete or implausible.
Nearby-call counts are screening evidence, not automatic proof of copy cost.
Zero samples means cold in this recording, not unreachable or free.
These profiles support investigation targets, not predicted speedups.

# How copies were found

The analyzed binary hashes and build IDs match the recorded profiles.
The analysis used `perf report`, `perf script`, `nm -C -S`, `objdump`, and `addr2line -i`.
Immutable source reads supplied the corresponding source locations.
No new compilation, profile, or workload formed part of this analysis.
Analysis commands used CPU 8 while the separate timing work proceeded.

Site attribution uses `symbol+offset`, not runtime addresses minus ELF addresses.
The neighborhood is 24 bytes on either side of each site.
Overlapping neighborhoods do not produce additive counts.
No MIR was generated.
The absence of a memcpy string in MIR does not establish the absence of a copy.

The fixture initialization illustrates the need for instruction-level attribution.
Its memset neighborhood contains 19 samples.
Ten belong to the preceding allocation call.
Only nine support the initialization site.

# Copy inventory

ELF start addresses distinguish generic specializations with similar names.
The publication maps each profile name to an absolute binary path and hash.
Source locations refer to the frozen source revisions.

| Profile and site | Size | Hit evidence | Kind | Necessary |
| --- | ---: | --- | --- | --- |
| Client `PollFn::poll`, start `0x7ed80`, site `+0xb9` | Dynamic, at most 16,384 B | 19 libc samples at caller `+0xbe`, parent 2 self / 21 inclusive | Explicit memcpy from native receive staging | Current staging design requires it. An empty staging buffer can permit a direct path. |
| Wrapper `Unfold::poll_next`, start `0x6bf70`, site `+0x82` | 16,384 B in this fixture | 9 initialization samples, parent 4 self / 23 inclusive | Explicit `memset(..., 0x78, len)`, not zero-init | The current fixture requires initialized payloads. |
| Wrapper `MaybeDone::poll`, start `0x6eed0`, site `+0x9f2` | 264 B | 7 libc samples at caller `+0x9f7`, parent 48 self / 104 inclusive | Stack-to-stack input-state move | Ownership transfer is necessary. The intermediate location is not intrinsically necessary. |
| WebSocket `Server::next`, start `0x43630`, site `+0x890` | 80 B | 6 self samples at `+0x893`, parent 70 self / 71 inclusive | Iterator-state move during deadline selection | No external lifetime requires this staging value. |
| WebSocket `TaskState::schedule_new`, start `0x3b1a0`, site `+0x2d` | 928 B | 2 caller samples | Task-future move | The task needs stable ownership. |
| WebSocket driver `run`, start `0x33100`, site `+0x25d1` | 280 B | 2 nearby samples, one libc and one self | Completion-mailbox event move | Ownership transfer is necessary. |
| Static `Ring::submit`, start `0x40730`, site `+0x8f` | 392 B | 4 nearby samples, including 3 allocation samples | Move into stable boxed storage | Kernel-visible storage must remain stable through the original completion. |
| Wrapper `Pin::poll`, start `0x4efd0`, site `+0xdc5` | 16,400 B | Cold: 0 nearby samples | Move into pinned read state | Stable ownership is necessary. |
| WebSocket `Chat::next`, start `0x51eb0`, site `+0xf84` | 1,616 B | Cold: 0 nearby samples | Upgrade-state move | The next protocol needs owned state. |

The client memcpy also has a matching libc instruction.
`libc.so.6+0x169b09` is `rep movsb` in the recorded libc build.
No hot heap clone was established.

# Proposals in order

This order uses site samples, not predicted benefit across different workloads.
These are proposals, not implemented optimizations.

1. **Bypass empty client receive staging.** The explicit memcpy has 19 supported samples.
   At client revision `295a956cb34f1717bc9768824570cce904786671`, the path is `kimojio/src/async_stream.rs:151-175`.
   A direct read can use caller storage when `read_available == 0`.
   Buffered leftovers still need the existing path.
   The retained `ReadOp` in `kimojio-http1/src/io.rs:77-95` supplies the necessary lifetime.
   Required coverage includes short reads, small destinations, EOF, deadlines, cancellation with late success, and reuse.
2. **Remove an intermediate wrapper input-state move.** The 264-byte move has seven supported samples.
   At server revision `284812f1f9b59256907524a8b3ef5529e56379d2`, the source is `kimojio-http1/src/driver.rs:1001`.
   Ready input can potentially occupy one stable internal location.
   Completion ownership must still transfer exactly once.
   Required coverage includes command backpressure, revoked admission, cancellation, body-buffer return, and terminal settlement.
3. **Avoid WebSocket deadline iterator staging.** The 80-byte move has six supported samples.
   At the same server revision, the source is `kimojio-fsm-websocket/src/server.rs:586-595`.
   Direct minimum selection can avoid the local iterator value.
   No callback or pending operation needs that iterator.
   Required coverage includes enabled and disabled deadlines, minimum selection, stale expiry, timeout transitions, and close behavior.

# What not to cut first

The client payload comparison must remain byte-exact.
Removing it would change the benchmark contract.
A different equivalent implementation needs a new benchmark publication before any comparison.

The WebSocket masking loop is not a memcpy.
Its cost belongs in a separate masking investigation.
Fixture payload initialization also cannot disappear from only one comparison target.

Large read-state and upgrade-state moves have no nearby samples here.
Their size alone does not justify priority.
The static executor's stable operation storage protects kernel-accessible addresses.
Any alternative must retain that lifetime.

Allocation conclusions come from the separate allocator probes.
No profile result establishes an allocation count or whole-process memory limit.

## Replay commands

The recorded commands in `publication.json` recreate the workloads and captures.
The following commands inspect an existing profile and its exact binary:

```sh
perf report --stdio --no-children -i PROFILE/perf.data
perf script -i PROFILE/perf.data -F +symoff --inline
perf script -i PROFILE/perf.data -F +symoff --no-inline --no-demangle
nm -C -S EXACT_BINARY
objdump -d --start-address=SYMBOL_START --stop-address=SYMBOL_END EXACT_BINARY
addr2line -f -C -i -e EXACT_BINARY ELF_ADDRESS
```
