# HTTP/2 engine

## Extraction checkpoint

The private protocol components come from `ae1c3402b12a338d321246596aed33025fbc3b91`.
They include HPACK, compact fields, frames, flow arithmetic, and separate client and server stream halves.
They do not include the mixed HTTP driver or an HTTP/1 parser.
The only production dependency is `rustc-hash`.
The benchmark uses the workspace Criterion dependency.

The extraction retains 205 component tests.
Those tests do not qualify a complete owned-operation engine.
Private compatibility entry points remain available to these tests, not to applications.
Dead-code allowances apply to this private migration layer.

The endpoint uses caller-supplied elapsed time for its control budget.
Adaptive receive windows require their explicit-time methods.
The compact event path preserves DATA flow length and discarded-DATA events.

## Engine contract

The public engine exposes direct `Client` and `Server` types.
Each type uses `next(&mut ports) -> Option<P::Output>`.
Ports receive owned read, write, body, and close operations.
Read and write operations have separate outstanding slots.
Completions return the original operation and its storage.
Wrong-owner completions return an error with the original completion.

Receive pages use bounded reusable `Rc` storage.
Body fragments retain a page range and an explicit release receipt.
Capacity accounting includes full retained pages, not only body ranges.
Read operations fill exclusive pages without a mandatory DATA payload copy.
Fragmented frames use a separate bounded assembly page.

Outbound DATA retains the original application buffer.
Each write exposes a frame header and a payload slice.
Exact partial progress advances a cursor across both slices.
Unknown progress terminates the transport without replay.

Protocol state, application leases, and transport settlement remain separate.
Normal selection uses recorded readiness.
Connection-wide SETTINGS work can visit all affected streams.

## Support and acceptance boundary

The standalone suite covers both direct roles and their owned-operation boundary.
The protocol target is RFC 9113 with HPACK from RFC 7541.
Passing this suite does not establish complete RFC conformance.
Independent peers, the outer composite, runtime integration, and performance qualification remain separate acceptance work.

| Area | Implemented behavior |
| --- | --- |
| Messages | Request and response bodies, informational responses, final headers, and both trailer directions |
| Duplex | A response END_STREAM closes only the receive half of a client stream |
| Message rules | Content-length accounting, HEAD and status-based body restrictions, and field syntax |
| CONNECT | Classic CONNECT and independent tunnel halves after a successful response |
| Push | Disabled advertisement, pre-ACK HPACK consumption and promised-stream cancellation, post-ACK connection error |
| Flow control | Separate stream and connection windows, padding credit, discard credit, and receipt-based body credit |
| Shutdown | GOAWAY retry boundary, server two-stage GOAWAY, supplied deadlines, and cancellation joins |
| Compression | Bounded HPACK tables, compact fields, CONTINUATION assembly, and committed output order |

The core does not implement extended CONNECT, RFC 9218 scheduling, h2c, socket I/O, TLS, or an asynchronous runtime.
The core ignores legacy priority information rather than using it for scheduling.
The pure path does not instantiate an HTTP/1 parser.
Some inherited error variants and the HTTP/1 request-count limit remain public compatibility vocabulary.
They do not select another protocol or constrain the HTTP/2 request count.

A configured stream receive window applies when the stream record begins.
If that window is smaller than 65,535 bytes, already-in-flight uploads can receive a stream FLOW_CONTROL_ERROR reset.
The receiver uses a reset rather than retaining negative inbound window debt.
This policy is separate from signed outbound debt after peer SETTINGS changes.

## Public API checkpoint

These signatures describe the implemented callback boundary.
`B: SendBuffer` is the application-owned send buffer type.
The default buffer type is `Vec<u8>`.
The core does not require that applications allocate a `Vec` for each body chunk.

```rust,ignore
pub struct Client<B = Vec<u8>> { /* private */ }
pub struct Server<B = Vec<u8>> { /* private */ }

pub trait SendBuffer: AsRef<[u8]> {
    fn retained_capacity(&self) -> usize;
}

pub trait Ports<B: SendBuffer> {
    type Output;
    fn read(&mut self, op: ReadOp) -> Option<Self::Output>;
    fn write(&mut self, op: WriteOp<B>) -> Option<Self::Output>;
    fn headers(&mut self, head: Head<'_>) -> Option<Self::Output>;
    fn body(&mut self, op: BodyOp) -> Option<Self::Output>;
    fn send_ready(&mut self, permit: SendPermit) -> Option<Self::Output>;
    fn send_stopped(&mut self, stream: StreamId, reason: SendStop)
        -> Option<Self::Output>;
    fn sent(&mut self, result: Sent<B>) -> Option<Self::Output>;
    fn ended(&mut self, end: ReceiveEnd) -> Option<Self::Output>;
    fn retired(&mut self, result: StreamResult) -> Option<Self::Output>;
    fn cancel(&mut self, op: CancelOp) -> Option<Self::Output>;
    fn wake(&mut self, op: WakeOp) -> Option<Self::Output>;
    fn close(&mut self, op: CloseOp) -> Option<Self::Output>;
    fn closed(&mut self, result: ConnectionResult) -> Option<Self::Output>;
    fn reschedule(&mut self) -> Option<Self::Output>;
}

// Both roles:
fn new(config: Config, now: Duration) -> Result<Self, CommandError>;
fn next<P: Ports<B>>(&mut self, ports: &mut P) -> Option<P::Output>;
fn advance_time(&mut self, now: Duration) -> Result<(), CommandError>;
fn send(&mut self, permit: SendPermit, buffer: B, end: bool)
    -> Result<(), Rejected<(SendPermit, B)>>;
fn trailers(&mut self, stream: StreamId, fields: &[H2HeaderField])
    -> Result<(), CommandError>;
fn reset(&mut self, stream: StreamId, code: H2ErrorCode)
    -> Result<(), CommandError>;
fn complete_read(&mut self, completion: ReadCompletion)
    -> Result<(), Rejected<ReadCompletion>>;
fn complete_write(&mut self, completion: WriteCompletion<B>)
    -> Result<(), Rejected<WriteCompletion<B>>>;
fn release_body(&mut self, completion: BodyRelease)
    -> Result<(), Rejected<BodyRelease>>;
fn complete_cancel(&mut self, completion: CancelCompletion)
    -> Result<(), Rejected<CancelCompletion>>;
fn complete_wake(&mut self, completion: WakeCompletion)
    -> Result<(), Rejected<WakeCompletion>>;
fn complete_close(&mut self, completion: CloseCompletion)
    -> Result<(), Rejected<CloseCompletion>>;
fn shutdown(&mut self) -> Result<(), CommandError>;
fn abort(&mut self);
fn abort_with_cause(&mut self, cause: ConnectionResult);
fn set_deadline(&mut self, stream: StreamId, deadline: Option<Duration>)
    -> Result<(), CommandError>;

// Client:
fn request(&mut self, fields: &[H2HeaderField], end: bool)
    -> Result<StreamId, CommandError>;
fn request_ref(&mut self, fields: &[H2RawHeaderRef<'_>], end: bool)
    -> Result<StreamId, CommandError>;

// Server:
fn respond(&mut self, stream: StreamId, fields: &[H2HeaderField], end: bool)
    -> Result<(), CommandError>;
fn respond_ref(&mut self, stream: StreamId, fields: &[H2RawHeaderRef<'_>], end: bool)
    -> Result<(), CommandError>;
// Both roles:
fn trailers_ref(&mut self, stream: StreamId, fields: &[H2RawHeaderRef<'_>])
    -> Result<(), CommandError>;
```

Request and response commands accept complete pseudoheader sections.
This avoids a separate CONNECT constructor and permits sensitive regular fields.
Header callbacks borrow a compact, validated field section for the callback duration.
Applications can copy selected metadata if they need to retain it.
Body callbacks transfer an owned page range instead.

Borrowed command fields use the existing `H2RawHeaderRef<'a>` descriptor:

```rust
use kimojio_fsm_http2::H2RawHeaderRef;
let field = H2RawHeaderRef {
    name: b"authorization",
    value: b"example-token",
    sensitive: true,
};
```

The descriptor contains `name: &'a [u8]`, `value: &'a [u8]`, and `sensitive: bool`.
The `_ref` commands accept slices of these descriptors.
They borrow field bytes through synchronous validation and encoder planning.
They do not create an owned-field staging list.
No field borrow survives the command.
HPACK table entries and committed output still own their required storage.
This API change is not a measured performance claim.

`ReadOp::buffer_mut()` exposes exclusive initialized storage.
`ReadOp::complete(outcome)` returns the original operation as a typed completion.
`WriteOp::slices()` exposes the remaining header and payload without concatenation.
`WriteOp::complete(outcome)` preserves the original buffer and exact partial cursor.
Write failure distinguishes exact progress from a known lower bound.
Read, write, wake, and close completions expose `into_parts` for recovery after rejection.
Body releases and cancellation completions expose `into_op` because they have no separate outcome.
These methods return the original operation without a new token or storage copy.

`BodyOp::bytes()` exposes one retained fragment.
`BodyOp::release()` consumes the fragment and creates its release receipt.
The engine does not refund application DATA credit before that receipt arrives.
Padding and discarded DATA have separate immediate credit paths.
Release consumes a whole fragment, not the whole connection buffer.

Every operation carries an unforgeable machine identity and a nonwrapping sequence.
Cancellation acknowledgment does not settle the original operation.
`CancelOp::complete()` means that the driver accepted the cancellation request.
It does not mean that the kernel released the original buffer or that the original operation stopped.
Only `admission_changed` has a default implementation.
Its default returns `None` for compatibility with adapters that do not retry metadata commands.
`reschedule` means that the drive budget ended with runnable work.

## Metadata admission

`request`, `respond`, `trailers`, and their `_ref` variants return `CommandError::Blocked` for temporary admission pressure.
This includes local stream slots, peer concurrency, queued control items, and currently retained control bytes.
`Blocked` accepts no command, consumes no stream ID, and leaves HPACK and stream semantics unchanged.
The adapter retains its queued fields.
Synchronous command success remains the only acceptance event.

For these metadata commands, `Capacity` indicates a fixed bound that cannot fit the command.
An empty output queue does not change that result.
The encoder uses a conservative field-size bound, not the compressed size of a speculative HPACK block.
Header-count and HTTP field-size errors retain their specific `Message` results.
Exhausted stream IDs return `SequenceExhausted`, even if all local stream slots remain occupied.
Constructor errors and non-metadata control commands retain their existing `Capacity` contracts.

State checks precede field checks.
Requests check exhausted IDs before temporary pressure.
The local stream-slot check still precedes request-field checks.
Thus, `Blocked` can mask invalid fields until a later attempt.
Field checks and the fixed output bound precede temporary output pressure.
Peer concurrency follows those checks.
Neither a notification nor `Blocked` certifies the fields.

The optional callback has this signature:

```text
fn admission_changed(&mut self) -> Option<Self::Output> { None }
```

`Blocked` arms one connection-wide notification.
Relevant changes move this fixed-size observer to its pending state.
Multiple changes coalesce until the core calls `admission_changed`.
The core disarms the observer before that callback.
Another `Blocked` result arms the next notification.
Repeated `next` calls without a new change do not repeat the notification.

Changes include accepted peer SETTINGS, protocol-slot release, application retirement, queue-item release, and control-byte release.
Queue items become available at write issuance.
Control bytes remain reserved through partial writes and become available at final settlement.
Stream invalidation, GOAWAY, shutdown, abort, and stream-ID exhaustion also produce a notification for an armed observer.
Affected queued commands can then resolve without waiting for retained body receipts.
The notification does not promise capacity or command validity.

### Adapter retry procedure

1. If a metadata command returns `Blocked`, retain its fields in the adapter.
2. Forward `admission_changed` through each composite or callback adapter.
3. After the notification, retry the queued command through its normal public method.
4. If the retry returns `Blocked`, wait for the next notification.
5. If the command succeeds, remove it from the adapter queue.
6. If the command returns another error, resolve that command through its error contract.

The adapter owns any metadata queue.
The core retains protocol framing and concurrency calculations.
This interface adds no request permits.
Successful commands retain HPACK commitment order, including commands whose transport writes remain outstanding.

## Source demand and retained capacity

`send_ready` transfers one single-use permit for one stream.
The permit reserves bounded application storage, not current wire credit.
The engine schedules the admitted buffer against both wire windows.
Applications do not poll `send` or calculate window balances.
One admitted buffer can span several DATA frames.

The permit states separate maximum byte length and retained capacity.
The byte allowance includes declared content length and the remaining message-body limit.
Applications do not recalculate those limits.
A zero-byte permit still accepts empty final DATA without stream or connection wire credit.
The application can also finish with trailers.
Zero allowance does not assert that the producer reached EOF.
Rejected bytes leave the original permit and buffer available to the caller.

Client CONNECT source demand waits for a final response.
A successful response removes the HTTP body limit before the first source permit.
This avoids a stale zero-byte permit when a CONNECT request becomes a tunnel.
An unsuccessful response does not implicitly abort the request producer.

`Vec<u8>` reports its allocation capacity.
`BodyOp` reports the complete page capacity, even for a one-byte range.
Custom buffers must report all backing allocations that their ownership retains.
The engine returns both the permit and the buffer after an admission error.

Reset, excluded GOAWAY streams, and connection failure revoke outstanding permits.
`send_stopped` reports source revocation independently of outstanding write settlement.
`sent` returns the original buffer after its last transport obligation settles.
A successful complete write remains successful after a concurrent stream reset.
Its stream outcome can still report that reset.
A later connection failure preserves an already completed exchange and an existing reset, deadline, or retry outcome.
An observed response prevents a later GOAWAY boundary from making that exchange retryable.
Producer completion alone does not establish successful transport settlement of END_STREAM.
A subsequent RST_STREAM(NO_ERROR) does not discard a complete response.
Its metadata and body fragments remain available.
Its `ReceiveEnd` keeps `StreamOutcome::Complete`.
The separate source-stop and retirement outcomes can report `Reset(0)`.

## Pages and drive cost

Pages contain 32 KiB of initialized storage.
The pool transfers exclusive ownership to each `ReadOp`.
Complete frames borrow that page without a DATA payload copy.
Fragments retain a shared page range.
The last page owner returns the page to the reusable pool.

The aggregate limit counts each allocated page once.
The per-stream limit conservatively charges one complete page for each retained fragment.
The fragment count also limits empty DATA.
This conservative policy can reset a stream before the aggregate page pool fills.
The connection reports aggregate exhaustion explicitly.

The default stream window is 65,535 bytes.
The default connection window is 1 MiB.
The default receive capacity is 8 MiB.
`HttpLimits::max_body_bytes` limits message length separately from these storage and window settings.

Blocked senders occupy bounded ordered sets.
A connection WINDOW_UPDATE swaps a set in constant time.
Each subsequent drive transition examines one recorded sender.
SETTINGS window changes preflight all affected windows before commitment.
Ordered readiness entries have per-stream removal keys.
Retirement removes those entries even when an earlier stream retains its permit.
One-byte connection refunds rotate among blocked siblings without a whole-stream scan.

Released receive credit uses one connection counter and one counter per live receive half.
One ordered entry per stream records pending stream credit.
Repeated releases do not allocate one queued WINDOW_UPDATE per fragment.
The writer emits at most one connection update and one stream update in a credit batch.
Only that committed output restores the ordinary receive-window balance.
The counters remain separate from application fragment leases.

Existing queued control blocks precede a new credit batch.
After each credit batch, the currently queued control blocks receive a turn.
After those blocks, eligible DATA receives a turn before another credit batch.
Partial writes retain their original output before either class can proceed.
This preserves HPACK block order and prevents control or credit starvation.
Closing a receive half removes its uncommitted stream credit, but preserves connection credit.
Terminal connection shutdown can discard uncommitted credit.

| Default limit | Value |
| --- | --- |
| Active application records | 100, including records with retained body fragments |
| Header section | 64 KiB encoded bytes and decoded field accounting |
| Header occurrences | 100 |
| Message body | 8 MiB per direction, separate from flow windows |
| Retained receive capacity per stream | 256 KiB |
| Retained fragments per stream | 128, including empty fragments |
| Retained send capacity per connection | 8 MiB |
| Send permit length and capacity | 64 KiB each |
| Queued control items | 512 |
| Retained control capacity | 2 MiB |
| Drive transitions per turn | 128 |
| SETTINGS ACK deadline | 10 seconds after output settlement |
| Shutdown deadline | 30 seconds after the shutdown command |

Successful CONNECT tunnels do not use the HTTP message-body limit.
The decoder uses its advertised table limit.
The encoder also has a 1 MiB hard table limit.
The page and send limits do not represent a bound on total process memory.
Codec tables, compact-field storage, collection nodes, and operation metadata have separate bounded storage.
The generic buffer type also occupies inline metadata space.
A compact buffer handle avoids large inline arrays in every stream record.

The control budget refills from supplied time once per second.
Its limits are 64 SETTINGS, 4096 WINDOW_UPDATE, 64 PING, 256 RST_STREAM, four GOAWAY, and 256 PRIORITY frames.
Protocol progress also restores up to two WINDOW_UPDATE credits per frame.
Aggregate output exhaustion permits one additional emergency GOAWAY.
It does not permit an unbounded error queue.

## Alarms and settlement

`WakeOp` is a deadline alarm, not an I/O-readiness request.
The core maintains separate SETTINGS, stream, and shutdown deadlines.
Each alarm has a distinct operation token.
An earlier replacement deadline causes cancellation of the old alarm.
The caller must complete both the cancellation request and the original alarm.
New alarm admission counts outstanding alarms and all unacknowledged cancellations together.
The normal limit is `max_outbound_items`.
Three additional cancellation slots permit the current alarm, read, and write to settle during teardown.
Thus, these two ledgers together retain at most `max_outbound_items + 3` identities.
An exhausted ledger causes explicit aggregate failure without removing an issued obligation.

An invalidated alarm completion releases its token without advancing protocol time.
It cannot trigger a replacement deadline.
This also applies after a deadline command but before the next drive dispatches cancellation.
An unchanged shared deadline keeps its original alarm operative.
An early current alarm completion settles that alarm but does not expire its deadline.
The next drive requests a new alarm at the same deadline.
The SETTINGS ACK deadline starts after the complete original SETTINGS write settles successfully.
The server shutdown PING wait starts after its complete original write settles successfully.
The overall shutdown deadline still starts at the shutdown command.
Expiry requests cancellation of blocked transport operations and abandons unsent output.
The final connection result remains `Graceful` for caller-requested shutdown.
Individual unfinished streams report their separate failure outcomes.

Stream deadlines remain active until both protocol halves end and the final outbound write settles.
Retained application fragments alone do not keep those deadlines active.
Due deadlines run before new normal work.
An exhausted output budget during a deadline causes explicit aggregate failure.
It does not silently remove the deadline.

Transport close waits for outstanding read, write, cancellation, and alarm operations.
Application body receipts can remain outstanding after transport close.
Stream retirement still waits for those receipts.

The role wrappers expose shared commands through `Deref<Target = Connection<B>>`.
The shared type has no public constructor and no role-specific request or response command.
This arrangement avoids duplicate forwarding code without a universal operation interface.

## Hard abort and native alarm failure

`abort()` initiates hard teardown with `ConnectionResult::Aborted`.
`abort_with_cause(cause)` accepts an explicit connection result, including `IoFailed`.
Both methods return `()` and are idempotent.
They abandon unsent output and request cancellation through the normal ports.
They do not fabricate read, write, or timer completions.
No graceful PING wait or shutdown deadline delays these cancellation requests.

The first non-graceful connection result remains primary.
A hard abort can escalate graceful drain, including a pending close operation.
Abort after close completion has no effect.
A close failure changes an otherwise graceful result to `IoFailed`, but does not replace an established non-graceful result.
Original operations and cancellation acknowledgments still gate close.
Already successful exchanges retain their outcomes.

`WakeOp::complete(now)` remains the successful alarm input.
`WakeOp::failed(error)` reports `IoFailure` without a time sample.
`WakeOutcome` contains `Fired(Duration)` and `Failed(IoFailure)`.
`WakeCompletion::token()` and `outcome()` expose read-only routing information.
`WakeCompletion::into_parts()` now returns `(WakeOp, WakeOutcome)`, not `(WakeOp, Duration)`.
This is a deliberate completion-shape change for adapters.

An active alarm failure hard-aborts with `IoFailed`.
This includes an unexpected `Failed(Cancelled)` on an active alarm.
An alarm becomes obsolete when its deadline is no longer current, even before the core dispatches cancellation.
All obsolete alarm completions only settle the original obligation.
They do not advance time, change the primary result, or discard committed protocol output.
An unchanged earliest deadline keeps the original alarm active, even if one stream changes its own deadline.
An adapter must not substitute the requested deadline for a failed native timer.

## Executable API example

`examples/memory.rs` drives both roles with seven-byte I/O completions.
`examples/support/mod.rs` supplies explicit callback and completion wiring.
`examples/interop.rs` belongs to the parent integration work and is not part of this implementation.
`src/http/**` and `tests/http_composition.rs` also belong to the parent.
The pure engine does not create or drive an HTTP/1 parser.

## Standalone qualification

The all-feature suite contains 213 unit tests, 77 integration tests, and two doctests.
Eight unit tests cover new direct-engine bounds and defensive state transitions.
The default suite omits five feature-specific component tests.
These counts do not include the six Criterion smoke workloads.
Eight admission tests cover exact HPACK wire equivalence, partial-write budgets, peer concurrency, terminal invalidation, and held ownership joins.
They use yielding and continuing notifications and repeated idle turns to bound notification counts.

The independent ownership model uses obligation sets rather than the engine selector.
It explores 336 stream schedules and 480 connection schedules.
The stream schedules combine 14 write cuts, six event orders, and four callback suspension policies.
The connection schedules use all 120 orders of five outstanding completions and four callback suspension policies.
Assertions cover exact wire bytes, original buffer identity, forbidden duplicate writes, receipts, and retirement joins.
This is bounded schedule exploration, not an exhaustive proof or a concurrency model.
Another 48 schedules place response END_STREAM and RST_STREAM(NO_ERROR) in one read batch.
They cover HEADERS, DATA, and trailer endings, four callback policies, two release orders, and two write cuts.
An upload remains outstanding throughout response processing.
Assertions preserve the complete response and require sibling completion.
The hard-abort model adds 11,520 schedules.
It explores all 720 orders of three original operations and three cancellation acknowledgments.
Four callback policies and four write outcomes cover cancellation, partial acceptance, a fully successful race, and an uncertain write receipt.
Close requires all six obligations, and each original send buffer returns exactly once.

The sustained-credit cases send 9 MiB plus 17 bytes in each direction with the message-body limit raised.
The advertised flow windows retain their defaults.
Another case completes 1,030 separate 1 KiB streams.
Small-window cases cover padding, reset, discarded DATA, negative send windows, and one-byte connection refunds.
Held-page cases exercise sibling progress and explicit aggregate exhaustion.
Late-outcome cases cover batched release, failed reads, exact final writes, HEAD responses, and contradictory GOAWAY boundaries.

The resource cases assert exact control sequences and HPACK continuity after rejected header sections.
They also cover supplied-time budgets, shutdown admission failure, ACK deadlines, tunnel limits, and malformed trailers.
The inherited component tests remain useful regression coverage, not independent peer evidence.

### Frozen-baseline review regressions

The independent source review used the earlier `938f631d` baseline.
The following regressions cover its seven reported defects:

| Defect | Regression evidence |
| --- | --- |
| Response end removes upload half | `response_end_closes_only_receive_while_upload_continues` covers HEADERS, DATA, and trailers |
| Partial write reports full success after read failure | `original_final_write_receipt_decides_success_after_unrelated_read_failure` includes 10 accepted wire bytes for a 100-byte body |
| Body admission loses state consistency | `streaming_body_limit_without_content_length_preserves_both_role_buffers` preserves both rejected buffers and permits |
| CONNECT uses HTTP body limits | `classic_connect_tunnel_bytes_are_not_an_http_message_body_limit` exercises both tunnel directions |
| HEAD informational response carries END_STREAM | `head_informational_response_does_not_end_stream_or_apply_representation_body_limit` asserts the exact frame sequence |
| Read cancellation removes queued GOAWAY | `all_connection_original_and_cancellation_ack_orders` covers 480 orders with continuing and suspended callbacks |
| Obsolete wake completion hides unbounded cancellation debt | `alarm_replacement_bounds_originals_and_delayed_cancel_acknowledgments` covers limits of four and 512 identities |

The alarm cases retain acknowledgments alone or both acknowledgments and original alarms.
They return these obligations in either order after explicit exhaustion.
All issued completions remain acceptable, and close waits for the complete settlement join.
A forced-shutdown case exercises all three reserved cancellation slots.
Separate cases cover obsolete completion before cancellation dispatch and unchanged shared deadlines.

An additional fault-injection test exercises an unexpected private DATA planning error.
The engine emits one INTERNAL_ERROR reset and removes both private and application stream state.
Retirement waits for reset output settlement.
Late headers produce no application callback, but still update HPACK before a sibling response.
If reset admission fails, the engine reports aggregate resource exhaustion.

### Concurrent receive-credit regression

The corrected `34ee4b71` integration still failed a concurrent large-duplex workload.
Its causal trace showed about 512 queued WINDOW_UPDATE frames with one-byte increments.
The server then emitted GOAWAY with ENHANCE_YOUR_CALM and reported aggregate resource exhaustion.
This was separate from the earlier send-half defect.

A local negative control reproduced exhaustion after three released bytes with an outbound-item limit of four.
The corrected case retains the same limit and processes 600 one-byte fragments behind an outstanding write.
It then emits exactly three updates: connection credit 600, followed by stream credits 200 and 400.
The same test also uses the default item limit.

A second regression runs ten batches of eight concurrent bidirectional 1 MiB exchanges.
It uses exact direct transport receipts, yielding body callbacks, immediate fragment release, and fixed supplied time.
All 160 MiB of payload match the expected bytes, and all streams retire successfully.
Each batch also requires response DATA to progress before all uploads finish.
Coalescing alone passed the byte totals but failed this directional-progress assertion.
The writer now alternates eligible DATA with credit batches without weakening ordinary control priority.
Neither regression increases a capacity limit or accepts a failed stream outcome.

## Frozen benchmark workload

`benches/engine.rs` defines the workload family `pure-http2-v1`.
Each iteration creates direct engines and uses an owned-operation executor.
The executor copies transport bytes between in-memory queues.
Thus, the workload includes executor cost, not only protocol cost.
It excludes sockets, TLS, HTTP/1, runtime wrappers, and application header clones.
Send buffers return through `Sent` for reuse.

| Workload | Exchange |
| --- | --- |
| `new-1x-empty` | One empty response |
| `new-1x-1KiB` | One 1 KiB response |
| `new-16x-1KiB` | Sixteen concurrent 1 KiB responses |
| `new-4x-64KiB-held` | Four 64 KiB responses with one fragment retained for eight turns |
| `new-1x-4MiB` | One 4 MiB response |
| `new-1x-1KiB-fragment7` | One 1 KiB response with seven-byte I/O completions |

Only compilation and Criterion test mode qualify this workload here.
No timing result or copy-cost reduction claim accompanies this stage.

Run the following commands from the workspace root:

```sh
export CARGO_TARGET_DIR=/workspace/kimojio-rs/target/http2-program/build-http2-core
cargo fmt --all
taskset -c 8-31 cargo test -p kimojio-fsm-http2
taskset -c 8-31 cargo test -p kimojio-fsm-http2 --all-features
taskset -c 8-31 cargo test -p kimojio-fsm-http2 --release
taskset -c 8-31 cargo test -p kimojio-fsm-http2 --release --all-features
taskset -c 8-31 cargo clippy
taskset -c 8-31 cargo clippy --all-targets --all-features
taskset -c 8-31 cargo run -p kimojio-fsm-http2 --example memory
taskset -c 8-31 cargo bench -p kimojio-fsm-http2 --bench engine -- --test
```
