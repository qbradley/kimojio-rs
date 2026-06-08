# Durable Storage Client

## Overview

This work adds `kimojio-stack-storage`, a low-level durable object-storage
client foundation for `kimojio-stack` applications. The crate is intentionally
request/transport oriented: callers own scheduling, connection selection,
authentication, retry policy, deadlines, buffering, and integration-test gates.

The implementation targets storage workloads that need page-style writes,
metadata and ownership fencing, archive/config flows, snapshots/copy, and
backup status records without hidden runtimes or background work.

## Architecture and Design

### High-Level Architecture

The storage crate layers on the existing stackful HTTP client:

1. Callers build typed storage requests and policies.
2. Requests are represented as `RequestParts` with explicit method, URI,
   metadata headers, replayable body, deadline, and cancellation state.
3. A `Transport` executes requests using either fake/integration transports or
   `StackHttpTransport` over `kimojio-stack-http`.
4. Operation modules build specific storage requests and parse response
   metadata or streamed body chunks.

The HTTP stack was extended with incremental response-body callbacks,
header-aware streaming so service error bodies are not delivered to success
sinks, split HTTP/1.1 request head/body writes, direct HTTP/2 DATA writes from
`Bytes`, and transport-local plaintext I/O deadlines. TLS transport deadlines
currently perform expired-deadline pre-checks only; stalled TLS I/O timeout
enforcement is deferred with connector/TLS timeout support.

### Design Decisions

- **Low-level API first**: the crate exposes request builders and small clients
  rather than a high-level service object. This keeps request costs and retry
  behavior visible.
- **Replayability is explicit**: `ReplayBody` distinguishes replayable bytes
  from non-replayable streams so retry policy cannot silently resend consumed
  data.
- **Ambiguous page writes require recovery**: page-write transport failures are
  not directly retried. Callers refresh properties/sequence state first.
- **Ownership epochs are metadata, not lease headers**: lease operations only
  carry lease identity. Epoch persistence uses an explicit leased metadata
  update that preserves existing metadata.
- **External tests are gated**: emulator and real-storage smoke tests are safe by
  default and require explicit environment variables. Live real-storage
  execution is deferred until a connector/dialer exists.

### Integration Points

- `kimojio-stack-http` supplies `Body`, `HttpConfig`, protocol-neutral client
  wrappers, and `StackTransport`.
- `kimojio-stack` supplies stackful `RuntimeContext`, waitables, cancellation,
  and I/O deadlines.
- `kimojio-stack-storage` tests include fake transports and gated integration
  modes that exercise the public request surfaces without requiring external
  services by default.

## User Guide

### Prerequisites

- A `kimojio-stack` runtime context.
- A connected HTTP/1.1 or HTTP/2 `kimojio-stack-http` client connection, or a
  custom `Transport`.
- An auth strategy selected by the caller (`AuthMode` or `AuthProvider`).

### Basic Usage

Use request builders when an application already has an execution loop:

```rust
use kimojio_stack_storage::{
    PageRange, ReplayBody, upload_range_request,
};

let range = PageRange::aligned_len(0, 512)?;
let request = upload_range_request(&object, range, ReplayBody::from_vec(vec![0; 512]), None)?;
transport.execute(cx, &request)?;
```

Use small clients when the module owns a simple execution flow:

```rust
let page = kimojio_stack_storage::PageClient;
page.read_range(cx, &mut transport, &object, PageRange::new(0, 4095)?, |chunk| {
    sink.write_all(&chunk)?;
    Ok(())
})?;
```

### Advanced Usage

- Use `RetryPolicy::decide_with_state` with cancellation and elapsed-time state
  to make retry choices explicit.
- Use `PageWriteConditions` for sequence-number guarded page writes.
- Use `OwnershipWriteGuard` to classify ownership-loss outcomes into caller
  retry, reacquire-and-retry, or terminal failures.
- Use `ListClient::fetch_page_with_retry` for retry-aware paged listing.
- Use `FakeService` for deterministic fault injection and request assertions.

Authentication is caller-owned. The crate models identity, bearer token,
account-key, signed-source, and emulator modes through `AuthMode`, and allows
per-attempt refresh through `AuthProvider`. Secret-bearing values use redacted
debug output; signing and token acquisition remain outside this low-level crate.

## API Reference

### Key Components

- **Transport core**: `Transport`, `RequestParts`, `ResponseParts`,
  `AttemptError`, `StackHttpTransport`.
- **Model and metadata**: `StorageContext`, `ObjectRef`, `MetadataMap`,
  `TypedMetadata`, `ObjectProperties`.
- **Page/ownership**: `PageClient`, `PageRange`, `PageWriteConditions`,
  `LeaseContext`, `OwnershipWriteGuard`.
- **Containers/listing**: `ContainerClient`, `RoutingTable`, `Conditions`,
  `ListClient`, `ListOptions`.
- **Archive/config/snapshot/copy**: `BlockClient`, `ArchiveClient`,
  `ConfigReader`, `SnapshotClient`, `CopyClient`, `BackupStatusClient`.
- **Testing**: `FakeService`, `FakeResponse`, `DeterministicEmulator`.

### Configuration Options

- `PoolConfig` exposes keepalive, idle timeout, connect timeout, total timeout,
  and per-destination idle capacity.
- `ConcurrencyConfig` exposes global and per-operation limits.
- `RetryPolicy` exposes max attempts, base delay, max delay, total timeout, and
  caller-supplied jitter budget.

### Cost and Memory Model

The crate avoids hidden background work and keeps buffering explicit:

- Upload bodies use `Bytes`-backed replayable buffers when caller-owned or
  caller-shared payloads need cheap replay.
- Streaming reads deliver chunks to caller callbacks rather than collecting full
  responses unless callers use explicit small-object collection helpers.
- Large storage reads are expected to be split into ranged requests that each
  fit within `HttpConfig.max_body_bytes`; that limit remains a total response
  safety quota, not an unlimited streaming allowance.
- HTTP error bodies are drained without being delivered to success sinks.
- Listing memory is bounded by the current page body and caller-owned result
  collections; continuation streaming lets callers process one page at a time.
- Allocation/request-count tests compare representative page upload, metadata,
  range download, listing, and retry-loop paths.

## Testing

### How to Test

Default validation does not require external storage:

```sh
cargo test -p kimojio-stack-storage
cargo clippy -p kimojio-stack-storage --all-targets -- -D warnings
```

Workspace validation used for this implementation:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo test --doc
```

Optional gates:

- Set `KIMOJIO_STACK_STORAGE_EMULATOR` and
  `KIMOJIO_STACK_STORAGE_EMULATOR_ADDR=host:port` plus emulator account/key
  variables to run live Azurite HTTP operations.
- Set `KIMOJIO_STACK_STORAGE_REAL_SMOKE=1` with account/container environment
  variables to enable real-storage request-surface smoke checks.

Compatibility fixtures live in `CompatibilityFixtures::built_in()` and back the
default integration tests for routing, metadata normalization, error mapping,
and caller-environment expectations. They encode externally defined semantics
into repository-local tests without requiring an external service for normal
validation.

### Edge Cases

- Error response bodies are drained without being delivered to success sinks.
- HTTP/1.1 `HEAD` responses ignore `Content-Length` bodies.
- Missing or zero snapshot delta sizes remain pending.
- XML list names and metadata values are entity-decoded.
- Updated config reads require a non-empty ETag.
- Tenant initialization writes the durable initialized marker only after required
  objects are created or verified.

## Limitations and Future Work

- The crate does not yet include endpoint dialing, service discovery, or live
  credential-backed request signing.
- Live real-storage execution is intentionally deferred until connector/dialer
  support exists.
- Stalled TLS read/write timeout enforcement is not yet implemented; use
  plaintext deadline coverage or caller-side connection management until
  connector/TLS timeout support is added.
- Compression/decompression for archives is represented by metadata integration
  points, not an embedded codec.
- APIs are low-level and may be wrapped by future higher-level conveniences.
