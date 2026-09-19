# WebSocket broadcast chat

## Run

The example combines the standalone HTTP/1 FSM, the WebSocket FSM, and a pure broadcast hub.
The outer executor uses Kimojio raw I/O operations.

Build the executable:

```sh
cargo build --release -p websocket-chat
```

Start a loopback server:

```sh
./target/release/websocket-chat --bind 127.0.0.1:0 --run-for-ms 60000
```

The readiness line is `LISTEN 127.0.0.1:PORT`.
The default runtime is 60 seconds.
At the runtime limit, the root stops admission and requests close code 1001.
After the shutdown grace period, it cancels remaining operations and waits for their original completions.
The final stderr line reports `STOP clients=0`, accounted storage, and peak accounted storage.

Use `--help` for the complete CLI.
The limit flags are `--max-clients`, `--max-message-bytes`, `--max-queued-messages`, `--max-client-bytes`, and `--max-total-bytes`.
`--max-connections` is an alias for `--max-clients`.
Timeout flags control handshake, idle, frame, message, write, and close deadlines.
`--frame-bytes` controls outgoing fragmentation.
`--send-buffer-bytes` controls the kernel send buffer, outside the storage cap.

This is a public demonstration service without TLS or authentication.
It accepts valid handshakes on every path and does not restrict origins.
It selects no subprotocol or extension.
The default bind address is loopback.
It does not provide outgoing producer streaming.

## Application contract

`Hub::next` uses generic callback ports that return `Option<Output>`.
A callback owns each operation before it returns.
`None` consumes the operation without suspension.
The root controls work budgets through selective outputs and `yield_turn`.
An intrusive ready list contains only clients with new work.

`Chat::next` composes the three pure machines through the same callback contract.
An indexed connection array and a bounded ready queue prevent repeated scans of idle connections.
A sorted, bounded deadline table supplies the next root timer.
The root drives at most 128 externally visible work items before it yields.
Native workers report completions through one bounded root mailbox.
No channel or task separates the protocol and application machines.

The WebSocket handshake validator supplies protocol policy.
HTTP retains ownership until the complete 101 response and `upgrade_ready`.
`take_upgrade` transfers the receive buffer and its exact unread range.
HTTP and WebSocket operations have distinct completion identities.

`admit` reserves a client slot and its storage.
`activate` makes the client a broadcast recipient after the HTTP upgrade completes.
Pending handshakes never receive messages.

`begin`, `append`, and `finish` assemble one message per publisher.
The hub checks complete text messages for UTF-8 errors before publication.
Every eligible recipient receives each complete text or binary message, including the sender.
All recipient queues preserve one publication order.
The hub permits one outgoing lease per recipient.

`SharedPayload` implements `AsRef<[u8]>`.
The hub shares immutable payload storage through `Rc`.
The public wrapper does not implement `Clone`.
Each send operation returns its original wrapper through `DeliveryCompletion`.
Rejected completions retain their payload and identity.

## Storage limits

Default limits:

| Resource | Limit |
| --- | --- |
| Admitted clients, including pending and closing clients | 64 |
| Bytes per complete message | 1 MiB |
| Queued and in-flight messages per recipient | 32 |
| Queued and in-flight storage per recipient | 4 MiB |
| Aggregate accounted storage | 32 MiB |

`Chat::new` rejects configurations whose WebSocket buffer limit cannot hold the maximum hub message.
The receive buffer can remain smaller because the WebSocket FSM receives messages incrementally.

The aggregate limit covers these resources:

- The hub, ledger, and allocated client slots.
- Fixed-capacity queues and one external delivery-completion slot per client.
- Incomplete message allocations.
- Shared payload allocations and their `Rc` metadata.
- Root-owned storage declared through `external_bytes_per_client`.
- Fixed root arrays, ready queues, deadline entries, and completion-mailbox capacity.

The hub charges each shared payload once.
Per-recipient limits charge its allocated capacity for every retained reference.
Queue metadata and the delivery-completion slot also consume the per-recipient limit.
An incomplete incoming message consumes the message limit and aggregate limit, not the outgoing recipient limit.

Boxed slices have explicit capacities.
Growth reserves both the old and new capacities before allocation.
Tiny messages allocate only their next power-of-two capacity, capped at the message limit.
Empty messages allocate neither payload storage nor shared payload metadata.
An allocator test checks this property.

A recipient that exceeds its outgoing limit leaves the broadcast set.
The hub requests close code 1008.
Other recipients still receive the message.
An aggregate assembly failure removes the publisher with code 1008 before publication.
The hub does not remove unrelated clients to free space.
Invalid text produces code 1007.

Logical closure clears queues and incomplete messages.
It does not release the in-flight lease or its client reservation.
`closed` means that the root released its external storage and settled every original I/O operation.
The hub reuses the slot only after both `closed` and the final delivery completion.
A cancellation acknowledgement is not an original I/O completion.

The limit is **not whole-process RSS**.
It excludes allocator overhead, stacks, executable pages, kernel socket storage, and runtime allocations outside declared reservations.
The executor must declare its per-client reservations before admission.
Normal allocator exhaustion can still terminate the process.

The native executor reserves two original-operation future slots per client.
Its reservation includes cancellation tokens, descriptor ownership, protocol states, and the 16 KiB receive buffer.
HTTP metadata reserves a conservative sixteen times the 8 KiB handshake-head limit, including vector growth.
This reservation persists until final connection retirement.
Shared message capacities still have exact, separate charges.
The native task scheduler and ring bookkeeping remain outside this application-storage cap.

## Tests

Run the application, composition, cancellation, and actual-wire tests:

```sh
cargo test -p websocket-chat
```

The tests cover publication order, sender inclusion, complete-message checks, admission, memory limits, and empty-message allocations.
They also cover slow recipients, stale identities, logical closure, late completions, and both resource-settlement orders.
The public composition regression receives a 20,000-byte message through 16 KiB reads and accepts outgoing writes in 37-byte steps.
It checks exact sender and peer payloads, a healthy follow-up broadcast, and complete storage settlement.
The actual-wire test requires Python 3 and a working native io_uring runtime.
It covers concurrent publishers, sender inclusion, coalesced upgrade input, invalid text, ping, empty messages, a slow recipient, reset recovery, and bounded shutdown.
It also checks that the child descriptor count returns to its baseline.
An additional actual-wire case sets the message and outgoing buffer limits to exactly 20,000 bytes.
It checks both recipients, legal outgoing fragments, a healthy follow-up broadcast, and normal close handshakes with the unchanged 16 KiB receive buffer.

## Native operation contract

`native::read_once` uses one Kimojio raw read.
`native::write_once` uses one Kimojio raw writev.
Neither function retries partial progress.
The protocol receives the exact original completion count.

A canceled token prevents submission of another operation through that token.
Cancellation of an in-flight operation keeps its original future, buffers, and iovecs alive until the original completion.
A successful completion retains its actual byte count, even after cancellation.
The functions do not use write-all operations or `io_scope_cancel`.

Real-kernel tests cover vectored writes, reads, EOF, pre-submission revocation, a blocked read, a blocked write, and a positive partial write.
A deterministic race test covers success after cancellation.
The hub also accepts a failed delivery receipt after the recipient closes.
A typed WebSocket integration test checks rejection after previously observed send capacity.

The accepted sockets do not use `O_NONBLOCK`.
io_uring supplies the asynchronous wait for their raw operations.
Kimojio currently has no raw readiness operation.
An unexpected explicit readiness request receives an unsupported-operation error and terminates that connection.
The driver never reports simulated readiness or retries in a polling loop.

A closing connection finishes its current frame before the close frame.
If the peer drains data too slowly, the close deadline cancels remaining I/O and terminates the transport.
The peer can then observe EOF before a complete close frame.
The default close deadline is one second.
`--close-timeout-ms` changes that bound, not the frame-ordering policy.
