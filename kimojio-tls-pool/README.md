# kimojio-tls-pool

Runtime-independent OpenSSL TLS streams with pool-managed work placement.

`kimojio-tls-pool` lets callers create TLS streams from normal threads and submit read/write operations with callbacks. Operations can run immediately, route to background executor threads, or use adaptive placement based on message size and executor load.

## Usage

Create a `TlsPool` with `PoolConfig`, then create client or server streams from OpenSSL connector/acceptor values and connected streams.

Callbacks receive operation results exactly once. Immediate operations call back on the submitting thread; background operations call back on the selected executor thread.

## Placement modes

- `ImmediateOnly`: run operations on the submitting thread.
- `BackgroundOnly`: route operations to pool executors.
- `Adaptive`: choose placement from message size and executor load.

## Statistics

Pool statistics report submitted, immediate, background-routed, queued, completed, failed, ready-spin, and idle-transition counts. Stream statistics report submitted, queued, active, and maximum active operations for same-stream serialization checks.

## Benchmarks

```sh
cargo bench -p kimojio-tls-pool --bench rpc_write
```

The benchmark covers single-pair and three-pair RPC write scenarios across 4 KiB, 8 KiB, 16 KiB, 24 KiB, and 32 KiB bodies.
