# kimojio-fsm-websocket

This crate supplies a server-side RFC 6455 state machine.
It performs no I/O and reads no clock.
It uses the HTTP/1 foundation for the opening handshake.
It has no dependency on a runtime, HTTP/2, the HTTP wrapper, or the static-file application.

## Construction and handshake

`Server<B, W = B>` accepts mutable receive storage `B: Buffer`.
Outgoing storage `W` only needs `AsRef<[u8]>`.
The core does not clone outgoing storage or require reference counting.

`Server::new(connection, config, buffer, now)` starts framing after an external handshake.
`Server::from_handoff(http.take_upgrade()?, config, now)` consumes an HTTP handoff.
Constructor rejections return the receive storage.
A rejected handoff returns `UpgradeInput { connection, buffer, range }`.

`Handshake::validate(request_head)` validates WebSocket handshake fields.
The application owns resource selection, origin policy, authentication, and authorization.
The handshake requires GET, HTTP/1.1, a valid key, version 13, and an empty request body.
It rejects `Expect` and transfer coding.
It validates extension offers but selects no extensions.
It validates requested subprotocol tokens but selects no subprotocol.

1. After the HTTP request callback returns, call `handshake.accept(&mut http, exchange)`.
2. Continue HTTP progress until `upgrade_ready`.
3. Call `take_upgrade`.
4. Pass the handoff to `Server::from_handoff`.

The handoff contains the connection identity and exact unread bytes, not a socket.
The outer executor retains the transport.
No WebSocket output can precede the completed HTTP 101 output.
The receive buffer retains coalesced WebSocket bytes without a copy.
RFC clients normally wait for 101 before they send frames.

`HandshakeError::respond` emits an empty 400 or 426 response through HTTP.
A 426 response includes `Sec-WebSocket-Version: 13`.
The owner must request HTTP graceful shutdown after a rejected handshake.
SHA-1 supplies the RFC handshake digest, not authentication.

```rust
use kimojio_fsm_websocket::{
    Config, ConnectionId, MessageKind, SendMessage, Server, Tick,
};
use std::rc::Rc;

let mut server = Server::<Vec<u8>, Rc<[u8]>>::new(
    ConnectionId { slot: 1, generation: 1 },
    Config::default(),
    vec![0; 8192],
    Tick(0),
).unwrap();
let payload: Rc<[u8]> = Rc::from(b"hello".as_slice());
let id = server.send_message(SendMessage {
    kind: MessageKind::Text,
    buffer: payload,
    range: 0..5,
}).unwrap();
assert_eq!(id.connection(), server.connection());
assert!(!server.can_send());
```

## Progress and owned operations

Call `next(&mut ports) -> Option<P::Output>` to make progress.
Every callback returns `Option<P::Output>`.
`None` accepts the operation without completing it.
`Some` suspends with caller output.
The callback must not reenter the machine.
The root owns scheduling budgets.
Each call drains finite buffered work.
Callbacks cannot complete operations through reentry.
One outstanding read and one outstanding write bound additional work.
Operation IDs belong to this protocol and a connection generation.
The executor must route HTTP and WebSocket completions to their original owners.

Read and write operations own their buffers independently.
Use `ReadOp::bytes_mut()` or `WriteOp::slices()` for I/O.
Return `op.complete(result)` through `complete_read` or `complete_write`.
One read and one write can remain outstanding together.
The FSM owns every short-write cursor, including frame headers.
`WouldBlock` requests a readiness operation before a retry.
`Interrupted` permits a retry with no additional accepted bytes.
An executor completes readiness through `complete_readiness`.
A zero-byte write is a terminal failure.

Raw Kimojio `operations::read` and `operations::writev` return exact counts.
`AsyncStreamWrite::write` and `AsyncStreamWrite::writev` instead write all offered bytes.
A write-all success means the complete offered length.
A write-all failure can conceal accepted bytes.
The executor must report `UnknownProgress` for that failure.
`UnknownProgress` and `CancelledUnknownProgress` are terminal.
Their receipts report `Acceptance::LowerBound`.
The FSM never retries an unknown prefix.

Cancellation requests do not release storage.
The executor must retain submitted operations until their original completion.
Submitted buffer, frame-header, and iovec addresses must remain stable until completion.
A successful completion can win a cancellation race.
Rejected completions retain their operations and results.
`into_parts` recovers read and write operations.
`ChunkCompletion::into_op` recovers a rejected chunk.
Wrong-owner, stale, and invalid-count completions do not change live operations.

## Internal state and coordination

The implementation follows the family-wide [state and coordination guidance](../docs/fsm-composition.md#maintainable-state-and-coordination).

`Server` contains independent state components instead of overlapping lifecycle and ownership flags.
The public commands, callbacks, completions, and storage bounds remain unchanged.

| Component | Exclusive alternatives |
| --- | --- |
| Lifecycle | Open, close handshake, transport termination, transport closed |
| Local close | Pending close reason or completed close write |
| Transport close | Resource settlement or an outstanding close identity |
| Peer close | Absent, pending notification, or reported reason |
| Each I/O direction | Idle, readiness needed, original operation in flight, or cancellation requested |
| Receive storage | Available buffer, outstanding read, or retained chunk lease |
| Outgoing message | Idle, pending payload, framed payload ownership, or returned receipt |
| Incoming message | Idle, active message with start notification, or completed message notification |
| Receive framing | Partial header or payload with a complete header |
| Timers | Independent deadline candidates and the selected notification |

A framed payload belongs either to queued output or to the original external write.
Its receipt contains the same payload storage.
The machine cannot accept another message until it reports that receipt.
Parser state cannot simultaneously contain a partial header and an active frame.
The active message retains UTF-8 and fragmentation state across control frames.

[`coordinator.rs`](src/coordinator.rs) separates pure transition selection from committed effects.
`next` selects one transition, applies it, and repeats unless a callback yields.
No eligible transition means that the machine is blocked.
Private transition labels contain no operations or buffers.
The coordinator introduces no queue, payload allocation, payload clone, or runtime dependency.

The lifecycle restricts eligible work:

| Lifecycle | Permitted protocol work |
| --- | --- |
| Open | Transmit control or data, report message boundaries, parse input, and issue reads |
| Closing handshake | Return unstarted data, finish a partial frame, send close, and receive permitted peer frames |
| Transport termination | Return retained output, cancel original operations, await storage, and issue transport close |
| Awaiting transport close | Wait for the original close completion |
| Closed | Report one terminal result |

Pending timer changes, outgoing receipts, and peer-close notifications precede lifecycle work.
Close output takes priority over pending pong and message output.
A protocol failure or peer close stops new input and requests cancellation of the original read or readiness operation.
A local close without failure still permits input because the peer close can follow unread frames.
Transport termination does not issue reads, writes, or readiness requests.
It does not deliver incoming messages.

Soft close cannot discard a partly accepted frame or insert close bytes into that frame.
Transport termination can abandon retained output, including a partial pong after the close handshake completes.
It still waits for the original write completion before returning externally owned storage.
The final transport close requires settled read, write, readiness, and chunk ownership.
Repeated aborts preserve an outstanding close identity.

Close-write completion and peer-close acceptance explicitly advance the handshake.
The coordinator does not reconstruct handshake completion from an ordered scan of booleans.
The recorded close-write outcome survives transport settlement and contributes to `ConnectionResult::clean`.
The first failure remains separate from handshake progress.

Deadline refresh is an explicit transition.
It precedes notification delivery and preserves the existing timeout policy.
Selection neither consumes a notification nor changes timer generations.
Each selected transition commits its notification or operation state before the callback.

Debug assertions cover local ownership and cross-component relationships at drive and public transition boundaries.
The bounded termination model explores 108 schedules in four drive modes.
It covers three input owners, five output states, both settlement orders, abort, timeout, exact completion, and unknown write progress.
An independent ownership join determines when close must become eligible.
The model checks exact receipts, retained identities, forbidden effects, selection purity, and internal-cycle absence.
Additional cases cover both handshake orders, partial pong settlement, and operation or timer sequence exhaustion.
This model has fixed payloads and bounded completion schedules. It is not a proof of the complete protocol.

## Incoming messages

`message_started` identifies an incoming message.
`chunk` transfers an unmasked receive-buffer lease.
Copy or consume `op.bytes()`, then return `op.release()` through `release_chunk`.
Release consumes the complete offered chunk.
Only `message_finished` reports a complete, valid message.
Earlier chunks of text can precede a later validation failure.
Do not broadcast partial messages.
A retained chunk stops further reads, but does not stop outgoing I/O.
Control frames cannot bypass unread data when the application retains the only receive buffer.
The application must return all chunk leases during cancellation.

The parser preserves masking offsets across reads.
It preserves UTF-8 state across text fragments and intervening control frames.
It rejects nonminimal lengths, invalid opcodes, reserved bits, unmasked input, and invalid fragmentation.
Control frames require FIN and at most 125 payload bytes.
Ping responses contain the same payload.
The machine retains at most one pending pong, in addition to an issued control write.
A newer ping can replace the pending pong.
An unsolicited pong produces no response.

## Outgoing messages

`send_message(SendMessage { kind, buffer, range })` admits one complete outgoing message.
`can_send` indicates admission capacity.
Rejection returns the command and storage.
`message_sent` returns that storage with payload acceptance and result.
Acceptance means transport acceptance, not peer receipt.
The machine fragments outgoing messages at `outgoing_frame_bytes`.
It supports incremental reception, not outgoing producer streaming.
The machine validates outgoing text before admission.
It does not expose an outgoing producer stream.
`fail_source` reports an application failure and starts a 1011 error close.

## Close and time

`close(CloseReason)` starts the closing handshake.
It stops new message admission and returns abandoned outgoing storage.
An already-started frame finishes before the close frame.
The machine never inserts close bytes inside another frame.
The server closes the transport after both close frames complete.
The root executes `CloseOp` and returns `complete_close`.
`closed` occurs only after transport close and operation settlement.

| Condition | Result |
| --- | --- |
| Protocol violation | Error close 1002 |
| Invalid text or close-reason UTF-8 | Error close 1007 |
| Frame, message, or fragment limit | Error close 1009 |
| Application failure | Error close 1011 |
| Transport failure, abort, or timeout | Transport termination |
| EOF without a peer close | `Failure::UnexpectedEof` |

A protocol failure stops further peer-data processing.
The machine attempts its error close without waiting for a peer close.
The close deadline bounds this attempt.
`abort` requests cancellation instead of a closing handshake.
The machine retains the first failure.
Outgoing receipts can arrive after that failure.

`CloseReason::empty()` represents a close without a status code.
`CloseReason::new(code, reason)` validates server codes and a UTF-8 reason of at most 123 bytes.
The machine accepts defined codes through 1014 and application codes from 3000 through 4999.
It rejects reserved codes 1004, 1005, 1006, and 1015.
It rejects unassigned protocol codes from 1016 through 2999.
It accepts client code 1010 but replies with 1000 rather than echoing a client-only code.
Local reports use `None` for absent status codes, not wire codes 1005 or 1006.
`ConnectionResult::clean` requires both close frames and successful transport settlement.

All durations use nanoseconds in one caller-supplied monotonic domain.
`observe_time` updates time without expiring deadlines.
`deadline_changed` reports the earliest deadline.
`expire` rejects stale, early, wrong-owner, and regressed observations.
The root schedules deadlines without reconstructing protocol policy.

| Deadline | Default | Policy |
| --- | --- | --- |
| Idle | 60 seconds | Positive reads reset it |
| Frame | 30 seconds | Absolute from the first header byte through frame consumption |
| Message | 30 seconds | Absolute across data fragments and chunk retention |
| Write | 30 seconds | Positive write progress resets it |
| Close | 5 seconds | Absolute from the closing decision |

`None` disables a deadline.
Disabling deadlines permits indefinite retention by a stalled peer or application.

## Storage limits and chat composition

Default limits are 1 MiB per message, 1 MiB per data frame, and 65,536 fragments per message.
The default outgoing frame size is 16 KiB.
`max_buffer_bytes` bounds the addressable length of each accepted buffer.
The core owns no payload allocator or broadcast queue.
It retains one receive buffer, one outgoing message, and fixed-size parser and control storage.
The output type can use exclusive storage, `Rc`, or `Arc`.
No atomic reference count is mandatory.

The root must account for backing capacities that `AsRef` does not expose.
A slice can hide a much larger retained allocation.
A chat cap describes owned storage and accounted allocations, not total process RSS.
Allocator metadata, kernel memory, and unrelated runtime storage need separate budgets.

For bounded message assembly:

1. Reserve the new capacity before allocation.
2. During growth, charge both old and new allocations.
3. After growth, release the old capacity charge.
4. At publication, retain the full backing-capacity charge until its last reference disappears.
5. Charge shared payload storage once and queue entries per recipient.
6. Include assemblies, receive buffers, pending operations, and preallocated queue capacities in the root cap.

Empty messages need no payload allocation.
Tiny messages do not reserve a maximum-size message allocation.
A wrapper around `Rc<Vec<u8>>` can implement `AsRef<[u8]>` without copying payload bytes during publication.

The chat consumer must broadcast only complete text or binary messages to all open clients, including the sender.
Per-client overflow removes that recipient from membership and starts a 1008 close.
Assembly-budget overflow removes the publisher and starts a 1008 close without publishing its incomplete message.
Connection admission fails when the root cannot reserve its initial storage.
These policies belong to the chat consumer, not the protocol machine.

## Scope and validation

The crate supports server framing and HTTP upgrade composition.
It excludes client-mode framing, compression, extensions, subprotocol selection, outgoing producer streaming, TLS, and runtime execution.
Applications can supply established TLS transports through the same owned operations.
The crate does not supply a chat executable.

The tests cover framing splits, masking, fragmentation, UTF-8, lengths, control frames, close races, ownership, cancellation, deadlines, and HTTP handoff.
The HTTP handoff test uses one-byte response writes at every request split.
It requires the complete 101 before handoff and retains the receive-buffer address.
Native-driver interoperability and comparative performance require the separate chat consumer.

Validation commands:

```sh
cargo test -p kimojio-fsm-websocket
cargo test -p kimojio-fsm-websocket --release
cargo fmt
cargo clippy
cargo clippy --all-targets --all-features
```

References: [RFC 6455](https://www.rfc-editor.org/rfc/rfc6455),
[IANA close codes](https://www.iana.org/assignments/websocket/close-code-number.csv),
and the [family composition contract](../docs/fsm-composition.md).
