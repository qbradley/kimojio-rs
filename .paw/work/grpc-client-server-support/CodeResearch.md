---
date: 2026-06-07T03:32:42.075+00:00
git_commit: "da9113bbfd7d402e16c2a29e105814e6f3c55dc5 (git HEAD); jj @ 51c6f07b1823acae67172af39352c1688f1846de"
branch: "detached HEAD; jj working copy @ pqwryuvn / 51c6f07b1823acae67172af39352c1688f1846de"
repository: kimojio-rs
topic: "gRPC client/server support code research"
tags: [research, codebase, grpc, http, kimojio-stack, tls]
status: complete
last_updated: 2026-06-07
---

# Research: gRPC Client/Server Support

## Research Question

Map the existing repository structure, stackful runtime I/O APIs, TLS integration, tests, benchmarks, documentation, and dependency inventory needed to plan new `kimojio-stack-http` and `kimojio-stack-grpc` crates. The target feature is HTTP/1.1 and HTTP/2 client/server transport with TLS/plaintext where practical, unary-first gRPC with prost-compatible low-level APIs, tokio-ecosystem interoperability tests, and the repository's low-overhead/predictable-latency philosophy (`.paw/work/grpc-client-server-support/Spec.md:6-14`, `.paw/work/grpc-client-server-support/Spec.md:95-116`, `.paw/work/grpc-client-server-support/Spec.md:131-138`).

## Summary

- The repository is a Cargo workspace with resolver `3` and current members `kimojio`, `kimojio-macros`, `kimojio-stack`, `kimojio-stack-tls`, `kimojio-tls`, `perf`, and `examples` (`Cargo.toml:1-13`). Workspace-level package metadata and dependency versions are centralized in the root manifest (`Cargo.toml:15-58`), while member crates reference workspace dependencies through `.workspace = true` (`kimojio-stack/Cargo.toml:11-17`, `kimojio-stack-tls/Cargo.toml:11-18`).
- `kimojio-stack` is an intentionally non-async, stackful, single-threaded cooperative runtime whose `Runtime::block_on` supplies a `RuntimeContext`; scoped coroutines are created with `RuntimeContext::scope` and `Scope::spawn` (`kimojio-stack/src/lib.rs:4-24`, `kimojio-stack/src/lib.rs:428-452`, `kimojio-stack/src/lib.rs:461-493`, `kimojio-stack/src/lib.rs:1781-1809`).
- `RuntimeContext` already exposes socket setup, accept/connect, read/write/readv/writev, send/recv/sendmsg/recvmsg, shutdown, close, async owned-buffer I/O, heterogeneous waits, and fixed fd/buffer registration (`kimojio-stack/src/lib.rs:1053-1122`, `kimojio-stack/src/lib.rs:1122-1420`, `kimojio-stack/src/lib.rs:1509-1530`, `kimojio-stack/src/wait.rs:52-160`, `kimojio-stack/src/lib.rs:542-559`).
- `kimojio-stack-tls` adapts OpenSSL memory-BIO TLS to `kimojio-stack` by filling OpenSSL read BIO buffers with `RuntimeContext::read` and draining OpenSSL write BIO buffers with `RuntimeContext::write`; its public API wraps `SslContext` as `TlsContext` and returns concrete `TlsStream` values over `OwnedFd` sockets (`kimojio-stack-tls/src/lib.rs:4-12`, `kimojio-stack-tls/src/lib.rs:21-65`, `kimojio-stack-tls/src/lib.rs:196-223`).
- Tests and measurements are primarily Rust unit tests, one `kimojio/tests` integration test tree, Criterion benches, CI cargo commands, and test-only allocation counters (`kimojio/tests/virtual_clock_integration.rs:1-10`, `kimojio-stack/benches/runtime_baseline.rs:27-34`, `kimojio-stack/src/lib.rs:3142-3183`, `.github/workflows/rust.yml:34-50`).
- Current internal manifests and lockfile include reusable building blocks such as `futures`, `openssl`, `rcgen`, `criterion`, `rustix`, `rustix-uring`, `zerocopy`, and `mimalloc`, but the root workspace dependency list does not currently declare `bytes`, `http`, `h2`, `httparse`, `httpdate`, `prost`, `hpack`, `hyper`, `tonic`, or `tokio` (`Cargo.toml:23-58`, `Cargo.lock:403-415`, `Cargo.lock:813-825`, `Cargo.lock:932-943`, `Cargo.lock:998-1018`, `Cargo.lock:1617-1623`).

## Documentation System

- **Framework**: Plain Markdown plus Rustdoc/Cargo docs. The repository has root Markdown documents and a `docs/` directory referenced from `README.md`, and CI validates doc tests with `cargo ... test --doc` (`README.md:79-81`, `.github/workflows/rust.yml:43-44`).
- **Docs Directory**: `docs/` contains feature documentation such as `docs/virtual-clock-guide.md` and `docs/virtual-clock-design.md`; crate-local docs also exist under `kimojio-stack-tls/docs/` (`README.md:79-81`, `docs/virtual-clock-guide.md:1-7`, `docs/virtual-clock-design.md:1-9`, `kimojio-stack-tls/docs/CUSTOM_BIO.md:1-7`).
- **Navigation Config**: N/A for static site navigation; documentation is linked directly from `README.md` and stored as Markdown files (`README.md:79-81`).
- **Style Conventions**: Documentation uses H1/H2/H3 headings, fenced code blocks, tables, and bullet lists; examples include Markdown usage/build sections and Rust/TOML/bash code fences (`docs/virtual-clock-guide.md:8-18`, `docs/virtual-clock-guide.md:45-86`, `examples/tls-echo-server.md:12-29`, `docs/virtual-clock-design.md:11-41`).
- **Build Command**: CI uses `cargo +${{ matrix.rust }} test --doc` for docs checks (`.github/workflows/rust.yml:43-44`).
- **Standard Files**: Root standard files include `README.md`, `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md`, and `LICENSE.txt`, each with project-level content (`README.md:1-5`, `CONTRIBUTING.md:1-14`, `CODE_OF_CONDUCT.md:1-10`, `SECURITY.md:1-14`, `LICENSE.txt:1-20`).

## Verification Commands

- **Format Command**: Repository instructions require `cargo fmt`; CI checks formatting with `cargo +${{ matrix.rust }} fmt --all -- --check` (`.github/copilot-instructions.md:7-10`, `.github/workflows/rust.yml:46-47`).
- **Lint Command**: Repository instructions require `cargo clippy` and `cargo clippy --all-targets --all-features`; CI runs `cargo +${{ matrix.rust }} clippy --all-targets -- -D warnings` and `cargo +${{ matrix.rust }} clippy --all-targets --all-features -- -D warnings` (`.github/copilot-instructions.md:7-16`, `.github/workflows/rust.yml:34-50`).
- **Test Command**: CI runs `cargo +${{ matrix.rust }} test --all-targets --features ssl2`, `cargo +${{ matrix.rust }} test --all-targets --features virtual-clock`, and `cargo +${{ matrix.rust }} test --doc` (`.github/workflows/rust.yml:37-44`).
- **Build Command**: The CI job's `Build` step is clippy-based rather than a separate `cargo build`; example docs show a release build for an example binary with `cargo build --release -p examples --bin tls-echo-server --features tls` (`.github/workflows/rust.yml:34-35`, `examples/tls-echo-server.md:12-29`).
- **Toolchain**: The repository pins Rust `1.92.0` in `rust-toolchain.toml`, while CI tests matrix versions `1.90.0` and `1.92.0` (`rust-toolchain.toml:1-7`, `.github/workflows/rust.yml:9-21`).

## Detailed Findings

### Workspace Structure and Cargo Conventions

- The root `Cargo.toml` defines a workspace with seven current members: `kimojio`, `kimojio-macros`, `kimojio-stack`, `kimojio-stack-tls`, `kimojio-tls`, `perf`, and `examples` (`Cargo.toml:1-13`).
- The workspace package metadata sets version `0.16.4`, edition `2024`, MIT license, homepage/repository URLs, and root README for workspace packages (`Cargo.toml:15-21`).
- Workspace dependencies are centralized in `[workspace.dependencies]`, including local path crates and shared third-party versions (`Cargo.toml:23-58`).
- Member manifests inherit workspace metadata: `kimojio-stack`, `kimojio-stack-tls`, `kimojio`, `kimojio-tls`, and `kimojio-macros` all use `version.workspace`, `edition.workspace`, `license.workspace`, `homepage.workspace`, `repository.workspace`, and `readme.workspace` (`kimojio-stack/Cargo.toml:1-9`, `kimojio-stack-tls/Cargo.toml:1-9`, `kimojio/Cargo.toml:1-9`, `kimojio-tls/Cargo.toml:1-9`, `kimojio-macros/Cargo.toml:1-9`).
- Member manifests reference root dependency versions with `.workspace = true`; examples include `kimojio-stack` using `corosensei.workspace`, `rustix.workspace`, and `rustix-uring.workspace`, and `kimojio-stack-tls` using `foreign-types-shared.workspace`, `kimojio-stack.workspace`, `kimojio-tls.workspace`, `openssl.workspace`, and `rustix-uring.workspace` (`kimojio-stack/Cargo.toml:11-14`, `kimojio-stack-tls/Cargo.toml:11-18`).
- Feature flags are crate-local. `kimojio` has default `tls`, optional `virtual-clock`, `ssl2`, and other features, while `examples` maps feature flags to the `kimojio` crate and optional `openssl` dependency (`kimojio/Cargo.toml:34-52`, `examples/Cargo.toml:7-18`).
- Benchmarks are declared per crate with `[[bench]]` and `harness = false`; `kimojio-stack` has `runtime_baseline`, while `kimojio` declares `iouring`, `mut_in_place_cell_bench`, `async_bench`, and `async_stream_bench` (`kimojio-stack/Cargo.toml:16-22`, `kimojio/Cargo.toml:58-78`).
- The `perf` crate is a workspace member with direct dependencies on `kimojio`, `mimalloc`, and `rustix` (`perf/Cargo.toml:1-9`).

### Stackful Runtime Model

- Crate-level docs state that `kimojio-stack` is intentionally not an async runtime; it runs stackful coroutines cooperatively on the current OS thread and exposes scoped spawning so coroutines cannot outlive the scope that created them (`kimojio-stack/src/lib.rs:4-24`).
- The scheduler model is documented as flat: a coroutine blocked on joins, scopes, channels, or I/O records a waiter and suspends; only the root context drives the scheduler loop (`kimojio-stack/src/lib.rs:27-32`).
- `RuntimeConfig` contains stack size, ring entries, ring enter policy, and fixed file/buffer slot counts; defaults are a 64 KiB stack, 128 ring entries, `AfterReadyBatch`, and no registered slots (`kimojio-stack/src/lib.rs:229-231`, `kimojio-stack/src/lib.rs:341-365`).
- `RingEnterPolicy` has `AfterReadyBatch` and `AfterReadyTasks(usize)` variants that control how many ready tasks run before entering io_uring (`kimojio-stack/src/lib.rs:367-385`).
- `Runtime::block_on` constructs a scheduler, builds a root `RuntimeContext`, invokes the provided closure, and asserts that scoped coroutines and in-flight I/O are drained before return (`kimojio-stack/src/lib.rs:428-452`).
- `RuntimeContext` is the capability object for scoped spawning, yielding, stack dumping, io_uring operations, and waiting, as described in crate docs and represented by the `RuntimeContext` struct (`kimojio-stack/src/lib.rs:12-17`, `kimojio-stack/src/lib.rs:461-466`).
- `RuntimeContext::scope` creates a `Scope`, invokes the caller closure, waits for all spawned children, and propagates panics after the scope wait completes (`kimojio-stack/src/lib.rs:468-493`).
- `Scope::spawn` reserves a scheduler task slot, allocates a `DefaultStack`, creates a coroutine, and returns a `JoinHandle` that can be joined for the task result (`kimojio-stack/src/lib.rs:1781-1841`).
- `JoinHandle::join` repeatedly checks `try_join`, registers a waiter, and parks via `cx.park()` until the coroutine result is ready (`kimojio-stack/src/lib.rs:1850-1877`).
- `RuntimeContext::yield_now` either drives the scheduler from the root context or suspends the current coroutine as ready (`kimojio-stack/src/lib.rs:495-505`).
- `RuntimeContext::park` drives the scheduler from the root context or suspends the current coroutine as parked; the root path asserts against deadlock when no runnable coroutine can make progress (`kimojio-stack/src/lib.rs:1741-1753`).
- `Waiter` requeues a task by calling scheduler `schedule`, while `Waiters` stores one waiter inline and additional waiters in a `Vec` (`kimojio-stack/src/lib.rs:2685-2735`).

### Stackful I/O APIs Relevant to HTTP Transports

- Crate-level I/O docs list blocking-from-coroutine APIs including `read`, `write`, `send`, `recv`, `sendmsg`, `recvmsg`, `accept`, `connect`, `shutdown`, `open`, filesystem operations, `close`, `nop`, and `sleep`; these submit one SQE, park only the current coroutine, and return after the CQE is reaped (`kimojio-stack/src/lib.rs:56-75`).
- `RuntimeContext::socket` creates sockets through io_uring when `IORING_OP_SOCKET` is supported and falls back to synchronous `socket(2)` otherwise (`kimojio-stack/src/lib.rs:1053-1076`).
- `RuntimeContext::bind` and `RuntimeContext::listen` are synchronous setup helpers because Linux does not expose io_uring operations for them (`kimojio-stack/src/lib.rs:1078-1092`).
- `RuntimeContext::accept` submits an io_uring `Accept` operation and returns an `OwnedFd`; `RuntimeContext::connect` submits an io_uring `Connect` operation against a `SocketAddrArg` (`kimojio-stack/src/lib.rs:1094-1115`).
- `RuntimeContext::read` and `RuntimeContext::write` take borrowed buffers and use io_uring `Read`/`Write` with offset `u64::MAX`, returning the byte count (`kimojio-stack/src/lib.rs:1117-1131`, `kimojio-stack/src/lib.rs:1291-1305`).
- Vectored I/O is available as `readv` and `writev` over `IoVec` slices (`kimojio-stack/src/lib.rs:1133-1143`, `kimojio-stack/src/lib.rs:1307-1318`).
- Socket-specific byte and message APIs are available as `send`, `sendmsg`, `recv`, and `recvmsg` (`kimojio-stack/src/lib.rs:1373-1414`).
- `RuntimeContext::shutdown` submits io_uring `Shutdown`, and `RuntimeContext::close` consumes an `OwnedFd` and submits io_uring `Close` (`kimojio-stack/src/lib.rs:1509-1526`).
- Async owned-buffer I/O is available through `read_async` and `write_async`; these return `IoResult` handles that own buffers until completion (`kimojio-stack/src/lib.rs:77-92`, `kimojio-stack/src/lib.rs:1198-1216`, `kimojio-stack/src/lib.rs:1416-1434`).
- `IoReadBuffer` and `IoWriteBuffer` define pointer/length contracts for owned buffers, with implementations for `Vec<u8>`, `Box<[u8]>`, and `RegisteredBuffer` (`kimojio-stack/src/lib.rs:1930-1998`, `kimojio-stack/src/lib.rs:2150-2168`).
- `ReadOutput` and `WriteOutput` return the byte count plus the original owned buffer after async I/O completion (`kimojio-stack/src/lib.rs:2000-2016`).
- `IoResult::try_get` observes already-reaped completions without entering io_uring, and `IoResult::get` parks until completion (`kimojio-stack/src/lib.rs:2337-2375`).
- `Waitable` is readiness-only, and `RuntimeContext::wait_all`, `wait_any`, `select`, and `join` wait on heterogeneous references before typed consumers retrieve results (`kimojio-stack/src/wait.rs:9-39`, `kimojio-stack/src/wait.rs:52-160`).
- `RuntimeContext::register_fd` and `register_buffer` expose fixed-file and fixed-buffer registration when the runtime was configured with fixed slots (`kimojio-stack/src/lib.rs:94-103`, `kimojio-stack/src/lib.rs:542-559`).
- Registered fixed-resource I/O has blocking and async variants for fixed fds, fixed buffers, and combined fixed fd+buffer paths (`kimojio-stack/src/lib.rs:1145-1196`, `kimojio-stack/src/lib.rs:1218-1289`, `kimojio-stack/src/lib.rs:1320-1371`, `kimojio-stack/src/lib.rs:1436-1507`).
- `RegisteredFd` and `RegisteredBuffer` retire their resources on drop and defer cleanup while in-flight I/O still holds leases (`kimojio-stack/src/lib.rs:2018-2148`, `kimojio-stack/src/lib.rs:2175-2266`).
- The scheduler keeps ready task IDs, a coroutine table, one io_uring, fixed-resource free lists, an I/O state pool, completion scratch storage, and an in-flight I/O count (`kimojio-stack/src/lib.rs:2769-2814`).
- Scheduler I/O state is reused through `io_state_pool`, and completion handling takes completed CQEs out of the scheduler borrow before applying results to `IoState` (`kimojio-stack/src/lib.rs:2909-2923`, `kimojio-stack/src/lib.rs:3014-3028`, `kimojio-stack/src/lib.rs:3103-3133`).
- Plain `kimojio-stack` tests define local `send_all` and `recv_to_end` helpers for complete socket transfers, while `kimojio-stack-tls` provides a public `read_exact_or_eof` helper on `TlsStream` (`kimojio-stack/src/lib.rs:3297-3315`, `kimojio-stack-tls/src/lib.rs:130-145`).

### Error Handling Surface

- `kimojio-stack` publicly re-exports `rustix_uring::Errno`, and runtime I/O methods return `Result<_, Errno>` (`kimojio-stack/src/lib.rs:202-203`, `kimojio-stack/src/lib.rs:1057-1062`, `kimojio-stack/src/lib.rs:1122-1131`, `kimojio-stack/src/lib.rs:1296-1305`).
- `kimojio-tls` models TLS operations as `Response::{Success, Fail, Eof, WantRead, WantWrite}` and TLS failures as either `TlsServerError::Errno(Errno)` or `TlsServerError::TlsError(Vec<u64>)` (`kimojio-tls/src/lib.rs:54-58`, `kimojio-tls/src/lib.rs:201-207`).
- `kimojio-tls::get_response` maps OpenSSL WANT_READ/WANT_WRITE and EOF-style results into `Response` variants, maps unsupported WANT cases to `Errno::PROTO`, and maps OS errors through `Errno::from_raw_os_error` (`kimojio-tls/src/lib.rs:209-249`).
- `kimojio-stack-tls` maps `TlsServerError::Errno` through unchanged and `TlsServerError::TlsError(_)` to `Errno::PROTO`; operations on a closed/missing socket return `Errno::PIPE` (`kimojio-stack-tls/src/lib.rs:225-239`).
- `TlsStream::read` returns `Ok(0)` on clean TLS EOF, while `TlsStream::write` treats zero-length OpenSSL progress or TLS EOF during write as `Errno::PIPE` (`kimojio-stack-tls/src/lib.rs:115-128`, `kimojio-stack-tls/src/lib.rs:147-166`).

### Cooperative Synchronization and Backpressure Primitives

- `kimojio-stack` exports `Mutex`, `Semaphore`, `Notify`, `Watch`, `Barrier`, `RwLock`, `channel`, and `once` primitives as crate modules/reexports (`kimojio-stack/src/lib.rs:105-123`, `kimojio-stack/src/lib.rs:211-227`).
- Bounded channels allocate a `VecDeque` with the requested capacity, park senders while the queue is full, wake one receiver after send, and implement `Waitable` readiness for send capacity (`kimojio-stack/src/channel/bounded.rs:12-68`, `kimojio-stack/src/channel/bounded.rs:96-109`).
- Bounded receivers park while the queue is empty/open, wake one sender after receiving, and implement `Waitable` readiness when data is present or senders are closed (`kimojio-stack/src/channel/bounded.rs:117-191`).
- `Semaphore` tracks permits in a `RefCell`, parks on acquire when no permit is available, and wakes waiting coroutines when permits are added or released (`kimojio-stack/src/semaphore.rs:9-65`, `kimojio-stack/src/semaphore.rs:94-103`).
- `Notify` provides one-permit `notify_one`, generation-based `notify_waiters`, and waitable `Notified` handles that park through `RuntimeContext` (`kimojio-stack/src/notify.rs:9-45`, `kimojio-stack/src/notify.rs:62-123`).
- `Mutex` is cooperative: `lock` parks when already locked, `try_lock` is nonparking, and guard drop wakes one waiter (`kimojio-stack/src/mutex.rs:10-58`, `kimojio-stack/src/mutex.rs:87-110`).

### Stack TLS API and Implementation

- `kimojio-stack-tls` depends on `foreign-types-shared`, `kimojio-stack`, `kimojio-tls`, `openssl`, `rustix` with `net`, and `rustix-uring` (`kimojio-stack-tls/Cargo.toml:11-18`).
- Crate docs state that OpenSSL reads encrypted bytes directly from an internal BIO buffer returned by `kimojio-tls`, that the runtime fills that buffer with `RuntimeContext::read`, that OpenSSL writes encrypted bytes into another BIO buffer, and that the runtime drains it with `RuntimeContext::write` (`kimojio-stack-tls/src/lib.rs:4-12`).
- `TlsContext::from_openssl` transfers ownership from an `openssl::ssl::SslContext` into a `kimojio_tls::TlsServerContext` (`kimojio-stack-tls/src/lib.rs:21-38`).
- `TlsContext::server` and `TlsContext::client` create server/client `TlsServer` handles with a configured buffer size, wrap them in `TlsStream`, and perform the corresponding handshake before returning (`kimojio-stack-tls/src/lib.rs:40-64`).
- `TlsStream` stores a `TlsServer` and optional `OwnedFd` socket (`kimojio-stack-tls/src/lib.rs:67-80`).
- `TlsStream::ssl` exposes an OpenSSL `SslRef` backed by the raw SSL pointer owned by `TlsServer` (`kimojio-stack-tls/src/lib.rs:82-87`).
- Client and server handshake loops handle `Response::WantRead` by filling the TLS read BIO and `Response::WantWrite` by flushing the TLS write BIO (`kimojio-stack-tls/src/lib.rs:89-113`).
- `TlsStream::read`, `read_exact_or_eof`, `write`, `shutdown`, `shutdown_write`, and `close` form the stream I/O API over `RuntimeContext` (`kimojio-stack-tls/src/lib.rs:115-194`).
- `fill_tls_read` obtains `ssl.get_push_buffer()`, calls `cx.read(socket, buffer)`, and advances the OpenSSL push buffer; `flush_tls_write` obtains `ssl.get_pull_buffer()`, calls `cx.write(socket, buffer)`, and advances the OpenSSL pull buffer (`kimojio-stack-tls/src/lib.rs:196-223`).
- The lower-level `kimojio-tls` crate declares C FFI functions for TLS handle creation, BIO push/pull buffer access, SSL read/write, handshakes, and shutdown (`kimojio-tls/src/lib.rs:141-181`).
- The C implementation creates an OpenSSL BIO pair with `BIO_new_bio_pair`, installs it with `SSL_set_bio`, exposes write-side space through `BIO_nwrite0`, and exposes read-side pending ciphertext through `BIO_nread0` (`kimojio-tls/src/openssl_stream.c:159-199`, `kimojio-tls/src/openssl_stream.c:202-229`).
- `kimojio-tls/build.rs` compiles `src/openssl_stream.c` with `cc` and links dynamic `ssl` and `crypto` libraries (`kimojio-tls/build.rs:3-13`).
- Stack TLS tests generate self-signed OpenSSL contexts in memory, disable client certificate verification for the test connector, and convert both contexts into `TlsContext` (`kimojio-stack-tls/src/lib.rs:263-296`).
- `tls_echo_over_stackful_socketpair` uses a UNIX `socketpair`, a single `Runtime`, two scoped stackful tasks, TLS server/client handshakes, small read buffers, echo writes, shutdown, close, and equality assertion (`kimojio-stack-tls/src/lib.rs:310-368`).
- `tls_rpc_write_header_body_response_over_stackful_socketpair` uses a UNIX `socketpair`, a 64-byte header, an 8 KiB body, a 64-byte response, `read_exact_or_eof`, and separate server/client scoped tasks (`kimojio-stack-tls/src/lib.rs:370-438`).
- `kimojio-stack-tls/docs/CUSTOM_BIO.md` records a removed custom-BIO experiment and measured RPC-shaped TLS workloads with 64-byte length headers, 8/16/23/24 KiB bodies, and 64-byte responses (`kimojio-stack-tls/docs/CUSTOM_BIO.md:1-20`, `kimojio-stack-tls/docs/CUSTOM_BIO.md:22-37`).
- The custom-BIO notes state that the memory-BIO path exposes OpenSSL BIO-pair buffers directly with `BIO_nwrite0` and `BIO_nread0`, matching the current C implementation (`kimojio-stack-tls/docs/CUSTOM_BIO.md:63-72`, `kimojio-tls/src/openssl_stream.c:202-229`).

### Existing Tests, Integration Tests, and Peer-Test Patterns

- The repository has one current Rust integration test file under `kimojio/tests/virtual_clock_integration.rs`; it is feature-gated with `#![cfg(feature = "virtual-clock")]` and uses `#[kimojio::test]` async tests (`kimojio/tests/virtual_clock_integration.rs:1-22`).
- `kimojio-stack` uses in-crate `#[test]` modules that construct `Runtime::new()` and call `runtime.block_on`, including a TCP echo server/client test using `socket`, `bind`, `listen`, `accept`, `connect`, `recv`, `send`, `shutdown`, and `close` (`kimojio-stack/src/lib.rs:3277-3295`, `kimojio-stack/src/lib.rs:3999-4060`).
- `kimojio-stack-tls` uses in-crate `#[test]` modules with one runtime, scoped client/server coroutines, OpenSSL-generated TLS contexts, and UNIX socketpairs (`kimojio-stack-tls/src/lib.rs:241-260`, `kimojio-stack-tls/src/lib.rs:310-438`).
- `kimojio` declares dev-dependencies on `criterion`, `rcgen`, and `tempfile`, while `kimojio-stack` declares `criterion` as a dev-dependency (`kimojio/Cargo.toml:58-61`, `kimojio-stack/Cargo.toml:16-18`).
- Current workspace manifests do not declare tokio, hyper, tonic, h2, prost, or HTTP parser/type crates in the central workspace dependency list (`Cargo.toml:23-58`).
- The feature spec states that tokio-based ecosystem crates may be used only for interoperability test peers because they are test dependencies rather than public runtime dependencies of the new stackful crates (`.paw/work/grpc-client-server-support/Spec.md:152-159`).

### Benchmarking and Allocation/Performance Infrastructure

- `kimojio-stack` crate docs describe the allocation model: setup paths allocate scheduler containers, io_uring, guarded stacks, join state, and channel state; hot I/O paths are designed to avoid Rust heap allocation after warmup via I/O state reuse, completion scratch reuse, and inline single-waiter storage (`kimojio-stack/src/lib.rs:140-157`).
- `kimojio-stack` installs a test-only global `CountingAllocator` under `#[cfg(test)]` (`kimojio-stack/src/lib.rs:232-235`).
- The `allocation_tracking` test module tracks allocations, allocated bytes, deallocations, reallocations, and exposes `measure` plus `AllocationCounts` helpers (`kimojio-stack/src/lib.rs:3142-3183`).
- Allocation tests assert that warmed blocking pipe ping-pong and reused-buffer async pipe ping-pong perform zero counted allocation operations and zero allocated/reallocated bytes (`kimojio-stack/src/lib.rs:4206-4273`, `kimojio-stack/src/lib.rs:4275-4348`).
- `kimojio-stack/benches/runtime_baseline.rs` uses Criterion `iter_custom` and helper functions that run work inside `Runtime::new()` or `Runtime::with_registered_resources` (`kimojio-stack/benches/runtime_baseline.rs:1-25`, `kimojio-stack/benches/runtime_baseline.rs:27-34`).
- Stack benches cover scheduler yield, spawn/join, sync primitives, blocking I/O, registered fd/buffer I/O, async I/O, and two-task pipe ping-pong (`kimojio-stack/benches/runtime_baseline.rs:36-186`, `kimojio-stack/benches/runtime_baseline.rs:188-420`).
- The stack benchmark group configures Criterion with 1 second warmup and 4 seconds measurement time (`kimojio-stack/benches/runtime_baseline.rs:413-420`).
- The `perf` crate installs a custom global allocator backed by `mimalloc` that can panic on allocation when a `noalloc` flag is set, pins the current thread to a CPU, performs warmup, and measures `operations::nop()` latency (`perf/src/main.rs:10-47`, `perf/src/main.rs:49-92`).
- The async `kimojio` benches use Criterion `iter_custom`, `kimojio::run`, `operations::spawn_task`, socket benchmarks, pipe benchmarks, and warmup/measurement configuration (`kimojio/benches/iouring.rs:17-33`, `kimojio/benches/iouring.rs:35-83`, `kimojio/benches/iouring.rs:127-175`, `kimojio/benches/async_bench.rs:155-160`).

### Internal Dependency Inventory for HTTP/gRPC Planning

- Central workspace dependencies currently include `futures`, `openssl`, `rcgen`, `criterion`, `mimalloc`, `rustix`, `rustix-uring`, `zerocopy`, local Kimojio crates, macro dependencies, and example dependencies (`Cargo.toml:23-66`).
- `futures` is present in the lockfile at version `0.3.32` with `futures-channel`, `futures-core`, `futures-executor`, `futures-io`, `futures-sink`, `futures-task`, and `futures-util` dependencies (`Cargo.lock:403-415`).
- `openssl` is present in the lockfile at version `0.10.76` with dependencies including `foreign-types`, `libc`, `openssl-macros`, and `openssl-sys` (`Cargo.lock:813-825`).
- `rcgen` is present in the lockfile at version `0.14.7` with dependencies including `pem`, `ring`, `rustls-pki-types`, `time`, `x509-parser`, and `yasna` (`Cargo.lock:932-943`).
- `criterion` is present in the lockfile at version `0.8.2` with dependencies including `anes`, `ciborium`, `clap`, `criterion-plot`, `serde`, and `tinytemplate` (`Cargo.lock:257-277`).
- `mimalloc` is present in the lockfile at version `0.1.52` with `libmimalloc-sys` (`Cargo.lock:727-733`).
- `rustix` and `rustix-uring` are present in the lockfile at versions `1.1.4` and `0.6.0`; `rustix-uring` depends on `bitflags` and `rustix` (`Cargo.lock:998-1018`).
- `zerocopy` is present in the lockfile at version `0.8.42` with `zerocopy-derive` (`Cargo.lock:1617-1623`).
- The current root workspace dependency list does not include `bytes`, `http`, `h2`, `httparse`, `httpdate`, `prost`, `hpack`, `hyper`, `tonic`, or `tokio` (`Cargo.toml:23-58`).

#### External Public Crate Facts Checked for Candidate Categories

These are external public facts from official crate documentation or the crates.io API; internal repository claims above remain cited with file:line references.

| Candidate | Public facts observed | External citation |
| --- | --- | --- |
| `http` | Described as "a general purpose library of common HTTP types" with `Request` and `Response` examples. | <https://docs.rs/crate/http/latest> |
| `bytes` | Described as a utility library for working with bytes; docs mention `Bytes`, `BytesMut`, `Buf`, `BufMut`, optional serde support, and `no_std` support with default `std` disabled. | <https://docs.rs/crate/bytes/latest> |
| `httparse` | Described as a push parser for HTTP/1.x that avoids allocations and copies and supports `no_std` with `std` disabled. | <https://docs.rs/crate/httparse/latest> |
| `httpdate` | Described as HTTP date parsing/formatting with `parse_http_date`, `fmt_http_date`, and `HttpDate`. | <https://docs.rs/crate/httpdate/latest> |
| `prost` | Described as a Protocol Buffers implementation for Rust; docs state it uses `bytes::{Buf, BufMut}` abstractions for serialization and can serialize/deserialize existing Rust types via attributes. | <https://docs.rs/crate/prost/latest> |
| `hpack` | Described as an HPACK coder implementation with `Decoder` and `Encoder`; docs state encoder does not implement Huffman string literal encoding. | <https://docs.rs/crate/hpack/latest> |
| `h2` | Described as a Tokio-aware HTTP/2 client/server implementation built on Tokio; crates.io dependency metadata for `0.4.14` reports normal dependencies including `tokio` and `tokio-util`. | <https://docs.rs/crate/h2/latest>, <https://crates.io/api/v1/crates/h2/0.4.14/dependencies> |
| `hyper` | Described as an asynchronous HTTP/1 and HTTP/2 client/server library; crates.io dependency metadata for `1.10.1` reports a normal `tokio` dependency and optional `h2`, `httparse`, and `httpdate` dependencies. | <https://docs.rs/crate/hyper/latest>, <https://crates.io/api/v1/crates/hyper/1.10.1/dependencies> |
| `tonic` | Described as gRPC over HTTP/2 focused on interoperability and flexibility; docs state its HTTP/2 implementation is based on hyper on top of tokio, while the generic implementation can support any HTTP/2 implementation and encoding via traits. | <https://docs.rs/crate/tonic/latest> |

## Code References

- `Cargo.toml:1-13` - Workspace members and resolver.
- `Cargo.toml:15-58` - Workspace package metadata and dependency versions.
- `kimojio-stack/Cargo.toml:11-22` - Stack runtime dependencies, Criterion dev-dependency, and `runtime_baseline` bench.
- `kimojio-stack/src/lib.rs:4-24` - Stackful runtime crate-level model.
- `kimojio-stack/src/lib.rs:56-103` - Blocking I/O, async I/O, and registered resource crate docs.
- `kimojio-stack/src/lib.rs:140-157` - Allocation model documentation.
- `kimojio-stack/src/lib.rs:341-365` - `RuntimeConfig` fields and defaults.
- `kimojio-stack/src/lib.rs:428-452` - `Runtime::block_on` root context setup.
- `kimojio-stack/src/lib.rs:468-493` - `RuntimeContext::scope` structured concurrency.
- `kimojio-stack/src/lib.rs:1053-1122` - Socket creation, bind/listen, accept/connect, and read APIs.
- `kimojio-stack/src/lib.rs:1291-1414` - Write, vectored write, send, sendmsg, recv, recvmsg APIs.
- `kimojio-stack/src/lib.rs:1509-1526` - Close and shutdown APIs.
- `kimojio-stack/src/wait.rs:52-160` - `wait_all`, `wait_any`, `select`, and `join`.
- `kimojio-stack/src/lib.rs:1930-2016` - Owned buffer traits and read/write output structs.
- `kimojio-stack/src/lib.rs:2018-2168` - Registered fd/buffer public structs and buffer trait implementations.
- `kimojio-stack/src/lib.rs:2769-3133` - Scheduler queues, I/O state pooling, ring entry, and completion handling.
- `kimojio-stack/src/lib.rs:3999-4060` - Stackful TCP echo server/client test.
- `kimojio-stack/src/lib.rs:4206-4348` - Allocation hot-path tests.
- `kimojio-stack-tls/src/lib.rs:4-12` - Stack TLS memory-BIO integration summary.
- `kimojio-stack-tls/src/lib.rs:21-65` - `TlsContext` and client/server construction.
- `kimojio-stack-tls/src/lib.rs:115-194` - `TlsStream` read/write/shutdown/close API.
- `kimojio-stack-tls/src/lib.rs:196-223` - TLS BIO fill/flush via `RuntimeContext::read`/`write`.
- `kimojio-stack-tls/src/lib.rs:310-438` - Stack TLS socketpair tests.
- `kimojio-tls/src/lib.rs:141-181` - TLS C FFI declarations.
- `kimojio-tls/src/openssl_stream.c:159-229` - OpenSSL TLS handle creation and BIO buffer exposure.
- `kimojio-stack/benches/runtime_baseline.rs:413-420` - Criterion benchmark group configuration.
- `.github/workflows/rust.yml:34-50` - CI verification commands.

## Architecture Documentation

- `APOLOGY.md` documents the stackful runtime rationale: ordinary stack frames are the suspension unit, io_uring provides nonblocking kernel work, only the current coroutine parks, scoped spawning allows parent-stack borrowing, and the cost model emphasizes explicit latency/memory/scheduling/I/O costs (`APOLOGY.md:1-31`, `APOLOGY.md:33-60`, `APOLOGY.md:62-90`).
- `WHY_NOT_RUST_ASYNC.md` documents the repository's rationale for avoiding async state-machine overhead, including future-shaped objects, pass-through async functions, duplicated suspend states, panic paths, and future layout growth (`WHY_NOT_RUST_ASYNC.md:1-18`, `WHY_NOT_RUST_ASYNC.md:19-67`, `WHY_NOT_RUST_ASYNC.md:68-123`, `WHY_NOT_RUST_ASYNC.md:143-162`).
- `TODO.md` already lists "gRPC client and server" and "http client and server (over TLS)" as future items, alongside stack introspection, polling mode, tokio/thread interop, and TLS offload ideas (`TODO.md:1-10`).
- `FUTURE.md` records current `kimojio-stack` io_uring coverage and remaining areas such as provided-buffer/multishot I/O, zero-copy networking, inter-ring messaging, and ring setup/submission tuning (`FUTURE.md:1-29`).
- `kimojio-stack/WORKSTEALING.md` describes the current stack runtime's single-threaded assumptions (`Rc`, `Weak`, `Cell`, `RefCell`, non-`Send` scoped spawn, local cooperative primitives, local waiters, and same-thread coroutine resume) and sketches multithreaded task classes as architecture research (`kimojio-stack/WORKSTEALING.md:34-67`, `kimojio-stack/WORKSTEALING.md:68-93`).
- `kimojio-stack-tls/docs/CUSTOM_BIO.md` records TLS BIO performance research around memory BIO versus custom BIO for RPC-shaped workloads and notes that current memory BIO exposes OpenSSL BIO-pair buffers directly (`kimojio-stack-tls/docs/CUSTOM_BIO.md:22-37`, `kimojio-stack-tls/docs/CUSTOM_BIO.md:39-59`, `kimojio-stack-tls/docs/CUSTOM_BIO.md:63-78`).
- `README.md` describes the original async `kimojio` runtime as single-threaded, cooperatively scheduled, Linux/io_uring-based, without automatic load balancing, and optimized for latency (`README.md:1-20`).

## Open Questions

- The current workspace dependency list does not record selected HTTP/gRPC protocol crates such as `bytes`, `http`, `httparse`, `httpdate`, `prost`, `hpack`, `h2`, `hyper`, or `tonic`; the spec records runtime-agnostic protocol/type crates as dependencies to be selected during research/planning (`Cargo.toml:23-58`, `.paw/work/grpc-client-server-support/Spec.md:184-189`).
- The current stack TLS tests exercise TLS over UNIX socketpairs, not TCP TLS ALPN negotiation; the two stack TLS tests construct socketpairs and perform protocol-shaped byte exchange over TLS (`kimojio-stack-tls/src/lib.rs:310-319`, `kimojio-stack-tls/src/lib.rs:370-379`).
- Existing `kimojio-stack` public APIs expose concrete fd-based I/O and concrete `TlsStream`; no existing HTTP transport abstraction is present in the referenced stack runtime/TLS public APIs (`kimojio-stack/src/lib.rs:1053-1122`, `kimojio-stack/src/lib.rs:1291-1414`, `kimojio-stack-tls/src/lib.rs:67-80`, `kimojio-stack-tls/src/lib.rs:115-194`).
- Current integration tests do not include tokio-based peer crates; the current integration test tree contains virtual-clock tests for the original `kimojio` crate, and current manifests do not declare tokio/hyper/tonic dependencies (`kimojio/tests/virtual_clock_integration.rs:1-22`, `Cargo.toml:23-58`).
- The spec defers generated stubs and non-unary gRPC streaming while requiring an architecture that does not block future streaming support (`.paw/work/grpc-client-server-support/Spec.md:12-14`, `.paw/work/grpc-client-server-support/Spec.md:152-159`, `.paw/work/grpc-client-server-support/Spec.md:176-182`).
