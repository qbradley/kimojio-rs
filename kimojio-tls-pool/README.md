# kimojio-tls-pool

Runtime-independent OpenSSL TLS streams with pool-managed work placement.

`kimojio-tls-pool` lets callers create TLS streams from normal threads and submit read/write operations with callbacks. Operations can run immediately, route to background executor threads, or use adaptive placement based on message size and executor load.

## Usage

Create a `TlsPool` with `PoolConfig`, then create client or server streams from OpenSSL connector/acceptor values and connected fd-backed streams. The pool sets each stream fd to nonblocking mode and assumes exclusive control over that fd's blocking-mode state while the TLS stream is alive.

Callbacks receive operation results exactly once. Immediate operations call back on the submitting thread when they can complete without waiting for socket readiness; background and readiness-resumed operations call back on executor threads. Callbacks should be nonblocking notifications and must not synchronously wait for follow-up work on the same stream.

Operations submitted to the same stream are serialized, so OpenSSL stream state is not accessed concurrently even when adaptive placement moves later work between executors.

Reads reject buffers larger than the configured maximum read length. The default maximum is 32 KiB.

Reads and writes make nonblocking TLS progress. Reads first attempt progress inline so already-buffered plaintext can complete immediately. If OpenSSL reports WANT_READ or WANT_WRITE, the pool waits for the requested socket readiness before resuming the operation on an executor, so operations waiting for peer/socket progress do not occupy pool executor threads.

## Placement modes

- `ImmediateOnly`: run operations on the submitting thread.
- `BackgroundOnly`: route operations to pool executors.
- `Adaptive`: choose placement from message size and executor load.

## Statistics

Pool statistics report submitted, immediate, background-routed, queued, readiness-waited, readiness-resumed, completed, failed, ready-spin, and idle-transition counts. Stream statistics report submitted, queued, active, and maximum active operations for same-stream serialization checks.

The aggregate queued count includes both same-stream queueing and executor queueing; separate counters expose each source. Readiness waits are reported separately from normal executor queueing because they park socket-blocked operations without occupying a worker.

Callbacks are type-erased because they may execute on a different thread from the submitter. For repeated payloads, `write_shared` accepts `Arc<[u8]>` so callers can avoid per-call payload allocation/copying. `write_batch` amortizes callback and queueing overhead across multiple shared chunks. Future APIs may add lower-allocation completion targets for workloads that can use channels or fixed completion handles instead of arbitrary callbacks.

## Benchmarks

```sh
cargo bench -p kimojio-tls-pool --bench rpc_write
```

For a benchmark smoke test, run:

```sh
cargo bench -p kimojio-tls-pool --bench rpc_write -- --test
```

The benchmark covers single-pair, three-pair per-connection-pool, and three-pair shared-pool RPC write scenarios across 4 KiB, 8 KiB, 16 KiB, 24 KiB, and 32 KiB bodies. It also includes fixed-workload RPC/TLS write scaling groups, a saturated `tls_write/saturated_encrypt_scaling` group, and corrected direct-OpenSSL baselines. The saturated group gives each executor one independent TLS stream and a fixed amount of 32 KiB full-write work, which measures aggregate encryption throughput with 1, 2, 4, and 8 executor threads.

On the current test host, saturated pool throughput scales from about 4.9 GiB/s with one executor to about 9.6 GiB/s with two, 18.7 GiB/s with four, and 29.3 GiB/s with eight. Corrected direct-OpenSSL baselines show the same shape for this batched crypto path; fixed-workload and per-operation groups remain useful for measuring scheduler and callback overhead.

## Limitations

The crate currently does not provide tokio, kimojio runtime, kernel TLS, or hardware TLS offload integration.
