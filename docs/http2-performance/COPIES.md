# Hottest stacks

All numbers in this report use the final `d4bc869` binary.
Its SHA256 is `205f19afeaa93d7d6161a3f32dd3359098bf2805d2b0bca08c5cd8a7c077c527`.
The release build retains debug information.
Profiles use CPU2, `cpu-clock:u`, 999Hz, and DWARF call stacks.
Profile elapsed times do not enter the noninstrumented timing results.

| Profile | Workload | Samples | Client | Server | Shared or unknown ancestry |
|---|---|---:|---:|---:|---:|
| direct-empty | 2,000,000 exchanges, C1 | 5045 | 403 | 416 | 4226 |
| selected-empty | 2,000,000 exchanges, C1 | 5802 | 448 | 661 | 4693 |
| direct-empty128 | 16,000 cohorts, C128 | 4707 | 393 | 361 | 3953 |
| direct-1m | 8192 duplex exchanges, C1 | 4304 | 343 | 575 | 3386 |

The classifier uses concrete endpoint ancestors, typed protocol endpoints, and client/server protocol methods.
It excludes generic pair symbols that mention both endpoints.
A shared helper without a visible endpoint ancestor remains shared.
These bucket totals are not estimates of each endpoint's complete CPU cost.

The hottest attributed direct-empty client stack is `H2Client::accept_driver_bytes_ref`, with 35 samples.
The hottest server stack contains an unresolved libc leaf, `Result::branch`, and `H2Server::accept_driver_event_bytes_ref`, with 79 samples.
The server stack corresponds to the 144-byte memcpy described below.

The hottest shared direct-empty stack is `Connection::select`, with 96 samples.
The hottest direct-empty resolved leaf is the hash-table helper `_mm_movemask_epi8`, with 153 samples.
Other resolved leaves include `probe_seq` (108), `Connection::select` (104), and `find_inner` (99).
The unknown leaf total is 404.
Hash-table work, header validation, and connection state selection matter more than a selector branch alone.

For the 1MiB case, the hottest client stack has 104 samples in `H2Client::accept_driver_bytes_ref`.
The hottest server stack has 151 samples at the same 144-byte memcpy.
The hottest shared stack has 330 samples in the complete-slice payload comparison.
That comparison is harness work, not protocol work.
Transport copy also contributes to this profile.

DWARF frequently supplied only an inlined chain or a partial physical stack.
The preliminary binary also received a 65528-byte stack-capture experiment.
That experiment did not recover reliable complete ancestry.
The final report leaves such samples shared instead of inventing endpoint ownership.
The original binaries and raw profile data remain available for another unwinder.

# How copies were found (MIR vs objdump vs perf)

The investigation started with public APIs and usage examples.
Core source inspection followed the first recorded profile.
No source-only search for a large array selected the proposals.

`nm -C -S` supplied symbol addresses and sizes from the profiled binary.
`objdump -d -C` exposed memcpy calls and `movups`, `movaps`, `movdqa`, and `movdqu` instructions.
The instructions before each memcpy supplied either a constant length or a live register.
`addr2line` connected the hot instructions to the matching frozen source.

`perf script -F +symoff` supplied symbol-relative offsets.
The inventory counts samples within `0x18` bytes of each copy site.
Each sample contributes at most once to a particular physical symbol and offset.
The parent count includes visible physical frames for that symbol.
A parent count is not the global leaf count.
Nearby samples show a relevant site, not an exact duration for one instruction.

No runtime address subtraction substitutes for symbol-relative offsets.
No MIR absence claim forms part of this report.
LLVM can introduce memcpy and vector moves after MIR.
The inventory instead uses final machine instructions and recorded samples.

`evidence/copy-sites.json` contains 44 inspected sites in sampled parent functions.
`evidence/copy-disassembly.txt` retains instructions around the hit sites.
`evidence/profile-and-source-hashes.txt` binds the binary, profiles, disassembly, and core source.
The scripts retain full symbol names where this report uses short names.

# Copy inventory

The sample columns use **near-site samples / parent samples**.
E means direct-empty, S means selected-empty, C means direct-empty128, and L means direct-1m.
The paths start at `kimojio-fsm-http2/`.

| Site and source | Size | E | S | C | L | Kind | Necessary |
|---|---:|---:|---:|---:|---:|---|---|
| `H2Server::accept_driver_event_bytes_ref+0x69c`, `src/server/h2/server.rs:941`, file `0x9634c` | 144 | hit 79/167 | hit 89/186 | hit 27/139 | hit 151/285 | move through `Result::branch` | Not for an intermediate progress value |
| Direct `Pair::tick+0x146b`, `examples/composition_bench_support/mod.rs`, file `0x6527b` | live `%rbp`, bounded by fragment/page/write | hit 9/499 | not this route | hit 7/441 | hit 132/844 | explicit transport copy | Yes for this in-memory read-page contract |
| `HashMap<StreamId, Stream>::insert+0x147`, insertion at `src/engine.rs:427,1941`, file `0x70117` | 184 | hit 55/98 | hit 40/88 | hit 30/91 | cold 0/0 | move of stream state | Final bucket ownership is necessary |
| Direct `Pair::tick+0x10fa`, `examples/composition_bench_support/mod.rs`, file `0x64f0a` | live `%rbp`, bounded by fragment/page/write | hit 7/499 | not this route | hit 6/441 | hit 102/844 | explicit transport copy | Yes for this contract |
| `Connection::maybe_retire+0x134`, `src/engine.rs:967`, file `0x4eea4` | 184 | hit 33/103 | hit 34/135 | hit 42/122 | cold 0/57 | move of removed stream state | Metadata is necessary, the whole intermediate value is not |
| `HashMap<StreamId, Stream>::insert+0x1ae`, `src/engine.rs:427,1941`, file `0x7017e` | 188 | hit 24/98 | hit 30/88 | hit 47/91 | cold 0/0 | move of key and stream state | Final bucket ownership is necessary |
| Client `validate_decoded_header_fields_by+0x1c2`, `src/server/h2/headers.rs:1440`, file `0x781e2` | 208 | hit 26/92 | hit 32/95 | hit 23/74 | cold 0/0 | move into consuming `finish` | No, the local validator remains live |
| Server `validate_decoded_header_fields_by+0x1c2`, same source, file `0x783f2` | 208 | hit 10/27 | hit 18/36 | hit 42/58 | cold 0/0 | move into consuming `finish` | No, the local validator remains live |
| `Connection::next+0xed3`, inlined `Ports::write`, file `0x4ade3` | 88-byte value, vector instructions | hit 30/551 | hit 29/593 | hit 43/473 | hit 66/640 | move into executor storage | Yes, the executor retains the owned operation |
| `Connection::next+0x167d,+0x198a`, `src/engine.rs` | live register | cold 0/551 | cold 0/593 | cold 0/473 | cold 0/640 | explicit copy on other paths | No measured basis for a cut |

The nearby-sample rule can include work next to the site.
The report does not sum overlapping instruction windows into a total copy percentage.
The full-frame payload copy belongs to the executor, not exclusively to either endpoint.
The 88-byte operation move belongs to the owned callback boundary.
Its inlined location inside `Connection::next` does not make it a hidden payload clone.

No sampled hot site established a large arena move or a hot zero-init.
The page allocator contains zero-init, but these profiles do not justify a zero-init proposal.
The inventory does not claim that every indirect libc call resolved.
Unknown libc leaves remain visible in the profile totals.

# Proposals in order

## 1. Avoid the intermediate server progress move

The strongest removable core site is the 144-byte move at `accept_driver_event_bytes_ref+0x69c`.
It receives 79 direct-empty and 151 large-body near-site samples.
The intermediate result contains an event, counters, output storage, and error metadata.
It is not a 144-byte payload copy.

A private result-layout or forwarding change can avoid the intermediate whole-value move.
The caller can retain the final progress value and update its consumed count directly.
The exact Rust shape needs an assembly experiment.
The proposal does not promise that the compiler will eliminate the move.

The input slice remains live through the immediate callback.
Connection-owned decoded fields remain live until the current header callback ends.
No event reference can survive the next decode, insertion, eviction, or recycle operation.
No public owned operation becomes a borrowed operation.

Required regressions include partial preface, fragmented headers, continuation frames, body receipts, and callback yield.
They also include recoverable stream errors, connection errors, output ordering, and original-operation settlement.

## 2. Reduce stream-state moves without a per-stream allocation penalty

The insertion sites receive 55 and 24 direct-empty near-site samples.
Retirement adds 33 samples at a separate site.
The current map moves 184-byte values and 188-byte key/value pairs.

Construction directly in the final map slot is a candidate experiment.
A compact map index with separate stable stream storage is a broader alternative.
Neither option is a measured improvement yet.
Adding one heap allocation per stream solely to shorten a move is not the default proposal.

The stable stream slot must outlive send receipts, body receipts, and original writes.
`Stream::can_retire` supplies the current lifetime boundary.
The slot cannot return to a free list until every retirement join completes.
Generational indexes must reject stale references after reuse.

Required regressions include stream-ID reuse protection, reset, deadline, cancellation, and delayed body release.
They also include concurrent send completion, failed writes, shutdown, and retained-capacity limits.

## 3. Borrow the local header validator during finalization

The client finalization site receives 26 direct-empty near-site samples.
The server finalization site receives another 10.
Both move 208 bytes into `finish(mut self)`.

A private `finish(&mut self)` or a smaller finalization record can avoid this move.
The validator already remains local until finalization returns.
The result must contain owned metadata or indexes, not a borrow into the local validator.

Required regressions include pseudo-header ordering, content-length, CONNECT, trailers, overflow, and all invalid header cases.
Both byte-at-a-time and slice visitor paths must retain identical behavior.
The public header callback already borrows fields and must remain borrowed.

## 4. Keep executor costs separate

The direct transport copies receive 132 and 102 large-body near-site samples.
Those copies are necessary for this read-page transport.
A different transport contract changes the workload.
Removing the copy is not an equivalent core optimization.

The owned `WriteOp` move also receives samples.
The executor must retain that operation until its original completion.
An in-place executor slot experiment can reduce staging without changing that lifetime.
Any such experiment needs a new binary and a new baseline.

# What not to cut first

- Large cold copies do not outrank the sampled 144-byte progress move.
- Dynamic-table insertion reserve sites have no nearby samples in these profiles.
- Continuation accumulation and overflow paths need separate workloads before a copy proposal.
- The header slice visitor already classifies the live slice without a staging copy.
- A page zero-init is not a measured bottleneck in this evidence.
- A public borrow must not become an owned clone.
- Hash-table probes and header-byte validation are real costs, but they are not copies.
- The scalar comparison from the preliminary harness is not a core regression.
- Failed body cases are correctness blockers, not slow samples.

# Measurement limits and decision

The final empty C128 medians differ by +1.5% for selected and -2.4% for auto versus direct.
Their overlapping ranges do not support a small universal overhead claim.
Some cells show much larger spread despite CPU affinity and alternating order.
CPU affinity does not isolate shared caches, memory bandwidth, or frequency changes.

The 1MiB full-fragment case is slower than its 1024-byte-fragment case at C1.
The transport fragment changes framing and credit progress, not only copy size.
That inversion needs a flow-control investigation before an optimization claim.

The concurrent body failures block the core gate.
The wrapper must not hide them with larger capacities or weaker assertions.
The core owner later attributed the frozen-core failures to premature removal of the client upload half.
The supplied correction is `0d1416641d88578e02fd0ebce9f1c67f459afa66`.
This report does not qualify that correction or attribute the failures to a control budget.
After a core correction, the essential timing cells and profiles need a new source freeze.
Old addresses and samples cannot support claims about the corrected source.

# Replay appendix

The raw files remain under `target/http2-program/build-http2-performance/frozen-d4bc869/`.
That directory contains the binary, `.data` files, demangled stacks, `nm.txt`, and `objdump.txt`.
The repository stores their hashes and compact evidence, not the large raw profiles.
The earlier scalar-comparison binary and profiles remain in `frozen-9d881b7/`.

After CPU2 becomes available, record the direct empty profile:

Replace `FROZEN` with the frozen directory path in each command.

```sh
taskset -c 2 perf record -e cpu-clock:u -F 999 --call-graph dwarf \
  -o FROZEN/direct-empty.data -- \
  FROZEN/composition_bench direct empty 1 65536 2000000 steady
```

Use the same command with these profile arguments:

| File | Mode | Case | Concurrency | Batches |
|---|---|---|---:|---:|
| selected-empty | selected | empty | 1 | 2000000 |
| direct-empty128 | direct | empty | 128 | 16000 |
| direct-1m | direct | 1m | 1 | 8192 |

For analysis, run these commands on the frozen binary:

```sh
taskset -c 8-31 perf script -i FROZEN/direct-empty.data -F +symoff \
  | c++filt -s rust > FROZEN/direct-empty.demangled.txt
taskset -c 8-31 nm -C -S FROZEN/composition_bench > FROZEN/nm.txt
taskset -c 8-31 objdump -d -C FROZEN/composition_bench > FROZEN/objdump.txt
taskset -c 8-31 python3 kimojio-fsm-http2/examples/composition_bench_support/profile_summary.py \
  FROZEN/direct-empty.demangled.txt docs/http2-performance/evidence/direct-empty.profile.json
taskset -c 8-31 python3 kimojio-fsm-http2/examples/composition_bench_support/copy_sites.py \
  FROZEN docs/http2-performance/evidence/copy-sites.json
```
