# Profile-guided wrapper experiment

## Frozen baseline

The original binary and this worktree have different source revisions.
All original addresses below refer to `58e304d811335952167809914e85dce6015fadf6`.
The initial worktree revision is `5b2bd2c198a908e3e4676b13bfdb30ae6c3dbbe6`.
Production changes wait for the final common base.

- Binary: `/workspace/kimojio-rs/target/wrapper-lab/frozen/original/keepalive_bench`
- SHA-256: `993602a3ba20bd884e7c77ad23ef3fa05fca1afe1d17e5afa3dbd2ca92d8eade`
- Build: `CARGO_PROFILE_RELEASE_DEBUG=2 CARGO_TARGET_DIR=target/wrapper-lab/build-original cargo build --release -p kimojio-http1 --example keepalive_bench --offline`
- Profile: `/workspace/kimojio-rs/target/wrapper-lab/original-small.perf.data`
- Record: `perf record -q -e cpu-clock:u -F999 --call-graph dwarf,16384 --delay300 -- taskset -c2 BINARY --iterations100000 --warmup5000 --response-bytes128`

The benchmark measures complete client/server exchanges over a socketpair.
Both endpoints share one runtime and core.
It includes cleanup, full byte equality, and exact same-connection exchange counts.
These results are not external server QPS.

The original three-trial results, in microseconds per exchange:

| Workload | Original |
| --- | ---: |
| Small | 44.414 |
| Empty | 30.680 |
| POST small | 57.372 |
| Large | 164.528 |
| Chunked, 1 MiB each direction | 2322.966 |
| Fragmented, 512 bytes | 390.441 |

## Hottest stacks

The existing profile contains 4,188 samples and no lost samples.
Shared leaves dominate the result:

| Leaf or stack | Self time |
| --- | ---: |
| `next_input` poll closure | 10.74% |
| `WaitAsyncEventFuture::poll` | 10.12% |
| FSM `Core::next` | 7.71% |
| `malloc` | 4.25% |
| `next_input` async body | 4.23% |
| `AsyncEventSource::unregister` | 2.82% |
| `cfree` | 2.65% |

The `WaitData` allocation stack accounts for approximately 2.98% of all samples.
Its destructor accounts for approximately 1.79% through `cfree`.
These are allocation and wait-registration costs, not payload copies.
The supplied allocator experiment measured approximately 329.76 Rust allocator calls per complete exchange.

Conservative ancestor attribution identifies 221 client samples, 301 server samples, and 3,666 shared or unresolved samples.
The rule requires a client `Connection` ancestor or a server `driver` specialization with a `run_pair` ancestor.
Truncated or ambiguous stacks remain shared.
Thus these counts are lower bounds for endpoint attribution, not endpoint CPU shares.

The hottest identified server stack reaches `Core::receive_metadata` through `Server::next` and the server `drive` specialization (45 leaf samples).
The hottest identified client leaf is the client `drive` specialization (25 samples).
The client also reaches `receive_metadata` (22 leaf samples).
The shared `next_input` closure alone has 257 inline-leaf samples.
There is insufficient ancestry to charge that shared work to only one endpoint.

## How copies were found (MIR vs objdump vs perf)

`nm -C -S` locates the input poll at file address `0x6bc80`, size `0x1425`.
`objdump -d -C` shows explicit `memcpy` calls in that range.
Lengths come from `edx`, not Rust type-size assumptions.
`perf script -F comm,ip,sym,symoff,dso` supplies symbol-relative offsets.
No runtime address subtraction or current-tree line attribution is necessary.
MIR is unnecessary for this inventory because the binary exposes the calls directly.

## Copy inventory

All sites below belong to `driver::next_input`'s `PollFn::poll` in `kimojio-http1/src/driver.rs`.
The parent appears in 503 sample stacks.
The hit column counts samples within `0x18` bytes of each call.
Nearby counts can overlap and do not measure time inside libc.

| Site offset | Size | Hit | Kind | Necessary |
| --- | ---: | ---: | --- | --- |
| `+0x13b5` | 272 | 20 | move of selected input | Ownership transfer required, staging not inherently required |
| `+0x74b` | 183 | 14 | move of selected input | Same |
| `+0xd9d` | 183 | 6 | move of selected input | Same |
| `+0x1392` | 263 | 6 | move of selected input | Same |
| `+0xdc2` | 183 | 2 | move of selected input | Same |
| `+0x98b`, `+0xd6b`, `+0x1225`, `+0x127c`, `+0x1353` | 184–264 | 1 each | move of selected input | Same |
| `+0x1257` | 263 | 0, cold | move of selected input | Same |

Only one sample has a `memcpy` leaf.
Its ancestors include `realloc` and `RawVecInner::finish_grow`, without a reliable endpoint owner.
This profile does not support a payload-copy redesign as the first experiment.

## Proposals in order

1. Avoid wait registration while the FSM can advance synchronously.
   The original `drive` calls `next_input` after each callback.
   The selector constructs receive and cancellation futures before it knows whether suspension is necessary.
   Polling those futures registers waits that the next callback immediately discards.
   A synchronous channel probe can consume ready input without a wait allocation.
   A separate pending path must register every applicable wake source before suspension.
2. Preserve receive waits across callbacks if the first change leaves registration costs dominant.
   Channel lifetimes extend across the entire driver loop.
   Active cancellation needs a separate lifetime because exchanges replace its token.
   Extra state and completed-future handling are costs of this alternative.
3. Reduce intermediate input moves only if the next profile still attributes material time to those sites.
   Owned inputs must remain alive through `State::input`.
   The original profile gives this lower priority than wait churn.

Required tests cover cancellation, source lifecycle, input credit, early responses, leases, write errors, reuse, and custom transports.
The common base also requires lease forwarding and explicit duplex opt-in.
Benchmark fixtures and correctness assertions remain unchanged.

## What not to cut first

- Payload ownership and leases: these are correctness constraints.
- Protocol callbacks or error handling based only on struct size.
- Metadata copies without stronger sample evidence.
- Time observations without deadline and fairness evidence.

## Experiment plan

The first implementation will retain the public API and the existing FSM interface.
It will use the final common base and a private build directory.
CPU affinity for compilation and tests is `8-31`.
Timing and new profiles require an exclusive slot from the experiment coordinator.
No new timing run occurred during this initial analysis.
