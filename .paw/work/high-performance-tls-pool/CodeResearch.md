---
date: "2026-06-07T09:05:13.875+00:00"
git_commit: "666c76b754c0f70311bb7157b87a6e7065b656d9"
branch: "feature/high-performance-tls-pool"
repository: "kimojio-rs"
topic: "Runtime-independent high-performance TLS pool library over OpenSSL"
tags: [research, codebase, tls, openssl, workspace, benchmarks]
status: complete
last_updated: "2026-06-07"
---

# Research: Runtime-Independent High-Performance TLS Pool Library over OpenSSL

## Research Question

Document existing codebase implementation details relevant to creating a runtime-independent high-performance TLS pool library layered on OpenSSL, focusing on workspace structure, existing TLS crates and wrappers, stack TLS patterns present on `origin/main`, tests and benchmark conventions, documentation infrastructure, and verification commands.

## Summary

- The repository is a Cargo workspace with `kimojio`, `kimojio-macros`, `kimojio-tls`, `perf`, and `examples` members (`Cargo.toml:1-9`).
- The current reusable OpenSSL wrapper is split between a runtime-independent-looking `kimojio-tls` crate that compiles a C wrapper and links `ssl`/`crypto` (`kimojio-tls/Cargo.toml:11-16`, `kimojio-tls/build.rs:3-12`) and the `kimojio` runtime crate, which gates TLS modules behind `tls` and `ssl2` features (`kimojio/Cargo.toml:23-25`, `kimojio/Cargo.toml:34-52`, `kimojio/src/lib.rs:41-51`).
- `origin/main` contains the same TLS code paths inspected here: `kimojio-tls`, `kimojio/src/tlscontext.rs`, `kimojio/src/tlsstream.rs`, and `kimojio/src/ssl2/*`; the current branch has no non-`.paw` code diff from `origin/main` based on git comparison during research.
- The existing primary TLS flow wraps an OpenSSL `SSL_CTX` in `TlsContext`, creates client/server `TlsStream` values, and drives handshakes/read/write loops through `WantRead` and `WantWrite` responses (`kimojio/src/tlscontext.rs:18-58`, `kimojio/src/tlsstream.rs:85-125`, `kimojio/src/tlsstream.rs:216-320`).
- The experimental stack TLS pattern is under `kimojio/src/ssl2`: it wraps `openssl::ssl::SslStream<SyncBufferedStream>` and pumps buffered bytes to/from a generic `AsyncStreamRead + AsyncStreamWrite` transport (`kimojio/src/ssl2/mod.rs:4-12`, `kimojio/src/ssl2/mod.rs:24-39`, `kimojio/src/ssl2/io.rs:14-20`, `kimojio/src/ssl2/io.rs:67-103`).
- Existing tests use `#[crate::test]`/`#[kimojio::test]` async test macros that run inside the kimojio runtime (`kimojio-macros/src/lib.rs:99-173`, `kimojio/src/tlscontext.rs:122-167`, `kimojio/tests/virtual_clock_integration.rs:21-34`).
- Existing benchmarks use Criterion with `iter_custom` to keep runtime setup out of timing in several benchmarks (`kimojio/benches/iouring.rs:17-24`, `kimojio/benches/async_stream_bench.rs:13-37`, `kimojio/benches/async_bench.rs:13-30`).

## Documentation System

- **Framework**: Plain Markdown plus Rustdoc/docs.rs. The README links docs.rs for crate documentation (`README.md:1-5`), and Cargo docs.rs metadata enables all features and docsrs cfg for `kimojio` (`kimojio/Cargo.toml:54-56`). No `mkdocs.yml`, Docusaurus config, Sphinx `conf.py`, `book.toml`, `package.json`, `Makefile`, or `justfile` was found by repository file discovery.
- **Docs Directory**: `docs/` contains virtual clock user and design docs (`README.md:79-81`, `docs/virtual-clock-guide.md:1-17`, `docs/virtual-clock-design.md:1-13`).
- **Navigation Config**: N/A; Markdown docs are linked from README rather than a site navigation config (`README.md:79-81`).
- **Style Conventions**: Docs use top-level `#` titles, `##`/`###` sections, tables for API summaries, and fenced code blocks (`docs/virtual-clock-guide.md:1-17`, `docs/virtual-clock-guide.md:45-86`, `docs/virtual-clock-design.md:9-41`). Example docs include feature lists, build/run commands, and test commands (`examples/tls-echo-server.md:1-19`, `examples/tls-echo-server.md:97-158`, `examples/copyfile.md:1-47`).
- **Build Command**: Documentation is checked in CI with `cargo +${{ matrix.rust }} test --doc` (`.github/workflows/rust.yml:43-44`).
- **Standard Files**: `README.md` provides project overview and links `CONTRIBUTING.md` (`README.md:1-7`, `README.md:55-57`); `CONTRIBUTING.md` describes CLA and code-of-conduct process (`CONTRIBUTING.md:1-14`); `SECURITY.md` contains Microsoft security reporting guidance (`SECURITY.md:1-14`); `LICENSE.txt` is MIT (`LICENSE.txt:1-20`); `CODE_OF_CONDUCT.md` links the Microsoft Open Source Code of Conduct (`CODE_OF_CONDUCT.md:1-9`).

## Verification Commands

- **Test Command**: CI runs `cargo +${{ matrix.rust }} test --all-targets --features ssl2`, `cargo +${{ matrix.rust }} test --all-targets --features virtual-clock`, and `cargo +${{ matrix.rust }} test --doc` (`.github/workflows/rust.yml:37-44`).
- **Lint Command**: CI runs `cargo +${{ matrix.rust }} clippy --all-targets -- -D warnings` and `cargo +${{ matrix.rust }} clippy --all-targets --all-features -- -D warnings` (`.github/workflows/rust.yml:34-35`, `.github/workflows/rust.yml:49-50`). Repository instructions also list `cargo clippy` and `cargo clippy --all-targets --all-features` for code changes (`.github/copilot-instructions.md:7-15`).
- **Build Command**: CI labels the `cargo +${{ matrix.rust }} clippy --all-targets -- -D warnings` step as “Build” (`.github/workflows/rust.yml:34-35`). The TLS example docs also show `cargo build --release -p examples --bin tls-echo-server --features tls` (`examples/tls-echo-server.md:12-19`).
- **Format Command**: CI runs `cargo +${{ matrix.rust }} fmt --all -- --check` (`.github/workflows/rust.yml:46-47`), and repository instructions list `cargo fmt` for code changes (`.github/copilot-instructions.md:7-15`).
- **Type Check**: Rust type checking is covered by the Cargo build/clippy/test commands above; no separate typecheck command file was found. The workspace pins Rust `1.92.0` in `rust-toolchain.toml` (`rust-toolchain.toml:1-7`) while CI tests Rust `1.90.0` and `1.92.0` (`.github/workflows/rust.yml:18-21`).
- **Bench Command**: Bench targets are registered in `kimojio/Cargo.toml` (`kimojio/Cargo.toml:63-77`) and use Criterion entry points (`kimojio/benches/iouring.rs:247-252`, `kimojio/benches/async_bench.rs:155-160`, `kimojio/benches/async_stream_bench.rs:125-130`, `kimojio/benches/mut_in_place_cell_bench.rs:24-29`).

## Detailed Findings

### Workspace and Package Structure

- The workspace members are `kimojio`, `kimojio-macros`, `kimojio-tls`, `perf`, and `examples` with resolver version 3 (`Cargo.toml:1-11`).
- Workspace package metadata sets version `0.16.4`, edition `2024`, MIT license, Azure GitHub homepage/repository, and root README (`Cargo.toml:13-19`).
- Workspace dependencies include `criterion`, `futures`, `kimojio`, `kimojio-macros`, `kimojio-tls`, `openssl`, `rcgen`, `rustix`, `rustix-uring`, `scopeguard`, `tempfile`, and `uuid` (`Cargo.toml:21-61`).
- The main `kimojio` crate describes itself as a thread-per-core Linux io_uring async runtime and depends optionally on `kimojio-tls` and `openssl` (`kimojio/Cargo.toml:1-25`).
- `kimojio` enables `tls` by default, maps `tls` to `dep:openssl` and `dep:kimojio-tls`, and has a temporary `ssl2` feature that depends on `tls` (`kimojio/Cargo.toml:34-52`).
- The public crate module table exposes `ssl2` only under `cfg(feature = "ssl2")`, and exposes `tlscontext`/`tlsstream` only under `cfg(feature = "tls")` (`kimojio/src/lib.rs:41-51`).
- `kimojio-tls` is a separate workspace crate named “Kimojio OpenSSL integration” with only `rustix-uring` as a normal dependency and `cc` as a build dependency (`kimojio-tls/Cargo.toml:1-16`).
- The examples crate has `tls` and `virtual-clock` feature flags, depends on `kimojio`, and makes `openssl` optional behind its `tls` feature (`examples/Cargo.toml:1-17`).
- The `tls-echo-server` example binary is registered with required feature `tls` (`examples/Cargo.toml:28-31`).
- The `perf` workspace member is a small binary crate depending on `kimojio` and `rustix` (`perf/Cargo.toml:1-8`).
- The macro crate is a proc-macro crate depending on `quote` and `syn` (`kimojio-macros/Cargo.toml:1-16`).

### Runtime Model and Existing Pool-Like Helper

- The README describes Kimojio as a single-threaded, cooperatively scheduled runtime where tasks do not migrate between threads (`README.md:7-11`).
- The README lists runtime characteristics as single-threaded cooperative scheduling, io_uring async disk I/O, explicit concurrency/load-balancing control, and no locks/atomics/thread synchronization (`README.md:13-20`).
- `TaskPool` is the existing pool-like helper in the runtime crate; it holds an `Rc<AsyncSemaphore>` and limits how many spawned kimojio tasks run at one time (`kimojio/src/task_pool.rs:11-26`).
- `TaskPool::spawn_task` awaits semaphore acquisition, spawns the provided future using `operations::spawn_task`, and releases the semaphore with a scopeguard when the spawned task exits (`kimojio/src/task_pool.rs:28-43`).
- `TaskPool` tests use `#[crate::test]`, `AsyncEvent`, `operations::yield_io`, and runtime tasks to assert sequential and parallel limits (`kimojio/src/task_pool.rs:46-168`).
- The `perf` binary pins the current thread to CPU 2 with `sched_setaffinity`, runs kimojio with `BusyPoll::Always`, and measures `operations::nop()` latency while a custom allocator can panic on allocation (`perf/src/main.rs:10-47`, `perf/src/main.rs:49-59`, `perf/src/main.rs:61-94`).

### Low-Level OpenSSL FFI Wrapper (`kimojio-tls`)

- `kimojio-tls/build.rs` compiles `src/openssl_stream.c`, links dynamic `ssl` and `crypto`, and declares rebuild triggers for `build.rs` and the C source (`kimojio-tls/build.rs:3-12`).
- The Rust FFI layer declares opaque raw types for TLS handles and contexts, a `RawError` layout, and a `Slice` layout used to move buffers across the FFI boundary (`kimojio-tls/src/lib.rs:9-33`).
- Rust declares OpenSSL-oriented error variants and a `TlsServerError` enum for errno and TLS error stacks (`kimojio-tls/src/lib.rs:34-58`).
- `TlsServerError::Display` formats errno directly and formats OpenSSL error-stack entries by calling `ERR_lib_error_string`, `ERR_func_error_string`, and `ERR_reason_error_string` (`kimojio-tls/src/lib.rs:60-104`).
- The FFI declarations include handle/context close, dup, create, push/pull buffer accessors, read/write, client/server handshake, shutdown, OpenSSL error helpers, version, raw SSL getter, and minimum protocol getter (`kimojio-tls/src/lib.rs:141-181`).
- `TlsServerContext` wraps a raw `SSL_CTX` pointer, and `TlsServer` wraps a raw TLS handle pointer (`kimojio-tls/src/lib.rs:183-194`).
- Both `TlsServerContext` and `TlsServer` have unsafe `Send` implementations with comments stating OpenSSL is safe to call from different threads as long as it is not called at the same time, and that the types are Send but not Sync (`kimojio-tls/src/lib.rs:187-199`).
- `Response` maps successful byte counts, failures, EOF, and OpenSSL `WantRead`/`WantWrite` outcomes (`kimojio-tls/src/lib.rs:201-207`).
- `get_response` maps raw C response codes to `Response`, converting OpenSSL `SSL_ERROR_WANT_READ` and `SSL_ERROR_WANT_WRITE` into `WantRead`/`WantWrite`, `SSL_ERROR_ZERO_RETURN` into EOF, several protocol conditions into errno `PROTO`, and errno-fail responses into `Errno::from_raw_os_error` (`kimojio-tls/src/lib.rs:209-249`).
- `TlsServerContext::server` and `TlsServerContext::client` allocate server/client TLS handles through `tls_handle_create` with the `is_server` flag set to `true` or `false` (`kimojio-tls/src/lib.rs:264-287`).
- `TlsServer` methods call through to C for handshakes, raw SSL access, shutdown, reads, writes, and push/pull buffer access (`kimojio-tls/src/lib.rs:310-377`).
- `TlsServer::clone` calls `tls_handle_dup`, and `Drop` closes the C handle (`kimojio-tls/src/lib.rs:379-397`).
- The C wrapper defines `ResponseType`, `TlsHandleState`, and `TlsHandle` with `SSL *ssl`, a BIO pair, and state (`kimojio-tls/src/openssl_stream.c:13-43`).
- `tls_handle_dup` validates inputs, copies the `TlsHandle`, calls `SSL_up_ref` on the shared `SSL`, and returns the copied handle on success (`kimojio-tls/src/openssl_stream.c:94-118`).
- `tls_handle_create` allocates a handle, calls `SSL_new`, creates a `BIO_new_bio_pair`, sets accept/connect state, and installs the BIO with `SSL_set_bio` (`kimojio-tls/src/openssl_stream.c:159-200`).
- Push/pull functions expose the BIO pair's writable/readable memory with `BIO_nwrite0`/`BIO_nread0` and advance with `BIO_nwrite`/`BIO_nread` (`kimojio-tls/src/openssl_stream.c:202-230`).
- `tls_handle_read` and `tls_handle_write` call `SSL_read`/`SSL_write` and map `SSL_ERROR_WANT_WRITE`, `SSL_ERROR_WANT_READ`, and `SSL_ERROR_ZERO_RETURN` to the C response types (`kimojio-tls/src/openssl_stream.c:232-272`).
- Client and server handshakes call `SSL_do_handshake`, set `TlsHandleState_Started` on success, and otherwise return `WantRead`, `WantWrite`, or an SSL failure based on the current state and `SSL_get_error` (`kimojio-tls/src/openssl_stream.c:275-362`).
- Shutdown calls `SSL_shutdown`, maps an initial zero return to `WantWrite`, and otherwise reuses the generic I/O result mapper (`kimojio-tls/src/openssl_stream.c:364-372`).

### Kimojio TLS Context and Stream Wrapper

- `TlsContext` stores a `TlsServerContext` from `kimojio-tls` (`kimojio/src/tlscontext.rs:18-20`).
- `TlsContext::from_openssl` accepts an `openssl::ssl::SslContext`, obtains the raw pointer via `ForeignType`, transfers ownership by forgetting the original OpenSSL wrapper, and stores it as `TlsServerContext` (`kimojio/src/tlscontext.rs:22-32`).
- `TlsContext::server` creates a server-side TLS handle, wraps it in `TlsStream`, drives `server_side_handshake`, and returns the stream (`kimojio/src/tlscontext.rs:34-45`).
- `TlsContext::client` creates a client-side TLS handle, wraps it in `TlsStream`, drives `client_side_handshake`, and returns the stream (`kimojio/src/tlscontext.rs:47-58`).
- `as_io_error` converts `TlsServerError` into `Errno`, emitting `Events::TlsError` for errno and OpenSSL error-stack codes, and maps TLS error stacks to `EPROTO` (`kimojio/src/tlscontext.rs:61-84`).
- `TlsStream` stores a `TlsServer` and optional `OwnedFd` socket (`kimojio/src/tlsstream.rs:24-27`).
- `TlsStream::client_side_handshake` and `server_side_handshake` loop until success, calling `try_read` or `try_write` on `WantRead`/`WantWrite` and mapping failure through `handle_tls_error` (`kimojio/src/tlsstream.rs:85-125`).
- `TlsStream::get_ssl` exposes an `&openssl::ssl::SslRef` from the raw SSL pointer without owning it (`kimojio/src/tlsstream.rs:127-133`).
- `TlsStream::split` clones the socket fd for the read half, stores read/write sockets in `AsyncLock<Option<OwnedFd>>`, moves the TLS handle into the read half, clones it for the write half, and returns `TlsReadStream`/`TlsWriteStream` (`kimojio/src/tlsstream.rs:136-166`).
- `try_read` obtains the read socket, gets a push buffer from OpenSSL, fills it with `operations::read_with_deadline`, and advances the OpenSSL BIO push buffer (`kimojio/src/tlsstream.rs:169-187`).
- `try_write` obtains the write socket, repeatedly reads pull-buffer data from OpenSSL, writes it with `operations::write_with_deadline`, and advances the pull buffer (`kimojio/src/tlsstream.rs:189-211`).
- `write_internal` calls `ssl.write`, advances the source slice on success, calls `try_read`/`try_write` on `WantRead`/`WantWrite`, and returns `EPIPE` on EOF (`kimojio/src/tlsstream.rs:213-234`).
- `tls_overhead` estimates overhead by dividing buffer size into 1024-byte frame units and multiplying by a 40-byte maximum TLS header length (`kimojio/src/tlsstream.rs:236-244`).
- `try_read_impl`, `read_impl`, `writev_impl`, `write_impl`, and `shutdown_impl` drive OpenSSL read/write/shutdown state machines and flush encrypted data as needed (`kimojio/src/tlsstream.rs:261-337`).
- `TlsStream`, `TlsReadStream`, and `TlsWriteStream` implement `AsyncStreamRead`/`AsyncStreamWrite` by delegating to the helper functions and closing the owned socket half on error (`kimojio/src/tlsstream.rs:348-486`).

### Generic Async Stream Traits Used by TLS

- `AsyncStreamRead` defines `try_read` for partial reads and `read` for exact-fill reads with optional deadlines (`kimojio/src/async_stream.rs:11-32`).
- `AsyncStreamWrite` defines `write`, `shutdown`, `close`, and a default `writev` implementation that writes each buffer sequentially (`kimojio/src/async_stream.rs:34-64`).
- `SplittableStream` defines associated read/write halves and async `split` (`kimojio/src/async_stream.rs:66-75`).
- `OwnedFdStream` wraps an optional `OwnedFd` with a 16 KiB read buffer and implements `split` by cloning the fd for the read half and moving the fd into the write half (`kimojio/src/async_stream.rs:77-149`).
- `OwnedFdStream` read helpers use `operations::read_with_deadline`, internal buffering, and EOF/error mapping (`kimojio/src/async_stream.rs:151-195`).
- `OwnedFdStream` write helpers use `operations::write_with_deadline`, `operations::writev_with_deadline`, `operations::shutdown`, and `operations::close` (`kimojio/src/async_stream.rs:233-288`).
- `OwnedFdStreamRead`, `OwnedFdStreamWrite`, and `OwnedFdStream` implement the async stream traits by delegating to those helper functions (`kimojio/src/async_stream.rs:197-231`, `kimojio/src/async_stream.rs:290-340`).

### Stack TLS Pattern on `origin/main` (`ssl2`)

- The `ssl2` module describes itself as a new experimental SSL implementation and states it wraps `OwnedFdStream` while making `openssl::ssl::SslStream` think it is using a synchronous stream (`kimojio/src/ssl2/mod.rs:4-12`).
- `ssl2::SslStream<S>` stores an `openssl::ssl::SslStream<io::SyncBufferedStream>` plus an underlying transport `S` (`kimojio/src/ssl2/mod.rs:24-28`).
- `SslStream::new` constructs OpenSSL's `SslStream` over a fresh `SyncBufferedStream` and stores the provided transport (`kimojio/src/ssl2/mod.rs:30-36`).
- For transports implementing both `AsyncStreamRead` and `AsyncStreamWrite`, `connect` and `accept` repeatedly call OpenSSL connect/accept, flush pending write-buffer bytes, fill read-buffer bytes on `WouldBlock`, and return I/O errors otherwise (`kimojio/src/ssl2/mod.rs:39-97`).
- `shutdown_internal` flushes the write buffer, calls OpenSSL shutdown, and handles `WouldBlock` by flushing or filling buffers until a shutdown result is produced (`kimojio/src/ssl2/mod.rs:99-130`).
- `try_read_internal` loops on `inner_s.read`, fills the read buffer on `WouldBlock`, and converts other I/O errors into `Errno` (`kimojio/src/ssl2/mod.rs:132-159`).
- `write_internal` loops on `inner_s.write`, flushes the write buffer after successful writes or `WouldBlock`, and converts other I/O errors into `Errno` (`kimojio/src/ssl2/mod.rs:161-195`).
- `ssl2::SslStream` implements `AsyncStreamWrite` and `AsyncStreamRead` for transports that implement both traits (`kimojio/src/ssl2/mod.rs:198-246`).
- `SyncBufferedStream` is a synchronous `Read`/`Write` adapter backed by `VecDeque` read and write buffers with EOF state (`kimojio/src/ssl2/io.rs:14-20`).
- `SyncBufferedStream::read` reads from the first contiguous read-buffer segment and returns `WouldBlock` when no buffered data is available (`kimojio/src/ssl2/io.rs:32-51`).
- `SyncBufferedStream::write` appends bytes to the write buffer, and `flush` is a no-op because external code calls `flush_write_buff` (`kimojio/src/ssl2/io.rs:54-64`).
- `fill_read_buff` reads up to 1024 bytes from an async stream into the read buffer and marks EOF on zero bytes; `flush_write_buff` writes the current contiguous write buffer to an async stream, clears it, and returns the flushed size (`kimojio/src/ssl2/io.rs:67-103`).
- `ssl2` unit tests create OpenSSL connector/acceptor contexts, exercise Rust-client/C-server, Rust-server/Rust-client, and C-client/Rust-server scenarios over `bipipe`, and assert “hello”/“goodbye” payload exchange plus shutdown states (`kimojio/src/ssl2/mod.rs:300-361`, `kimojio/src/ssl2/mod.rs:363-467`).
- `ssl2/e2e_tests.rs` adds a test constructing `TlsContext` from OpenSSL contexts, creating client/server streams via the existing C-wrapper path, and exchanging “hello”/“goodbye” over `bipipe` (`kimojio/src/ssl2/e2e_tests.rs:13-31`, `kimojio/src/ssl2/e2e_tests.rs:33-54`, `kimojio/src/ssl2/e2e_tests.rs:56-73`).

### TLS Tests and Test Utilities

- TLS tests live inside `kimojio/src/tlscontext.rs` under `#[cfg(test)] pub(crate) mod test` and import runtime operations, socket helpers, `TlsStream`, and async stream traits (`kimojio/src/tlscontext.rs:86-116`).
- `test_echo_example` creates certs, starts a server listener on port 8999, spawns two client loops, waits for both clients, signals a done event, and waits for the server task (`kimojio/src/tlscontext.rs:166-186`).
- `client_loop_split` writes a 1 MiB payload over split read/write halves, reads it back, then sends two 512-byte buffers with `writev` and reads/asserts the echoed 1024-byte response (`kimojio/src/tlscontext.rs:209-239`).
- `client_loop` performs the same 1 MiB echo and two-buffer `writev` flow on an unsplit `TlsStream` (`kimojio/src/tlscontext.rs:241-269`).
- `server_listener` accepts sockets until a done event fires, updates accept socket options, spawns `server_loop` for each accepted client, and awaits collected tasks (`kimojio/src/tlscontext.rs:271-292`).
- `server_loop` creates a TLS server stream, splits it, repeatedly `try_read`s up to 8192 bytes and writes the same bytes back until EOF, then shuts down and closes the write half (`kimojio/src/tlscontext.rs:294-316`).
- Version restriction tests assert the TLS minimum protocol version is TLS 1.3 (`0x0304`) for both server and client contexts (`kimojio/src/tlscontext.rs:318-352`).
- CRL tests create `bipipe` pairs, generate certs, create CRLs with the OpenSSL command line, and assert handshake failure when revoked certificates are used (`kimojio/src/tlscontext.rs:354-436`, `kimojio/src/tlscontext.rs:438-507`, `kimojio/src/tlscontext.rs:974-1069`).
- Test context builders create OpenSSL client/server contexts, set certificate/private key/CA files, set peer verification flags, set minimum protocol TLS 1.3, optionally load CRLs, check private keys, and convert to `TlsContext` (`kimojio/src/tlscontext.rs:852-894`, `kimojio/src/tlscontext.rs:896-940`).
- Test certificate utilities define default cert/key/CA/lock paths, create a locked setup around cert generation, generate CA/server/client certificates with `rcgen`, and write PEM contents to files (`kimojio/src/tlscontext.rs:838-850`, `kimojio/src/tlscontext.rs:1071-1129`, `kimojio/src/tlscontext.rs:1131-1170`, `kimojio/src/tlscontext.rs:1237-1344`).
- The async test macro `#[kimojio::test]` expands async test functions into synchronous `#[test]` functions that call `::kimojio::run_test` (`kimojio-macros/src/lib.rs:99-173`).
- Virtual-clock integration tests are feature-gated with `#![cfg(feature = "virtual-clock")]` and use `#[kimojio::test]` plus `operations::virtual_clock_enable`/`advance`/`poll_once` (`kimojio/tests/virtual_clock_integration.rs:1-18`, `kimojio/tests/virtual_clock_integration.rs:21-76`).

### Benchmark and Performance Conventions

- The workspace dependency list includes Criterion with default features disabled (`Cargo.toml:21-24`), and `kimojio` dev-dependencies include Criterion and test cert helpers (`kimojio/Cargo.toml:58-61`).
- `kimojio` registers four Criterion bench targets without the default harness: `iouring`, `mut_in_place_cell_bench`, `async_bench`, and `async_stream_bench` (`kimojio/Cargo.toml:63-77`).
- `iouring` benchmarks raw `Nop`, kimojio `nop`, parallel `nop`, `yield_io`, task spawn, socket one-way, `bipipe` one-way, pipe one-way, and thread-local storage patterns (`kimojio/benches/iouring.rs:25-245`).
- The `iouring` benchmark uses `iter_custom` for runtime-backed measurements to keep io_uring initialization outside the measured duration (`kimojio/benches/iouring.rs:17-24`, `kimojio/benches/iouring.rs:35-83`).
- `async_bench` benchmarks `AsyncEvent`, ping-pong events, `AsyncLock`, async channels, channel wait, and sleep overshoot using `kimojio::run` inside `iter_custom` (`kimojio/benches/async_bench.rs:13-153`).
- `async_stream_bench` benchmarks direct async stream write/read over `bipipe` and two manual channel-stream shapes (`kimojio/benches/async_stream_bench.rs:13-123`).
- The closest existing RPC-shaped benchmark pattern is `manual_channel_stream`, which writes a length, id, and iteration payload via three `IoSlice`s, reads a length-prefixed response, extracts the id, and validates the payload (`kimojio/benches/async_stream_bench.rs:39-79`).
- `manual_read_all_macro_channel_stream` writes a fixed buffer containing length/id/payload fields, reads a length-prefixed response, extracts the id, and validates the payload (`kimojio/benches/async_stream_bench.rs:81-123`).
- Each Criterion bench file defines a `criterion_group!` with one-second warmup and four-second measurement time, then calls `criterion_main!` (`kimojio/benches/iouring.rs:247-252`, `kimojio/benches/async_bench.rs:155-160`, `kimojio/benches/async_stream_bench.rs:125-130`, `kimojio/benches/mut_in_place_cell_bench.rs:24-29`).

### TLS Example Application

- The TLS echo server example creates an OpenSSL TLS 1.3 acceptor, loads server certificate/private key/CA, enables peer certificate verification, checks the private key, and converts the context with `TlsContext::from_openssl` (`examples/src/bin/tls_echo_server.rs:43-81`).
- `handle_client` performs `tls_context.server(16384, client_socket, None)`, then loops on `try_read`, writes the same bytes back, and shuts down/closes the TLS stream (`examples/src/bin/tls_echo_server.rs:83-135`).
- The server creates a listening socket, accepts connections, updates accept socket options, and spawns one kimojio task per client (`examples/src/bin/tls_echo_server.rs:137-176`).
- The example uses `#[kimojio::main]` for its async main, validates the cert/key/CA file paths, and then runs the server loop (`examples/src/bin/tls_echo_server.rs:179-203`).
- The example docs show how to build/run the binary and test it with `openssl s_client`, curl, or ncat (`examples/tls-echo-server.md:12-29`, `examples/tls-echo-server.md:97-158`, `examples/tls-echo-server.md:171-196`).

## Code References

- `Cargo.toml:1-11` - Workspace member list and resolver.
- `Cargo.toml:21-61` - Workspace dependencies relevant to TLS, testing, benchmarking, and Rust I/O.
- `kimojio/Cargo.toml:17-25` - Optional `kimojio-tls` and `openssl` dependencies in the runtime crate.
- `kimojio/Cargo.toml:34-52` - Default `tls` feature and temporary `ssl2` feature.
- `kimojio/Cargo.toml:63-77` - Registered Criterion benchmark targets.
- `kimojio/src/lib.rs:41-51` - Feature-gated `ssl2`, `tlscontext`, and `tlsstream` modules.
- `kimojio-tls/Cargo.toml:1-16` - Standalone OpenSSL integration crate metadata and dependencies.
- `kimojio-tls/build.rs:3-12` - C compilation and OpenSSL dynamic linking.
- `kimojio-tls/src/lib.rs:141-181` - FFI function declarations for C TLS handle operations.
- `kimojio-tls/src/lib.rs:183-207` - Rust wrapper structs, Send declarations, and response enum.
- `kimojio-tls/src/lib.rs:209-249` - Raw response to Rust response/error mapping.
- `kimojio-tls/src/lib.rs:264-300` - Context creation of server/client TLS handles and raw OpenSSL context ownership.
- `kimojio-tls/src/lib.rs:310-397` - TLS handle operations, clone, and drop.
- `kimojio-tls/src/openssl_stream.c:37-43` - C handle shape with `SSL`, BIO pair, and state.
- `kimojio-tls/src/openssl_stream.c:94-118` - Handle duplication via `SSL_up_ref`.
- `kimojio-tls/src/openssl_stream.c:159-200` - `SSL_new`, BIO pair creation, role selection, and `SSL_set_bio`.
- `kimojio-tls/src/openssl_stream.c:202-230` - BIO push/pull buffer APIs.
- `kimojio-tls/src/openssl_stream.c:232-272` - `SSL_read`/`SSL_write` response mapping.
- `kimojio-tls/src/openssl_stream.c:275-372` - Handshake and shutdown response mapping.
- `kimojio/src/tlscontext.rs:18-84` - `TlsContext` construction, client/server stream creation, and TLS error event mapping.
- `kimojio/src/tlscontext.rs:166-316` - Echo test client/server loops and listener.
- `kimojio/src/tlscontext.rs:852-940` - OpenSSL client/server context builders used by tests.
- `kimojio/src/tlscontext.rs:974-1069` - OpenSSL command-line CRL generation in tests.
- `kimojio/src/tlscontext.rs:1071-1170` - Test certificate path structures and setup helpers.
- `kimojio/src/tlscontext.rs:1237-1344` - CA/server/client certificate generation and PEM writing.
- `kimojio/src/tlsstream.rs:24-27` - `TlsStream` state.
- `kimojio/src/tlsstream.rs:85-125` - Client/server handshake loops.
- `kimojio/src/tlsstream.rs:136-166` - TLS stream splitting into read/write halves.
- `kimojio/src/tlsstream.rs:169-211` - Socket/BIO read and write pumping.
- `kimojio/src/tlsstream.rs:213-337` - TLS read/write/shutdown state machine helpers.
- `kimojio/src/tlsstream.rs:348-486` - Async stream trait implementations for TLS stream halves.
- `kimojio/src/async_stream.rs:11-75` - Generic stream traits used by TLS wrappers.
- `kimojio/src/async_stream.rs:77-149` - `OwnedFdStream` state and splitting behavior.
- `kimojio/src/async_stream.rs:151-340` - `OwnedFdStream` read/write/shutdown/close implementations.
- `kimojio/src/ssl2/mod.rs:4-12` - Experimental stack TLS module description.
- `kimojio/src/ssl2/mod.rs:24-246` - Stack TLS stream implementation over `openssl::ssl::SslStream` and async transports.
- `kimojio/src/ssl2/mod.rs:300-467` - Stack TLS unit tests across C/Rust client/server combinations.
- `kimojio/src/ssl2/io.rs:14-103` - `SyncBufferedStream` buffering adapter.
- `kimojio/src/ssl2/e2e_tests.rs:13-73` - End-to-end OpenSSL context/TLS stream test.
- `kimojio/src/task_pool.rs:11-43` - Existing kimojio task concurrency pool helper.
- `kimojio-macros/src/lib.rs:13-97` - Async `#[kimojio::main]` macro.
- `kimojio-macros/src/lib.rs:99-173` - Async `#[kimojio::test]` macro.
- `kimojio/benches/async_stream_bench.rs:39-123` - Existing RPC-shaped stream benchmarks.
- `examples/src/bin/tls_echo_server.rs:43-203` - TLS echo server example flow.
- `.github/workflows/rust.yml:34-50` - CI build, test, doc, format, and all-feature clippy commands.

## Architecture Documentation

- The current OpenSSL layering has a low-level C-backed TLS handle layer (`kimojio-tls`) underneath runtime-specific async stream wrappers (`kimojio/src/tlscontext.rs` and `kimojio/src/tlsstream.rs`) (`kimojio-tls/build.rs:3-12`, `kimojio/src/tlscontext.rs:18-58`, `kimojio/src/tlsstream.rs:24-125`).
- The low-level C handle uses a memory BIO pair to decouple OpenSSL from socket I/O; Rust code gets writable BIO memory for network reads and readable BIO memory for network writes (`kimojio-tls/src/openssl_stream.c:159-200`, `kimojio-tls/src/openssl_stream.c:202-230`, `kimojio/src/tlsstream.rs:169-211`).
- OpenSSL progress is represented as `Success`, `Fail`, `Eof`, `WantRead`, or `WantWrite`, and higher layers respond by reading or writing the underlying socket before retrying OpenSSL (`kimojio-tls/src/lib.rs:201-249`, `kimojio/src/tlsstream.rs:85-125`, `kimojio/src/tlsstream.rs:261-337`).
- The stack TLS experiment uses the opposite shape: OpenSSL's Rust `SslStream` is kept intact and supplied with a synchronous buffered stream adapter, while async transport I/O is manually pumped into and out of that adapter (`kimojio/src/ssl2/mod.rs:4-12`, `kimojio/src/ssl2/mod.rs:24-39`, `kimojio/src/ssl2/io.rs:14-103`).
- Current TLS tests commonly use in-memory `bipipe` transports for client/server pairs and generated certificates from test utilities (`kimojio/src/tlscontext.rs:354-358`, `kimojio/src/tlscontext.rs:582-598`, `kimojio/src/ssl2/mod.rs:408-466`, `kimojio/src/ssl2/e2e_tests.rs:56-73`).
- Current benchmarking style uses Criterion `iter_custom` with `kimojio::run` inside the timing closure for runtime-backed operations and one-second warmup/four-second measurement windows (`kimojio/benches/iouring.rs:17-24`, `kimojio/benches/iouring.rs:247-252`, `kimojio/benches/async_stream_bench.rs:125-130`).

## Open Questions

None requiring user input for this code research artifact.
