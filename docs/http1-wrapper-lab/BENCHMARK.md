# Repeated-connection benchmark

## Workload

`kimojio-http1/examples/keepalive_bench.rs` runs a real client and server through the public wrapper.
The two endpoints use one native Unix stream socket pair.
No simulated completion supplies the measured I/O.
There is no reconnect path, pool, retry, or second connection.

Each request and response contains a deterministic binary payload.
Both endpoints compare every received byte with the expected slice.
The server counts every exchange, including warmup.
The client checks status, complete framing, payload length, and the absence of unexpected trailers.
The final count must equal warmup plus measured exchanges.

The initial server consumes the request before it returns the response.
This permits persistent connections under the original conservative response policy.
A later duplex workload will exercise concurrent input and output explicitly.
That workload must not be confused with this request-then-response baseline.

The measured interval starts after warmup on the established connection.
It ends after all measured exchanges and successful shutdown of the application, client driver, and server.
The elapsed and process CPU intervals cover the same work.
Setup and report serialization remain outside that interval.
The interval includes application metadata, payload production, comparison, wrapper execution, and native I/O.
It does not isolate the protocol core or exclude fixture allocations.

The socket pair avoids TCP policy, connection setup, and an external load generator.
It is an optimization workload, not a replacement for TCP or independent-peer results.
Both endpoints run in one runtime on one CPU.
Separate client/server profiles require stack attribution rather than assuming that a shared leaf belongs to one side.

## Reproduction

Build a frozen source revision with a private target directory:

```sh
CARGO_PROFILE_RELEASE_DEBUG=2 CARGO_TARGET_DIR=target/wrapper-lab/build-original \
  cargo build --release -p kimojio-http1 --example keepalive_bench --offline
```

Run a small-body sample:

```sh
taskset -c 2 target/wrapper-lab/build-original/release/examples/keepalive_bench \
  --iterations 20000 --warmup 1000 --response-bytes 128 \
  --json target/wrapper-lab/small.json
```

Run a large bidirectional chunked sample:

```sh
taskset -c 2 target/wrapper-lab/build-original/release/examples/keepalive_bench \
  --iterations 200 --warmup 20 --request-bytes 1048576 \
  --response-bytes 1048576 --chunked --json target/wrapper-lab/chunked.json
```

The JSON file contains one machine-readable result.
Runtime diagnostics can also appear on stdout.
A failed exchange, incorrect count, payload mismatch, or watchdog expiration invalidates the run.
An invalid run has null throughput and a nonzero exit.
The watchdog is an emergency limit, not successful connection settlement.

## Comparison runner

`perf/wrapper-lab/compare.py` accepts a manifest with this shape:

```json
{
  "candidates": [
    {
      "name": "original",
      "revision": "full source commit",
      "binary": "/absolute/path/to/frozen/keepalive_bench",
      "build_command": "exact release build command"
    }
  ]
}
```

The runner records each binary hash and rejects binary changes during execution.
It randomizes candidate/workload order with a recorded seed.
All executions remain sequential and use one selected CPU.
The result retains every trial, command, failure, and workload count.
Summaries contain medians, ranges, and population standard deviations, not confidence intervals.
Any failed row invalidates the comparison.

```sh
python3 -B perf/wrapper-lab/compare.py \
  --manifest target/wrapper-lab/manifest.json \
  --output target/wrapper-lab/comparison.json --trials 5 --cpu 2
python3 -B -m unittest discover -s perf/wrapper-lab -p 'test_*.py' -v
```

## Allocation and profile runs

`alloc-http1-keepalive` compiles the same entry point with the existing counting allocator.
Its totals include startup, warmup, measured exchanges, shutdown, and reporting before the final allocator record.
Two runs with different measured counts and identical warmup expose a per-exchange slope.
Instrumented throughput is not a timing result.

Profiles use the uninstrumented release binary and a separate execution.
The publication must retain the source identity, binary hash, command, event, and sample scope.
Performance comparisons never use profile-instrumented elapsed time.
