# Explicit duplex response policy

## Scope

This change implements priority 4 at the HTTP/1 core boundary.
The base commit is `05545524`.
It changes no production code in `kimojio-http1`.
It adds no runtime calls, heap queues, payload copies, or callback types.
The public progress contract remains `next(&mut ports) -> Option<Output>`.

Previously, `Core::respond` required connection close whenever request framing remained incomplete.
After response output settled, `Boundary::OutgoingSettled` paused the unread request.
That behavior protected early rejection, but also prevented reuse for a response that continued to consume the request.

## Alternatives and decision

| Alternative | Decision |
| --- | --- |
| Infer consumption from body credit | Reject. Credit does not prove that a consumer will remain alive. |
| Infer consumption from response status or body length | Reject. A successful streaming response can still abandon the request. |
| Change `respond` for every caller | Reject. This changes early rejection and can leave abandoned uploads pending. |
| Add a connection-wide configuration flag | Reject. Intent belongs to each exchange, not every handler on the connection. |
| Add a public policy enum and another general response method | Defer. Two behaviors need only one additional method. |
| Add `Server::respond_duplex(exchange, response)` | Select. The existing method remains conservative, and the caller explicitly promises continued consumption. |

The core records this promise in `Exchange::consume_request`.
Only an accepted duplex response sets the promise.
A rejected command cannot change it.
Each new exchange starts without the promise.
The private `ResponseMode` enum distinguishes conservative responses, duplex responses, and upgrades at command acceptance.

The coordinator remains the owner of cross-direction protocol policy.
At `OutgoingSettled`, it pauses unread input only without the consumption promise.
The existing receive selector, retirement join, parser, timers, and operation ownership remain authoritative.
No adapter-side keep-alive rule replaces these decisions.

## Semantics

`respond_duplex` starts a final response and retains permission to consume the current request.
It does not grant body credit.
It does not dispatch another exchange before retirement.
It does not force persistence.

The application must keep the request consumer alive until `incoming_finished`.
The application must return every body lease and maintain bounded body credit.
If consumption stops, the application must call `cancel_exchange` or `fail_source`.
A disabled body timeout does not provide a fallback for an abandoned consumer.

The final response can finish before the request.
Pending reads and readiness operations retain their original identities.
A held body lease prevents successful retirement.
After the lease returns, the receive machine continues from the exact consumed cursor.

The request can finish before the response.
The core reports `incoming_finished`, but waits for the final write and output receipts before retirement.
Short writes retain the original output cursor.
Neither source completion nor a partial write proves transport settlement.

Reuse still requires all of these conditions:

- Request framing is complete.
- Response output is settled.
- No read, readiness operation, write, or body lease remains outstanding.
- Normal persistence rules permit reuse.
- No cancellation, failure, shutdown, or EOF prohibits reuse.

The normal close rules still apply to explicit duplex responses.
For example, `Connection: close` prevents reuse but does not revoke the explicit consumption promise.
The server consumes the request before successful close, unless a failure or cancellation intervenes.

## Observable ordering

1. An accepted command records response mode before the next drive.
2. An outstanding `Expect: 100-continue` receives 100 before the final head if no request body bytes are buffered.
3. This duplex continue rule also covers empty responses and zero current credit.
4. The default `respond` continue and early-rejection rules remain unchanged.
5. `source_finished` reports the end of response production, not the end of request consumption.
6. Final output settlement leaves duplex receive work eligible.
7. `incoming_finished` precedes successful `exchange_finished`.
8. The next pipelined request callback follows `exchange_finished`.

Cancellation and failure retain their existing ordering.
An unsuccessful `exchange_finished` can precede resource return.
Transport close still waits for the original operations and body leases.
No successful request-completion callback is fabricated for an abandoned body.
Stale completions cannot acquire authority over the next exchange.

Duplex selection does not change upgrade admission.
`accept_upgrade` still requires complete input and its existing handshake checks.
Neither 101 nor successful CONNECT can use `respond_duplex` as an alternate handoff path.

## Wrapper integration

The wrapper needs one explicit per-response selection at `Driver::respond`.
The current call in `kimojio-http1/src/driver.rs` uses `server.respond`.
A wrapper marker or equivalent application command can carry continued-consumption intent.
The wrapper must not infer this intent from status, streaming output, credit, or a retained body handle.

The exact core selection is:

```rust
use kimojio_fsm_http1::{Buffer, CommandError, ExchangeId, Response, Server};

fn select_response<B: Buffer, W: AsRef<[u8]>>(
    server: &mut Server<B, W>,
    exchange: ExchangeId,
    response: Response<'_>,
    consume_request: bool,
) -> Result<(), CommandError> {
    if consume_request {
        server.respond_duplex(exchange, response)
    } else {
        server.respond(exchange, response)
    }
}
```

The boolean in this example represents application intent, not adapter protocol policy.
The wrapper requires no new executor operation.
It continues to return read completions, write completions, and body leases through the existing methods.

For wrapper integration:

1. Keep conservative `respond` as the default.
2. For an explicit consumption promise, select `respond_duplex`.
3. Keep the request consumer and its credit path active after final response output.
4. On consumer abandonment, submit `cancel_exchange` through the existing cancellation path.
5. Return outstanding operations and leases even after cancellation.
6. Keep `source_finished` separate from incoming completion.
7. Use the core `exchange_finished.reusable` result for connection reuse.

The current wrapper releases its outgoing producer at `source_finished`.
A response-first consumer cannot rely on that producer to remain alive afterward.
An echo producer can own the request stream until request completion.
A separate response-first consumer needs an independent lifetime.
Otherwise, producer release abandons the request and must trigger cancellation.

## Regression coverage

`tests/duplex_policy.rs` exercises the public server API with a scripted peer:

- Request-first and response-first completion with fixed and chunked request bodies.
- A partial final write and a body lease held across final output settlement.
- A final payload receipt while the request lease remains outstanding.
- Conservative policy on a second POST after a duplex exchange.
- Partial lease consumption and explicit credit restoration.
- A second pipelined request on the same connection after successful retirement.
- Pending read and readiness ownership after response completion.
- Stale readiness completion after connection reuse.
- Byte-at-a-time output of 100 before an empty final response without prior credit.
- An informational-response limit failure followed by conservative rejection.
- Conservative early rejection with cancellation of the original read.
- Explicit abandonment with both orders of read and write settlement.
- Body-limit, malformed-chunk, truncated-body, and timeout failures after final output.
- A held lease during timeout and delayed transport close.
- Nonpersistent requests, explicit close, request limits, graceful shutdown, and rejected upgrade selection.

`tests/duplex.rs` connects the real client and server cores.
It now runs two streaming exchanges on each connection and requires reuse from both cores.
It covers 1-, 2-, 7-, and 1024-byte fragments, with and without `Expect: 100-continue`.
The request producer waits for the response head before it sends data.
The response producer waits for request completion before it ends.

The server-only response-first cases deliberately use a scripted peer.
The client core still stops its own unfinished upload after it receives the complete response.
The new server policy cannot compel that client, or any other peer, to continue.
The paired echo case therefore covers request-first completion without changing client rejection policy.

## Results

All commands use CPUs 8-31 and `CARGO_TARGET_DIR=/workspace/kimojio-rs/target/wrapper-lab/build-duplex`.
No benchmark or profiling command ran.
Performance comparison belongs to the parent benchmark task.

| Command | Result |
| --- | --- |
| `cargo fmt --all` | Passed |
| `cargo test -p kimojio-fsm-http1 --test duplex_policy --test duplex` | Passed, 12 tests |
| `cargo test -p kimojio-fsm-http1 --quiet` | Passed, 98 tests and 4 doctests |
| `cargo test -p kimojio-fsm-http1 --release --quiet` | Passed, 98 tests and 4 doctests |
| `cargo clippy --quiet` | Passed with one pre-existing warning outside this change |
| `cargo clippy --all-targets --all-features --quiet` | Passed with three pre-existing warnings outside this change |
| `cargo clippy -p kimojio-fsm-http1 --all-targets --all-features --quiet -- -D warnings` | Passed without warnings |

The existing warnings concern `question_mark` in `examples/http1-static/src/app.rs:413` and two `byte_char_slices` cases in `kimojio/src/pipe.rs:64-65`.
These files remain unchanged.

## Remaining risks

The promise requires a live consumer.
Incorrect wrapper selection can wait until timeout, or indefinitely if timeouts are disabled.
Cancellation is the required fallback, not an implicit drain.

The server can advertise persistence before later input fails.
That failure closes the connection and prohibits reuse.
No later HTTP error response can replace a final response that already reached the transport.

These tests establish core behavior, not conventional wrapper behavior.
The wrapper still needs explicit response selection and equivalent same-connection tests.
There is no performance claim until that integration and benchmark comparison finish.
