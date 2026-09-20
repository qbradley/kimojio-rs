# Exact native transport prerequisite

## Objective and boundaries

This change adds an explicit native backend to the conventional HTTP/1 wrapper.
Its base is `90b7ff60fcd0f582a8978fb4fbe3a45876f6af18`.
It precedes the four optimization PoCs.
It is not the minimum-operation-overhead PoC.

The implementation uses one driver and one protocol state implementation.
It does not change HTTP policy, body forwarding storage, or duplex response selection.
It does not change `keepalive_bench` or the comparison runner.
It adds no dependency or unsafe block.
No performance measurement accompanies this change.

## Public API

The existing APIs and public generic driver type remain available:

```rust
connect<S: SplittableStream>(stream: S, config: Config) -> (Client, Connection<S>)
serve_connection(stream, config, handler)
serve_connection_with_shutdown(stream, config, shutdown, handler)
```

The explicit native APIs consume an established `kimojio::OwnedFd`:

```rust
connect_native(socket, config) -> (Client, NativeConnection)
serve_connection_native(socket, config, handler)
serve_connection_native_with_shutdown(socket, config, shutdown, handler)
```

`NativeConnection::run` has the same polling and shutdown contract as `Connection::run`.
`Client`, `IncomingBody`, `OutgoingBody`, configuration, and handlers are shared.
Neither constructor connects a socket or performs DNS resolution.
The native backend does not supply TLS.

## Implementation

`transport.rs` defines the private connection-level split interface.
`StreamTransport` delegates to `SplittableStream`.
`NativeTransport` creates native read and write halves for one descriptor.
`io.rs` defines the private receive, transmit, and close interfaces.
The generic stream implementations preserve their scope cancellation and write-all error mapping.

The same read worker, write worker, and `driver::run` serve both backends.
The native path has no second parser, protocol machine, source loop, or body cursor.
The native halves remain statically dispatched.
They add no per-operation boxed future.

### Exact operations

A native receive passes `ReadOp::bytes_mut()` to `operations::read`.
The operation writes directly into the core's receive storage.
There is no `OwnedFdStream` buffer or staged payload copy.

A native transmit borrows the slices of the original `WriteOp`.
It submits one `operations::writev_with_timeout` with no timeout or offset override.
The original completion supplies the exact byte count.
A positive short result returns immediately to the HTTP machine.
Only that machine decides whether another operation is necessary.

Native `ECANCELED` maps to `IoErrorKind::Cancelled`.
Native `EINTR` maps to `Interrupted`, which leaves retry policy with the machine.
Other native errors map to `Other`.
A native error does not conceal a previous write-all prefix.
Thus, native forwarded receipts retain exact acceptance instead of a generic write-all lower bound.

The generic backend still maps write-all errors to `UnknownProgress` or `CancelledUnknownProgress`.
That distinction remains necessary because a stream adapter can accept a prefix before its final error.

### Cancellation and ownership

The native path uses the original-operation cancellation pattern from `examples/websocket-chat/src/native.rs`.
It does not depend on the example crate.
A canceled token prevents creation of a new native operation.
For an existing operation, `settle_one` races the original future against the token.
If cancellation wins, it calls `cancel` on that original future and then awaits it.
A late success remains a success with its original byte count.

The native path has no write-all continuation and needs no cancellation scope around each operation.
It retains the operation, slices, receive storage, and descriptor owner until the original future settles.
The write worker returns the original `WriteOp` in its completion.
Forwarded receive leases remain in that operation until the core returns their body receipt.

### Actual close

The two native halves share an `Rc<OwnedFd>`.
They do not duplicate the descriptor.
The read half also shares a stop event with the write half.

The driver requests close only after its pending read and write completions return.
It drops the read-request sender before it queues writer close.
The read worker then drops its half.
The native read destructor releases its descriptor owner before it signals the stop event.

The native writer awaits that event.
It extracts the sole remaining `OwnedFd` with `Rc::try_unwrap`.
It then awaits `operations::close` before it returns the close completion.
Dropping one `Rc` is not a successful close result.
The tests require peer EOF after close.

The driver also stops polling the read-completion channel after it closes the read-request channel.
Without this guard, the earlier reader shutdown exposed a completed channel future to another poll.
The existing generic regression suite caught that error during implementation.

## Benchmark integration recipe

Keep the request, response, body, and shutdown loops unchanged.
Select the constructor at connection setup.
For the generic path, wrap the descriptor in `OwnedFdStream`.
For the native path, pass the descriptor directly to `connect_native`.

The two driver types differ.
If a shared local variable is necessary, box both driver futures at the same boundary:

```rust
use futures::{FutureExt, future::LocalBoxFuture};
use kimojio_http1::{Client, Error, connect, connect_native};

let (client, driver): (Client, LocalBoxFuture<'_, Result<(), Error>>) =
    if use_native {
        let (client, connection) = connect_native(socket, config);
        (client, connection.run().boxed_local())
    } else {
        let (client, connection) = connect(kimojio::OwnedFdStream::new(socket), config);
        (client, connection.run().boxed_local())
    };
```

For the server, select `serve_connection_native` or `serve_connection` at the same boundary.
Use the corresponding shutdown variant when the benchmark supplies a `Shutdown` handle.
The transport choice does not select duplex behavior.
The independent wrapper duplex change applies to both backends through their shared driver.

Record the backend in each benchmark result.
Keep payloads, framing, deadlines, connection policy, and future-boxing boundaries equal.
This commit does not modify the benchmark or prescribe a winner.

## Test evidence

The new primitive tests cover:

- Exact vectored byte counts.
- Reads into only the supplied slice, with no hidden prefetch of remaining socket bytes.
- Positive short writes and no hidden suffix retry.
- Pending-read cancellation and pre-canceled admission.
- Blocked-write cancellation before payload return.
- Close that waits for reader release and produces peer EOF.
- A controlled late-success race that preserves the original exact count.

The raw worker forwarding test covers both native cancellation and native success after a cancellation request.
It asserts the original lease pointer, exact receipt acceptance, and lease release only after the receipt.

The new integration tests cover:

- All four generic/native client/server combinations.
- Three exchanges over each single established connection.
- Chunked 64-KiB responses and trailers with a restricted socket send buffer.
- Cross-connection native forwarding of the complete payload and trailers.
- Abort during a pending native read, followed by peer EOF.

The existing generic tests still cover unknown-progress errors, virtual deadlines, source release, body leases, and shutdown.

Run from the isolated worktree:

```sh
cd /workspace/kimojio-rs/target/wrapper-lab/worktrees/raw-transport
export CARGO_TARGET_DIR=/workspace/kimojio-rs/target/wrapper-lab/build-raw-transport
taskset -c 8-31 timeout 120s cargo test -p kimojio-http1 --lib --test native_transport
taskset -c 8-31 timeout 120s cargo test -p kimojio-http1 --all-targets --features virtual-clock
taskset -c 8-31 cargo fmt
taskset -c 8-31 cargo clippy
taskset -c 8-31 cargo clippy --all-targets --all-features
```

The default-feature command passed 22 tests: 19 library and 3 native transport tests.
The all-target command passed 61 tests: 19 library, 17 connection, 3 forwarding, 3 native transport, 10 settlement, and 9 example tests.
Both workspace Clippy commands exited successfully.
They reported only existing warnings at `examples/http1-static/src/app.rs:413` and `kimojio/src/pipe.rs:64–65`.
No build or test command used CPUs 0 through 7.

## Remaining limits and costs

- Native operation tests require Linux and available io_uring support.
- Native errors other than `EINTR` and `ECANCELED` remain terminal, including unexpected `EAGAIN`.
- This backend adds no user-space readiness loop, TLS adapter, connection pool, or retry mechanism.
- Abrupt driver destruction still uses resource destructors, not the normal cooperative close sequence.
- The existing worker channels, operation cancellation tokens, and connection-level boxes remain.
- Native splitting adds a shared descriptor allocation and a shared reader-stop event allocation per connection.
- Kernel-internal copies remain outside the wrapper's payload-copy claim.
- This change does not establish allocation-free operation or faster throughput.
- The complete workspace test suite did not run. Both workspace Clippy configurations did run.
