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

## Coordination contracts

The design follows [the composition guidance](../../docs/fsm-composition.md).
The hub separates lifecycle, delivery settlement, and ready-list membership.
The receive assembly belongs only to `Active`.
An outgoing delivery can remain outstanding across logical removal and external settlement.
The fixed queue allocation remains resident until both settlement conditions hold.

| Hub lifecycle | Eligible callback | Transition and ownership |
| --- | --- | --- |
| `Handshake` | None | Admission reserves storage. Activation enters `Active`. |
| `Active` | `send`, if queued data exists and no delivery is outstanding | The hub transfers one payload and records its identity and capacity before the callback. |
| `ClosePending` | `close` | The state owns the first close code. Issuance enters `AwaitingExternal` before the callback. |
| `AwaitingExternal` | None | Only original delivery completion and external settlement can release resources. |
| `ExternalSettled` | None | The slot remains resident until the outstanding delivery returns. |

Logical removal clears the receive assembly and queued payloads.
It decrements the active count exactly once.
Repeated removal preserves the first close code.
External settlement suppresses a close callback that the hub has not issued.
Delivery failure requests code 1011 only if the client remains active or in its handshake.
Wrong-owner and stale receipts return their original payload without changing valid outstanding work.

`Client::select` inspects state without mutation.
The drive method commits the selected transition before the callback.
The intrusive list determines FIFO scheduling across recipients.
Its `Linked` variant owns both links, and `Idle` owns neither.
An outstanding delivery blocks only that recipient.
A close obligation does not wait for the delivery receipt.

The composite keeps independent readiness and deadline facts.
The ready queue contains at most one entry per live connection and one hub entry.
Each connector marks affected children ready without recursive entry.
A child callback causes another ready turn, even if the outer callback returns `None`.
Thus, a blocked child cannot hide a runnable sibling.
The hub and composite offer a cooperative yield after 64 turns.

The deadline table holds at most one entry per connection.
Entries contain the client generation, protocol phase, and original protocol deadline token.
The root timer reports time, not a protocol decision.
Expiration routes the original token back to its owner.
The dirty flag represents an unreported change to the earliest deadline, not a second timer state.

HTTP owns the connection until its upgrade handoff.
Only the WebSocket phase owns the outgoing delivery correlation.
If shutdown precedes handoff, HTTP cancels the upgrade and closes without activation.
The composite handles handoff and activation in one synchronous turn.
Shutdown after that turn belongs to the WebSocket phase.
Protocol closure requires every delivery receipt before connection retirement.
Retirement removes stale readiness and deadlines before its callback.

The root lifecycle is `Running`, `Draining`, or `Aborting`.
Each timed state owns its deadline.
The transport state owns either an open descriptor or the fact that close owns it.
Close requires both original I/O worker slots to be empty.
Retirement requires descriptor transfer and settlement of both workers.
This example owns no file operations, so there is no separate file-settlement condition.

The root uses the existing native preallocated channel.
At most `2 * max_clients + 2` workers can publish one completion each.
The driver checks this bound before every send.
A worker slot cannot issue another operation until the root consumes its completion and joins its task.
The channel preserves FIFO completion order and owns wakeups and closed-channel errors.

Each resident client owns two original-operation slots, one for each I/O direction.
Transport close occupies the read slot only after both slots settle.
One additional slot owns accept, and another owns the root timer.
Cancellation does not free a slot or create another completion.
Admission and occupied worker slots provide backpressure before channel publication.

The root retains one received event across its next scheduling turn.
That event consumes the original worker slot, not another worker credit.
The queue, pending event, and root completion therefore share the same bound.
The root cannot replace an accept, timer, or I/O worker while its event remains in any of these locations.
The root retains its sender and receiver until all clients, accept, and timer settle.

The origin, observations, and timer deadlines all use `kimojio::clock_now`.
Virtual time and real time therefore use the same domain as runtime sleep.
Cancellation still waits for the original native operation, not its cancellation acknowledgement.

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

The runtime accounting API uses actual queue capacity and the private channel state size.
Its `Rc` header and padding model follows the current standard-library representation, not a stable allocation-size API.
The root also reserves its mailbox wrapper, receiver handle, receive future, and one pending event.
The reservation does not depend on a duplicate of the private channel layout.
Sender handles reside in the charged mailbox wrapper and worker futures.
The hub accounts for separately owned message allocations.

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
It excludes allocator bookkeeping and rounding, stacks, executable pages, kernel socket storage, and runtime allocations outside declared reservations.
The executor must declare its per-client reservations before admission.
Normal allocator exhaustion can still terminate the process.

The native executor reserves two original-operation future slots per client.
Its reservation includes cancellation tokens, descriptor ownership, protocol states, and the 16 KiB receive buffer.
HTTP metadata reserves a conservative sixteen times the 8 KiB handshake-head limit, including vector growth.
This reservation persists until final connection retirement.
Shared message capacities still have exact, separate charges.
The native task scheduler and ring bookkeeping remain outside this application-storage cap.
This exclusion includes runtime wait registrations, both for the former `AsyncEvent` mailbox and for the native channel.

## Tests

Run the application, composition, cancellation, and actual-wire tests:

```sh
cargo test -p websocket-chat
```

The tests cover publication order, sender inclusion, complete-message checks, admission, memory limits, and empty-message allocations.
They also cover slow recipients, stale identities, logical closure, late completions, and both resource-settlement orders.

The hub model explores all 720 permutations of six actions for two clients and two publications.
The actions are removal, external settlement, two original delivery receipts, and two drives.
Both receipt outcomes and both callback modes produce 2,880 runs.
Temporal assertions specify exact delivery orders and close codes independently of the production selector.
Each run checks ready-list links, ownership counts, resource charges, slot generations, and complete settlement.

The composite model explores all six orders of abort, read completion, and write completion.
Two write outcomes and two callback modes produce 24 runs.
It checks exact traces, unique cancellation, delivery return before retirement, and transport close after both originals settle.
Four additional runs cover shutdown at the completed HTTP handshake.
These bounded models do not prove arbitrary schedules or parser correctness.

The native mailbox tests cover capacities for one through three clients, FIFO delivery, wakeups, closure, and overflow rejection before growth.
The `virtual-clock` feature adds timer-domain and complete root-shutdown tests.

Run the virtual-time tests:

```sh
cargo test -p websocket-chat --all-features
```

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
