# Minimum per-operation overhead PoC

## Hypothesis and experiment boundary

The shared native backend already reports exact raw I/O results.
However, it passes every operation through request and completion channels.
It also creates a cancellation token for each operation and registers a wait on that token.
The driver repeatedly constructs completion-channel waits during unrelated protocol and application work.

The hypothesis is that persistent original-operation slots can remove this transport plumbing without changes to HTTP policy.
This PoC changes the native backend only.
Generic `SplittableStream` connections retain their workers, write-all semantics, and scope cancellation.
Both backends still use the same `State`, protocol machine, body storage, and input-selection loop.

This is a structural transport experiment, not a ready-probe scheduler or an eager full-body experiment.
It does not claim the minimum possible overhead for the complete wrapper.

## Source identities

- Frozen common base: `0bd3e950f4b0aedb552fbfd8fab96139ef399bb9` (`upnvvwuq`).
- PoC source: the commit that contains this record.
- Worktree: `/workspace/kimojio-rs/target/wrapper-lab/worktrees/overhead-poc`.
- Build directory: `/workspace/kimojio-rs/target/wrapper-lab/build-overhead-poc`.
- Unchanged `keepalive_bench.rs` Git blob: `7757a917cdeabcd13798c54b500696a6629816b5`.

Production-source SHA-256 values for the allocation runs:

| File | SHA-256 |
| --- | --- |
| `kimojio-http1/src/io_driver.rs` | `72d7a95545d2bc74389ea9b3db2340061fa1061cae85e2788eadcf8ea74018b7` |
| `kimojio-http1/src/driver.rs` | `0ebdf8f0622018a9e5ace36c77579d989e212788822ef0b2cd2af8eb5f09b3bc` |

The shared fixture, comparison runner, `body.rs`, and HTTP core have no changes.
The allocation executable includes the shared fixture through the existing allocation-probe crate.

## Mechanisms and code delta

`io_driver.rs` adds a private `IoDriver` interface.
`WorkerIo` contains the existing generic transport channels and operation tokens.
`NativeIo` contains one read slot, one write/close slot, one descriptor owner, and two cancellation flags.

Each slot owns a `Pin<Box<Option<F>>>`.
The box is allocated once per connection.
`Pin::set` installs the next concrete async future in the same allocation.
The original future remains in that slot across all unrelated driver events and completion-observer drops.
After completion, the slot drops the completed future in place and becomes empty.
No per-operation boxed future or trait-object dispatch is necessary.

The native async future owns the original core operation and a descriptor reference.
It borrows the operation's storage only after the slot pins that future.
The compiler maintains those internal borrows.
The implementation adds no unsafe code or dependency.

Each native slot has one connection-lifetime `Rc<Cell<bool>>` cancellation flag.
The driver sets the flag while it processes the core's cancellation event.
The existing input-selection loop then polls the slot.
The slot sends cancellation to its original operation at most once and awaits the original result.
No token wait, waiter allocation, or cancellation wake registration occurs on this path.
Only the running driver changes these private flags, so cancellation needs no separate wake.

`driver.rs` separates transport execution from protocol state.
Native constructors enter `run_native`, while generic constructors retain `run` and its worker join.
Both functions call the same `drive`.
The input rotation, source-admission checks, progress budget, timers, and abandonment logic remain unchanged.
Protocol time still starts after transport setup, including asynchronous generic `split`.

`slot_tests.rs` reuses the forwarding fixtures to exercise direct slots.
The duplex integration tests add native cases instead of changing the shared benchmark.
The generic native-worker adapter remains as a tested reference, but public native constructors select slots in this PoC.

## Ownership and behavioral constraints

- A native read fills the supplied receive storage directly.
- A native write submits one `writev` and returns its exact original result.
- The HTTP machine remains the sole owner of the partial-write cursor.
- A late success remains successful after cancellation.
- A forwarded lease remains inside its operation or completion until the core returns its receipt.
- Dropping a pending slot drops its original borrowed-resource future before it releases the descriptor.
- Explicit close requires an empty read slot and an empty write slot.
- Completed slots release their descriptor references before the close slot extracts the sole `OwnedFd`.
- The close slot awaits an actual native close operation.

The public constructors and client API do not change.
`--native` in the unchanged shared fixture selects this PoC.
Without `--native`, the fixture selects the generic worker backend.
The existing `--duplex` and `--copy-forward` flags retain their meanings.

## Structural operation costs

These counts describe wrapper transport plumbing, not total process allocation:

| Item | Common native backend | Native slots |
| --- | --- | --- |
| Transport request/completion channels | Four per connection | Zero |
| Request/completion channel messages | Two per read/write operation | Zero |
| Operation cancellation token | One allocation per operation | Zero |
| Cancellation-event waiter | Registered when the original operation remains pending | Zero |
| Completion-channel wait recreation | On repeated input selection | Zero |
| Pinned operation storage | Worker futures retain operations | Two reusable allocations per connection |
| Cancellation state | Per-operation token | Two shared flags per connection |
| Original read/write operation | One native operation | One native operation |
| Positive partial-write continuation | Core decides | Core decides |
| Descriptor close | One actual close | One actual close |

The native slot still clones two local `Rc` references when it starts an operation.
Runtime completion records, application metadata, body sources, application channels, and protocol timers still have their existing costs.
The slot tests repeat 1,024 operations in the same pinned allocation.
That test establishes storage reuse, not allocation-free HTTP exchanges.

## Allocation evidence

These are process-wide Rust allocator counts from the existing `alloc-http1-keepalive` executable.
They include setup, warmup, measured exchanges, validation, and teardown.
They are not per-operation counters or timing results.
Each row repeated identically across three runs, and every fixture result reported `valid: true`.

| Workload | Revision | Alloc calls | Realloc calls | Requested bytes |
| --- | --- | ---: | ---: | ---: |
| Fixed 128-byte response, 100 iterations + 10 warmup | Common base | 34,599 | 556 | 2,546,391 |
| Same fixed workload | Native slots | 24,830 | 556 | 1,859,492 |
| Chunked duplex, 64 KiB each direction, 20 iterations + 2 warmup | Common base | 59,771 | 118 | 5,589,851 |
| Same duplex workload | Native slots | 38,510 | 117 | 4,118,976 |

Allocator calls decreased by 28.23% for the fixed workload and 35.57% for the duplex workload.
These reductions include indirect effects from fewer driver/worker handoffs.
They do not establish an equivalent latency or throughput improvement.

Executable SHA-256 values:

- Common base: `c2da820ae2fcead7b60d432c43c6ae472a3cd5a5c88e5c03f7d5acc3d7c9e9b1`.
- Native slots: `59cea23dff929d7194fc1892b387deab90a0943d6c3f722be0cae505d9e0c75b`.

The commands used CPUs 8–31.
No timing or profiling run used CPUs 0–7.
The parent retains responsibility for final matched performance comparisons.

### Reproduction commands

The baseline archive and saved executables were scratch artifacts inside this worktree.
They were removed after the counts entered this record.

```sh
cd /workspace/kimojio-rs/target/wrapper-lab/worktrees/overhead-poc
export CARGO_TARGET_DIR=/workspace/kimojio-rs/target/wrapper-lab/build-overhead-poc
mkdir -p target/baseline-src target/overhead-evidence
git archive 0bd3e950f4b0aedb552fbfd8fab96139ef399bb9 |
  tar -x -C target/baseline-src
taskset -c 8-31 cargo build --locked --release \
  --manifest-path target/baseline-src/Cargo.toml \
  -p fsm-allocation-probes --bin alloc-http1-keepalive
cp "$CARGO_TARGET_DIR/release/alloc-http1-keepalive" target/overhead-evidence/baseline-alloc

# Shared-target artifact reuse initially produced identical executables.
# This targeted clean forced compilation from the PoC source.
taskset -c 8-31 cargo clean --release -p kimojio-http1 -p fsm-allocation-probes
taskset -c 8-31 cargo build --locked --release \
  -p fsm-allocation-probes --bin alloc-http1-keepalive
cp "$CARGO_TARGET_DIR/release/alloc-http1-keepalive" target/overhead-evidence/slots-alloc

# Run each command three times with each executable.
taskset -c 8-31 target/overhead-evidence/baseline-alloc \
  --native --iterations 100 --warmup 10 --response-bytes 128
taskset -c 8-31 target/overhead-evidence/slots-alloc \
  --native --iterations 100 --warmup 10 --response-bytes 128
taskset -c 8-31 target/overhead-evidence/baseline-alloc \
  --native --duplex --chunked --iterations 20 --warmup 2 \
  --request-bytes 65536 --response-bytes 65536 --chunk-bytes 4096
taskset -c 8-31 target/overhead-evidence/slots-alloc \
  --native --duplex --chunked --iterations 20 --warmup 2 \
  --request-bytes 65536 --response-bytes 65536 --chunk-bytes 4096
```

## Validation

The default-feature run passed 98 core tests and 74 wrapper tests.
The all-feature run passed 98 core tests and 77 wrapper tests.
Each run also passed the core's eight benchmark smoke cases.

New direct-slot tests cover pinned-storage reuse, persistent pending futures, one-shot cancellation, late success, forwarded lease retention, and pending-read destruction.
Expanded native integration cases cover Expect, fixed/chunked duplex reuse, consumer abandonment with and without leases, and deadlines after final output.
A ready-empty-source regression exercises fairness with native slots.
A virtual-clock regression preserves protocol deadlines across a delayed generic split.

```sh
taskset -c 8-31 timeout 180s cargo test \
  -p kimojio-http1 -p kimojio-fsm-http1 --all-targets
taskset -c 8-31 timeout 180s cargo test \
  -p kimojio-http1 -p kimojio-fsm-http1 --all-targets --all-features
taskset -c 8-31 cargo fmt
taskset -c 8-31 cargo clippy
taskset -c 8-31 cargo clippy --all-targets --all-features
taskset -c 8-31 cargo clippy -p kimojio-http1 --all-targets --all-features -- -D warnings
```

Both workspace Clippy modes succeeded with only existing warnings.
Those warnings are at `examples/http1-static/src/app.rs:413` and `kimojio/src/pipe.rs:64–65`.
The changed wrapper also passes strict Clippy without warnings.

## Rejected ideas and remaining limits

- A new per-operation boxed future replaces one allocation source with another. Reusable concrete-future slots avoid that trade.
- A raw self-referential operation structure needs unsafe lifetime handling. Pinned owning async futures avoid that requirement.
- Replacing the generic stream backend risks its write-all continuation and custom cancellation contracts. This PoC leaves that backend intact.
- Ready-probe scheduling and eager full-body production belong to other experiments and are absent here.
- Moving the partial-write cursor into transport conflicts with exact original-operation semantics and is absent here.
- The existing application channels, handler/body boxing, metadata conversions, and source polling remain.
- Native `EAGAIN` remains terminal, and the native backend still supplies no TLS.
- Abrupt slot destruction can synchronously settle borrowed kernel storage, as the runtime already requires.
- This record proves allocation reduction and tested behavior, not universal race safety or faster throughput.
