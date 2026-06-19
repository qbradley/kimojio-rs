# kimojio-stack-check

`kimojio-stack-check` is a post-build diagnostic tool for tuning
`kimojio-stack` and `kimojio-stack-steal` coroutine stack sizes. It reads LLVM
stack-frame metadata from an ELF binary, walks direct calls found in the
disassembly, and reports the known stack lower bound for named entry functions.

Use it as one input to stack tuning. It does not replace guarded stacks or
runtime high-water measurements because Rust programs commonly contain indirect
calls, trait-object calls, panic/drop paths, FFI, and recursion that cannot be
proven by this first-pass analyzer.

## What it measures

The checker combines:

- `.stack_sizes` metadata emitted by LLVM for local function frame sizes.
- `llvm-objdump` direct-call disassembly for call graph edges.
- A simple max-path calculation from one or more requested entry patterns.

For each matching symbol it prints:

- `frame_bytes`: local stack frame size for that function.
- `known_stack_bytes`: local frame plus the largest known direct-call child path.
- `unknown_edges`: indirect or external calls that make the result incomplete.
- `cycles`: detected call cycles, usually recursion or analysis ambiguity.
- `status`: budget comparison result when a budget is supplied.

Treat `known_stack_bytes` as a **known lower bound** unless `unknown_edges=0` and
`cycles=0`.

## Build a binary with stack metadata

The Rust support is currently nightly-only and ELF-only. Build the target binary
with `-Z emit-stack-sizes` and preserve the linker section with the included
linker script:

```sh
RUSTFLAGS="-Z emit-stack-sizes -C link-arg=-Wl,-T$(pwd)/kimojio-stack-check/keep-stack-sizes.x" \
  cargo +nightly build --release -p examples --bin object-gateway-stack-host
```

If the linker discards `.stack_sizes`, the checker fails with a message saying no
stack-size metadata was found.

## Run the checker

Run the tool against the built ELF and provide one or more `--entry` patterns.
Patterns are substring matches against raw or demangled symbols.

```sh
cargo run -q -p kimojio-stack-check -- \
  --binary target/release/object-gateway-stack-host \
  --entry serve_stack_grpc_listener=32K \
  --entry serve_stack_admin_listener=32K
```

Budgets may be supplied per entry with `pattern=budget`, or globally:

```sh
cargo run -q -p kimojio-stack-check -- \
  --binary target/release/object-gateway-stack-host \
  --budget 32K \
  --entry serve_stack_grpc_listener \
  --entry serve_stack_admin_listener
```

Use raw bytes or `K`/`M` suffixes. Add `--fail-on-unknown` only for restricted
code paths where unknown indirect/external edges and cycles should fail the run.
That flag is intentionally strict and will usually be too noisy for general
`std`-using application binaries.

## Tuning workflow

1. Pick stackful entry points that correspond to long-lived or high-cardinality
   coroutine tasks, such as connection handlers, request handlers, or worker
   tasks.
2. Build with preserved `.stack_sizes`.
3. Run `kimojio-stack-check` with the current stack budget.
4. Inspect `known_stack_bytes`, `unknown_edges`, and `cycles`.
5. Validate candidate budgets with runtime high-water data from
   `RuntimeContext::stack_usage` or `JoinHandle::join_with_stack_usage`.
6. Leave page-level margin. Stack usage is page-granular in practice because the
   runtime uses guarded stacks, and small source changes can move frame sizes.
7. Measure memory and throughput before and after changing the application stack
   budget.

For the Object Gateway host, the practical loop is:

```sh
# Static lower-bound check.
cargo run -q -p kimojio-stack-check -- \
  --binary target/release/object-gateway-stack-host \
  --entry serve_stack_grpc_listener=20K \
  --entry serve_stack_admin_listener=20K \
  --entry serve_steal_grpc_listener=20K \
  --entry serve_steal_admin_listener=20K

# Runtime high-water self-check.
cargo run --release -q -p examples --bin object-gateway-stack-host -- \
  --runtime stack \
  --grpc-addr 127.0.0.1:0 \
  --admin-addr 127.0.0.1:0 \
  --stack-size-bytes 20480 \
  --shutdown-after-ready \
  --print-stack-usage
```

The standalone Object Gateway currently defaults to 20 KiB guarded coroutine
stacks in release builds. Debug builds default higher because unoptimized frames
are larger. Use `--stack-size-bytes` to tune that application budget.

## Limitations

- Only ELF binaries with preserved LLVM `.stack_sizes` are supported.
- LLVM omits functions with dynamic stack allocations from `.stack_sizes`.
- Direct-call reconstruction from disassembly is incomplete by design.
- Indirect calls, trait-object calls, vtables, callbacks, FFI, dynamically
  linked code, and panic/drop paths can produce unknown edges.
- Recursion requires a separate bound; detected cycles are reported, not solved.
- Optimization level matters. Analyze the same profile you plan to deploy.
- The result is not a Rust compile-time proof. It is a post-build engineering
  check to guide stack-budget decisions.

## Tool options

```text
--binary <elf>              ELF binary to analyze.
--entry <pattern>[=<size>]  Symbol substring to analyze; may be repeated.
--budget <size>             Default budget for entries without inline budgets.
--readobj <path>            llvm-readobj command path, default llvm-readobj.
--objdump <path>            llvm-objdump command path, default llvm-objdump.
--fail-on-unknown           Fail when unknown edges or cycles are present.
```
