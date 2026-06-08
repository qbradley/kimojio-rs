# Stackful Storage

`kimojio-stack-storage` provides low-level durable object-storage request
foundations for `kimojio-stack` applications. It is built on
`kimojio-stack-http` and keeps scheduling, transport selection, authentication,
retry policy, deadlines, and buffering explicit.

The crate does not start a runtime, background worker, connection pool, or async
task. Callers execute requests from stackful code through a `Transport`.

## Crate

| Crate | Purpose |
|-------|---------|
| `kimojio-stack-storage` | Storage-domain request builders, replayable bodies, retry/diagnostics, page and block object operations, metadata, ownership, listing, archive/config/snapshot/copy helpers, and integration-test harnesses |

## Transport model

Storage operations are expressed as `RequestParts` and executed by a `Transport`.
`StackHttpTransport` adapts those requests to a protocol-neutral
`kimojio-stack-http` client connection. Tests and applications can also provide
custom transports.

Important behavior:

- Request bodies use `ReplayBody` so retry eligibility is visible.
- Response body chunks can stream into caller sinks without buffering whole
  objects.
- Large reads should use ranged requests sized below `HttpConfig.max_body_bytes`;
  the HTTP body limit remains a per-response safety quota.
- Service error bodies are drained but not delivered to success sinks.
- Connected plaintext transport deadlines are stored on the transport, not in a
  global or task-global state.
- TLS transport deadlines currently perform expired-deadline pre-checks only;
  stalled TLS I/O timeout enforcement is deferred with connector/TLS timeout
  support.

## Operation families

| Area | Supported foundations |
|------|-----------------------|
| Page data | Create-if-absent, aligned range write/clear, properties, written ranges, sequence-number conditions, streaming reads |
| Ownership | Lease acquire/renew/change/break requests, epoch metadata persistence, stale-epoch classification |
| Metadata and containers | Container create/delete/exists, HEAD/properties, metadata set, conditional delete, routing |
| Listing | Prefix/marker listing, metadata/snapshot parsing, object-kind filtering, continuation streaming, merge/dedup |
| Block/archive/config | Block upload/download/delete, bounded small-object collection, archive CRC metadata/validation, config JSON reads with ETags |
| Snapshot/copy/status | Snapshot lifecycle requests, delta-size polling, copy-from-source diagnostics, backup status JSON update/readback |

## Retry and diagnostics

`RetryPolicy` is explicit and does not hide background work. Replayable
operation failures can be retried according to caller policy, elapsed time, and
cancellation state. Ambiguous page writes are not directly retried; callers must
refresh sequence/properties state before deciding whether a retry is safe.

`Diagnostics` records operation class, attempt number, status, service code,
request id, elapsed time, and retriable classification.

## Testing

Default tests require no external service:

```sh
cargo test -p kimojio-stack-storage
```

Optional gates:

- `KIMOJIO_STACK_STORAGE_EMULATOR=1` plus
  `KIMOJIO_STACK_STORAGE_EMULATOR_ADDR=host:port` and emulator account/key
  variables runs live Azurite HTTP operations.
- `KIMOJIO_STACK_STORAGE_REAL_SMOKE=1` with account/container environment
  variables enables real-storage request-surface smoke checks. Live request
  execution is deferred until connector/dialer support is added.

## Current limitations

- Endpoint dialing and request signing are not implemented yet.
- Stalled TLS read/write timeout enforcement is not implemented yet.
- Archive compression/decompression is represented through metadata integration
  points; codecs are caller-owned.
- The API is intentionally low-level. Higher-level conveniences can be layered
  later without changing the explicit cost model.
