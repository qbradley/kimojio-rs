# Hottest stacks

## Frozen profiles and scope

All profiles use the source and primary binary in [README.md](README.md).
The normal `runtime` binary uses the default allocator and release debug level 2.
`profiles.json` records the exact commands, workload outcomes, data paths, and hashes.
The event was `cpu-clock:user` at 999 Hz with a 32768-byte DWARF stack capture.

The four normal captures contain 3977 native-empty, 4607 generic-empty,
3501 native-duplex, and 4356 generic-duplex samples.
Each uses C8, static 16 KiB producers, warmup, full payload comparisons, retirement, and close.
No workload stalled or failed.

Normal DWARF ancestry was poor: roughly 1.9 physical frames per sample.
A 65528-byte native retry did not repair it.
This host's perf has libdw unwinding but not libunwind.
About 92–95% of normal samples lack a unique endpoint ancestor.
Normal-binary leaf and immediate call-site evidence remains usable.
It does **not** support a precise endpoint CPU split.

A separate diagnostic build uses `RUSTFLAGS="-C force-frame-pointers=yes"` for all dependencies:

```text
path:   /workspace/kimojio-rs/target/http2-program/build-http2-wrapper-performance/measured-6e003746/runtime-frame-pointer
SHA256: 2290ec695935e26634997a979cb2cf77bcdb210364b65d25360bf6c9a03f32e2
source: 6e00374698d40dedebd33b87d3fd183ebb0e2682
```

This is **supplementary attribution**, not the primary timing binary.
`frame-pointer-freeze.json` records its separate target, build command, and flags.
Five paired perturbation trials gave FP/default medians of approximately 1.020–1.035.
Individual ratios ranged from 0.781 to 1.492, so these trials do not establish negligible perturbation.
The primary timing matrix does not mix the two builds.
Profile workload durations are not timing results: stack capture and host variability can perturb execution.

Native FP captures use 32768-byte stacks.
Generic `IoScopeFuture<run_stream>::poll` reserves roughly 46 KiB of stack in this build.
Its prologue contains `sub r11,0xb000`, page probes, then `sub rsp,0x5b8`.
Generic ancestry therefore needed 65528-byte captures.
A generic-duplex 999 Hz capture reported one lost chunk.
A larger mmap-ring attempt failed permission checks and produced no valid capture.
The final generic-duplex capture uses **499 Hz**, with no reported losses.
All attempts and stderr remain preserved; only the selected captures below support the endpoint table.

| Selected FP profile | Samples | Client | Server | Shared or unattributed |
|---|---:|---:|---:|---:|
| native-empty-fp | 4082 | 1778 | 1734 | 570 |
| native-duplex1m-fp | 3531 | 1585 | 1432 | 514 |
| generic-empty-fp64 | 5343 | 2417 | 2272 | 654 |
| generic-duplex1m-fp64-f499 | 2615 | 1142 | 1138 | 335 |

Attribution uses ancestor symbols, not the leaf's crate name.
Identical-function aliases that name both endpoints are ambiguous.
A join type naming both children is not itself an endpoint.
Detached producers and other tasks without an endpoint ancestor remain shared/unattributed.
Shared helpers called from an identifiable endpoint inherit that endpoint for this table.

### Hottest resolved stack paths

Counts below group complete resolved physical symbol paths, not all samples with a common leaf.
Names are shortened for readability; full paths are in `profile-analysis.json.gz`.

| Profile | Bucket | Hottest resolved path, leaf first | Samples |
|---|---|---|---:|
| native empty | client | `next_input::poll → IoScopeFuture<run_native>::poll → NativeConnection::run → drive` | 97 |
| native empty | server | `next_input::poll → IoScopeFuture<run_native>::poll → serve_native → drive` | 82 |
| native empty | shared | `submit_and_complete_io → submit_and_complete_io_all → Runtime::block_on` | 32 |
| generic empty | client | `next_input::poll → IoScopeFuture<run_stream>::poll → Connection::run → drive` | 111 |
| generic empty | server | `IoScopeFuture<run_stream>::poll → serve_connection → drive` | 99 |
| generic empty | shared | `poll_task → Runtime::block_on → run_with_configuration` | 28 |
| native duplex | client | libc comparison `→ support::compare → TryJoinAll<exchange>::poll → cohort → drive` | 123 |
| native duplex | server | libc comparison `→ support::compare → receive → handler drain task → poll_task` | 112 |
| native duplex | shared | `submit_and_complete_io → submit_and_complete_io_all → Runtime::block_on` | 44 |
| generic duplex | client | libc comparison `→ support::compare → TryJoinAll<exchange>::poll → cohort → drive` | 69 |
| generic duplex | server | libc comparison `→ support::compare → receive → handler drain task → poll_task` | 75 |
| generic duplex | shared | `producer::poll → Select::poll → State::upload task → poll_task` | 29 |

The comparison leaves are often unresolved libc symbols.
Their actual `bcmp` return sites establish the operation; they are not assumed to be memcpy.
The generic buffered-read memcpy paths separately have 60 client and 50 server samples in the selected FP duplex capture.

### Shared runtime costs

These dedicated helper self counts avoid attributing an entire inlined driver loop to the scope wrapper.
Percentages are of each selected profile's **user-CPU samples**, not total process time.

| Area | Native empty | Generic empty | Native duplex | Generic duplex |
|---|---:|---:|---:|---:|
| WaitData pointer hashing, self | 136 (3.33%) | 123 (2.30%) | 188 (5.32%) | 87 (3.33%) |
| `register_wait`, self | 58 (1.42%) | 88 (1.65%) | 83 (2.35%) | 52 (1.99%) |
| `WaitData::retire_from_scopes`, self | 83 (2.03%) | 100 (1.87%) | 110 (3.12%) | 62 (2.37%) |
| `WaitAsyncEventFuture::poll`, inclusive | 402 (9.85%) | 472 (8.83%) | 572 (16.20%) | 300 (11.47%) |

The inclusive row overlaps the others and must not be added to them.
The common waiter paths are hotter than either individual empty-case routing stack.
Native duplex hashing alone has more self samples than the native driver-scope leaf (143)
or the core `Connection::next` leaf (141).
Thus scope membership and hashing are real costs, not conclusions inferred from allocation counts.
These paths also include pre-existing waiter logic; they are not all costs introduced by the runtime repair.

Current reverse-membership vector growth has 35/3531 native and 27/2615 generic duplex inclusive samples.
The vector deallocation site has 37/3531 and 15/2615 exact return-site samples.
This is about 2.0% and 1.6% of user samples across those two non-overlapping phases.
It is much smaller than the complete waiter path.
Avoiding a vector allocation would not remove all registry hashing or waiter work.

Whole-process `getrusage` controls used the normal binary for longer runs.
Observed user fractions were 75.4% native-empty, 79.9% generic-empty,
63.8% native-duplex, and 67.6% generic-duplex.
These four controls include startup, warmup, close, and output.
They are not primary-window distributions.
They show why a percentage of user samples cannot be presented as the same percentage of wall-time savings.
Kernel/system CPU is not covered by `cpu-clock:user`.

# How copies were found (MIR vs objdump vs perf)

The order was binary freeze, profile, stack attribution, disassembly, then source inspection.
No production implementation hunt preceded the profile.
MIR string searches were not used as evidence that copies were absent.

`profile_analysis.py` obtains exact addresses and sizes from `nm -n -S`.
It resolves Rust v0 names with `c++filt -s rust`.
It reads Intel-syntax `objdump -d` and GOT relocations, including relative allocator relocations.
The private profile directory retains the full symbol and disassembly outputs.
`hot-site-source.txt.gz` and `inline-sites.txt` retain matching `addr2line` and instruction evidence.

Samples use `symbol+offset` to map back to file addresses.
No runtime ASLR address is subtracted from a file address.
The inventory initially used a ±24-byte neighborhood to find candidate calls.
That alone is insufficient when call sites are adjacent.

In particular, the cold allocation after a failed payload comparison was within 24 bytes of the hot `bcmp` return.
A cold runtime-construction memcpy was also near another call's sampled return.
Both would have received false inclusive hits.
Final attribution requires the actual next-instruction return PC, or its one-byte unwinder adjustment.
Nearby leaf/self hits remain a separate field, not extra proven callee time.
Deterministic controls make sure the two adjacent cold calls receive no hot attribution.
`profile-analysis-initial.json.gz` is an obsolete intermediate, **not performance evidence**.

Even exact return-site counts are sampled observations, not call counts or cycle-perfect attribution.
They can include a sample immediately after return.
No unresolved libc leaf is classified as malloc/free/copy without caller evidence.

# Copy inventory

Unless marked FP, addresses and offsets below refer to the normal binary.
`E/S/P` means exact return-site hits / nearby leaf hits within ±24 bytes / parent-function stack samples.
E and S can overlap and must not be added.
Normal profiles have weak ancestry, so P is conservative.

| Site and matching source | Size | Hit evidence | Kind | Necessary? |
|---|---|---|---|---|
| Generic `PollFn<stream::settle<try_read_impl>>::poll+0xc3`, `0x9ec73`; `kimojio/src/async_stream.rs:174` | variable `rdx=r14`, bounded by the 16 KiB internal buffer | generic duplex **190/1/210**, 4.36% exact | explicit `copy_from_slice` | Yes for already-buffered bytes. Potentially avoidable when the internal buffer is empty and the destination can receive directly. |
| Generic `Connection<Data>::next<Ports<StreamIo>>+0x1877`, `0xb86d7`; `kimojio-fsm-http2/src/engine.rs:2109` | variable `rdx=r15`, partial frame bytes | generic duplex **123/0/364**, 2.82% exact | explicit frame assembly copy | Necessary when a frame spans input pages under the current contiguous-frame contract. Reduce fragmentation rather than discard ownership. |
| Native `IoScopeFuture<run_native>::poll+0xc25`, `0xfd985`; `driver.rs:1586,618` | `edx=0xd0`, **208 B** | native duplex **39/1/512**; empty **25/3/432** | move into suspended future state | The operation state must survive suspension. A shorter ownership path might reduce staging, but cannot borrow a stack local across await. |
| Native `PollFn<next_input>::poll+0x78b`, `0x9bdfb`; `driver.rs:1630` | `edx=0x8f`, **143 B** | native duplex **35/1/273**; empty **31/3/264** | move of selected input | Selected input owns receipts and queued data. Any replacement must preserve exactly-once settlement. |
| Generic `PollFn<next_input>::poll+0x8bb`, `0x9c6db`; `driver.rs:1630` | **143 B** | generic duplex **41/4/244** | move | Same ownership constraint. Not the old server forwarding PoC. |
| Native `NativeIo::poll_write+0x663`, `0xdc8c3` | `edx=0xa0`, **160 B** | native duplex **28/3/68** | move of I/O completion | Original buffer and receipt ownership must survive until the core accepts completion. |
| Native `IoScopeFuture<run_native>::poll+0xe54`, `0xfdbb4` | **160 B** | native duplex **19/8/512** | move into future state | Same suspension and settlement constraints. |
| Generic `IoScopeFuture<run_stream>::poll+0xe0d`, `0xfb9dd` | **208 B** | generic duplex **29/0/797** | move | Not a payload-sized copy. |
| Generic `IoScopeFuture<run_stream>::poll+0x1036`, `0xfbc06` | **160 B** | generic duplex **22/4/797** | move | Same receipt lifetime constraint. |
| BTree node leaf removal `+0xa5`, `0x89b15` | `rdx=count*0x348`; **840 B per Stream** | native empty **20/0/54** | memmove of map values | Required by the current packed BTree node representation. Size alone does not justify a storage rewrite. |
| Handler future boxing `serve_connection_with_shutdown…+0x85`, `0x98455` | `edx=0xc18`, **3096 B** | native empty **4/0/41** | move into heap-owned future | The spawned task outlives the caller. A large but low-hit site; aliases can display a generic symbol in the native path. |
| `metadata::field+0x50`, `0x130cb0`; `metadata.rs:111` | variable field length | native empty **4/2/58** | heap clone | Queued/owned metadata must remain valid after caller temporaries die. Static-source specialization is a separate lifetime design. |
| `support::compare+0xcf`, FP `0xd4def` | comparison length up to 16 KiB | native duplex **235/0**; generic duplex **144/0** | **bcmp, not a copy** | Full payload assertions are required; do not remove them to claim a protocol win. |

The first two generic sites total 313/4356 normal user samples.
The FP diagnostic independently shows 110+81 = 191/2615 at the corresponding sites.
Do not add counts across binaries or use the FP addresses with the normal binary.

Source inspection explains the pair:

1. `OwnedFdStreamRead::try_read_impl` reads into its 16 KiB internal buffer.
2. It copies those bytes into the caller's buffer.
3. A full 16 KiB HTTP/2 DATA payload needs another nine framing bytes.
4. Such a frame cannot fit in one 16 KiB buffered read.
5. `engine::process_input` then copies incomplete input into an assembly page.

The second hot site is **receive frame assembly**, not writer staging.
The exact matching source at `engine.rs:2109` establishes this distinction.

## Inline moves and zero initialization

The search also included `rep movs`, `movaps`, `movups`, `movdqa`, and `movdqu`.
Real sampled inline copies exist even without memcpy calls:

- Normal native `0xfd77e`, in `run_io` at `driver.rs:582`: 32 duplex self samples on a 16-byte load.
  The surrounding sequence moves four 16-byte pieces plus one eight-byte piece.
- Normal generic `0xfb7d6`, the same source path: 33 duplex self samples.
- Native `0xfdc48` / generic `0xfbc99`, `driver.rs:619`: 19 / 28 duplex self samples on a staging load.
- `0xdd4ad`, `RingFuture::with_polled` at `ring_future.rs:99`: 18 native duplex samples on a 16-byte store.

These are instruction hits, not proof that every sample represents a complete value copy.
The full surrounding instructions are in `inline-sites.txt`.
The driver loop owns completion and input state across polling; a source rewrite needs a lifetime argument.

No dominant sampled zero-init site was established.
Some `pxor` instructions in hot hashing code combine data and are not zero initialization.
A sampled `mov [rsp],0` in generic empty is a stack probe, not an application array clear.
Stack probing is a safety mechanism, not a proposed copy cut.

# Proposals in order

## 1. Generic empty-buffer direct read, subject to the stream contract

**Evidence:** 190 normal samples at the buffered-read copy and 123 at frame assembly.
This is the strongest explicit copy target in the measured generic large-body path.
The FP profile supports the same result.

**Candidate:** when `read_available == 0` and the caller buffer is sufficiently large,
read into that destination rather than into the internal 16 KiB buffer.
Keep the existing path for buffered tails and small destinations.
A larger read can also reduce the mandatory frame assembly copies.
It does not guarantee that the kernel returns a complete frame.

**Lifetime argument to prove:** the caller's destination is mutably borrowed until the original read settles.
The wrapper retains its owned `ReadOp` for that interval.
No buffer may be recycled because a cancel request or ACK alone arrived.
EOF, partial reads, deadline races, cancellation, and error reporting must retain their current meanings.
The general `AsyncStream` contract also applies outside HTTP/2; wrapper success alone is not enough.
No reference into the internal buffer may escape or survive a later refill.

**Required tests:** small destination with buffered remainder; empty and nonempty internal buffer;
partial/EOF reads; deadline and cancellation races; exact original/ACK ownership;
all native/generic boundary and peer cases; full payload and overlap benchmarks;
the 1 KiB sensitivity; allocation plateaus.
Repeat the same paired normal-binary timings and fresh profiles on a new immutable binary.
Do not predict a 7.2% wall-time win from a 7.2% user-sample target.

## 2. Inline one reverse scope membership with a multi-scope fallback

**Evidence:** `register_wait` pushes a weak membership at `kimojio/src/task/io_scope.rs:90`.
The accepted allocation ledger attributed 341,115 native and 329,414 generic allocations to this vector at duplex cohort 32.
The fresh FP growth/deallocation stacks account for about 2.0% / 1.6% of duplex user samples.
These counts support a smaller target than the entire waiter path.

**Candidate:** avoid a separate vector allocation for the common one-membership case,
while retaining a general fallback for migration and multiple scopes.
Measure membership distribution and the larger `WaitData` allocation-size class before acceptance.
This is not a request to revert the live registry repair.

**Lifetime argument to prove:** memberships remain weak; registries retain strong entries until genuine retirement.
Pointer keys remain protected from reuse by those strong entries.
A canceled retry must get a fresh generation.
Retirement must detach the membership storage before callbacks or final destruction.
No callback may run while a task/registry mutable borrow remains active.
The fallback must retain all scopes when the same future migrates.

**Required tests:** all 22 scope tests in the existing three configurations, nine wait tests,
cross-task migration, deduplication, nested/sibling scopes, wake-versus-cancel,
drop/reentrancy, original CQE/cancel ACK ordering, long plateau controls, wrapper boundary/interop matrix,
and paired CPU/allocation measurements.

Hash lookup itself is also substantial.
A separate design could investigate its representation or hashing, but internal pointer equality,
collision behavior, large live registries, shrink bounds, and amortized costs must remain safe.
Do not infer that removing vector allocations removes the measured hash cost.

## 3. Revisit small state moves only after the first measurements

The 143/160/208-byte moves have actual hits, but each is roughly 0.5–1.1% of normal duplex samples.
A narrower synchronous input-consumption path might reduce staging.
It must preserve readiness rotation, suspension state, original-buffer ownership, and all cancellation joins.
Do not add per-operation boxes to remove a small move without measuring allocation cost.
No stable-slot rewrite or second forwarding-layout PoC is authorized here.

# What not to cut first

- The 3096-byte handler move has only four normal native-empty return-site hits.
  Its size alone is not a priority argument.
- Header clones have required owned lifetimes and few copy-site hits in this workload.
- Frame assembly copies cannot simply disappear when source pages will be recycled.
- The payload comparison is assertion work, not a copy. Keep the oracle intact.
- Error-path boxing and runtime-construction copies were cold under exact return-site attribution.
  Proximity to a neighboring hot call is not sufficient evidence.
- The large generic stack frame is real, but stack size alone is not a measured memcpy cost.
- Do not resurrect rejected `525ab23d`, change caps, or call a protocol stall a copy cost.

## Replay appendix

Representative normal profile command; obtain a new CPU2 lease before replay:

```sh
taskset -c 8-31 perf record -N -e cpu-clock:u -F 999 --call-graph dwarf,32768 \
  -o NEW_CAPTURE.data -- taskset -c 2 \
  /workspace/kimojio-rs/target/http2-program/build-http2-wrapper-performance/measured-6e003746/runtime \
  --backend generic --case duplex --bytes 1048576 --concurrency 8 \
  --phase steady --cohorts 400 --warmup 8 --timeout-seconds 90
```

Use a new evidence path, not an existing capture.
For final generic FP attribution, substitute the FP binary, `-F 499`, and `dwarf,65528`.
`profiles*.json` retains the actual recorded cohort counts and every attempt.
Offline analysis runs on CPUs 8–31:

```sh
taskset -c 8-31 python3 docs/http2-performance/wrapper/measured/profile_analysis.py
```

This requires the retained private perf data and matching binaries.
The committed compressed analysis and source/disassembly excerpts remain readable without perf.
