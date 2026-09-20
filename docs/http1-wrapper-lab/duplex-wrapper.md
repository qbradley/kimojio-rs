# Explicit duplex responses in the HTTP/1 wrapper

## API and scope

`OutgoingBody::continue_request_body()` selects a server response policy.
The wrapper uses `Server::respond_duplex` only for that response.
The body stores one Boolean flag.
The selection needs no `http::Extensions` entry or additional allocation.

```rust
async fn duplex_echo(
    request: kimojio_http1::http::Request<kimojio_http1::IncomingBody>,
) -> Result<kimojio_http1::http::Response<kimojio_http1::OutgoingBody>, kimojio_http1::Error> {
    let mut incoming = request.into_body();
    incoming.accept().await?;
    Ok(kimojio_http1::http::Response::new(
        kimojio_http1::OutgoingBody::from_incoming(incoming)
            .continue_request_body(),
    ))
}
```

The default policy remains conservative.
The example `/echo` route still uses that policy.
An early `/early` rejection still discards permitted input and closes according to the existing core policy.
This change adds no example route and changes no benchmark mode.

All body constructors use the default policy.
An application can apply the builder to an empty, complete, streaming, or forwarded response body.
Client request bodies reject the flag with `Error::InvalidMetadata` before the core admits a request.
The client connection remains available for a valid request.

The handler must retain a consumer for the request.
A response source can own that consumer.
Alternatively, another application future can own and poll the consumer while the server future runs.
The wrapper does not create a background consumer.

## Three independent lifetimes

### Outgoing source lifetime

The core emits `source_finished` as soon as it needs no more outgoing frames.
A known-length source can finish before its final write starts.
The wrapper drops that source at this notification.
Source destruction can also drop an `IncomingBody` that the source owns.

### Original transport lifetime

An admitted `Forward` frame owns the incoming lease through its original outgoing operation.
Cancellation requests do not release this lease.
The original operation must settle and return its outgoing receipt.
Receipt destruction sends the receive completion through the existing return channel.

The new response policy does not change this ownership.
Native partial writes and write-all continuations still retain the original storage.
Source failure and protocol cancellation still require operation settlement.

### Incoming consumer lifetime

`incoming_finished` means that the core completed the input.
Response completion and source completion do not substitute for this notification.
Connection reuse still requires complete input, complete output, and returned operations and leases.
The core retains ownership of framing, deadlines, limits, and reuse decisions.

If an unfinished `IncomingBody` drops, its existing cancellation token records consumer abandonment.
Ordinary server responses retain their existing discard behavior.
Explicit duplex responses instead cancel an input stream that remains incomplete.

## Deferred abandonment

Immediate cancellation on consumer destruction is incorrect for a known-length forwarding source.
That source can drop its consumer while its final outgoing frame still owns the final incoming lease.
The source no longer needs frames, but the original write remains valid.

The wrapper records whether one incoming lease is outstanding for the active exchange.
It sets this state at `BodyOp` delivery.
A successful `release_body` clears the state.
The state records ownership, not payload counts or HTTP framing.

The abandonment sequence is:

1. The wrapper observes the cancelled consumer token once.
2. It disables future delivery to that consumer.
3. It waits for an outstanding incoming lease to return.
4. It drains core notifications.
5. If `incoming_finished` arrived, no abandonment cancellation is necessary.
6. Otherwise, it cancels the explicit duplex exchange.

The existing cancellation-observed flag stops repeated polling of the already-cancelled token.
While a lease remains outstanding, the driver waits for actual input, write, release, or deadline progress.
It does not use the cancelled token as a repeated wake source.

A returned lease does not grant new input credit to an abandoned duplex consumer.
Credit that the core already holds can still produce another body delivery.
That new delivery proves that the request contained more input than the final retained lease.
The wrapper cancels immediately instead of silently discarding that new payload and permitting reuse.

Both paths use `cancel_exchange`.
They do not manufacture an input-completion event.
Cancellation also marks new core work as runnable, so the driver cannot sleep before it processes cancellation notifications.

## Known-length forwarding

A custom source can use `OutgoingBody::from_stream(Some(length), source).continue_request_body()`.
It can yield `OutgoingFrame::Forward(chunk)` while its state owns the incoming body.
Automatic source destruction does not cancel its outstanding final write.
After receipt return, the core can report input completion and permit reuse.

Tests cover fixed-length input and chunked input with buffered terminal metadata.
They hold the final write after one positive partial completion.
The next pipelined request cannot reach the handler until the final write and incoming lease settle.

The wrapper cannot infer that an input payload is final from an outgoing length.
It does not reconstruct content-length or chunked-framing counters.
If chunked terminal metadata still requires additional input after consumer destruction, the core has not completed that input.
The wrapper cancels that incomplete exchange after the notification drain.
An application that needs later input or trailers must keep a consumer alive independently or use the streaming `from_incoming` helper.

An outstanding lease does not excuse genuine abandonment.
The lease can contain only a prefix of a larger request.
Tests cover both buffered and absent suffix bytes.
Neither case permits a second response or connection reuse.

## Completion order and failure

The response can finish before the request body arrives.
The request consumer must continue to grant credit and return leases.
The core keeps the input deadline active after response completion.
`Expect: 100-continue` still precedes the final response where the core requires it.

The request can also finish before response output.
Normal write settlement then completes the exchange.
Neither ordering changes the wire version or ordinary close conditions.

A dropped consumer with no outstanding lease cancels after the core notification drain.
A held lease delays this abandonment decision, not the core deadline.
A body timeout or source failure can still cancel the exchange.
The driver must remain polled until native operations and retained leases settle.

This change does not add retries, synthetic terminal chunks, or success responses after a source error.
The source-error regression requires the exact partial chunked output, without a terminating zero chunk.
The failed connection cannot admit another exchange.

## Tests and evidence

The eight native tests in `kimojio-http1/tests/duplex.rs` cover:

| Test | Evidence |
| --- | --- |
| Explicit forwarded streaming response | Four gated uploads reuse one socket, with fixed/chunked requests and both continue-handshake modes |
| Known-length source destruction | Fixed/chunked input, a held final lease, a positive partial write, and exactly one later response |
| Response-first and input-first completion | Three exchanges reuse one socket with exact payloads and no premature close header |
| Abandoned forwarded prefix | Buffered and absent suffixes cancel without another handler call |
| Dropped consumer without a lease | Final output does not prevent cancellation of incomplete input |
| Body deadline | A timeout remains active after final output while the last lease remains held |
| Body-source failure | Exact partial chunked output has no forbidden success terminator |
| Client policy misuse | Invalid policy causes no request transmission and leaves the connection usable |

Each test uses a native timeout of three or five seconds.
The existing conservative echo and early-rejection tests remain unchanged.
The forwarding and native cancellation suites also remain active.
They cover pointer identity, exact lease return, unknown write progress, and late successful completion after cancellation.

All checks use CPUs 8–31 and a private build directory.
No timing or profiling ran on CPUs 0–7.

```sh
export CARGO_TARGET_DIR=/workspace/kimojio-rs/target/wrapper-lab/build-duplex-wrapper
taskset -c 8-31 cargo fmt -p kimojio-http1
taskset -c 8-31 timeout 90s cargo test -p kimojio-http1 --all-targets --features virtual-clock
taskset -c 8-31 timeout 240s cargo test -p kimojio-http1 --all-targets --features virtual-clock --release
taskset -c 8-31 timeout 90s cargo test -p kimojio-http1 --test duplex --quiet
taskset -c 8-31 timeout 90s cargo test -p kimojio-http1 --test duplex --release --quiet
taskset -c 8-31 cargo clippy --quiet
taskset -c 8-31 cargo clippy --quiet --all-targets --all-features
taskset -c 8-31 cargo clippy --quiet -p kimojio-http1 --all-targets --all-features -- -D warnings
```

The debug and release suites each passed all 59 tests with `virtual-clock`.
The eight duplex tests also passed in both profiles without that feature.
Formatting and strict wrapper Clippy passed.
Both workspace Clippy commands completed successfully.
They reported only pre-existing warnings in `examples/http1-static/src/app.rs` and `kimojio/src/pipe.rs`.

## Integration limits

The core `respond_duplex` API is a prerequisite.
The wrapper introduces no core changes and no transport-worker changes.
The flag moves with the response body and remains independent of transport selection.
There are no new API exports or metadata allocations.

The source must bound its retained state.
The existing forwarding capacity rule still applies to the complete receive allocation.
A custom transport that does not settle cancellation cannot guarantee bounded shutdown.
An application that retains a lease indefinitely can prevent normal retirement.
There is no performance claim for this policy selection.
