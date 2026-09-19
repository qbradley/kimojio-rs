# Native allocation probes

These diagnostic executables compile the original example entry points with a counting Rust allocator.
They do not replace protocol, application, or driver code.
The regular executables retain their original allocator and timing behavior.

Build the probes:

```sh
cargo build --release -p fsm-allocation-probes
```

The executables accept the original CLI arguments:

| Probe | Original executable | Bounded normal exit |
| --- | --- | --- |
| `alloc-http1-static` | `http1-static` | `--stop-after N` exchanges |
| `alloc-http1-wrapper` | Wrapper `server` example | `--connections N`, then close those connections |
| `alloc-websocket-chat` | `websocket-chat` | `--run-for-ms N` |
| `alloc-http1-client` | Wrapper `bench_client` example | `--warmup-ms N --duration-ms N` |

After the original entry point returns, stderr contains one `ALLOC_STATS` JSON record.
The probe preserves the original exit status.
A failed report causes a nonzero exit.
External termination or an original entry point that calls `process::exit` does not produce a complete report.

## Measurement scope

Counters cover Rust `GlobalAlloc` calls throughout the process, including startup and shutdown before the report.
They exclude native-library allocations that bypass Rust, allocator metadata, kernel storage, and later process cleanup.
The `live_bytes` field is not a leak count or RSS.
The `peak_live_bytes` field measures live requested sizes at allocator method boundaries, not internal allocator workspace.

`alloc_calls`, `zeroed_calls`, and `realloc_calls` count attempted calls.
`failed_calls` counts null allocation results.
`requested_bytes` sums requested sizes for successful allocation and reallocation calls.
A successful reallocation adds its entire new size to that total.
Live bytes change by the difference between its old and new sizes.
A failed reallocation preserves the old allocation.

Use a fixed successful operation count and normal shutdown for allocation runs.
Record warmup and startup scope instead of presenting whole-run totals as steady-state counts.
Different operation counts can distinguish setup costs from per-operation costs.
Do not use instrumented throughput as an ordinary performance result.
Atomic counters add overhead.

Run bounded HTTP and sender-inclusive WebSocket workloads:

```sh
python3 -B perf/allocation-probes/run.py \
  --counts 32 128 --sizes 128 65536 --fanout 4 \
  --output target/perf/allocations.json
```

The runner requires complete, byte-exact responses and normal process shutdown.
It uses the independent wire helpers from `interop/`.
Each chat run has a fifteen-second process lifetime and a finite socket deadline.
`--chat-run-for-ms` sets that explicit lifetime, up to thirty seconds.
The transcript bound permits at most 8 MiB of payload per recipient per run.
Every result records its command, successful operation count, binary hash, and counter scope.
The output remains marked incomplete unless every requested case succeeds.
