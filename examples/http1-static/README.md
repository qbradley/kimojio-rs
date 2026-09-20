# Static files through composed HTTP/1 state machines

This Linux example composes a static-file application with `kimojio-fsm-http1`.
The outer executor uses `rustix-uring` directly.
It does not use Kimojio, Tokio, async tasks, or blocking socket I/O.

## Run

Run the example from the repository root:

```sh
cargo run -p http1-static -- --bind 127.0.0.1:0 --root ./public
```

The document root must exist.
The server prints its bound address after initialization:

```text
LISTEN 127.0.0.1:PORT
```

Use `--help` to display the configuration flags.
Use `--stop-after 10` to stop after ten completed exchanges.

The default connection limit is 128.
The maximum connection limit is 256.
The default timeout is ten seconds.
`--timeout-ms` sets the HTTP head, body, and idle deadlines.
`--stop-after` also counts failed exchanges that reached an exchange identity.

## Three separate responsibilities

| Module | Responsibility |
| --- | --- |
| `app.rs` | Pure file-service state, response decisions, file offsets, and body ownership |
| `composite.rs` | HTTP/application connections and a local ready set |
| `driver.rs` | Socket setup, root clock, ready connections, deadlines, and completion routing |
| `file.rs` | Four file operations: open, metadata, positioned read, and close |
| `ring.rs` | Stable kernel-operation storage, SQ/CQ access, cancellation, and settlement |

The application and HTTP machines do not make system calls or read clocks.
Their callbacks return `Option<Output>` with a caller-selected output type.
`None` accepts an operation without suspension.
It does not complete an operation.

The composite resolves HTTP request and body callbacks through the application.
The application produces HTTP responses through a connector.
The HTTP machine selects the response wire version and queues final headers behind informational output.
The connector does not reconstruct versions, parse Expect, or retry final-response admission.
Unresolved network and file operations reach the root executor.
The connectors do not call the machine that currently owns the active drive call.

The composite retains ready bits for its two machines.
The root retains a ready queue for connections and an ordered deadline map.
Neither loop scans all connections on each turn.
Both loops use a turn budget.

## File and HTTP behavior

- `GET` sends file contents.
- `HEAD` sends the same `Content-Length` without file contents.
- `/` selects `index.html`.
- Missing files and directories produce `404`.
- Unsupported methods produce `405` with `Allow: GET, HEAD`.
- Responses use `application/octet-stream`.
- The HTTP machine owns keep-alive, pipelined input, framing, and partial writes.
- The service discards request bodies through bounded HTTP delivery credit.
- The service waits for the complete request body before its final response.
- The HTTP machine emits `100 Continue` when application credit accepts an expected request body.
- The HTTP machine emits bounded error responses before closure when no final response has started and the transport remains usable.

The application obtains metadata from the opened descriptor.
It reads at most one 16 KiB body chunk for each HTTP demand.
It does not read the next chunk until HTTP returns the current buffer.
Zero HTTP body capacity causes a source failure, not zero-length file I/O.
An HTTP timeout can revoke body admission during a file read.
The connector returns a rejected body buffer to the application before file settlement.
It does not assume that an earlier body demand reserves admission forever.
The HTTP `source_finished` callback stops further file production.
The application can then close the file, but it still waits for outstanding HTTP body buffers and exchange settlement.
The HTTP receive allocation is 32 KiB per connection.
Operation slots, pending accept work, and deadlines have connection-derived limits.

The initial metadata fixes `Content-Length`.
File growth does not extend the response.
Early EOF or a read error after response headers causes a source failure and connection closure.
Concurrent file changes do not produce a snapshot of the original contents.

## Document-root policy

The application accepts origin-form targets and decodes the path once.
It rejects NUL, backslashes, control bytes, malformed escapes, and dot path components.
The query does not form part of the file path.

The executor opens paths relative to an already-open document-root descriptor.
It uses `OpenAt2` with `BENEATH | NO_SYMLINKS`.
It does not fall back to an unchecked path open.
Symlinks produce `403`, including symlinks whose targets remain inside the document root.
Only regular files receive a successful response.
Nonblocking open prevents FIFO opens from waiting indefinitely.

The document-root contents are administrator-controlled.
The policy does not reject hard links or mount points inside the root.
This example is not a filesystem sandbox.

## Ownership and cancellation

### Composition transition contracts

The application has three independent components:

| Component | Exclusive states and ownership |
| --- | --- |
| Lifecycle | Idle, producing an identified exchange, or draining that exchange |
| File | Planned path, original open, available descriptor key, original stat/read, original close, or absent |
| Payload | Local allocation, completed chunk, original read lease, or HTTP lease |

The file operation owns its identity until its original completion.
The pending read also owns its limit and file key.
The payload state records which component must return the allocation.
The offset advances only after a successful file read.
The remaining length decreases only by the accepted HTTP byte count.

Selection is pure and returns a unit-sized transition label.
Each transition commits ownership before its callback.
Producing permits open, stat, demand-driven read, and body delivery.
Draining permits failure notification, a pending response, file close, and settlement.
An exchange completion invalidates any pending response.
Draining never starts another open, stat, read, or body delivery.

| Transition | Prerequisite | Committed effect before callback |
| --- | --- | --- |
| Open | Producing with a planned path | Move the path to the port and retain the original identity |
| Stat | Producing with an opened file | Retain the file key and original identity |
| Read | Producing with metadata, local payload, and demand | Consume demand, retain the limit and identity, and transfer the payload |
| Body | Producing with a completed chunk | Replace the local chunk with an HTTP lease count |
| Response | Pending response for a live exchange | Consume the response obligation |
| Failure | Pending source failure | Consume the failure obligation |
| Close | Draining with an available file | Transfer the file key and retain only the close identity |
| Settle | Completed exchange, absent file, and local payload | Enter idle and notify the connector |

A source failure notification precedes a pending response or file close.
A response precedes file close for HEAD, empty files, and application errors.
Close waits for the original file operation, but not for an HTTP payload lease.
Settlement joins exchange completion, file closure, and payload return.
A successful late open requires exactly one close, without metadata or response production.
A late read returns its allocation without body delivery.
Close consumes ownership even on error. It never retries a possibly reused descriptor.

The composite separates serving, reuse waiting, termination, and closed lifecycles.
Reusable exchange completion blocks HTTP request admission until application settlement.
Termination does not block HTTP lease returns, cancellation, bounded error output, or socket close.
Closed permits only application settlement work.
The root retains each connection until both machines and their original operations settle.
Socket close requires the original network operations to return their descriptor references.
File close and socket close can complete in either order.

The composite first returns rejected payloads and incoming leases, then resolves pending credit.
A transport completion or deadline observation gives HTTP one priority turn.
This turn publishes terminal observations before competing application production.
Body rejection still returns its allocation when notification priority is absent.
Pending final responses wait for incoming-message completion.
Application work precedes ordinary HTTP work.
Each machine clears its ready obligation before its drive.
Connectors never reenter the machine that owns the current drive.

Each connector has at most one pending response, rejected payload, incoming lease, and credit obligation.
New entries assert that their slot is empty.
The request callback creates initial credit.
Incoming delivery creates a lease return and replacement credit.
Lease return precedes credit, so HTTP cannot reuse an allocation that the application still owns.
Exchange completion invalidates the pending response and incoming-message identity.
Termination discards unused credit instead of creating more producer demand.
The closed lifecycle cannot reopen through shutdown or a delayed source-failure notification.

Each selected transition consumes an obligation or advances a component.
Callbacks that return `None` continue selection, including settlement callbacks.
Only a selector with no eligible transition returns blocked.
The composite offers a cooperative yield after 64 transitions.
The root processes at most 256 ready connections before completions and deadlines.
These budgets do not release operations or imply completion.

The ring owns each operation in stable boxed storage.
Each operation retains its buffers, path, metadata storage, and descriptors until its original completion.
Completion identifiers do not reuse live generations.

A cancellation request has a separate completion.
Its completion never releases the original operation's storage.
Original operations and submitted cancellations have separate capacity limits.
The default cancellation limit equals the original-operation limit.
A cancellation slot remains occupied until its own completion, even if the original completes first.
Each original accepts at most one cancellation request.
When cancellation capacity is full, the request stays in the bounded original-operation slot.
It requires no additional pending queue.
If the original completes before submission, its deferred cancellation disappears.
Only submitted cancellations produce cancellation completions.
Successful late open or accept results still acquire descriptor ownership and require close.
Close operations are not cancelable.
This prevents cancellation from losing a descriptor before the close syscall.

During controlled shutdown, the driver cancels accept and completes active exchanges.
It closes connections, files, the listener, and the document root through the ring.
The ring destructor cancels and drains remaining cancelable operations.
Unexpected root errors retain RAII cleanup for descriptors.
An unrecoverable ring-settlement error aborts rather than release storage that the kernel can still access.

The example has no TLS, byte ranges, directory listings, or signal-driven graceful shutdown.
Process termination lets the operating system reclaim resources.
`--stop-after` exercises explicit completion and close settlement.

## Tests and environment evidence

Run the tests and lint commands:

```sh
cargo test -p http1-static
cargo clippy -p http1-static
cargo clippy -p http1-static --all-targets --all-features
```

The ring, file, and server tests require real io_uring support.
They fail if ring creation fails.
They do not substitute a simulated backend.
Test fixtures remain under the repository's `target` directory.
Test processes and fixtures have explicit cleanup.

The development probe used cached `rustix-uring 0.6.0` and `rustix 1.1.4`.
The environment reported `io_uring_disabled=0`.
Actual ring creation and NOP submission succeeded.
OpenAt, secure OpenAt2, descriptor Statx, positioned Read, and Close succeeded.
Loopback Socket, Connect, Accept, Send, and Recv succeeded.
AsyncCancel produced a successful cancellation completion and a separate original-receive `ECANCELED`.

Maintained tests repeat the file and cancellation checks.
The file tests open a real FIFO without a writer and reject it after metadata reports a nonregular file.
Cancellation tests cover both completion orders and real I/O with only one cancellation slot.
Server tests cover real HTTP traffic, slow clients, disconnects, pipelining, and explicit shutdown.
Deterministic composite tests cover terminal HTTP observations before successful late file completions.
The rejection test also disables notification priority to exercise buffer recovery directly.

`composition_model.rs` uses an independent resource ledger, without HTTP parser logic.
It runs 1,152 completion schedules and compares each schedule with 26 callback yield patterns.
The baseline always continues.
The patterns include always yield, alternating yields, and one yield at each of the first 24 callback positions.
All 31,104 runs use the same external inputs for each comparison.
They compare exact callback sequences, operation identities, output bytes, and file offsets.

The model starts with an outstanding open, stat, read, or body write.
It explores abort and timeout, both competing-completion orders, and failed, partial, or full original results.
It orders original completions, file close, socket close, and cancellation acknowledgements with all 24 priority permutations.
Timeout cases also complete the bounded HTTP error response.
The ledger rejects duplicate ownership, premature close, new producer work after termination, and repeated terminal notifications.
Every run must settle within 16 external completions.
Cancellation acknowledgements affect only the ledger's cancellation capacity.
Native ring tests separately establish the actual cancellation/original CQE contract.

Application tests cover all six orders of exchange completion, file-close completion, and HTTP payload return.
They include partial payload acceptance, close errors, and both callback continuation modes.
Literal sequence assertions require response-before-close and failure-before-close.
A buffered 256-chunk request checks cooperative yielding and retained ready work.
Repeated shutdown after closure leaves the service settled.

These are bounded checks, not a complete proof.
They assume that original operations and submitted cancellation requests eventually complete.
The refactor adds no production heap queues, dynamic dispatch, or payload copies.
No new throughput or latency claim follows from these structural changes.

Run the independent Python and Go server suite after the interoperability tools build the Go peer:

```sh
python3 -B interop/http1/server_suite.py \
  --profile strict --go-peer target/interop/http1-peer \
  -- "$PWD/target/debug/http1-static" --bind '{bind}' --root '{root}'
```

The strict suite also checks malformed-request responses from the HTTP core.

The composition refactor passed the default, release, and all-feature crate suites on CPUs 8–31.
Formatting and both default and all-target/all-feature Clippy checks passed.
The strict independent suite passed all 35 cases against debug and release binaries.
The native transport suite passed all 13 reset, timeout, and recovery cases.
Its sampled descriptor count returned from six to six.
These descriptor samples do not prove the absence of every possible leak.
No CPU2 timing or profiling run formed part of this refactor.
