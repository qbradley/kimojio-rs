# Hottest stacks

These profiles use baseline `eaac15fa`, not the earlier broken-core binaries.
They use `cpu-clock:u`, 999Hz, and DWARF stacks on CPU2.
Profile times remain separate from noninstrumented timings.

| Baseline profile | Workload | Samples | Client | Server | Shared or incomplete ancestry |
|---|---|---:|---:|---:|---:|
| direct-empty | 2,000,000 exchanges, C1 | 5744 | 389 | 423 | 4932 |
| selected-empty | 2,000,000 exchanges, C1 | 5947 | 400 | 637 | 4910 |
| direct-empty128 | 16,000 cohorts, C128 | 6386 | 492 | 432 | 5462 |
| direct-1m | 32,768 exchanges, C1 | 5635 | 303 | 449 | 4883 |

The classifier uses concrete client/server ancestors and typed protocol endpoints.
Generic pair frames that name both endpoints do not establish one endpoint as the owner.
Missing ancestry remains shared.
The bucket totals are not estimates of the complete CPU cost of either endpoint.
These profiles cover completion on `c08b7333`, not the later response-overlap scheduling correction `18a8f392`.
The harness did not assert response DATA before every upload completed.

The hottest direct-empty client stack is `H2Client::accept_driver_bytes_ref`, with 16 samples.
The hottest server stack has 28 samples in an unresolved libc leaf under `Result::branch` and `H2Server::accept_driver_event_bytes_ref`.
The latter stack matches the 144-byte staging memcpy.
The hottest shared stack is `issue_write → next → Pair::tick`, with 123 samples.

The hottest resolved direct-empty leaves include `semantic_value_byte` (177), `Connection::select` (142), and hash-table `_mm_movemask_epi8` (133).
The unknown leaf total is 373.
The selected profile has a 78-sample server stack through `http::Server::next` into `issue_write`.
This is child write scheduling, not evidence that a selector branch alone consumes those samples.

For 1MiB duplex, the hottest client stack has 55 samples in `H2Client::accept_driver_bytes_ref`.
The hottest server stack has 50 samples under the progress-extraction branch.
The hottest shared stack has 1442 samples in the harness's complete-slice body comparison.
The large-body profile also contains 384 and 360 near-site samples at transport memcpy calls.
Neither the body comparison nor the transport copy is exclusively an endpoint cost.

# How copies were found (MIR vs objdump vs perf)

The source and binary froze before these profiles.
`nm -C -S` supplies symbol ranges from that binary.
`objdump -d -C` supplies memcpy lengths and vector instructions.
`perf script -F +symoff` supplies symbol-relative positions.
The reports do not subtract runtime addresses from file addresses.

The near-site count uses a distance of at most `0x18` bytes.
The parent count includes visible physical frames for that symbol.
An inlined leaf can have a different count.
Nearby samples identify a relevant site, not an exact duration for its instruction.

The inventory uses machine instructions rather than a search for `memcpy` in MIR.
LLVM can add a memcpy or vector move after MIR.
The new analysis never reuses the historical binary's inline-move address.
The current inline `WriteOp` move received its own assembly inspection.

The original raw data and binaries remain in:

```text
target/http2-program/build-http2-performance-credit/frozen-eaac15f/
target/http2-program/build-http2-performance-forwarding/frozen-525ab23/
```

`evidence/profile-hashes.txt` binds the profiles, binary files, symbol tables, and disassembly.
`baseline-freeze.txt` and `poc-freeze.txt` bind the source and build commands.
Full stack summaries preserve the client/server/shared classification.
Partial DWARF ancestry and unresolved libc leaves remain explicit limitations.

# Copy inventory

All baseline addresses below belong to binary SHA256 `69daef245f511790cca00741884b8cb52db58d5bf2b8a26491dce5b780e90977`.
E means direct-empty, S means selected-empty, C means direct-empty128, and L means direct-1m.
Counts are near-site samples / parent samples.

| Baseline site | Size | E | S | C | L | Kind and lifetime |
|---|---:|---:|---:|---:|---:|---|
| `H2Server::accept_driver_event_bytes_ref+0x69c`, file `0xa303c`, `src/server/h2/server.rs:941` | 144 | hit 28/113 | hit 24/118 | hit 34/145 | hit 52/184 | Move during progress extraction. Intermediate staging does not require ownership beyond this call. |
| Direct `Pair::tick+0x1a62`, `examples/composition_bench_support/mod.rs` | Live register, bounded by fragment/page/write | hit 9/707 | other route | hit 6/705 | hit 384/1592 | Explicit transport copy. The read page must own these bytes. |
| Direct `Pair::tick+0x17dc`, same source | Live register, bounded by fragment/page/write | hit 7/707 | other route | hit 11/705 | hit 360/1592 | Explicit transport copy in the other direction. Necessary under this transport contract. |
| `HashMap<StreamId, Stream>::insert+0x1ae` | 316 | hit 38/76 | hit 39/93 | hit 34/105 | hit 2/2 | Move into the map bucket. Final stream storage must own its state. |
| `Connection::maybe_retire+0x148` | 304 | hit 20/111 | hit 25/133 | hit 64/163 | hit 1/42 | Move of stream state after its retirement join. |
| `HashMap<StreamId, Stream>::insert+0x147` | 304 | hit 32/76 | hit 20/93 | hit 44/105 | cold 0/2 | Staging move at map insertion. |
| Client `validate_decoded_header_fields_by+0x1c2` | 208 | hit 27/100 | hit 16/85 | hit 27/130 | hit 1/4 | Move into consuming validator finalization. The local validator remains live. |
| Direct `Pair::tick+0x15a4`, file `0x65704` | 88-byte `WriteOp` value | hit 5/707 | other route | not counted here | hit 8/1592 | Inline `movups` move from pending executor storage into the original completion path. |

The full copy inventory and assembly snippets remain in `evidence/`.
The 144-byte staging site remains the first narrowly scoped removable protocol candidate across these workloads.
The necessary transport copies have more large-body samples.
Map-state moves also matter, but a stable-slot rewrite is outside this experiment.
No positive sample evidence ranks zero-init first.

# Proposals in order

## 1. One isolated private forwarding experiment

Candidate commit: `525ab23d0ac2e89bc6af24c40bbbe48c65a8c5eb`.
Its worktree is `target/http2-program/worktrees/http2-performance-forwarding-poc`.
Its only source change is the private forwarding block in `src/server/h2/server.rs`.
The exact patch is `evidence/poc.diff`.

The candidate replaces `?` extraction and progress reconstruction with a mutable borrow of the returned `Result`.
It keeps the existing output append, consumed count, event, error fields, and output ordering.
It does not alter `flow_control_len`.
Input references keep their original lifetime.
Decoded header fields do not escape their original callback lifetime.
No public owned operation becomes a borrowed operation.

The candidate passes the default and all-feature core suites, all seven harness tests, and both clippy modes.
The paired runs retain identical exchange, payload, and wire-byte counts in all 54 pairs.
These results support behavioral equivalence for the exercised paths.
They do not establish a performance improvement.

### Machine-code result: not accepted

Candidate SHA256: `f5bbd36a0236758d2796919fe5962c62437803c7fb0513163bfb2ee1ce42f27c`.
The 144-byte staging call disappears.
The compiler instead emits a **232-byte aggregate return memcpy** at `accept_driver_event_bytes_ref+0x6d1`, file `0xa3071`.
`addr2line` places it at the final `result` expression, candidate `server.rs:947`.
The instructions load `0xe8` into `edx`.

| Candidate site | Empty samples / parent | Large-body samples / parent |
|---|---:|---:|
| `+0x6d1`, 232-byte normal return copy | hit 74/152 | hit 55/131 |
| `+0x60b`, 232-byte error return copy | cold 0/152 | cold 0/131 |
| `+0x65d`, live-length output append | cold 0/152 | hit 1/131 |

The candidate profiles contain 6295 empty-case samples and 4935 large-body samples.
Their client/server/shared buckets are 406/464/5425 and 267/234/4434.
Sample totals differ, so raw sample-count differences are not elapsed-time estimates.
The assembly and site hits still show that this attempt replaced the target copy with another hot copy.

The paired timing medians range from -0.21% to -1.45%.
Every nine-pair bootstrap interval includes a ratio of 1.0.
The experiment therefore provides no supported elapsed-time improvement.
The candidate remains separate and **must not enter the baseline as an accepted optimization**.

## 2. Stop this experiment

There is no second forwarding attempt in this work.
A different private return strategy needs separate authorization and another frozen profile.
The stable-slot and header-finalization proposals remain deferred.

# What not to cut first

- The larger PoC return copy is not an improvement because its source block is shorter.
- Transport copies are necessary for the current read-page contract.
- The exact body comparison must not become a sampled-byte comparison to improve a benchmark.
- Cold error-return copies do not outrank a hot normal-return copy.
- A stable-slot rewrite cannot bypass send, receive, cancellation, and retirement lifetimes.
- Header slice callbacks must remain borrowed.
- Unknown libc leaves do not prove a zero-init or clone cost.
- Historical addresses do not support claims about this integration or later API changes.

# Replay appendix

After a new CPU2 grant, run the paired script:

```sh
taskset -c 8-31 python3 kimojio-fsm-http2/examples/composition_bench_support/paired_trials.py \
  /workspace/kimojio-rs/target/http2-program/build-http2-performance-credit/frozen-eaac15f/composition_bench \
  /workspace/kimojio-rs/target/http2-program/build-http2-performance-forwarding/frozen-525ab23/composition_bench \
  docs/http2-performance/credit-coalescing/evidence/summary.json \
  NEW_EVIDENCE_DIRECTORY
```

The script uses nine alternating pairs for six cases.
Each case uses identical batch counts in both binaries.
The target duration is approximately 400ms per measured run, based on the baseline median.
The script stops on any failed strict workload.

The baseline profiles use these arguments:

| Profile | Mode | Case | C | Fragment | Batches |
|---|---|---|---:|---:|---:|
| direct-empty | direct | empty | 1 | 65536 | 2000000 |
| selected-empty | selected | empty | 1 | 65536 | 2000000 |
| direct-empty128 | direct | empty | 128 | 65536 | 16000 |
| direct-1m | direct | 1m | 1 | 65536 | 32768 |

The candidate profiles repeat the direct-empty and direct-1m rows.
The record command uses `taskset -c 2 perf record -e cpu-clock:u -F 999 --call-graph dwarf`.
All commands select the corresponding frozen binary and a separate `.data` output file.
