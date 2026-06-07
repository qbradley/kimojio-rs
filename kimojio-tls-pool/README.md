# kimojio-tls-pool

Runtime-independent OpenSSL TLS streams with pool-managed work placement.

`kimojio-tls-pool` lets callers create TLS streams from normal threads and submit read/write operations with callbacks. Operations can run immediately, route to background executor threads, or use adaptive placement based on message size and executor load.

## Usage

Create a `TlsPool` with `PoolConfig`, then create client or server streams from OpenSSL connector/acceptor values and connected streams.

Callbacks receive operation results exactly once. Immediate operations call back on the submitting thread; background operations call back on the selected executor thread.

Operations submitted to the same stream are serialized, so OpenSSL stream state is not accessed concurrently even when adaptive placement moves later work between executors.

Reads reject buffers larger than the configured maximum read length. The default maximum is 32 KiB.

Reads first attempt nonblocking TLS progress so already-buffered plaintext can complete immediately. If OpenSSL reports WANT_READ or WANT_WRITE, the pool waits for the requested socket readiness before resuming the read, so a read waiting for a future peer response does not occupy a pool executor thread.

## Placement modes

- `ImmediateOnly`: run operations on the submitting thread.
- `BackgroundOnly`: route operations to pool executors.
- `Adaptive`: choose placement from message size and executor load.

## Statistics

Pool statistics report submitted, immediate, background-routed, queued, completed, failed, ready-spin, and idle-transition counts. Stream statistics report submitted, queued, active, and maximum active operations for same-stream serialization checks.

The aggregate queued count includes both same-stream queueing and executor queueing; separate counters expose each source.

Callbacks are type-erased because they may execute on a different thread from the submitter. Future APIs may add lower-allocation completion targets for workloads that can use channels or fixed completion handles instead of arbitrary callbacks.

## Benchmarks

```sh
cargo bench -p kimojio-tls-pool --bench rpc_write
```

For a benchmark smoke test, run:

```sh
cargo bench -p kimojio-tls-pool --bench rpc_write -- --test
```

The benchmark covers single-pair, three-pair per-connection-pool, and three-pair shared-pool RPC write scenarios across 4 KiB, 8 KiB, 16 KiB, 24 KiB, and 32 KiB bodies.

## Limitations

The crate currently does not provide tokio, kimojio runtime, kernel TLS, or hardware TLS offload integration.
