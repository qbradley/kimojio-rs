# Hottest stacks

All four profiles belong to source `5505efc469c7939c43d6d42a2ecb10b94ab4d9e4`.
The binary SHA256 is `861d584ad7c355cbf89eddbb43b74f487e9ee6431ed44d3d7c8135f5f7c2d692`.
Each profile uses CPU2, `cpu-clock:u`, 999Hz, and DWARF stacks.
Profile runtimes do not enter the noninstrumented timing matrix.

| Profile | Workload | Samples | Client | Server | Shared or incomplete ancestry |
|---|---|---:|---:|---:|---:|
| E: direct-empty | 2000000 exchanges, C1 | 5972 | 353 | 396 | 5223 |
| S: selected-empty | 2000000 exchanges, C1 | 6306 | 404 | 739 | 5163 |
| C: direct-empty128 | 16000 cohorts, C128 | 6134 | 427 | 413 | 5294 |
| L: direct-1m | 32768 exchanges, C1 | 5472 | 294 | 361 | 4817 |

The classifier uses concrete client/server ancestors and typed endpoint frames.
Generic pair frames that name both endpoints do not identify an owner.
Ambiguous or incomplete ancestry remains shared.
The bucket totals are not complete estimates of client and server CPU cost.
Seven symbol groups share an address and size across client and server names.
Those folded helper names do not independently identify an owner.
Other unambiguous ancestors can still identify their caller.
The [alias inventory](evidence/role-aliases.json) records those groups.

For direct-empty, the hottest client stack is `H2Client::accept_driver_bytes_ref`, with 24 samples.
The hottest server stack is `H2Server::accept_driver_event_bytes_ref` with incomplete ancestry, with 35 samples.
The hottest shared stack is `issue_write → next → Pair::tick`, with 127 samples.

For selected-empty, the hottest server stack has 70 samples through `http::Server::next` into child `issue_write`.
That stack includes child scheduling, not only selector overhead.
The hottest shared stack is `Connection::select`, with 145 samples.

For 1MiB duplex, the hottest client stack is `H2Client::accept_frame_bytes_ref_typed`, with 52 samples.
The hottest server stack again reaches progress forwarding, with 42 samples.
The hottest shared stack has **1330 samples** in the harness's complete body-slice comparison.
The large-body profile also has 778 and 430 near-site samples at the two payload transport copies.
Those copies and comparisons are not exclusively client or server protocol work.

Resolved direct-empty leaves include header `semantic_value_byte` with 190 samples, `is_ascii_lowercase` with 175, and `Connection::select` with 150.
The unknown-leaf counts are 342, 370, 455, and 2610 for E, S, C, and L.
A known ancestor can still identify work beneath an unresolved leaf.
The report does not assign all unresolved leaves to a protocol role.

# How copies were found (MIR vs objdump vs perf)

The source and binary froze before recording.
`nm -C -S` and `objdump -d -C` use that exact binary.
`perf script -F +symoff` supplies symbol-relative positions.
The analysis joins each static call offset to sampled positions within approximately `±0x18` bytes.
It does not subtract runtime addresses from static addresses.

Copy sizes come from the current instructions before each call.
The 144-byte site loads `0x90` into `edx`.
The 304-byte retirement site loads `0x130`.
The 316-byte insertion region loads `0x13c`.
Transport sizes come from live registers and the accepted slice length.

The analysis also inspects inline vector moves.
DWARF type information reports an 88-byte `WriteOp<&[u8]>`.
Its current `movups` region maps to `Option::take` in the transport executor.
No previous inline-move address was reused.
No conclusion depends on the presence or absence of `memcpy` text in MIR.

# Copy inventory

Counts are near-site samples / physical parent samples.
A zero numerator means **cold in that profile**, not unreachable.
The copy lengths do not necessarily equal complete Rust type sizes.
ELF addresses and offsets below belong only to this frozen binary.

| Site and ELF address | Size | E | S | C | L | Kind and necessity |
|---|---:|---:|---:|---:|---:|---|
| Direct `Pair::tick+0x1ad2`, `0x66302` | Dynamic | hit 3/720 | other route | hit 10/686 | hit 778/1893 | Explicit payload copy into the peer's owned read page. Necessary for this transport contract. |
| Direct `Pair::tick+0x184c`, `0x6607c` | Dynamic | hit 7/720 | other route | hit 4/686 | hit 430/1893 | Same contract in the other direction. |
| `H2Server::accept_driver_event_bytes_ref+0x69c`, `0xa384c` | 144 | hit 18/119 | hit 28/145 | hit 36/138 | hit 43/147 | Progress staging move through `Result::branch`, not a DATA payload clone. |
| `Connection::maybe_retire+0x148`, `0x595b8` | 304 | hit 50/156 | hit 37/149 | hit 26/158 | hit 1/26 | Stream-state extraction after the retirement join. Ownership and settlement must remain intact. |
| `HashMap<StreamId, Stream>::insert+0x1ae`, `0x8214e` | 316 | hit 31/81 | hit 35/82 | hit 36/94 | hit 1/2 | Map-entry move. The destination must own the live stream state. |
| Header `decode_connection_header_fields+0x11e`, `0xa6f3e` | 208 | hit 18/124 | hit 23/114 | hit 40/126 | cold 0/2 | Header-validation metadata move. Borrowed fields still follow callback lifetimes. |
| Map insertion staging `+0x147`, `0x820e7` | 304 | hit 17/81 | hit 28/82 | hit 34/94 | cold 0/2 | Intermediate state move, separate from final map ownership. |
| Server header validation `+0x1c2`, `0x84532` | 208 | hit 17/51 | hit 20/53 | hit 18/42 | cold 0/0 | Consuming validation path, not a body clone. |
| Direct inline `Pair::tick+0x161c`, `0x65e4c` | 88-byte value | hit 2/720 | other route | hit 5/686 | hit 1/1893 | Owned write operation moves from pending storage to completion. Few near-site samples. |

The dynamic payload copies obey the fragment, remaining write, and remaining read-page bounds.
Their lengths vary with the actual frame and cursor.
The [full inventory](evidence/copy-sites.json) retains all observed calls and preceding instructions.
The [inline region](evidence/direct-inline-moves.txt) and [sample counts](evidence/inline-move-counts.json) retain separate evidence.

Current source mappings:

- Server progress staging: `src/server/h2/server.rs:941`.
- Stream retirement extraction: `src/engine.rs:1141`.
- Header decode metadata: `src/server/h2/headers.rs:1828`.
- Outbound header validation: `src/server/h2/headers.rs:1469`.
- Owned write extraction: `examples/composition_bench_support/mod.rs:577`.

The [inlined source chain](evidence/copy-inline-source.txt) preserves both standard-library and crate frames.

# Proposals in order

No new optimization is authorized or proposed for this acceptance run.
The profile ranks costs, not hypothetical wins.
Large-body transport copies have more near-site samples than the protocol staging sites.
They remain necessary because the peer read page must own the bytes.
Complete body comparisons remain necessary for the benchmark's strict assertions.

The 144-byte server staging move remains sampled across all four workloads.
It is not universally the hottest metadata move.
For direct-empty, retirement extraction has 50 near-site samples versus 18 at server staging.
Size alone does not determine priority.

The historical return-layout PoC remains rejected and was not repeated.
No stable-slot rewrite or header-finalization change ran.
Any future change still needs evidence for borrowed input/header lifetimes, owned-operation settlement, partial writes, and regression behavior.

# What not to cut first

- Do not remove transport ownership or complete body assertions to improve the measured result.
- Do not rank the 88-byte inline move first from its size alone.
- Do not claim a zero-init problem without positive site evidence.
- Do not treat an unresolved libc leaf as exclusively client or server work.
- Do not infer elapsed-time improvement from a short function or an avoided move.
- Do not apply these offsets to another binary.

The [profile hashes](evidence/profile-hashes.json) identify the original `.data` files and executables.
The [freeze record](evidence/freeze.json) contains exact profile commands and build flags.
The raw profiles and full symbol/disassembly files remain beside the preserved executable.
CPU2 was released after the profiles completed.

To reproduce the alias-aware classification, run:

```sh
taskset -c 8-31 python3 docs/http2-performance/final/analyze_profiles.py \
  ../../build-http2-final/frozen-5505efc4 \
  docs/http2-performance/final/evidence
```

This report-only classifier does not change either measured executable.
