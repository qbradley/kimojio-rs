# HTTP/1 wrapper lease forwarding

## Scope and API

This change adds an owned lease path to `kimojio-http1`.
The existing `OutgoingFrame::Data(Vec<u8>)` constructor remains available.
The new `OutgoingFrame::Forward(BodyChunk)` constructor transfers ownership without a payload copy.
An application can mix both constructors in `OutgoingBody::from_stream`.
Exhaustive matches on `OutgoingFrame` need a `Forward` arm.

`OutgoingBody::from_incoming(body)` forwards each data lease and the optional trailers.
This helper declares streaming framing.
It does not transfer request or response headers.
The wrapper retains the public generic callback architecture of the core.
This change does not modify the core or the runtime.

```rust
async fn echo(
    request: kimojio_http1::http::Request<kimojio_http1::IncomingBody>,
) -> Result<kimojio_http1::http::Response<kimojio_http1::OutgoingBody>, kimojio_http1::Error> {
    let mut incoming = request.into_body();
    incoming.accept().await?;
    Ok(kimojio_http1::http::Response::new(
        kimojio_http1::OutgoingBody::from_incoming(incoming),
    ))
}
```

For a known outgoing length, an application can yield `OutgoingFrame::Forward(chunk)` from `OutgoingBody::from_stream(Some(length), source)`.
The application remains responsible for that exact length.
The `from_incoming` helper does not infer a length from HTTP headers.

## Alternatives and choice

| Alternative | Consequence |
| --- | --- |
| Copy each lease into a `Vec<u8>` | Simple ownership, but one payload allocation and copy per nonempty chunk |
| Add a generic public body type | More wrapper type parameters and changes to ordinary handlers |
| Box each outgoing payload behind a trait | Allocation and dynamic dispatch for each frame |
| Convert receive storage to shared ownership | Changes the receive-buffer contract and obscures exclusive return to the core |
| Use an inline outgoing storage enum | Preserves existing constructors and moves a lease through the existing core write path |

The wrapper uses the inline enum.
Its private `OutgoingData` contains either an owned vector or a `BodyChunk`.
The receive machine still uses `Vec<u8>`.
The outgoing machine uses the independent `W: AsRef<[u8]>` parameter.
`WriteOp`, `WriteCompletion`, and `BodySent` carry that same storage type.

Shared immutable outgoing storage is not part of this change.
It does not solve exclusive receive-lease return.
An additional shared variant also needs an explicit retained-allocation contract for sliced storage.
The enum permits a later extension without a new callback architecture.

## Ownership and settlement

One receive operation owns the complete receive allocation.
Its `BodyOp` identifies only the visible body range.
`BodyChunk` owns that operation and a sender for the existing single-slot return channel.
The chunk is neither cloned nor converted into a vector.

| State | Owner and return behavior |
| --- | --- |
| Source has not yielded the frame | The source owns the lease. Source destruction returns it. |
| Frame has not entered the core | The frame or rejected command owns the lease. Destruction returns it. |
| Core has admitted the frame | The core command or original `WriteOp` owns the lease. |
| Cancellation request exists | The original operation still owns the lease. No return occurs. |
| Original operation settles | `WriteCompletion` returns the operation to the core. |
| Core emits `BodySent` | The receipt owns the lease, including after an error. |
| Wrapper drops the receipt | The chunk sends exactly one receive completion through the return channel. |
| Source driver receives that completion | The source core regains its buffer and input credit. |

The existing `BodyChunk` destructor consumes its `Option<BodyOp>` once.
Each source machine permits one receive lease, so its return channel has capacity for that lease.
Cross-connection forwarding uses the same channel as ordinary chunk destruction.
There is no new queue or event implementation.

`source_finished` and `BodySent` have different meanings.
The first permits source destruction.
It does not permit destruction of an admitted outgoing payload.
The native worker requests cancellation and continues to await the original operation.
The lease survives partial completions and write-all continuations.

A transport error can hide partial progress.
The existing `UnknownProgress` and `CancelledUnknownProgress` results remain lower bounds.
The wrapper does not replay a failed forwarded frame.
A successful original completion remains a success after a cancellation request.

If the source driver no longer exists, the return channel rejects the completion.
That rejected completion owns and releases the allocation.
It cannot restore a closed connection.
Abrupt driver destruction still follows native resource destruction, not normal asynchronous settlement.

## Visible bytes and retained allocation

The outgoing byte view is `BodyChunk::as_ref()`.
It exposes only `BodyOp::bytes()`, not the headers, chunk metadata, buffered suffix, or unused allocation space.
The outgoing range is `0..chunk.len()` within this view.
The pointer remains the original pointer into the receive allocation.

`BodyChunk::retained_capacity()` reports the complete vector capacity.
The wrapper records that capacity immediately after receive allocation.
The generic core can access slices, but cannot resize or replace this vector.
The capacity therefore stays valid through receive operations and body leases.

The destination applies two independent limits:

- Visible payload length must fit the current outgoing capacity.
- Complete retained capacity must fit `Config::protocol.max_buffer_bytes`.

A five-byte chunk from a 32-KiB receive allocation cannot enter a destination with a 16-KiB buffer limit.
The wrapper returns `Error::Limit` and drops the unadmitted lease.
This rule also applies to a large-capacity `Data(Vec<u8>)` with a short visible length.
The wrapper does not silently copy a lease to make it fit.

## Supported cases and integration

- Same-connection echo supports multiple chunks, binary bytes, trailers, and `Expect: 100-continue`.
- Cross-connection forwarding supports multiple chunks and trailers without a payload copy in the wrapper.
- Sources can mix owned vectors and forwarded leases.
- Empty vector frames do not terminate a source.
- Source failure, rejected commands, source destruction, and destination rejection retain the existing cancellation policy.
- Native tests use a three-second timeout for the new forwarding scenarios.

Same-connection handlers call `IncomingBody::accept()` before they return a response.
This grants input credit before the final response starts.
The current core closes an exchange whose response starts before the complete request arrives.
The echo regression explicitly expects `Connection: close`.
Forwarding does not opt into reusable duplex behavior.

Cross-connection forwarding requires concurrent polling of both drivers.
A held lease prevents another body delivery from its source.
The destination must settle the write before the source can advance.
The caller must not wait for source completion before it polls the destination.

A dropped destination source drops its retained `IncomingBody`.
For an unfinished client response, this cancels the source exchange.
For a server request, the existing policy discards permitted future input.
A custom transport that never settles cannot promise bounded cancellation.

Integration with a future explicit duplex policy belongs in the core.
That policy must not weaken receipt ownership or input-credit bounds.
Tests for reusable duplex need a separate opt-in configuration.
The current-policy close assertion remains valid for the default configuration.

## Costs and limitations

The forwarding variant adds no per-frame box, shared allocation, or payload copy.
It moves a larger inline descriptor through the existing channels and core operations.
Each lease retains a sender reference and one capacity value.
The `from_incoming` helper uses the existing boxed source representation once per body.

On this x86-64 build, Clippy reports 280-byte `WriteAction` and 288-byte `WriteResult` variants.
These fixed single-slot channels intentionally retain inline operations.
The two enums acknowledge `clippy::large_enum_variant` with explicit allocation-related reasons.
Boxing these variants trades smaller descriptors for one allocation per operation or receipt.
The benchmark must measure that tradeoff rather than assume that either representation is faster.

The receive allocation remains unavailable to the source until the outgoing receipt returns.
This couples input progress to destination write progress.
The wrapper still allocates channels, metadata, cancellation tokens, and source boxes.
`OwnedFdStream` still copies through its internal receive buffer.
Kernel and transport copies remain outside this wrapper guarantee.

There is no throughput, latency, or total-allocation claim.
Pointer tests prove storage identity, not a zero-cost abstraction.
No timing or profiling ran on CPUs 0–7.
The parent benchmark work measures performance separately.

The public API does not expose outgoing receipts to the application.
The wrapper consumes those receipts and returns leases internally.
The helper does not supply retry, fan-out, shared leases, length inference, or buffer-limit conversion.

## Test evidence

`src/forwarding_tests.rs` covers:

- Original allocation identity and the visible payload range.
- Two partial writes with the same payload allocation.
- Exact and lower-bound receipts.
- Cancellation before original completion, including a late successful completion.
- Lease retention after native cancellation and before original settlement.
- Lease retention after settlement and before receipt destruction.
- Single return after source destruction, frame destruction, and command rejection.
- Destruction after the return receiver no longer exists.

`tests/forwarding.rs` covers:

- A 65,573-byte cross-connection body with byte-exact data and trailers.
- Actual transport-write pointers inside the original receive allocation.
- Rejection of a small lease with excessive retained capacity.
- Early destination rejection and source destruction without a deadlock.

The existing echo regression now uses the forwarding helper.
It covers a gated upload, multiple chunks, empty frames, binary bytes, trailers, and both continue-handshake modes.
Existing settlement regressions still cover dropped consumers, native partial progress, and source release during a pending write.

All commands run from the detached forwarding worktree.
The build directory is separate from other wrapper experiments.

```sh
export CARGO_TARGET_DIR=/workspace/kimojio-rs/target/wrapper-lab/build-forwarding
taskset -c 8-31 cargo fmt -p kimojio-http1
taskset -c 8-31 timeout 90s cargo test -p kimojio-http1 --all-targets --features virtual-clock
taskset -c 8-31 timeout 240s cargo test -p kimojio-http1 --all-targets --features virtual-clock --release
taskset -c 8-31 timeout 90s cargo test -p kimojio-http1 --all-targets --quiet
taskset -c 8-31 timeout 90s cargo test -p kimojio-http1 --all-targets --release --quiet
taskset -c 8-31 cargo clippy --quiet
taskset -c 8-31 cargo clippy --quiet --all-targets --all-features
taskset -c 8-31 cargo clippy --quiet -p kimojio-http1 --all-targets --all-features -- -D warnings
taskset -c 8-31 cargo test -p kimojio-http1 --doc
```

With `virtual-clock`, the debug and release suites each passed all 47 tests.
Without that feature, both suites passed all 45 tests.
Both workspace Clippy commands completed successfully.
The workspace has pre-existing warnings in `examples/http1-static/src/app.rs` and `kimojio/src/pipe.rs`.
The focused wrapper lint command passed with `-D warnings`.
The documentation test command passed but found no executable documentation tests.
