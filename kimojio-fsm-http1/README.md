# kimojio-fsm-http1

This crate contains HTTP/1 client and server state machines.
The machines perform no I/O and read no clock.
They have no dependency on Kimojio, io_uring, or HTTP/2.

## Supported protocol surface

The machines support HTTP/1.0 and HTTP/1.1 with one active exchange.
They support fixed-length, chunked, and applicable close-delimited bodies.
The server retains pipelined input and processes requests in order.
The client does not initiate concurrent pipelining or replay requests.
If buffered bytes follow a completed final response, the client closes without reuse.
The completed response remains successful, but unsolicited bytes cannot become a response to a later request.
Explicit upgrade handoff still preserves bytes for the next protocol.

The core handles HEAD, bodyless responses, informational responses, trailers, and `100-continue`.
It rejects ambiguous framing, malformed chunks, forbidden trailers, and unsupported transfer codings.
Transfer codings other than `chunked` are outside this implementation.
Unsupported HTTP/1.1 expectations receive 417 before application dispatch.
HTTP/1.0 does not use the continue gate.

Upgrade and successful CONNECT responses use explicit transport handoff.
The core checks HTTP upgrade tokens and completion ordering.
The next protocol owns its additional handshake checks.
For example, WebSocket key verification does not belong to HTTP.

## Early server responses

`Server::respond` keeps its conservative policy.
If the request is incomplete, the response requires connection close.
After final output settles, the core stops unread input and cancels outstanding reads.

`Server::respond_duplex` explicitly keeps request consumption active after final output settles.
The caller must supply body credit and return body leases until `incoming_finished`.
If the consumer abandons the request, the caller must use `cancel_exchange` or `fail_source`.
The core does not drain the body without credit.

Successful retirement requires complete request framing, settled response output, returned body storage, and settled external operations.
The core reports `incoming_finished` before successful `exchange_finished`.
Only then can the server dispatch the next pipelined request.
Request-first and response-first completion both permit reuse.
Connection-close headers, nonpersistent requests, request limits, shutdown, and EOF can still prohibit reuse.
Body limits, timeouts, and upgrade rules do not change.

For explicit duplex responses, an outstanding `Expect: 100-continue` receives 100 before the final head if no body bytes are buffered.
This also applies to empty final responses and requests without current body credit.
The default rejection policy does not change.
Duplex selection does not require a peer to continue its upload after a final response.
The client core retains its existing early-response policy.

The [duplex policy report](../docs/http1-wrapper-lab/duplex-policy.md) describes wrapper integration and regression evidence.

## Construction

Both constructors use the same arguments:

```rust
use kimojio_fsm_http1::{Client, Config, ConnectionId, Server, Tick};

let server = Server::new(
    ConnectionId { slot: 1, generation: 1 },
    Config::default(),
    vec![0_u8; 16 * 1024],
    Tick(0),
)?;
let client = Client::new(
    ConnectionId { slot: 2, generation: 1 },
    Config::default(),
    vec![0_u8; 16 * 1024],
    Tick(0),
)?;
# Ok::<(), kimojio_fsm_http1::CommandError>(())
```

The receive buffer contains initialized bytes.
Its length defines transport read capacity.
The root assigns unique connection identities, including a generation for each reused connection slot.
An identity must remain unique while old completions can exist.

`Config` has public resource limits and timeout durations.
Durations and `Tick` values use nanoseconds in one monotonic domain.
The caller supplies time through `observe_time` and exact deadline observations through `expire`.

## Separate payload storage

The types are `Server<B, W = B>` and `Client<B, W = B>`.
The input type `B` implements `AsRef<[u8]> + AsMut<[u8]>`.
The output type `W` implements only `AsRef<[u8]>`.
The corresponding callback traits are `Ports<B, W>`, `ServerPorts<B, W>`, and `ClientPorts<B, W>`.

`new` retains the one-buffer-type default.
`with_output_type` selects a distinct output type without an output allocation.
This separation does not add a storage registry or shared ownership inside the machine.
The caller can choose reference counting for shared payloads.

```rust
use kimojio_fsm_http1::{Config, ConnectionId, Server, Tick};
use std::sync::Arc;

let server = Server::<Vec<u8>, Arc<[u8]>>::with_output_type(
    ConnectionId { slot: 4, generation: 1 },
    Config::default(),
    vec![0; 8192],
    Tick(0),
)?;
# let _ = server;
# Ok::<(), kimojio_fsm_http1::CommandError>(())
```

`WriteOp<W>` borrows read-only slices from the retained output value.
The core does not clone or copy payload storage.
Its small framing prefix and suffix remain separate from the payload.
`BodySent<W>` returns the same retained output value.
The HTTP/1 storage contract does not impose a family-wide transport storage type.

### Experimental eager full bodies

`Server::send_body_eager` and `Client::send_body_eager` can attach a complete fixed-length body to queued final metadata.
The caller first starts the request or response with the existing command.
Before the next drive, the caller can submit `SendBody { end: true, .. }` through the eager method.

The core accepts only the complete remaining fixed-length payload.
The core rejects eager admission for suppressed bodies, chunked framing, active writes, and an unresolved client continue gate.
A rejected command retains its original storage.
The caller can keep that storage for the normal demand path.
No rejection changes the queued head.

An accepted command ends the source before output completes.
The write operation owns the head and body as separate allocations.
`WriteOp::slices` exposes them without payload concatenation.
Partial-write accounting excludes metadata from `BodySent::accepted`.
The existing callback and cancellation contracts do not change.

The [interface PoC report](../docs/http1-wrapper-lab/poc-interface.md) records the experiment and its limits.

## One progress method

`next(&mut ports)` returns `Option<P::Output>`.
Each callback has the same optional output contract.
`Some(value)` suspends progress with the caller's value.
`None` from a callback continues progress without settling an operation.
`None` from `next` means that the machine has no immediate work.

An internal connector can use `Output = Infallible`.
An executor can use its own enum for `Output`.
The crate does not define an action enum.

The machine records issuance before each callback.
The callback accepts responsibility for completion, regardless of its return value.
The callback must not re-enter the active machine.

An `Infallible` connector records immediate completions in bounded local slots.
After `next` returns, the composite applies those completions and drives the affected children again.
A child's `None` does not make the composite idle when a completion or sibling remains ready.
This completion-return step avoids recursive mutation without requiring a task or queue at every layer.

## Internal state composition

The implementation uses a product of smaller state machines.
Existing commands, callbacks, and completion types remain compatible.
`Server::respond_duplex` adds explicit request-consumption intent without a new callback or operation type.

| State | Responsibility |
| --- | --- |
| `Lifecycle` | HTTP admission, upgrade preparation, bounded error output, close settlement, terminal notification, and transferred authority |
| `Rx` | Receive framing and parser progress |
| `ReceiveStorage` | Exactly one buffer owner: the machine, a read operation, a body lease, or the next protocol |
| `Transmit` | Producer demand, source end, early upload termination, and transport settlement |
| `IoState` | Idle, readiness demand, an outstanding operation, or cancellation requested for that operation |
| `Timers` | Independent timer candidates and one armed deadline paired with its purpose |
| Exchange control | Method semantics, the continue gate, and completion notifications for the current exchange |
| Metadata write role | Informational output, the final head, or body termination |

Lifecycle alternatives are exclusive.
A closed connection cannot also retain an independent upgrade-ready flag.
The close-wait state contains the original close identity.
Repeated shutdown does not replace that identity or issue another close.
An error response has its own lifecycle state because recording a failure does not immediately terminate its output.

Producer completion and transport settlement remain different states.
The last payload can remain in flight after the producer ends.
An early response can stop production while original operations still require completion.
The continue gate remains independent because an empty streaming source can end before a 100 response arrives.

Cancellation changes an outstanding I/O state without releasing its identity.
Only the original completion releases the operation slot.
Receive storage cannot simultaneously belong to the machine and an outstanding body lease.
The storage transitions move the original buffer without cloning its payload.
The small control enums introduce no additional allocation.

Some invariants cross component boundaries.
For example, close issuance requires settled I/O and returned body storage.
Debug assertions cover these relationships at drive boundaries.
They also permit a queued final head behind an outstanding informational write.
Completing that informational write cannot mark the queued final head as settled.
The write operation retains its metadata role instead of inferring it from current exchange flags.
Incoming body delivery remains eligible while the final response output is still in flight.
An explicit duplex response also permits incoming body delivery after final output settles.
An already received upgrade response still reaches the client callback before a pending graceful shutdown revokes handoff.

State types remove contradictory local combinations, not the need to order callbacks and settle external resources.
The coordinator makes those ordering rules explicit.
Transition tests cover cancellation, receive ownership, source versus transport completion, and all timer-candidate combinations.

### Coordination contracts

[`coordinator.rs`](src/coordinator.rs) separates transition selection from transition execution.
`next_transition` reads state without changing it.
`advance` commits one selected transition and can call a port.
`next` repeats these steps until a callback yields or no transition remains eligible.
A callback that returns `None` continues this loop, including after an upgrade notification.

`Transition` contains private control-flow labels, not operation payloads.
The executor still receives operations directly through callbacks.
The coordinator adds no event queue, allocation, dynamic dispatch, or public action enum.

Each lifecycle state selects from a restricted set of transitions:

| Lifecycle | Permitted work after pending deadlines and receipts |
| --- | --- |
| HTTP | Cross-direction policy, source notification, upload termination, receive cancellation, continue response, transmit, incoming notification, retirement, demand, receive |
| Error response | Source notification, receive cancellation, bounded output, entry into closing |
| Upgrade handshake | Source notification, handshake output, incoming notification, handoff readiness |
| Revoked upgrade | Failure transition after the response callback |
| Closing, settling | Source notification, cancellation, resource return, exchange termination, close issuance |
| Closing, awaiting close | No new work until the original close completes |
| Closed | One terminal notification |
| Upgrade ready | No protocol work before handoff |
| Handed off | No callbacks or transport authority |

The receive and transmit selectors own local eligibility rules.
For example, the receive selector requires available storage before it considers parsing, body delivery, or a new read.
The transmit selector requires a free operation slot before it prepares or issues output.
The lifecycle selector determines whether those local selectors can run.

The coordinator has these ordering dependencies:

| Dependency | Reason |
| --- | --- |
| Pending deadlines and receipts precede protocol work | The caller receives timer changes and returned storage before later callbacks |
| Cross-direction policy precedes new demand and I/O | A completed message can remove permission for work in the other direction |
| Source notification precedes upload cleanup | The producer learns that no further data is required |
| Buffered response metadata precedes upload output and demand | An early response can stop the upload |
| Output precedes new producer demand | The machine has bounded output capacity |
| Incoming notification precedes normal retirement or handoff readiness | The application observes message completion before those milestones |
| Resource settlement precedes close or handoff | External operations and body leases retain their original ownership |

A pending receipt has priority over a source notification.
There is no universal rule that every source notification precedes every receipt.
Cancellation requests never release operation identities.
Closing can report exchange termination before outstanding operations return, but it cannot issue close until they return.

Semantic completion boundaries record the cross-direction policy obligation.
Client receive completion records `IncomingEnded`.
Server transmit settlement records `OutgoingSettled`.
Each role records only one kind, so repeated records coalesce without a queue.
The coordinator applies the obligation during the next drive.
This preserves completion batches: a caller can return both a write and a body lease before the machine applies early-response policy.
Failure clears the obligation and transfers control to the error or closing lifecycle.

The selectors only determine eligibility.
The corresponding transition commits notification state, cancellation state, or operation ownership before it calls a port.
Every non-yielding transition must consume work or advance state.
Only the absence of an eligible transition means that the machine is blocked.

The transition reference separates eligibility from the committed effect:

| Transition | Required local fact | Committed effect |
| --- | --- | --- |
| `Deadline` | A timer notification is pending | Consume the notification and report the current deadline |
| `Receipt` | Returned output storage is pending | Transfer the receipt and storage to the caller |
| `Closed` | The terminal notification is pending | Mark it delivered before the callback |
| `Coordinate` | A semantic boundary is pending | Consume the boundary and apply cross-direction policy |
| `RevokeUpgrade` | The upgrade lifecycle is revoked | Record cancellation and enter termination |
| `SourceFinished` | Source end or failure, with a pending notification | Mark the notification delivered before the callback |
| `CancelRead`, `CancelWrite` | An original operation is in flight | Record cancellation without releasing its identity |
| `DiscardOutput` | Unissued or returned output remains during termination | Settle its retained storage |
| `ReturnBody` | Accepted body storage remains unissued during termination | Create its zero-acceptance receipt |
| `SettleUpload` | The stopped upload has no retained output or outstanding write | Record transport settlement |
| `FinishAbortedExchange` | A closing connection retains an exchange | Remove the exchange and report non-reusable termination |
| `Close` | Closing has no exchange, operation, or body lease | Reserve the close identity before issuance |
| `Continue` | Request-body demand needs input before a final response starts | Queue one informational response |
| `Write` | Output exists and the write slot is free | Issue a write or a readiness operation |
| `PrepareBody` | Accepted body storage has no preceding output | Move that storage into framed output |
| `BeginClosing` | The bounded error output settled | Enter closing and clear deadlines |
| `IncomingFinished` | Receive framing is done and its notification is pending | Mark the notification delivered before the callback |
| `UpgradeReady` | Handshake output and external ownership settled | Record handoff readiness and clear deadlines before the callback |
| `RetireExchange` | Both directions and external ownership settled | Decide reuse, remove the exchange, and report completion |
| `Demand` | The producer is ready, the continue gate is open, and output capacity is free | Record outstanding demand before the callback |
| `Metadata` | The parser has an available byte | Consume one byte and advance or reject metadata |
| `Body` | Payload, credit, and receive storage are available | Debit credit and transfer a body lease |
| `RejectBody` | A `205` response with EOF framing contains payload | Record a protocol failure |
| `Eof` | EOF has no buffered bytes or retained body lease | Complete EOF framing, close an idle connection, or report truncation |
| `Read` | The parser needs input and has free storage and an operation slot | Issue a read or a readiness operation |

The lifecycle table restricts these local facts further.
For example, a free write slot cannot authorize normal output during closing.
Clean idle EOF clears its deadline before close issuance.
An upgrade callback that returns `None` no longer leaves deadline cancellation pending behind a blocked return.

### Bounded coordinator model

[`coordinator_tests.rs`](src/coordinator_tests.rs) contains an independent ownership model for a two-byte request and response.
The model explores 474 terminal schedules from three initial input owners: read operation, readiness operation, and body lease.
The schedules include both completion orders, abort, timeout, short writes, readiness, interruption, and transport reset.
Each schedule runs with single-transition inspection and three callback modes: continue, yield, and mixed.

The model checks exact receipts, incoming completion, source completion, exchange termination, cancellation identities, and close eligibility.
Single-transition inspection checks selection purity and rejects repeated internal states.
An independent ownership join determines when close must become eligible.
The public drive modes must produce the same callback sequence without intervening external inputs.
Separate cases cover response priority, completion batches, and deadline cancellation after a non-yielding upgrade callback.

This bounded model is not a proof of all HTTP behavior.
It does not model arbitrary payload lengths, every parser state, or an executor that never returns its operations.
The protocol corpus and ownership tests cover additional cases.
The coordinator requires original operation completions and body-lease returns for eventual closure.

## Owned operations

`ReadOp<B>` and `WriteOp<B>` own their storage.
They do not borrow the machine.
One read and one write can remain outstanding together.

A read executor borrows `op.bytes_mut()` for I/O.
A write executor borrows `op.slices()` for scatter/gather I/O.
Both borrows can stay inside a future that owns the operation.
The executor must preserve the address of submitted storage until I/O settles.

The executor returns `op.complete(result)` through the matching completion method.
The machine handles short I/O, interruption, readiness, and write cursors.
A zero-byte write is a failure, not successful completion.

`WouldBlock` and `Interrupted` require zero progress during that operation.
A write-all adapter can report success with the complete offered length.
If a write-all operation fails after unknown progress, its adapter must report `IoErrorKind::UnknownProgress`.
That error is terminal and never causes a retry.
`WouldBlock`, `Interrupted`, `Cancelled`, `Reset`, and `Other` report no additional accepted bytes for that operation.
An adapter with exact write counts avoids this uncertainty.

`CancelledUnknownProgress` represents a confirmed write-all cancellation with an unknown accepted prefix.
During an early final response, this result stops the upload without discarding the response.
It still prohibits reuse and reports lower-bound acceptance.
An ordinary transport error remains fatal through `UnknownProgress`.

Rejected completions contain the original completion in `Rejected::value`.
`into_parts()` recovers the operation and result.
Wrong-owner, stale, and invalid-count completions do not change live state.

`CancelOp` identifies an outstanding operation.
Cancellation does not release that operation's resources.
The original operation must still complete.

## Metadata and commands

Request, response, and trailer callbacks borrow metadata only for that callback.
An async facade must copy metadata that survives the callback.
Body bytes use exclusive owned buffers instead.

```rust
use kimojio_fsm_http1::*;

let mut client = Client::new(
    ConnectionId { slot: 3, generation: 1 },
    Config::default(),
    vec![0; 8192],
    Tick(0),
)?;
let exchange = client.request(Request {
    head: RequestHead {
        method: "GET",
        target: "/index.html",
        version: Version::Http11,
        headers: &[Header { name: "host", value: b"localhost" }],
    },
    body: BodyLength::Empty,
    expect_continue: false,
})?;
# let _ = exchange;
# Ok::<(), CommandError>(())
```

The server callback supplies an `ExchangeId`.
The application uses that identity for `respond`, `inform`, and body commands.
`Response` contains a `ResponseHead` and a `BodyLength`.
The explicit body length controls framing.
Caller headers must not include `Content-Length`, `Transfer-Encoding`, or `Expect`.
These fields come from the command.

The server selects the response wire version from its request state.
It ignores the version field supplied in outgoing response metadata.
Incoming response metadata still reports the actual peer version.
The application does not need to retain or reconstruct request-version policy.

```rust
use kimojio_fsm_http1::{BodyLength, Response};

let response = Response::new(200, "OK", &[], BodyLength::Known(1024));
# let _ = response;
```

`ResponseHead::new(status, reason, headers)` also constructs informational response metadata without a version argument.
The core accepts one pending final response while informational output is queued or outstanding.
A second final response is an invalid command, not temporary backpressure.
Adapters do not need to retry `respond` after `InvalidState`.

`BodyLength::Empty` completes the outgoing body with its head.
`BodyLength::Known(n)` requires exactly `n` payload bytes.
`BodyLength::Streaming` uses chunked HTTP/1.1 output or close-delimited HTTP/1.0 responses.

`send_ready` represents one producer demand.
The machine issues that demand once, until `send_body`, `finish_body`, or `fail_source` resolves it.
`send_body` accepts a `SendBody` with an exchange, buffer, range, and end flag.
Rejection returns the complete command and buffer.
Cancellation and late producer commands cannot silently consume the buffer.

`source_finished` reports that the machine needs no further producer payload for the exchange.
The application can then release its body source.
This includes an input stream that an echo source retains.
The callback avoids adapter-owned HEAD, status, or early-response policy.

The machine emits this notification once, independently of transport completion.
Owned writes and `body_sent` receipts can remain pending.
The default callback returns `None` for callers without a retained body source.

`BodySent` returns outgoing storage.
Its `accepted` count contains payload bytes accepted by the transport.
It does not imply peer receipt.
`BodySent::acceptance` distinguishes `Acceptance::Exact` from `Acceptance::LowerBound`.
An unknown-progress failure reports a lower bound, even when an earlier cancellation already determined the exchange outcome.
The machine does not infer replay safety from this count.
`fail_source(exchange, Failure::Application)` reports handler or producer failure.

## Delivery credit

Each exchange starts with zero incoming payload credit.
The request or response head callback occurs before any body delivery.
The application can grant capacity after that callback returns.
Heads, informational responses, chunk metadata, and trailers do not consume payload credit.

Bodyless messages complete without a credit grant.
This includes HEAD responses, 204 and 304 responses, and zero-length bodies.
The machine can emit `incoming_finished` after the head callback without application input.

`grant_body_credit(exchange, bytes)` declares application capacity.
It is not an instruction for the adapter to compute HTTP framing or transport policy.
The credit cannot exceed `Config::max_buffer_bytes`.

For an expected HTTP/1.1 body, positive credit authorizes the core's continue policy.
The core sends 100 only when more input is necessary.
It omits 100 when buffered bytes contain the complete body.
A completed request body or final response prevents a later 100.
Authorized successful streaming responses preserve 100-before-final ordering when input is necessary.
Adapters do not parse Expect headers or issue automatic informational responses.

`body` transfers a `BodyOp` with an exact visible payload range.
`BodyOp::release(consumed)` returns storage and reports exact consumption.
Only `release_body` applies this result.

Offered bytes consume credit.
Storage release does not add credit.
Unconsumed bytes remain in the receive buffer.
A zero-consumption release clears residual credit and cannot cause a busy loop.
While the application holds a body operation, writes can still progress.

One `BodyOp` owns the complete receive buffer, including bytes outside its visible payload range.
Until the application returns it, the machine cannot read more transport data or parse the retained remainder.
Writes and unrelated completion settlement can continue.
This bound does not provide unlimited receive concurrency.

## Resource limits

`max_head_bytes` and `max_headers` apply cumulatively to heads, informational responses, and trailers in each direction.
Generated outgoing headers count toward the field limit.
Outgoing chunk termination also reserves space in the metadata budget.

`max_body_bytes` limits actual payload bytes in each direction.
HEAD and 304 representation lengths do not count as payload.
`max_chunk_line_bytes` limits each chunk-size line.
`max_chunk_metadata_bytes` limits cumulative chunk lines and delimiters in each direction.
`max_informational_responses` limits repeated informational responses.

`max_buffer_bytes` limits the addressable length of each accepted input or output buffer.
It also limits delivery credit and the size of an outgoing body command.
The caller accounts for hidden backing capacity in custom leases or shared allocations.
The core cannot inspect storage outside the slices that the buffer exposes.

There is one receive buffer and at most one accepted outgoing payload buffer.
Operation transfer moves these buffers rather than adding copies.
Head scratch, encoded heads, and retained connection tokens have limits proportional to `max_head_bytes`.
The parser uses a fixed stack array with at most 128 header fields.
No protocol queue grows with the number of exchanges.

`send_ready` reports the remaining payload and framing capacity.
A capacity of zero permits only `finish_body` or `fail_source`.
It never causes a zero-byte transport write.

## Deadline policies

The server starts with a head deadline.
An idle persistent connection uses the idle deadline until the next request starts.
The client starts idle and arms its response-head deadline when it accepts a request.
That head deadline includes time spent on the upload.

Body-progress deadlines cover application work and incoming body progress.
The client also has an independent upload-progress deadline after its request head leaves the transport operation.
Positive transport progress and application body release reset the applicable progress deadline.
A stalled consumer or producer does not reset it.

The continue deadline starts after the request head completes.
Its expiration permits body production without replacing other deadlines.
A 100 response also releases that gate.
A rejecting response, an Expect refusal, or a completed response stops an unfinished upload and prohibits reuse.
Successful streaming responses can progress alongside an upload.

`deadline_changed` exposes only the earliest active deadline.
The executor schedules it without interpreting the policy.
`expire(deadline, now)` rejects stale, early, wrong-owner, and regressed observations.
`observe_time(now)` updates time but does not expire a deadline by itself.
`None` disables an individual timeout policy.

## Lifecycle and handoff

`incoming_finished` marks the end of incoming message framing.
`exchange_finished` reports the semantic exchange outcome and reuse permission.
`closed` follows transport close and operation settlement.
These notifications have different meanings.

An early server response permits incoming body delivery while response output continues.
After output ends, the machine abandons unread request data and closes.
This permits streaming echo sources that consume request data after the response head.
An abandoned request does not receive successful incoming-body completion.
A rejecting early client response cancels pending upload work but permits response delivery.
`BodySent` reports `Failure::EarlyResponse` for a stopped upload.
Known accepted payload bytes remain distinct from the exchange outcome.

An exchange failure can precede late `body_sent` notifications.
The owner continues progress until `closed` and returns every outstanding body lease.
Cancellation acknowledges intent, not resource release.
Wrong-owner and stale completions return their resources without reviving a retired exchange.

Before final response output starts, the server can send a bounded error response.
Malformed requests receive 400, payload limits receive 413, and header limits receive 431.
A partial-request timeout receives 408.
The machine then closes and reports the original failure.
An unusable transport, an insufficient metadata budget, or an already started final response prevents automatic error output.
The application and transport adapter do not synthesize these protocol responses.

`shutdown(Graceful)` completes the current exchange without admitting another.
`shutdown(Abort)` cancels outstanding work and requests resource settlement.

An upgrade uses `accept_upgrade` on the server.
The machine reports `upgrade_ready` only after handshake output and HTTP operations settle.
`take_upgrade` returns transport identity and exact unread input.
The outer owner then transfers its transport to the next protocol.
The HTTP machine does not perform WebSocket handshake validation.

An upgrade-ready notification is not permanent transport authority.
Failure, cancellation, timeout, or shutdown before `take_upgrade` revokes the pending handoff.
The transfer method checks terminal state and all outstanding operation slots again.
After a successful transfer, HTTP cannot issue close and ignores later shutdown commands.
A stale deadline from before upgrade readiness does not revoke a valid handoff.

## Validation

The regression suite includes an independent response-framing corpus.
It runs each corpus case at every transport split point.
Historical request-framing and chunk-syntax negatives receive the same split coverage.

Other tests cover short writes across chunk prefixes, payloads, suffixes, and termination.
Lifecycle traces include deadline phases, early-final uploads, cancellation completion orders, retained body leases, and handoff.
Shared-output tests preserve payload addresses across partial writes.

The standalone tests do not measure native executor throughput.
The direct io_uring application and Kimojio facade provide separate integration and performance evidence.

## FSM CPU benchmarks

The `roundtrip` Criterion benchmark drives one real client or server FSM against a simulated peer.
The peer supplies prepared HTTP bytes and accepts the output through the normal operation completions.
The driver performs no socket calls, async scheduling, timer callbacks, or sleeps.
Criterion uses a clock outside the driver to measure elapsed time.

| Workload | Request payload | Response payload | Framing |
| --- | --- | --- | --- |
| `fixed_128b/client` | 128 B sent | 128 B received | Content-Length |
| `fixed_128b/server` | 128 B received | 128 B sent | Content-Length |
| `chunked_1mib/client` | 1 MiB sent | 1 MiB received | Chunked in both directions |
| `chunked_1mib/server` | 1 MiB received | 1 MiB sent | Chunked in both directions |

Each workload has `/continue` and `/yield` variants.
The callbacks return `None` in the first variant and `Some(())` in the second.
Both variants use bounded completion slots, not a queue or an enum that contains operation payloads.
The driver applies completions only after `next` returns.
The simulated peer waits for the complete request before it sends the response.
The server waits for incoming completion before it accepts the response command.

Each iteration completes one exchange on a reusable connection.
The receive buffer and outgoing chunks are 16 KiB.
Each large outgoing body requires 64 payload receipts, a head write, and a terminator write.
Receive fragmentation also splits chunk boundaries.
The driver returns each body lease and replenishes its delivery credit.
The request limit permits continued reuse throughout warmup and measurement.

Connection construction, payload generation, and peer-wire construction occur outside the timed loop.
Outgoing payloads borrow prepared slices without payload allocation or copying.
Incoming transport simulation copies bytes into the receive buffer during the timed loop.
The measurements include those copies, FSM metadata allocations, callback handling, completion forwarding, and constant-time success checks.
They exclude full payload comparisons and application processing.
They measure FSM-plus-driver elapsed cost, not isolated CPU cycles or network throughput.

Before timing, the driver checks every outgoing wire byte, incoming payload byte, receipt, and exchange result.
The timed driver retains checks for successful completion, reuse, byte totals, and notification counts.
Both paths use `black_box` for wire slices, received payload views, and returned statistics.
A failed or incomplete exchange stops the benchmark instead of contributing a timing sample.
Criterion reports time per exchange and aggregate payload throughput across both directions: 256 B or 2 MiB per iteration.

Run all measurements:

```sh
cargo bench -p kimojio-fsm-http1 --bench roundtrip
```

Run the optimized smoke cases and the independent workload tests:

```sh
cargo bench -p kimojio-fsm-http1 --bench roundtrip -- --test
cargo test -p kimojio-fsm-http1 --test benchmark_workloads
```

The workload tests include repeated connection reuse, one-byte reads and writes, and chunks with a short final payload.
They also compare callback modes and the timed path against the full byte-checking path.

### Revision comparisons

Use the same benchmark source, compiler, build flags, and CPU for each revision.
Run measurements sequentially on an otherwise idle machine.
On Linux, use `taskset -c <cpu>` before the command to select an available CPU.
Use a separate `CARGO_TARGET_DIR` for each revision.
A shared build directory can retain stale artifacts across worktrees.
Copy only Criterion results between those directories.

Save a baseline in the benchmark change, then compare from a descendant checkout:

```sh
export FOUNDATION_TARGET="$PWD/target/bench-foundation"
CARGO_TARGET_DIR="$FOUNDATION_TARGET" cargo bench -p kimojio-fsm-http1 --bench roundtrip -- --save-baseline foundation
# In the explicit-state or coordinator checkout:
export CANDIDATE_TARGET="$PWD/target/bench-candidate"
mkdir -p "$CANDIDATE_TARGET/criterion"
cp -R "$FOUNDATION_TARGET/criterion/." "$CANDIDATE_TARGET/criterion/"
CARGO_TARGET_DIR="$CANDIDATE_TARGET" cargo bench -p kimojio-fsm-http1 --bench roundtrip -- --baseline foundation
```

The benchmark change precedes both refactors, so each descendant inherits the same workloads.
Criterion stores estimates and samples in the selected target directory, under `criterion`.
The normal command uses Criterion's default sample size and measurement durations.
For a preliminary run, append `--sample-size 20 --warm-up-time 1 --measurement-time 2` after `--`.
Short runs remain sensitive to CPU frequency, shared-host load, and compiler code layout.
