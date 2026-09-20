# Hottest stacks

These profiles belong only to source `b3c6484b1cb664dca76b5c35399d2e7d1367ce87`.
The binary SHA256 is `80d578b012ab4a5d9d165c6174e7b12fd4e776d20f53a50b0cf72201773de850`.
All recordings use `cpu-clock:u`, 999Hz, DWARF stacks, and CPU2.
Their elapsed values do not enter the timing comparison.

| Profile | Workload | Samples | Client | Server | Shared or incomplete |
|---|---|---:|---:|---:|---:|
| E: direct-empty | 2000000 exchanges, C1 | 6143 | 392 | 423 | 5328 |
| S: selected-empty | 2000000 exchanges, C1 | 6280 | 397 | 796 | 5087 |
| C: direct-empty128 | 16000 cohorts, C128 | 5572 | 345 | 402 | 4825 |
| L: direct-1m | 32768 exchanges, C1 | 5504 | 258 | 398 | 4848 |

The classifier uses unambiguous ancestor frames.
Generic pair frames and folded client/server aliases do not independently establish an owner.
Missing or ambiguous ancestry remains shared.
The totals are not complete client/server CPU estimates.

For direct-empty, the hottest client stack reaches `H2Client::accept_driver_bytes_ref`, with 19 samples.
The hottest server stack reaches `H2Server::accept_driver_event_bytes_ref`, with 27 samples.
The hottest shared stack is `Connection::select`, with 171 samples.

The selected profile contains 83 samples in a server stack through `http::Server::next` into child `issue_write`.
That stack includes child scheduling, not only selector overhead.
Its hottest shared stack is `Connection::select`, with 142 samples.

For 1MiB duplex, the hottest client stack reaches `accept_driver_bytes_ref`, with 51 samples.
The hottest server stack has 58 samples through the progress-extraction branch.
The hottest shared stack has **1363 samples** in the harness's complete body-slice comparison.
The two payload transport copies have 714 and 420 near-site samples.

The unknown-leaf totals are 359, 393, 381, and 2591 for E, S, C, and L.
Known ancestors can identify some work beneath an unresolved leaf.
No unknown leaf is assigned to one endpoint without such evidence.

# How copies were found (MIR vs objdump vs perf)

The exact binary froze before recording.
`nm -C -S`, `objdump -d -C`, and `addr2line` use that binary.
The analysis joins static symbol-relative copy offsets with `perf script -F +symoff` samples.
Near-site counts use approximately `±0x18` bytes.
Runtime instruction pointers are not subtracted from static addresses.

The inventory records current `rdx` or `edx` lengths.
It also searches current inline vector moves and their inlined source chains.
DWARF reports an 88-byte owned `WriteOp<&[u8]>`.
The inline transfer region comes from a fresh source mapping, not a previous offset.
No absence claim relies on MIR text.

# Copy inventory

Counts are near-site samples / physical parent samples.
Lengths describe machine-code copies and do not necessarily equal complete Rust type sizes.
Every address below belongs to this admission binary.

| Site and ELF address | Size | E | S | C | L | Kind and ownership |
|---|---:|---:|---:|---:|---:|---|
| Direct `Pair::tick+0x1ad2`, `0x666a2` | Dynamic | hit 4/891 | other route | hit 7/826 | hit 714/2035 | Explicit transport copy into the peer's owned read page. Necessary for this contract. |
| Direct `Pair::tick+0x184c`, `0x6641c` | Dynamic | hit 4/891 | other route | hit 5/826 | hit 420/2035 | Same contract in the other direction. |
| Map insertion `+0x1ae`, `0x7cb3e` | 316 | hit 57/122 | hit 50/103 | hit 40/77 | hit 1/2 | Stream-entry move into owned map storage. |
| Server progress forwarding `+0x69c`, `0xa3b8c` | 144 | hit 26/146 | hit 29/133 | hit 33/129 | hit 60/192 | `Result::branch` staging move, not a DATA payload clone. |
| Map insertion staging `+0x147`, `0x7cad7` | 304 | hit 31/122 | hit 23/103 | hit 22/77 | hit 1/2 | Intermediate state move. Final map storage still needs ownership. |
| Stream retirement `+0x148`, `0x59918` | 304 | hit 8/146 | hit 20/125 | hit 19/103 | cold 0/23 | State extraction after the retirement join. |
| Header decode metadata `+0x11e`, `0xa727e` | 208 | hit 12/92 | hit 16/110 | hit 16/83 | cold 0/1 | Validation metadata move, not a body clone. |
| Inline transfer `+0x1261`, `0x65e31` | 88-byte value | hit 2/891 | other route | hit 4/826 | hit 7/2035 | Owned write extraction from pending storage. |

The dynamic sizes obey the actual fragment, write, and read-page bounds.
The [full inventory](evidence/copy-sites.json) contains all sites and preceding instructions.
The [source mappings](evidence/copy-source-lines.txt) retain standard-library and crate frames.
The [inline evidence](evidence/inline-write-counts.json) records its separate sample counts.

# Proposals in order

No optimization is authorized or proposed in this follow-up.
The paired experiment measures the admission revision, not an optimization candidate.
The 144-byte server move remains sampled, but other metadata moves can have more samples in small-message profiles.
Neither its size nor these samples justify repeating the rejected return-layout PoC.

The profile does not isolate the source of the approximately 2% common-path regression.
Instruction layout, integrated code changes, and host variability can affect the comparison.
The paired results establish the observed revision difference more directly than comparisons between profile sample totals.

# What not to cut first

- Do not remove the transport ownership copy or full body comparisons.
- Do not prioritize the 88-byte inline move solely from its size.
- Do not treat a shared helper as exclusively client or server work.
- Do not infer zero-init cost without positive site evidence.
- Do not extrapolate these common-path profiles to armed admission notifications.
- Do not reuse these addresses for another binary.

The [profile hashes](evidence/profile-hashes.json) identify the original recordings.
The [freeze record](evidence/freeze.json) records all profile commands and flags.
Raw profiles, demangled stacks, symbols, and disassembly remain beside the preserved executable.
CPU2 was released after these recordings.
