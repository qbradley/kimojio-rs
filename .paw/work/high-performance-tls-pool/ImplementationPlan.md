# High Performance TLS Pool Implementation Plan

## Overview

Implement a runtime-independent TLS pool crate that layers on the OpenSSL crate and provides callback-completing TLS stream operations. The pool will start with explicit immediate, background-only, and adaptive placement modes, observable scheduling statistics, and RPC-shaped tests/benchmarks that reveal latency behavior across small, medium, and near-maximum messages.

The approach is intentionally separate from existing kimojio runtime TLS wrappers. The repository already has a low-level C-backed OpenSSL integration crate and kimojio-specific TLS wrappers; this plan adds a new workspace crate for regular threaded callers while reusing existing workspace dependencies and benchmark conventions.

## Current State Analysis

- The workspace currently contains `kimojio`, `kimojio-macros`, `kimojio-tls`, `perf`, and `examples`; adding a new crate requires updating the root workspace member list and workspace dependency table as needed (`Cargo.toml:1-11`, `Cargo.toml:21-61`; CodeResearch.md).
- Existing TLS support is tied either to the C-backed `kimojio-tls` crate or to runtime-specific wrappers under `kimojio`; `kimojio-tls` compiles a C wrapper and links dynamic `ssl`/`crypto` (`kimojio-tls/Cargo.toml:1-16`, `kimojio-tls/build.rs:3-12`; CodeResearch.md).
- `kimojio` gates TLS wrappers behind runtime features and exposes TLS context/stream modules through feature-gated public modules (`kimojio/Cargo.toml:34-52`, `kimojio/src/lib.rs:41-51`; CodeResearch.md).
- Existing TLS tests generate OpenSSL contexts/certificates and exercise client/server data exchange over connected transports, but those helpers are runtime-coupled and test-private; the new crate will reimplement only the OpenSSL context/certificate setup pattern in standalone synchronous test utilities (`kimojio/src/tlscontext.rs:852-940`, `kimojio/src/tlscontext.rs:1071-1170`, `kimojio/src/ssl2/mod.rs:300-467`; CodeResearch.md).
- The new crate will use the Rust OpenSSL crate directly for blocking TLS streams over standard connected streams; this is a new runtime-independent pattern in this workspace rather than reuse of the existing runtime wrappers (`kimojio/src/tlscontext.rs:18-58`, `kimojio/src/tlsstream.rs:24-125`, `kimojio/src/ssl2/mod.rs:24-39`; CodeResearch.md).
- Existing Criterion benches use `iter_custom`, registered `[[bench]]` targets, and one-second warmup/four-second measurement windows (`kimojio/Cargo.toml:63-77`, `kimojio/benches/async_stream_bench.rs:125-130`; CodeResearch.md).
- Documentation is plain Markdown plus Rustdoc/docs.rs, with README-linked docs and CI doc tests (`README.md:79-81`, `.github/workflows/rust.yml:43-44`; CodeResearch.md).

## Desired End State

- A new workspace crate provides a runtime-independent TLS pool and TLS stream handle API over OpenSSL.
- Users can create client/server TLS streams and submit read/write operations with exactly-once callbacks.
- The pool can run operations immediately, always in the background, or adaptively based on message size and estimated executor load.
- Same-stream operations are serialized so TLS state is not accessed concurrently; adaptive routing may move a stream between executors over time, but it cannot run two operations for the same stream at once.
- Statistics expose submissions, immediate/background routing, queueing, completions, failures, and per-executor activity.
- Tests validate handshake, RPC write/read exchange, callback completion, same-stream serialization, and policy routing.
- Benchmarks cover single client/server and three-pair RPC write scenarios with small, medium, and near-maximum bodies.
- Docs describe the public API, callback threading behavior, scheduling modes, statistics, benchmark commands, and current limitations.

## What We're NOT Doing

- No tokio integration.
- No kimojio or kimojio-stack integration.
- No kernel TLS or hardware TLS offload.
- No full replacement of OpenSSL internals.
- No guarantee that adaptive scheduling is globally optimal in the first implementation.
- No non-blocking socket readiness integration in the first implementation unless required for correctness.

## Phase Status

- [x] **Phase 1: Crate Skeleton and Public Types** - Add the runtime-independent crate, API surface, configuration, and basic documentation comments.
- [x] **Phase 2: Pool Executor and Callback Operations** - Implement worker lifecycle, operation submission, callback completion, same-stream serialization, and statistics.
- [x] **Phase 3: TLS Handshake and RPC Tests** - Implement client/server stream construction and correctness tests for RPC-shaped TLS exchange.
- [ ] **Phase 4: Adaptive Scheduling and Benchmarks** - Add adaptive placement bookkeeping plus Criterion RPC write benchmarks for single-pair and three-pair scenarios.
- [ ] **Phase 5: Documentation** - Add workflow Docs.md and project documentation for the new crate and benchmark usage.

## Phase Dependencies

- Phase 2 depends on Phase 1 public types and configuration.
- Phase 3 depends on Phase 2 operation submission and callback completion.
- Phase 4 depends on Phase 3 correctness tests and Phase 2 statistics.
- Phase 5 depends on all implementation and benchmark phases.

## Phase Candidates

---

## Phase 1: Crate Skeleton and Public Types

### Changes Required:

- **`Cargo.toml`**: Add a new workspace member for the TLS pool crate, following the existing workspace member pattern (`Cargo.toml:1-11`; CodeResearch.md).
- **`kimojio-tls-pool/Cargo.toml`**: Create a crate manifest using workspace package metadata and dependencies on OpenSSL, Criterion, and test certificate helpers as needed, following existing crate manifest style (`kimojio-tls/Cargo.toml:1-16`, `kimojio/Cargo.toml:58-77`; CodeResearch.md).
- **`kimojio-tls-pool/src/lib.rs`**: Define the public module structure and public types for pool configuration, stream handles, operation results, callback type aliases, placement policy, and statistics.
- **`kimojio-tls-pool/src/error.rs`**: Define crate-local error and result types that represent TLS, I/O, shutdown, and policy/serialization failures.
- **`kimojio-tls-pool/src/config.rs`**: Define pool configuration for executor count, ready executor count, idle transition behavior, immediate/background policy mode, and size thresholds.
- **`kimojio-tls-pool/src/config.rs` tests**: Add unit tests for configuration validation and default values.
- **`kimojio-tls-pool/src/stats.rs` tests**: Add unit tests for statistics construction and counter snapshots.

### Success Criteria:

#### Automated Verification:
- [ ] Tests pass: `cargo test -p kimojio-tls-pool --quiet`
- [ ] Format: `cargo fmt --all -- --check`
- [ ] Lint/typecheck: `cargo clippy -p kimojio-tls-pool --all-targets --quiet`

#### Manual Verification:
- [ ] Public API names reflect the spec entities: pool, stream handle, operation result, executor statistics, and placement policy.
- [ ] The new crate does not depend on kimojio or another async runtime.

---

## Phase 2: Pool Executor and Callback Operations

### Changes Required:

- **`kimojio-tls-pool/src/pool.rs`**: Implement pool construction, worker thread lifecycle, shutdown, hot/ready executor behavior, queue selection, and shared statistics.
- **`kimojio-tls-pool/src/stream.rs`**: Implement stream handle state, same-stream operation serialization, and submission entry points for read and write operations.
- **`kimojio-tls-pool/src/operation.rs`**: Represent read/write work items, callback completion, and exactly-once completion behavior.
- **`kimojio-tls-pool/src/stats.rs`**: Track submitted, immediate, background-routed, queued, completed, failed, and per-executor counters.
- **`kimojio-tls-pool/src/pool.rs` tests**: Add unit tests using lightweight mock operations for immediate mode, background-only mode, ready/idle executor behavior, load-based executor selection, statistics, and shutdown behavior.
- **`kimojio-tls-pool/src/stream.rs` tests**: Add unit tests for same-stream serialization and callback exactly-once behavior.
- **`kimojio-tls-pool/src/stats.rs` tests**: Add concurrent statistics tests that read snapshots during multi-threaded submissions and verify monotonic submitted/completed/failed/queued accounting.

### Success Criteria:

#### Automated Verification:
- [ ] Tests pass: `cargo test -p kimojio-tls-pool --quiet`
- [ ] Format: `cargo fmt --all -- --check`
- [ ] Lint/typecheck: `cargo clippy -p kimojio-tls-pool --all-targets --quiet`

#### Manual Verification:
- [ ] Callbacks are invoked on the caller thread for immediate operations and on executor threads for background operations.
- [ ] Same-stream submissions are ordered and do not execute concurrently.
- [ ] Pool shutdown completes or rejects outstanding operations with explicit results.

---

## Phase 3: TLS Handshake and RPC Tests

### Changes Required:

- **`kimojio-tls-pool/src/tls.rs`**: Add OpenSSL client/server stream creation, handshake handling, and synchronous read/write execution used by operation workers.
- **`kimojio-tls-pool/src/stream.rs`**: Connect stream handle operations to OpenSSL-backed TLS work rather than mock operations.
- **`kimojio-tls-pool/tests/support.rs`**: Add standalone synchronous test utilities for generated certificates, OpenSSL client/server contexts, connected stream pairs, and RPC payload helpers. Reimplement only the certificate/context setup concepts from existing tests and do not depend on kimojio runtime wrappers or test macros (`kimojio/src/tlscontext.rs:852-940`, `kimojio/src/tlscontext.rs:1071-1170`; CodeResearch.md).
- **`kimojio-tls-pool/tests/rpc_write.rs`**: Add integration tests for connected client/server pairs using the standalone support utilities.
- **`kimojio-tls-pool/tests/rpc_write.rs`**: Cover single client/server threads, three clients/three servers, small replies, medium bodies, and near-maximum 32 KiB bodies.
- **`kimojio-tls-pool/tests/rpc_write.rs`**: Verify successful request header/body/response exchange, callback exactly-once completion, and mandatory surfaced TLS/I/O error paths.
- **`kimojio-tls-pool/tests/same_stream.rs`**: Add TLS-backed same-stream concurrency tests for immediate and background-only modes, asserting ordered execution, exactly-once callbacks, successful data exchange, and no overlapping TLS-state execution.

### Success Criteria:

#### Automated Verification:
- [ ] Tests pass: `cargo test -p kimojio-tls-pool --quiet`
- [ ] Format: `cargo fmt --all -- --check`
- [ ] Lint/typecheck: `cargo clippy -p kimojio-tls-pool --all-targets --quiet`

#### Manual Verification:
- [ ] RPC-shaped TLS exchanges pass in immediate-only and background-only modes.
- [ ] Test fixtures do not require kimojio runtime features.

---

## Phase 4: Adaptive Scheduling and Benchmarks

### Changes Required:

- **`kimojio-tls-pool/src/policy.rs`**: Implement adaptive placement model using operation size, configured thresholds, estimated executor queue cost, and per-executor load bookkeeping.
- **`kimojio-tls-pool/src/pool.rs`**: Integrate policy decisions with executor selection and readiness behavior.
- **`kimojio-tls-pool/benches/rpc_write.rs`**: Add Criterion benchmark target using repository Criterion conventions (`kimojio/Cargo.toml:63-77`, `kimojio/benches/async_stream_bench.rs:125-130`; CodeResearch.md).
- **`kimojio-tls-pool/Cargo.toml`**: Register the `rpc_write` benchmark with `harness = false`, following existing benchmark manifests (`kimojio/Cargo.toml:63-77`; CodeResearch.md).
- **Benchmarks**: Include single client/server thread and three-client/three-server scenarios where both client and server sides use the pool, and compare immediate-only, background-only, and adaptive modes for 4 KiB, 8 KiB, 16 KiB, 24 KiB, and 32 KiB bodies.
- **`kimojio-tls-pool/src/policy.rs` tests**: Add deterministic policy unit tests that assert small operations choose immediate placement under default adaptive configuration, near-maximum operations are eligible for background routing, load estimates select the lower estimated completion path, and insufficient load uses fewer executors.
- **`kimojio-tls-pool/tests/same_stream.rs`**: Extend TLS-backed same-stream concurrency tests to adaptive mode, asserting adaptive routing never violates the one-in-flight-per-stream invariant.

### Success Criteria:

#### Automated Verification:
- [ ] Tests pass: `cargo test -p kimojio-tls-pool --quiet`
- [ ] Benchmark smoke test: `cargo bench -p kimojio-tls-pool --bench rpc_write -- --test`
- [ ] Format: `cargo fmt --all -- --check`
- [ ] Lint/typecheck: `cargo clippy -p kimojio-tls-pool --all-targets --quiet`

#### Manual Verification:
- [ ] Benchmark output reports median, p95, and p99 latency for all required body-size bands and modes.
- [ ] Statistics distinguish immediate-only, background-only, and adaptive routing decisions.

---

## Phase 5: Documentation

### Changes Required:

- **`.paw/work/high-performance-tls-pool/Docs.md`**: Add an as-built technical reference using the PAW documentation guidance.
- **`kimojio-tls-pool/README.md`**: Add user-facing crate documentation covering pool construction, stream creation, callbacks, scheduling modes, statistics, and benchmark commands.
- **Root `README.md`**: Add a short workspace/crate reference if the new crate is intended to be discoverable from the project overview, following the existing plain Markdown style (`README.md:1-7`, `README.md:79-81`; CodeResearch.md).
- **Rustdoc**: Ensure public types and configuration fields have concise doc comments consistent with docs.rs usage (`kimojio/Cargo.toml:54-56`; CodeResearch.md).

### Success Criteria:

#### Automated Verification:
- [ ] Documentation tests/build: `cargo test --doc --quiet`
- [ ] Tests pass: `cargo test -p kimojio-tls-pool --quiet`
- [ ] Format: `cargo fmt --all -- --check`
- [ ] Lint/typecheck: `cargo clippy --all-targets --all-features --quiet`

#### Manual Verification:
- [ ] Docs explain callback threading behavior and same-stream serialization.
- [ ] Docs identify immediate-only, background-only, and adaptive policy modes.
- [ ] Docs list benchmark commands and current known limitations.

---

## References

- Issue: none
- Spec: `.paw/work/high-performance-tls-pool/Spec.md`
- Research: `.paw/work/high-performance-tls-pool/CodeResearch.md`
