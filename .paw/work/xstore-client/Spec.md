# Feature Specification: Durable Storage Client

**Branch**: stackful  |  **Created**: 2026-06-08  |  **Status**: Draft
**Input Brief**: Build a low-allocation durable storage client for the provided requirements brief.

## Overview

Storage-heavy services need a durable object-storage client that preserves existing page, metadata, lease, snapshot, archive, and configuration semantics while making request costs explicit. The client must support write-heavy page durability, range-based recovery, ownership fencing, initialization, backup/restore, archive access, and configuration polling without relying on hidden buffering or hidden background behavior.

The core value is operational safety under failure: writers must not corrupt data during failover, retries must not replay unsafe bodies, and callers must be able to distinguish transient storage errors from stale ownership, metadata corruption, sequence-number uncertainty, and not-found conditions. The feature also needs bounded memory behavior for large reads, writes, and listings so high-frequency storage paths can be measured and tuned.

The client should expose behavior at a storage-domain level where correctness invariants matter, while still allowing direct storage operations required by current workflows. Authentication, retry eligibility, deadlines, cancellation, memory use, and observability must be visible to callers so performance and correctness are measurable during migration.

## Objectives

- Preserve page and metadata correctness semantics for page-range writes, range reads, leases, sequence numbers, snapshots, copies, archive objects, and configuration reads.
- Prevent unsafe retry behavior by requiring replayable bodies for retryable writes and deterministic failures for non-replayable bodies.
- Bound memory usage for large reads, writes, and listings by chunk/page size plus caller-provided buffers.
- Support production, emulator, and test authentication modes without changing existing storage layout or metadata meaning.
- Preserve diagnostic information needed for retry decisions, supportability, and migration comparisons.
- Provide allocation and request-count observations for comparing new and existing storage paths.

## User Scenarios & Testing

### User Story US-001 P1 - Persist Dirty Pages Safely

Narrative: As a primary page writer, I need to persist dirty page ranges and associated metadata without extra payload copies while preserving ownership and ordering guarantees.

Independent Test: Run a writeback workload that creates page objects, advances metadata, uploads ranges under ownership fencing, retries transient failures, and verifies recovered pages and metadata.

Acceptance Scenarios:
1. Given a page object does not exist, When a dirty page range is flushed, Then the client creates the object with required metadata and uploads the page range.
2. Given stored high-water metadata is lower than the flushed page LSN, When a batch is flushed, Then metadata is advanced before page writes are issued.
3. Given an upload returns an ambiguous service error, When retry handling runs, Then the client can reset/replay the body and require sequence-number recovery before unsafe overwrite.
4. Given a stale writer loses ownership, When it attempts a metadata or page write, Then the client surfaces an ownership error or reacquires ownership only after epoch validation.

### User Story US-002 P1 - Recover Data by Streaming Ranges

Narrative: As a recovering shard or cache, I need to list objects, discover written page ranges, and stream required ranges into local sinks without materializing large objects in memory.

Independent Test: Initialize a local cache from multiple sparse page objects and verify local output matches expected content while memory remains bounded by configured chunk size plus caller buffers.

Acceptance Scenarios:
1. Given sparse page objects, When recovery lists objects and requests written ranges, Then contiguous returned ranges are coalesced and empty objects avoid unnecessary content downloads.
2. Given a large remote range, When downloading to a sink, Then the response is delivered as chunks with backpressure and without full-buffer allocation.
3. Given a transient range-read error, When retry policy permits retry, Then the range download is reissued and the caller receives either complete content or a typed failure.

### User Story US-003 P1 - Fence Ownership During Failover

Narrative: As a newly promoted writer, I need to acquire object ownership before writing so stale replicas cannot continue mutating data.

Independent Test: Simulate two writers with different epochs racing to acquire ownership and verify only the highest valid epoch can continue writing.

Acceptance Scenarios:
1. Given an object has a higher ownership epoch, When a lower-epoch writer attempts acquisition, Then acquisition fails before writes are allowed.
2. Given another process owns the object, When takeover is required, Then the client breaks and acquires ownership with the proposed ownership ID and updates epoch metadata.
3. Given an object disappears between listing and ownership acquisition, When cleanup removed a deleted object, Then callers can classify not-found as a safe skip where appropriate.

### User Story US-004 P1 - Initialize Tenant Storage

Narrative: As a tenant-initialization component, I need to create containers, store tenant state, upload initial data, and mark storage initialized with metadata required for startup.

Independent Test: Initialize a tenant from scratch and verify tenant state, initialization metadata, page objects, relation-size metadata, and container status can be read back.

Acceptance Scenarios:
1. Given a new tenant, When initialization runs, Then containers are created across configured accounts and metadata objects are initialized.
2. Given bootstrap data files, When upload runs, Then page objects are initialized once per unique object and relation-size metadata is written.
3. Given a container is already initialized, When initialization is attempted again, Then the storage API rejects reinitialization.

### User Story US-005 P2 - Manage Snapshots and Copies

Narrative: As backup and restore automation, I need to snapshot page objects, measure deltas, copy snapshots, checkpoint progress, and clean orphan snapshots.

Independent Test: Run a backup/restore copy flow against source and target containers and verify snapshots, copy status, checkpoints, and cleanup behavior.

Acceptance Scenarios:
1. Given a page object, When a snapshot is created, Then the returned snapshot ID can be used for properties, delta-size, copy, and delete operations.
2. Given a snapshot URL, When copying to a target object, Then the client issues copy-from-URL and captures copy/request diagnostic headers on errors.
3. Given many snapshots, When cleanup lists snapshots and known backups, Then orphan snapshots are identified and delete requests are issued with bounded concurrency.

### User Story US-006 P2 - Store and Retrieve Archive Objects

Narrative: As an archive reader/writer, I need to upload compressed objects, avoid overwrites when requested, download objects to local sinks, and validate integrity metadata.

Independent Test: Upload archive payloads with metadata, attempt duplicate upload-if-not-exists, download by exact and prefix-resolved name, and verify CRC and file size.

Acceptance Scenarios:
1. Given an object already exists, When upload-if-not-exists is called, Then the operation reports skipped without overwriting.
2. Given a downloaded object contains CRC metadata, When the payload is streamed to disk, Then computed CRC and persisted file size are validated.
3. Given a compressed archive object, When the reader downloads it, Then decompression produces the expected local file.

### User Story US-007 P2 - Read Configuration Efficiently

Narrative: As configuration polling code, I need to read JSON configuration objects and use ETags to avoid downloading unchanged content.

Independent Test: Read a config object, reload with the same ETag, update the object, and verify reload returns new content and a new token.

Acceptance Scenarios:
1. Given a valid config object, When it is read, Then the client returns object-root JSON and a non-empty ETag.
2. Given the ETag has not changed, When reload sends If-None-Match, Then unchanged status maps to unchanged without content transfer.
3. Given the object is missing, When read or reload occurs, Then a typed not-found error is returned.

### Edge Cases

- Non-replayable request body reaches a retryable write path: retry is rejected before silently replaying consumed bytes.
- Page range is not aligned to the storage service page boundary: request fails validation before sending malformed storage requests.
- Sequence number reaches overflow boundary: recovery path resets or updates according to caller policy.
- Ownership epoch is missing, malformed, lower, equal, or higher than the caller epoch: each case maps to deterministic acquisition behavior.
- Metadata required for correctness is missing or malformed: typed corruption or metadata-format error is returned.
- Listing returns multiple pages, duplicate objects across accounts, or continuation-marker failures: caller receives streamed pages or typed errors.
- Ranged download sees transient error after partial data: retry either restarts from a safe range boundary or returns a typed incomplete failure.
- Copy/snapshot response omits expected diagnostic headers: error still preserves available request/status context.
- Emulator differs from production behavior: deviation is classified and tested separately where needed.

## Requirements

### Functional Requirements

- FR-001: Perform storage operations with explicit operation inputs, deadline, cancellation, response status, response metadata, and streamed output chunks. (Stories: US-001,US-002,US-003,US-004,US-005,US-006,US-007)
- FR-002: Request bodies on retried writes must be replayable or explicitly non-retryable. (Stories: US-001,US-002,US-003,US-004)
- FR-003: Response bodies must be consumable as chunk streams with backpressure and without default full-buffer collection. (Stories: US-001,US-002,US-003,US-004,US-005,US-006,US-007)
- FR-004: Responses and errors must preserve service status, response metadata, storage request ID, storage error code/message, and diagnostics needed for support and retry decisions. (Stories: US-001,US-002,US-003,US-004,US-005,US-006,US-007)
- FR-005: Allow callers to bound connection reuse, idle duration, connection establishment time, total operation duration, per-destination idle capacity, and concurrent operations. (Stories: US-001,US-002,US-003,US-004,US-005,US-006,US-007)
- FR-006: Support production identity-based authentication with a caller-selected identity. (Stories: US-001,US-002,US-003,US-004,US-005,US-006,US-007)
- FR-007: Support bearer/access-token authentication for tests and controlled environments. (Stories: US-001,US-002,US-003,US-004,US-005,US-006,US-007)
- FR-008: Support key-based signing where archive or emulator paths require it. (Stories: US-005,US-006,US-007)
- FR-009: Support signed source references and optional source authorization for copy operations. (Stories: US-005,US-006,US-007)
- FR-010: Support local emulator endpoints, including multi-account naming and an authenticated local test configuration. (Stories: US-001,US-002,US-003,US-004,US-005,US-006,US-007)
- FR-011: Create, delete, and check existence of containers across configured accounts. (Stories: US-001,US-002,US-003,US-004)
- FR-012: Deterministically route internal/metadata objects to a primary account and data objects across configured accounts using existing naming/layout semantics. (Stories: US-001,US-002,US-003,US-004)
- FR-013: Expose account/container/object source-reference construction for snapshot and copy-source workflows. (Stories: US-005,US-006,US-007)
- FR-014: Treat already-existing container creation as idempotent and distinguish being-deleted and authorization failures. (Stories: US-001,US-002,US-003,US-004)
- FR-015: Create page objects with caller-provided size, metadata, creation preconditions, optional ownership ID, and optional ownership-epoch metadata. (Stories: US-001,US-002,US-003,US-004)
- FR-016: Upload aligned page ranges with explicit content length, optional ownership ID, and optional sequence-number equality condition. (Stories: US-001,US-002,US-003,US-004)
- FR-017: Clear page ranges with optional ownership ID. (Stories: US-001,US-002,US-003,US-004)
- FR-018: Get and coalesce written page ranges, including an empty-range representation that avoids unnecessary downloads. (Stories: US-001,US-002,US-003,US-004)
- FR-019: Increment and update page-object sequence numbers, including overflow recovery to zero. (Stories: US-001,US-002,US-003,US-004)
- FR-020: Get page-object properties including content length, timestamps, sequence number, ETag, snapshot identity, and metadata. (Stories: US-001,US-002,US-003,US-004,US-005,US-006,US-007)
- FR-021: Support streaming ranged reads for page objects. (Stories: US-001,US-002,US-003,US-004)
- FR-022: Upload block objects with metadata and optional ownership ID. (Stories: US-001,US-002,US-003,US-004,US-005,US-006,US-007)
- FR-023: Support upload-if-not-exists semantics for block objects. (Stories: US-005,US-006,US-007)
- FR-024: Download block objects either as streaming bodies or explicitly collected small objects. (Stories: US-005,US-006,US-007)
- FR-025: Delete base objects and snapshots with optional ownership ID, treating not-found idempotently where current callers require it. (Stories: US-001,US-002,US-003,US-004,US-005,US-006,US-007)
- FR-026: Copy objects from source references with optional source authorization and expected accepted-status handling. (Stories: US-005,US-006,US-007)
- FR-027: Retrieve object metadata/properties without downloading content. (Stories: US-001,US-002,US-003,US-004,US-005,US-006,US-007)
- FR-028: Set object metadata while preserving caller-provided metadata maps and supporting ownership ID. (Stories: US-001,US-002,US-003,US-004)
- FR-029: Preserve metadata invariants for highest LSN, oldest replay LSN, relation size, deletion/truncation markers, shard config, upload LSN, backup time, and ownership epoch. (Stories: US-001,US-002,US-003,US-004,US-005,US-006,US-007)
- FR-030: Support conditional operations using ETag/If-Match and If-None-Match, including conditional delete and config reload. (Stories: US-001,US-002,US-003,US-004,US-005,US-006,US-007)
- FR-031: Surface missing or invalid required metadata as typed corruption or metadata-format errors. (Stories: US-001,US-002,US-003,US-004,US-005,US-006,US-007)
- FR-032: Break and acquire ownership leases with proposed ownership ID and infinite duration support. (Stories: US-001,US-002,US-003,US-004)
- FR-033: Read, validate, and update ownership-epoch metadata as part of ownership acquisition. (Stories: US-001,US-002,US-003,US-004)
- FR-034: Provide a write wrapper that handles ownership loss or mismatch by optionally reacquiring ownership and requesting caller retry. (Stories: US-001,US-002,US-003,US-004)
- FR-035: Treat documented ownership-race errors as retriable while preserving stale-epoch failures as non-retriable. (Stories: US-001,US-002,US-003,US-004)
- FR-036: List objects by prefix with metadata, object type filtering, snapshot inclusion, continuation markers, retryable page fetching, and streaming results. (Stories: US-001,US-002,US-003,US-004,US-005,US-006,US-007)
- FR-037: List across multiple accounts and merge/deduplicate account-wide views when requested. (Stories: US-001,US-002,US-003,US-004,US-005,US-006,US-007)
- FR-038: Create snapshots, delete snapshots, get snapshot properties, and list base objects plus snapshots. (Stories: US-005,US-006,US-007)
- FR-039: Issue snapshot delta-size property requests and parse delta-size, treating absent or zero as pending. (Stories: US-005,US-006,US-007)
- FR-040: Map storage status/error-code combinations into typed errors for auth, authorization, not found, already exists, condition not met, ownership errors, sequence-number errors, range errors, service unavailable, timeout, and unhandled server errors. (Stories: US-001,US-002,US-003,US-004,US-005,US-006,US-007)
- FR-041: Support transport-layer retries for transient network/service failures and operation-layer retries for semantic races. (Stories: US-001,US-002,US-003,US-004,US-005,US-006,US-007)
- FR-042: Honor service-provided retry delays, bounded exponential backoff, jitter, total timeout, and cancellation. (Stories: US-001,US-002,US-003,US-004,US-005,US-006,US-007)
- FR-043: Distinguish ambiguous page-write service errors from known-not-committed failures so callers can recover sequence numbers only when required. (Stories: US-001,US-002,US-003,US-004)
- FR-044: Allow callers to classify operations for retry and observability dimensions. (Stories: US-001,US-002,US-003,US-004,US-005,US-006,US-007)
- FR-045: Hot page-range uploads must accept caller-owned or caller-shared payloads without duplicating payload bytes solely to perform the operation. (Stories: US-001,US-002,US-003,US-004)
- FR-046: Hot ranged downloads must stream chunks into caller-provided sinks or iterators without default full-range collection. (Stories: US-001,US-002,US-003,US-004)
- FR-047: Metadata/property operations must complete without transferring object content and must expose measured allocation/request counts. (Stories: US-001,US-002,US-003,US-004,US-005,US-006,US-007)
- FR-048: Object listing must stream one page at a time and avoid collecting all objects unless explicitly requested. (Stories: US-001,US-002,US-003,US-004,US-005,US-006,US-007)
- FR-049: Repeated attempts must preserve stable operation inputs and refresh only values that must change per attempt, such as authorization, continuation position, or replayed body state. (Stories: US-001,US-002,US-003,US-004)
- FR-050: Expose allocation and request-count metrics for migration comparisons. (Stories: US-001,US-002,US-003,US-004,US-005,US-006,US-007)
- FR-051: Provide demonstrated integration coverage showing both current caller environments can use the client without changing storage semantics. (Stories: US-001,US-002,US-003,US-004,US-005,US-006,US-007)
- FR-052: Support bounded concurrency controls at caller and client levels. (Stories: US-001,US-002,US-003,US-004,US-005,US-006,US-007)
- FR-053: Provide controlled validation surfaces for fault injection, retry observation, emulator behavior, and deterministic ownership-race scenarios. (Stories: US-001,US-002,US-003,US-004,US-005,US-006,US-007)

### Cross-Cutting / Non-Functional

- Hot-path allocation tests must cover page writes, metadata operations, range downloads, listing, and retries.
- Large reads and writes must be stream-first and backpressure-aware.
- Retries must be deterministic, bounded, observable, and safe with respect to body replay and ambiguous outcomes.
- Public behavior must expose memory use, retry eligibility, deadlines, cancellation, and concurrency limits.

## Success Criteria

- SC-001: Compatibility tests cover page durability, recovery, ownership fencing, initialization, backup/restore, archive access, and configuration-read operation families. (FR-001-FR-053)
- SC-002: Page writeback and recovery workloads complete with no regressions in LSN metadata, deletion/truncation metadata, ownership epoch, and sequence-number invariants. (FR-015-FR-021, FR-029, FR-032-FR-035, FR-043)
- SC-003: Steady-state page-range upload performs zero payload-byte copies attributable solely to the client transport abstraction for buffer-backed payloads. (FR-045)
- SC-004: Ranged downloads and listing memory use remain bounded by configured chunk/page size plus caller buffers, independent of object/container size. (FR-003, FR-046, FR-048)
- SC-005: Retry parity tests demonstrate terminal/retry behavior for authentication, authorization, not-found, already-exists, condition-not-met, ownership, sequence-number, range, unavailable, timeout, and unhandled server errors. (FR-040-FR-043)
- SC-006: Backup/restore tests demonstrate snapshot create/list/delete, delta-size polling, copy-from-URL diagnostics, and checkpoint/status object updates. (FR-026, FR-036-FR-039)
- SC-007: Configuration reload tests demonstrate unchanged behavior and no content transfer for unchanged objects. (FR-030)
- SC-008: Archive tests demonstrate upload, upload-if-not-exists, download-to-file, CRC validation, compression behavior, and prefix fallback. (FR-022-FR-024, FR-040)
- SC-009: Observability records operation class, attempt number, terminal/retriable classification, status/error code, elapsed time, and request/copy headers for every failed attempt. (FR-004, FR-044)

## Assumptions

- Cloud object-storage service semantics are the compatibility target, including page objects, block objects, metadata, leases, snapshots, copy, and listing.
- Existing object naming, layout, and metadata semantics remain stable for backward compatibility with existing containers.
- The final request-sending mechanism remains unspecified; the feature requires caller-visible boundaries that can be adapted.
- The client may expose domain-specific operations as long as existing behavior can migrate without semantic loss.
- Production validation may require real storage for features where the emulator is incomplete.

## Scope

In Scope:
- Durable object-storage client replacement for current service-owned storage usage.
- Multi-account routing, container operations, page objects, block objects, metadata, leases, snapshots, copy, retry, observability, emulator support, and authentication modes.
- Migration-supporting compatibility tests and low-allocation measurement hooks.

Out of Scope:
- Replacing unrelated database extensions or emulator internals.
- Changing object layout, metadata schema, ownership protocol, or backup format.
- Selecting the final request transport implementation.
- Introducing new storage product features unrelated to current replacement needs.

## Dependencies

- Cloud object-storage service compatibility.
- Existing object layout and metadata conventions from current storage callers.
- Role/epoch source for ownership fencing.
- Emulator and real-account validation infrastructure.
- Existing observability infrastructure for retry and request reporting.

## Risks & Mitigations

- Hidden behavior in existing storage clients may be relied upon. Impact: subtle authentication, retry, metadata, or body-replay differences. Mitigation: operation parity tests and real-storage validation for copy/snapshot/delta-size.
- Low allocation goals may conflict with retryable bodies. Impact: excess copying or unsafe retry. Mitigation: explicit body replay capability and deterministic non-replayable retry failures.
- Ownership/failover semantics may regress. Impact: stale writes or data corruption. Mitigation: deterministic epoch and ownership-race tests with fault injection.
- Emulator behavior may diverge from production. Impact: tests pass but production fails. Mitigation: classify emulator-only deviations and require production smoke tests for unsupported branches.
- Multi-account routing may change object placement. Impact: existing data may become unreadable. Mitigation: golden tests for routing and metadata/internal object placement.
- Error taxonomy may lose diagnostic details. Impact: retry misclassification and weaker supportability. Mitigation: preserve raw status, storage error code, request ID, and selected diagnostic headers.

## References

- Requirements source: provided low-overhead storage-client PRD.
