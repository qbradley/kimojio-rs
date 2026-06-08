# Durable Storage Client Implementation Plan

## Overview

Implement a new low-allocation durable object-storage client on the stackful runtime and HTTP stack. The client will expose storage-domain operations for page data, metadata, ownership fencing, listing, snapshots/copy, archive objects, configuration reads, retry behavior, and observability while making request costs explicit.

Because the specification spans many operation families, implementation is split into incremental vertical slices. Each phase introduces a bounded set of types, operations, and tests while preserving the ability to validate request/response behavior without choosing a final external HTTP library.

## Current State Analysis

- The repository already contains stackful HTTP primitives: `Body`, `BodyLimits`, `BodyBuilder`, `HttpConfig`, protocol-neutral client wrappers, HTTP/1.1 and HTTP/2 clients, `Headers`, `Trailers`, `StackTransport`, and inspectable `ErrorKind`/`LimitKind` (`.paw/work/xstore-client/CodeResearch.md:60-77`, `.paw/work/xstore-client/CodeResearch.md:121-126`).
- The existing HTTP body model is buffered around `bytes::Bytes` and `BytesMut`, and HTTP/1.1/HTTP/2 read paths currently accumulate response bodies into `Body` values (`.paw/work/xstore-client/CodeResearch.md:68-76`, `.paw/work/xstore-client/CodeResearch.md:102-105`).
- `StackTransport` already supports plaintext and optional TLS over connected sockets, delegates I/O to `RuntimeContext`, and exposes `read`, `write`, `write_all`, shutdown, and close behavior (`.paw/work/xstore-client/CodeResearch.md:109-118`).
- `RuntimeContext` exposes stackful socket/connect/read/write/close operations, async owned-buffer I/O, timeouts/cancellation helpers, and waitables that can support cooperative I/O and retry orchestration (`.paw/work/xstore-client/CodeResearch.md:128-160`).
- The recently added stack telemetry crate demonstrates a workspace-member pattern for low-level stackful clients, optional TLS feature wiring, caller-controlled export calls, allocation-count tests, and docs integration (`.paw/work/xstore-client/CodeResearch.md:18-20`, `.paw/work/xstore-client/CodeResearch.md:42-50`).
- The current repository does not contain an existing durable storage client implementation; the PRD references external source paths outside this repository, so compatibility behavior must be encoded from the PRD into tests and interfaces during implementation (`.paw/work/xstore-client/CodeResearch.md:16-20`).

## Desired End State

- A new workspace crate provides low-level durable storage client foundations with inherited workspace metadata and no hidden runtime.
- `kimojio-stack-http` exposes the additional streaming response and caller-owned request-body capabilities needed by storage hot paths.
- The client exposes caller-visible request parameters, deadlines, cancellation, retry policy, replayability, response metadata, and diagnostics.
- Page, metadata, ownership, listing, snapshot/copy, archive, and config-read operations are represented with typed request/response/error surfaces.
- Hot paths support caller-owned or caller-shared buffers, streaming/chunked reads, and explicit allocation/request-count observations.
- Unit and integration tests cover request construction, response parsing, retry classification, body replay, ownership fencing, routing, metadata invariants, listing pagination, range streaming, copy/snapshot behavior, config unchanged behavior, archive integrity behavior, and emulator/fake hooks.
- Compatibility fixtures capture externally defined routing, metadata, error, and caller-environment semantics needed for golden tests.
- Documentation explains supported operation families, cost model, retry/body replay rules, authentication modes, and validation commands.

## What We're NOT Doing

- Changing existing object layout, metadata schema, ownership protocol, or backup format.
- Selecting a final external request transport implementation.
- Replacing unrelated database extensions or emulator internals.
- Adding new storage product features outside current replacement needs.
- Hiding retries, buffering, worker tasks, or background I/O behind implicit behavior.

## Phase Status

- [x] **Phase 1: Crate and Core Model Foundation** - Add the workspace crate, typed storage context, request/response metadata, errors, operation classes, and cost/observability scaffolding.
- [x] **Phase 2: Transport, Body Replay, and Retry Core** - Add request execution abstraction, replayable bodies, cancellation/deadline plumbing, retry policy, and deterministic retry tests.
- [x] **Phase 3: Page Operations and Ownership Fencing** - Implement page object create/range write/range clear/properties/range listing/streaming read, sequence number, ownership lease, and metadata invariant surfaces.
- [x] **Phase 4: Containers, Routing, Listing, and Metadata Operations** - Add multi-account routing, container operations, metadata/property operations, conditional operations, and paged listing.
- [x] **Phase 5: Block Objects, Archive, and Config Flows** - Add block object upload/download, archive integrity flows, and config reload behavior.
- [x] **Phase 6: Snapshot, Copy, and Backup Status Flows** - Add snapshot lifecycle, delta-size, copy-from-source diagnostics, checkpoint/status object updates, and cleanup behavior.
- [x] **Phase 7: Integration, Fault Injection, and Compatibility Harness** - Add emulator/fake hooks, fault injection, parity tests, bounded concurrency tests, caller-environment integration tests, and allocation/request-count comparisons.
- [x] **Phase 8: Documentation** - Add Docs.md, project guide, README link, and final validation record.

## Phase Candidates

No initial candidates.

---

## Phase Dependencies

- Phase 1 establishes crate identity, common models, authentication modes, diagnostics, and errors used by all later phases.
- Phase 2 depends on Phase 1 and supplies the request execution, replay, retry, deadline, cancellation, pooling, and concurrency primitives used by every operation family.
- Phase 3 depends on Phases 1-2 and implements page/ownership behavior before routing/listing so high-value write and failover semantics are validated early.
- Phase 4 depends on Phases 1-3 for object identifiers, ownership context, metadata types, and transport/retry primitives.
- Phase 5 depends on Phases 1-4 for container/object references, metadata, retry, and streaming bodies.
- Phase 6 depends on Phase 5 for snapshot/copy and backup status object coverage.
- Phase 7 depends on all operation phases and adds cross-cutting compatibility, fault-injection, emulator, concurrency, and allocation coverage.
- Phase 8 documents the as-built behavior after all implementation phases complete.

---

## Phase 1: Crate and Core Model Foundation

**Status**: Complete. Added the `kimojio-stack-storage` workspace crate, redacted auth modes, caller-supplied auth provider/per-attempt refresh surface, signed source/key-secret types, stable error categories with diagnostic context, service status/error-code classification including timeout and sequence-number cases, operation classes, attempt diagnostics, request/allocation counts, storage context, pool/concurrency settings, object references, normalized metadata map, built-in compatibility fixture loading including metadata/error/routing/environment data, and TLS feature wiring. Phase validation passed with `cargo test -p kimojio-stack-storage`, `cargo clippy -p kimojio-stack-storage --all-targets -- -D warnings`, `cargo test -p kimojio-stack-storage --all-features`, `cargo clippy -p kimojio-stack-storage --all-targets --all-features -- -D warnings`, and dependency tree checks confirming TLS dependencies only appear with `--all-features`.

### Changes Required:

- **`Cargo.toml`**: Add a new workspace member and workspace dependency entry following existing member/dependency conventions (`.paw/work/xstore-client/CodeResearch.md:42-50`).
- **`kimojio-stack-storage/Cargo.toml`**: Create a stackful durable-storage crate with inherited workspace metadata and dependencies on `kimojio-stack`, `kimojio-stack-http` with TLS feature gating, and `bytes` where shared-buffer inputs are needed (`.paw/work/xstore-client/CodeResearch.md:42-50`, `.paw/work/xstore-client/CodeResearch.md:109-118`).
- **`kimojio-stack-storage/src/lib.rs`**: Define module layout and public re-exports for context, auth, errors, operation class, diagnostics, concurrency/cost metrics, and common object identifiers.
- **`kimojio-stack-storage/src/error.rs`**: Add stable error categories for auth, authorization, not found, already exists, condition failure, ownership/lease, sequence number, range, timeout, unavailable, metadata format, corruption, transport, and unhandled service error. Follow the inspectable error pattern used by stack HTTP (`.paw/work/xstore-client/CodeResearch.md:121-126`).
- **`kimojio-stack-storage/src/auth.rs`**: Define production identity, bearer/access token, key-based signing, signed source reference, and emulator auth modes as caller-visible configuration inputs.
- **`kimojio-stack-storage/src/auth.rs`**: Define caller-supplied auth/signing provider interfaces for identity tokens, bearer/access tokens, key-based signing, signed source references, and emulator auth. Include per-attempt refresh hooks so retry attempts can update authorization without rebuilding immutable request data.
- **`kimojio-stack-storage/src/model.rs`**: Define storage context, account/container/object references, object kind, page/block object properties, metadata map, ownership context, retry context identifiers, diagnostics, operation classes, connection/pooling settings, concurrency settings, and external compatibility fixture references.
- **`kimojio-stack-storage/src/compat.rs`**: Define fixture structures for routing golden cases, metadata key/value expectations, storage status/error-code mappings, and current caller-environment descriptors derived from the requirements source.
- **Tests**: Add unit tests for error classification, auth-mode and auth-provider selection/formatting, per-attempt auth refresh, metadata normalization, operation class formatting, diagnostic preservation, object-reference formatting, connection/concurrency settings, compatibility fixture loading, and feature-gated TLS dependency behavior.

### Success Criteria:

#### Automated Verification:
- [ ] Tests pass: `cargo test -p kimojio-stack-storage`
- [ ] Lint/typecheck: `cargo clippy -p kimojio-stack-storage --all-targets -- -D warnings`
- [ ] Feature check: default build excludes TLS dependencies; `--all-features` includes TLS helpers.

#### Manual Verification:
- [ ] Public types expose storage-domain concepts and caller-visible costs.
- [ ] Error categories cover all storage failure classes from FR-040.
- [ ] Auth modes cover identity, bearer/access token, key-based, signed source, and emulator configurations.
- [ ] Compatibility fixtures cover routing, metadata, error mapping, and caller-environment inputs needed by later phases.

---

## Phase 2: Transport, Body Replay, and Retry Core

**Status**: Complete. Added `kimojio-stack-http` incremental HTTP/1.1 body chunk readers, bounded HTTP/1.1 suppressed-body draining for large chunked error bodies, split HTTP/1.1 head/body request writes, header-aware response streaming that suppresses error bodies from success sinks while preserving service-error classification, HTTP/2 response chunk callbacks, and HTTP/2 DATA writes from `Bytes` slices without forced per-chunk copy. Added `ReplayBody`/`BodyReplay` request body replayability types with owned/shared payload replay through `Bytes`, `RequestParts`/`ResponseParts`, `Transport` abstraction, `StackHttpTransport` adapter over the protocol-neutral HTTP client, failure diagnostics, transport-local absolute deadline plumbing for connected plaintext reads/writes, deterministic caller-supplied retry jitter configuration, `RetryPolicy`/`RetryDecision`/`RetryObservation`/`RetryState`, `ConcurrencyLimiter`, fake transport tests, replayable/non-replayable body tests, Retry-After/max-attempt/cancellation/budget behavior, request part body preservation, invalid request rejection, idle expiry, deadline locality, and diagnostics-preserving response/error tests. Phase validation passed with `cargo test -p kimojio-stack-http`, `cargo clippy -p kimojio-stack-http --all-targets -- -D warnings`, `cargo test -p kimojio-stack-storage`, `cargo clippy -p kimojio-stack-storage --all-targets -- -D warnings`, `cargo test -p kimojio-stack-storage --all-features`, and `cargo clippy -p kimojio-stack-storage --all-targets --all-features -- -D warnings`.

### Changes Required:

- **`kimojio-stack-http/src/body.rs` and protocol clients**: Add incremental response-body consumption support so callers can receive chunks before the full body is buffered. Cover HTTP/1.1 fixed/chunked/EOF bodies and HTTP/2 DATA frames with bounded memory, preserving existing buffered APIs for current users (`.paw/work/xstore-client/CodeResearch.md:68-76`, `.paw/work/xstore-client/CodeResearch.md:102-105`).
- **`kimojio-stack-http/src/h2/client.rs` and related frame/body plumbing**: Add caller-owned or shared request-body write support for HTTP/2 DATA frames without forced `Bytes::copy_from_slice` on hot upload paths, preserving current behavior for ordinary buffered bodies (`.paw/work/xstore-client/CodeResearch.md:100-104`).
- **`kimojio-stack-http/src/http1/codec.rs` and body write path**: Add a request-body path that writes caller-owned or shared payload buffers without first serializing payload bytes into one large request buffer, preserving existing request-writing behavior for buffered/simple calls (`.paw/work/xstore-client/CodeResearch.md:76-84`).
- **`kimojio-stack-storage/src/transport.rs`**: Add caller-visible request/response execution adapters over the expanded HTTP stack that expose operation inputs, cancellation, status, metadata, diagnostics, streaming chunks, and absolute deadlines for already-connected plaintext transports. Connection establishment and TLS handshake timeout enforcement are explicitly deferred until the later phase that introduces endpoint dialing/TLS construction (`.paw/work/xstore-client/CodeResearch.md:60-77`, `.paw/work/xstore-client/CodeResearch.md:109-118`).
- **`kimojio-stack-storage/src/body.rs`**: Add replayable body variants for borrowed slices, shared bytes, owned buffers, and non-replayable streams; expose retry eligibility explicitly.
- **`kimojio-stack-storage/src/pool.rs`**: Add caller-visible connection reuse, keepalive, idle duration, total operation timeout, per-destination idle capacity, and client/caller concurrency controls. Preserve connection establishment timeout configuration for the later dialing phase rather than enforcing it before a connector exists.
- **`kimojio-stack-storage/src/retry.rs`**: Add retry policy, attempt classification, Retry-After handling, backoff/jitter configuration, cancellation/deadline checks, and retry observations.
- **`kimojio-stack-storage/src/diagnostics.rs`**: Preserve request ID, storage error code/message, status, elapsed time, attempt number, terminal/retriable classification, and selected diagnostic metadata.
- **Tests**: Add HTTP stack tests for incremental response chunks, bounded response memory, HTTP/2 caller-owned DATA writes, and HTTP/1.1 request payload writes without full-payload serialization. Add fake/loopback transport tests for non-replayable body retry rejection, replayable body reset, Retry-After handling, cancellation before retry, total timeout, error-body suppression, pooling/idle settings, bounded concurrency, transient/terminal classification, per-attempt auth refresh, and immutable request part reuse. Connection establishment timeout and TLS handshake timeout/failure tests are deferred to the dialing/TLS-construction phase.

### Success Criteria:

#### Automated Verification:
- [ ] Tests pass: `cargo test -p kimojio-stack-storage retry`
- [ ] Lint/typecheck: `cargo clippy -p kimojio-stack-storage --all-targets -- -D warnings`

#### Manual Verification:
- [ ] Retry logic never replays a consumed non-replayable body.
- [ ] Diagnostics are preserved on every failed attempt.
- [ ] Connection reuse and concurrency settings are observable and testable.
- [ ] Large responses can be consumed incrementally without full-body buffering.
- [ ] Hot upload payloads avoid client-attributable payload-byte copies in HTTP/1.1 and HTTP/2 write paths.

---

## Phase 3: Page Operations and Ownership Fencing

**Status**: Complete. Added typed metadata parsing/formatting for LSNs, relation size, deletion/truncation markers, shard config, upload LSN, backup time, and ownership epoch. Added ownership lease request builders for acquire/renew/change/break flows with explicit empty request lengths, explicit leased metadata-update requests that preserve existing metadata while persisting ownership epochs, epoch validation, and ownership-aware write failure classification with caller-retry/reacquire/terminal outcomes plus acquire/break/update race coverage. Added page operation builders and execution helpers for create-if-absent page object creation, explicit page-write request content lengths, aligned range upload/clear with optional sequence equality conditions, properties parsing, sequence-number update/increment and overflow recovery classification, ambiguous page-write recovery classification, written-range listing/coalescing, streaming range reads, caller sink error preservation, and typed incomplete failures after partial transport delivery. Phase validation passed with `cargo test -p kimojio-stack-storage`, `cargo clippy -p kimojio-stack-storage --all-targets -- -D warnings`, and `cargo clippy -p kimojio-stack-storage --all-targets --all-features -- -D warnings`.

### Changes Required:

- **`kimojio-stack-storage/src/page.rs`**: Add page object create, aligned range upload, range clear, properties, written-range listing/coalescing, sequence-number update/increment, and streaming ranged read operations.
- **`kimojio-stack-storage/src/ownership.rs`**: Add ownership lease acquire/break/validate/update flows with epoch comparison and stale-epoch error handling.
- **`kimojio-stack-storage/src/ownership.rs`**: Add ownership-aware write wrapper behavior that detects ownership loss/mismatch, optionally reacquires ownership, and returns a caller-retry signal when safe.
- **`kimojio-stack-storage/src/metadata.rs`**: Add typed metadata parsing/formatting for high-water LSN, oldest replay LSN, relation size, deletion/truncation markers, shard config, upload LSN, backup time, and ownership epoch.
- **Tests**: Add fake transport tests for create-if-absent, client-side aligned and unaligned range validation, metadata ordering before writes, ambiguous write error classification, sequence-number overflow recovery, ownership race outcomes, ownership-aware write wrapper retry/reacquire outcomes, stale-epoch terminal behavior, missing/malformed metadata, streaming range reads into caller sinks, and partial-data-then-transient range download behavior that either resumes from a safe boundary or returns a typed incomplete failure.

### Success Criteria:

#### Automated Verification:
- [ ] Tests pass: `cargo test -p kimojio-stack-storage`
- [ ] Lint/typecheck: `cargo clippy -p kimojio-stack-storage --all-targets -- -D warnings`

#### Manual Verification:
- [ ] Ownership and epoch behavior prevents stale writes in simulated races.
- [ ] Ownership-aware write wrapper requests caller retry only for safe reacquire/retry outcomes.
- [ ] Page upload path accepts caller-owned/shared payloads without extra payload-byte copies attributable solely to the client.
- [ ] Partial range download failures after delivered bytes are retried from safe boundaries or reported as typed incomplete failures.

---

## Phase 4: Containers, Routing, Listing, and Metadata Operations

**Status**: Complete. Added reusable ETag conditions, deterministic primary/data account routing with compatibility-fixture golden tests, container create/delete/exists operations with idempotent already-exists/not-found handling and distinct being-deleted/auth failures, metadata/property request helpers that avoid content transfer and preserve caller metadata, conditional object deletes, tenant initialization helpers with durable initialized-marker probing, explicit metadata object creation when missing, idempotent verification of already-created page objects on retry, and final-marker persistence only after page/object initialization succeeds, and paged listing with prefix/marker/include options, metadata/snapshot parsing, object-kind filtering that forces metadata inclusion, continuation streaming, retry-aware page fetches, merge/dedup, explicit retryability classification, and metadata/list request-count assertions. Phase validation passed with `cargo test -p kimojio-stack-storage`, `cargo clippy -p kimojio-stack-storage --all-targets -- -D warnings`, and `cargo clippy -p kimojio-stack-storage --all-targets --all-features -- -D warnings`.

### Changes Required:

- **`kimojio-stack-storage/src/routing.rs`**: Add deterministic primary-account and data-account routing with golden tests for object placement.
- **`kimojio-stack-storage/src/container.rs`**: Add create/delete/exists operations across configured accounts, with idempotent already-exists behavior and distinct being-deleted/auth failures.
- **`kimojio-stack-storage/src/properties.rs`**: Add HEAD/properties and metadata-set operations that avoid content transfer, preserve caller metadata maps, and support optional ownership ID.
- **`kimojio-stack-storage/src/tenant_init.rs`**: Add tenant/container initialization helpers that create containers, initialize metadata/page objects, mark initialized state, and reject reinitialization.
- **`kimojio-stack-storage/src/conditions.rs`**: Add reusable ETag/If-Match/If-None-Match condition types for metadata/property operations and conditional deletes.
- **`kimojio-stack-storage/src/list.rs`**: Add paged listing by prefix with metadata, object type filtering, snapshots, continuation markers, streaming pages, multi-account merge/dedup views, and retryable page fetching.
- **Tests**: Add routing golden tests from compatibility fixtures, container idempotency tests, tenant initialization and reinitialization rejection tests, conditional metadata/property tests, conditional delete tests, ETag/If-Match and If-None-Match tests, paged listing tests, continuation-marker failure tests, duplicate/dedup tests, and allocation/request-count checks for metadata/list hot paths.

### Success Criteria:

#### Automated Verification:
- [ ] Tests pass: `cargo test -p kimojio-stack-storage`
- [ ] Lint/typecheck: `cargo clippy -p kimojio-stack-storage --all-targets -- -D warnings`

#### Manual Verification:
- [ ] Listing memory is bounded by configured page size plus caller buffers.
- [ ] Metadata/property operations do not transfer object content.
- [ ] Continuation-marker failures return typed listing errors without dropping already delivered pages.

---

## Phase 5: Block Objects, Archive, and Config Flows

**Status**: Complete. Added block object upload and upload-if-not-exists request paths with replayable bodies, metadata, conditions, optional lease ownership, streaming downloads, range-chunked full-object reads, bounded small-object collection, and base/snapshot delete helpers with idempotent not-found behavior. Added archive upload/download helpers with CRC32 metadata/validation, compression integration metadata, fallback-prefix candidate construction, and streaming sink validation. Added config reader support for JSON object-root validation, non-empty ETag tracking, unchanged reload handling, and missing-object mapping. Phase validation passed with `cargo test -p kimojio-stack-storage`, `cargo clippy -p kimojio-stack-storage --all-targets -- -D warnings`, and `cargo clippy -p kimojio-stack-storage --all-targets --all-features -- -D warnings`.

### Changes Required:

- **`kimojio-stack-storage/src/block.rs`**: Add block object upload, upload-if-not-exists, streaming download using range chunking where needed, explicit small-object collection, delete base/snapshot, optional ownership ID, and conditional delete support.
- **`kimojio-stack-storage/src/archive.rs`**: Add archive-oriented upload/download helpers with CRC metadata validation, compression/decompression integration points, prefix fallback, and file-sink streaming.
- **`kimojio-stack-storage/src/config_reader.rs`**: Add JSON config reads, ETag tracking, unchanged reload handling, missing-object mapping, and object-root JSON validation.
- **Tests**: Add block upload/download/delete tests, not-found-idempotent base-object delete tests, conditional delete tests, upload-if-not-exists tests, archive upload/download/CRC/compression/prefix tests, config unchanged/not-found tests, full-object range-chunked streaming tests, and small-object collection vs streaming tests.

### Success Criteria:

#### Automated Verification:
- [ ] Tests pass: `cargo test -p kimojio-stack-storage`
- [ ] Lint/typecheck: `cargo clippy -p kimojio-stack-storage --all-targets -- -D warnings`

#### Manual Verification:
- [ ] Archive and config paths can be validated without full content buffering unless explicitly requested.
- [ ] Archive and config integrity/unchanged behaviors are covered.
- [ ] Base object delete and snapshot delete idempotency are both covered.

---

## Phase 6: Snapshot, Copy, and Backup Status Flows

**Status**: Complete. Added snapshot create/delete/properties/list request helpers with encoded list prefixes, XML entity decoding for listed snapshot object names, snapshot source-reference construction, delta-size parsing/polling that treats missing or zero sizes as pending, and list-item snapshot conversion. Added copy-from-source request helpers with optional source authorization, condition support, accepted-status validation enforced by the client, and response/error diagnostic extraction. Added backup checkpoint/status JSON update and readback helpers over block objects. Added orphan snapshot discovery using protected snapshot state and bounded delete execution. Phase validation passed with `cargo test -p kimojio-stack-storage`, `cargo clippy -p kimojio-stack-storage --all-targets -- -D warnings`, and `cargo clippy -p kimojio-stack-storage --all-targets --all-features -- -D warnings`.

### Changes Required:

- **`kimojio-stack-storage/src/snapshot.rs`**: Add snapshot create/delete/properties/list, delta-size polling behavior, and source-reference construction.
- **`kimojio-stack-storage/src/copy.rs`**: Add copy-from-source operations with optional source authorization and diagnostic capture for request/copy metadata.
- **`kimojio-stack-storage/src/backup_status.rs`**: Add checkpoint/status block-object update and readback operations used by backup/restore progress tracking.
- **`kimojio-stack-storage/src/snapshot_cleanup.rs`**: Add orphan snapshot discovery using listed snapshots plus known backup/checkpoint state and bounded-delete execution.
- **Tests**: Add snapshot lifecycle tests, delta-size pending/available tests, copy accepted/error diagnostic tests, checkpoint/status create/update/read tests, and cleanup/orphan snapshot classification plus bounded-delete tests.

### Success Criteria:

#### Automated Verification:
- [ ] Tests pass: `cargo test -p kimojio-stack-storage`
- [ ] Lint/typecheck: `cargo clippy -p kimojio-stack-storage --all-targets -- -D warnings`

#### Manual Verification:
- [ ] Snapshot/copy diagnostics remain available on failures.
- [ ] Backup checkpoint/status objects are created, updated, and read back.
- [ ] Orphan snapshot cleanup identifies candidates and deletes them with bounded concurrency.

---

## Phase 7: Integration, Fault Injection, and Compatibility Harness

**Status**: Complete. Added a public fake-service testing harness with deterministic emulator descriptors, operation-correct fault-injection diagnostics, request recording, and retry observations. Added default-safe integration tests for compatibility mappings/routing and representative operation-family semantics including initialization, backup/status, snapshot/delta, copy, archive, config, and conditional delete paths; a live Azurite emulator gate using explicit socket/account/key environment variables; real-storage request-surface gates; caller-environment modes; measured allocation/request-count comparisons across page upload, metadata, range download, list, and retry paths; and bounded concurrency. External service tests skip by default and identify required environment variables; live real-storage execution is deferred until a connector/dialer phase exists, while request-surface smoke coverage for copy/snapshot/delta is present. Phase validation passed with `cargo test -p kimojio-stack-storage`, `cargo clippy -p kimojio-stack-storage --all-targets -- -D warnings`, and `cargo clippy -p kimojio-stack-storage --all-targets --all-features -- -D warnings`.

### Changes Required:

- **`kimojio-stack-storage/src/testing.rs` or `tests/support/`**: Add fake service hooks for fault injection, retry observation, ownership-race tests, and deterministic emulator behavior.
- **`kimojio-stack-storage/tests/compatibility.rs`**: Add compatibility scenarios covering all operation families from the spec success criteria.
- **`kimojio-stack-storage/tests/emulator.rs`**: Add gated emulator-backed tests for supported local behavior and explicit skip/fail behavior for unavailable emulator infrastructure.
- **`kimojio-stack-storage/tests/real_storage.rs`**: Add gated real-storage smoke tests for emulator gaps such as copy, snapshot, and delta-size behavior. Until this crate has an endpoint connector/dialer, keep the real-storage gate explicit and validate request surfaces plus required credentials/account; live execution is deferred to the connector phase. Tests must be explicit about required credentials/account and skip or fail according to the documented gate.
- **`kimojio-stack-storage/tests/integration_modes.rs`**: Add coverage for both current caller environments so storage semantics remain unchanged.
- **`kimojio-stack-storage/tests/allocation.rs`**: Add allocation/request-count comparisons for page upload, metadata operations, range download, listing, and retry loops.
- **Tests**: Add bounded concurrency tests for caller/client limits and per-operation semaphores.

### Success Criteria:

#### Automated Verification:
- [ ] Default tests pass without external services: `cargo test -p kimojio-stack-storage`
- [ ] Emulator-backed tests pass when explicitly enabled with documented environment setup.
- [ ] Real-storage smoke tests pass when explicitly enabled with documented environment setup.
- [ ] Lint/typecheck: `cargo clippy -p kimojio-stack-storage --all-targets -- -D warnings`

#### Manual Verification:
- [ ] Compatibility harness maps every spec success criterion to at least one test group.
- [ ] External service tests identify whether they were skipped or executed.

---

## Phase 8: Documentation

**Status**: Complete. Added `.paw/work/xstore-client/Docs.md` as the as-built technical reference, added `docs/stack-storage.md` as the public guide, and linked it from `README.md` near the stackful networking guides. Final validation passed with `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-targets --all-features`, and `cargo test --doc`.

### Changes Required:

- **`.paw/work/xstore-client/Docs.md`**: Create an as-built technical reference using `paw-docs-guidance`.
- **`docs/stack-storage.md`**: Add a public guide following existing Markdown guide style (`.paw/work/xstore-client/CodeResearch.md:22-29`).
- **`README.md`**: Add a concise link near stackful networking guides.
- **Project docs verification**: Run doc tests and check links.
- **Final validation record**: Update this plan and Docs.md with final command results.

### Success Criteria:

#### Automated Verification:
- [ ] Docs verification: `cargo test --doc`
- [ ] Full validation: `cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo clippy --all-targets --all-features -- -D warnings && cargo test --all-targets --features ssl2 && cargo test --all-targets --features virtual-clock && cargo test --doc`

#### Manual Verification:
- [ ] Documentation clearly states supported operation families, retry/body replay rules, allocation behavior, authentication modes, emulator behavior, and limitations.
- [ ] Documentation avoids upstream product naming.
- [ ] Documentation explains external compatibility fixtures, emulator gates, and real-storage validation gates.

---

## References

- Issue: none
- Spec: `.paw/work/xstore-client/Spec.md`
- Research: `.paw/work/xstore-client/CodeResearch.md`
