# HTTP/2 composition and acceptance contracts

This document applies the [FSM composition guidance](fsm-composition.md) to HTTP/2.
It records the approved scope and the implementation gates.
It is not a claim that an implementation passes those gates.
The [qualification ledger](http2-qualification.md) records source-specific evidence and the remaining gates.

## Approved scope

The protocol target is RFC 9113, with RFC 7541 header compression.
Both roles support streaming bodies, informational responses, request trailers, response trailers, and classic CONNECT.
The core owns flow control, cancellation, error scope, stream retirement, GOAWAY, and graceful shutdown.

Server push is disabled.
The client must still process legal in-flight push before acknowledgment of the setting that disables push.
It must preserve compression history when it rejects promised streams.
After acknowledgment, PUSH_PROMISE is a connection error.

Extended CONNECT, RFC 9218 scheduling, and legacy h2c Upgrade are outside this change.
Priority frames still require the wire behavior that RFC 9113 specifies.
TLS, ALPN, DNS, and socket creation remain outside the protocol engine.

The user approved an HTTP/2 engine and a reusable HTTP/1 plus HTTP/2 composite.
The direct HTTP/2 path must not instantiate or drive an HTTP/1 parser.

## Reuse decision

HTTP/2 and HTTP/1 are sibling protocols.
HTTP/2 does not extend the HTTP/1 framing or single-exchange ownership model.
An HTTP/1 receive lease can stop connection input.
The same policy cannot serve as the default ownership model for multiplexed HTTP/2 streams.

The composite reuses the complete `kimojio-fsm-http1` machine for HTTP/1 connections.
It selects a protocol once, then delegates protocol policy to that child.
The composite must not duplicate stream state, receive credit, transport cursors, or shutdown decisions.

Clients select a protocol explicitly, including the result of external ALPN negotiation.
Servers can select a protocol explicitly or detect the HTTP/2 prior-knowledge preface.
Detection must preserve every input byte and handle fragmented prefixes.
It must never pass an HTTP/2 preface to the HTTP/1 parser.

Existing HTTP/1 operation constructors are sealed and their write storage contains HTTP/1 framing.
The HTTP/2 engine must not expose those internals or force binary frames through that storage.
Shared semantic primitives are reusable where their meanings agree.
Protocol-specific identities and storage remain distinct where their meanings differ.

## Historical sources

| Source | Use | Important limits |
|---|---|---|
| `ae1c3402b12a338d321246596aed33025fbc3b91` | Optimized HPACK, compact fields, frame codecs, flow arithmetic, and regression tests | Internal clock reads, complete-frame input, and legacy orchestration require changes |
| `d3b7d2ee857e5f398dfa3b5242ee55c951074fcf` | Callback semantics and lifecycle regressions | Client DATA and discarded DATA do not replenish receive credit |
| `25bd2f42f9e384f76a3dfad528eb394c5f14b1d5` | Receive-credit fixes and two regression cases | Immediate refunds do not establish consumer-controlled credit or bounded retention |

The optimized source and the newer callback source are different descendant lines.
Neither source includes all improvements from the other line.
The old combined driver is not an implementation template for the new coordinator.

The optimized compact events omit `flow_control_len`.
They also collapse discarded DATA into generic progress.
Those event definitions cannot cross the new ownership boundary unchanged.

The historical request validator requires `:scheme` and `:path` for every request.
Classic CONNECT needs different pseudoheader rules and tunnel semantics.
The historical client also rejects every PUSH_PROMISE, without the required acknowledgment distinction.

## Core interface and state

The engine remains synchronous and sans-I/O.
It does not read a clock, make a system call, spawn a task, or use an async runtime.
The caller supplies monotonic time.
Semantic callback ports use `next(&mut ports) -> Option<P::Output>`.

Each callback commits an issuance or notification before it calls the port.
`None` means accepted and continue.
It never means operation completion, rejection, or permission to discard sibling work.
A cooperative yield is distinct from an operation completion.

Connection lifecycle, receive framing, transmit settlement, compression, timers, and stream state are separate components.
Exclusive alternatives use enums.
Independent obligations remain independent.
Selection is pure and scoped to the active lifecycle.

Each stream has one authoritative protocol record.
Its receive half, transmit half, and outstanding ownership obligations are separate.
Protocol END_STREAM, producer completion, body release, write settlement, and application retirement are distinct events.
Retirement requires the documented join of those obligations.

Normal drive turns use recorded readiness rather than a scan of every stream.
Connection-wide changes, such as SETTINGS window adjustments, can visit every affected stream.
Such visits require explicit preflight and commit rules.
The scheduler must preserve control work and sibling progress during a long upload.
Persistent receive-credit work must not defer all eligible response DATA until uploads finish.
Directional overlap is a separate requirement from eventual completion of both directions.

### Metadata command admission

Synchronous command success is the only acceptance boundary for requests, responses, and trailers.
The caller owns metadata that a command does not accept.
It must not replay an accepted command while its transport write remains outstanding.

`CommandError::Blocked` identifies temporary admission pressure.
Peer concurrency, local stream slots, queued control items, and available control bytes can cause this pressure.
A command that exceeds an immutable bound is not a retryable capacity failure.
Exhausted stream IDs and closed admission are terminal for that connection.

`Ports::admission_changed` is a coalesced notification for blocked metadata commands.
It reports a change, not permission or guaranteed capacity for specific fields.
The next command attempt remains authoritative.
The core reports relevant capacity changes and invalidation without requiring adapter inspection of SETTINGS or protocol state.
An unchanged blocked state must not produce repeated notifications.
The notification does not introduce a request permit, an internal metadata queue, or a second drive mechanism.

## Storage, credit, and transport

The core must support independent read and write progress.
A blocked write must not prevent receipt and processing of an early response.
Stream cancellation must not cancel shared connection input or an unrelated stream's write.

Original operations own their storage until their original completions settle them.
Cancellation acknowledgment does not return storage.
Partial writes retain an exact cursor across frame headers and payload slices.
Unknown write progress must remain distinct from exact progress.

Outbound DATA uses scatter-gather storage where the transport supports it.
The frame header and the caller-owned payload need not share an allocation.
Committed header blocks remain in compression order across partial writes and cancellation.
Local validation failure must not corrupt encoder history.

Receive storage must permit a retained body fragment and further connection input at the same time.
A single exclusive connection-buffer lease is insufficient.
The implementation must define page ownership, fragment ownership, and the condition for buffer reuse.
The implementation choice must preserve bounded retention without a mandatory payload copy for every DATA frame.

Resource accounting includes retained capacity, not only visible payload bytes.
It also includes fragment descriptors, queued metadata, outstanding operations, and compression state.
Empty DATA must not create an unbounded queue that bypasses byte limits.
Per-stream pressure preserves sibling progress until an explicit aggregate bound is exhausted.

Wire flow control counts all DATA payload bytes, including padding.
Application body length excludes padding.
Discarded DATA after reset consumes and returns connection credit without reopening a stream.
END_STREAM must not strand connection credit needed by later streams.

Credit follows documented consumption or an explicit bounded-storage reservation.
Receipt of bytes is not, by itself, application consumption.
Advertised windows and storage reservations must agree.
Body-size limits must not silently determine receive-window policy.

## Error and shutdown contracts

The core decides whether an error is stream-scoped or connection-scoped.
The wrapper must not infer scope from incidental operation failures.
Terminal protocol errors retain precedence over later timer or cleanup failures.

SETTINGS acknowledgment obligations and graceful-shutdown obligations have separate state and deadlines.
GOAWAY must identify which requests are known to be unprocessed.
Stream IDs never wrap or silently reuse a live identity.

Graceful shutdown preserves already committed output and outstanding storage obligations.
The final transport close occurs after original operations settle.
A wrapper must perform the actual close rather than report success and rely on descriptor drop.

Hard abort is a separate command from graceful shutdown.
It preserves the original-operation settlement rules without a graceful-shutdown wait.
An alarm completion distinguishes a fired deadline from a timer failure.
The driver must not invent input failures or advance time to report another operation's failure.
An obsolete alarm failure settles its original obligation without a new timeout or a replacement primary failure.

## Implementation sequence

1. Refactor the static-server and chat coordinators in separate changes.
2. Extract and qualify the optimized HTTP/2 codecs and low-level protocol components.
3. Implement the owned-operation engine and its receive-credit lifecycle.
4. Implement the HTTP/1 plus HTTP/2 composite.
5. Qualify the core with independent peers, bounded models, and performance measurements.
6. Implement conventional Kimojio client and server wrappers in separate changes.
7. Repeat the qualification at the wrapper boundary and review the complete result.

Implementation friction can change this design.
Such a change must identify the failed assumption and preserve the approved behavior.
Historical test success is not a substitute for these gates.

## Acceptance matrix

| Area | Required evidence |
|---|---|
| Compression | RFC vectors, Huffman edge cases, table eviction, resource limits, and synchronization after refused or oversized sections |
| Framing | Fragmented preface and frames, CONTINUATION ordering, malformed control frames, unknown extensions, and exact error scope |
| Message semantics | Informational/final headers, trailers, no-body responses, content lengths, classic CONNECT, and disabled-push transitions |
| Credit | Compliant peers exceed both actual advertised windows, including padding, reset DATA, and aggregate small streams |
| Multiplexing | Paused consumers, independent sibling progress, upload fairness, early responses, and bounded aggregate exhaustion |
| Ownership | Late original completions, partial writes, cancellation orders, generation-safe tokens, and exactly-once releases |
| Lifecycle | ID exhaustion, reset isolation, GOAWAY retry classification, graceful shutdown, and actual close |
| Composition | Fragmented detection, byte preservation, direct-child behavior equivalence, and selective callback suspension |
| Runtime wrapper | Virtual time, capacity-release wakeups, dropped queued requests, empty-frame bounds, and independent read/write progress |
| Performance | Frozen equivalent builds, repeated connections, concurrent streams, small messages, large bodies, allocation counts, and profiles |

Independent peers must parse SETTINGS and track both stream and connection credit.
They must stop sending when either credit balance is exhausted.
Tests must exceed the actual advertised windows, not an assumed 65,535-byte default.
Sequential small streams and concurrent small streams must also exceed the connection window in aggregate.

Tests assert complete observable sequences and the absence of forbidden frames.
Bounded models derive expected behavior from contracts rather than copy the production selector.
Peer prerequisites must fail explicitly rather than turn a skipped test into apparent interoperability evidence.

Performance comparisons use separate build directories and the same workload semantics.
They separate codec cost, connection-engine cost, composition cost, and runtime-wrapper cost.
Measurements must include fragmented input and retained consumers, not only immediate in-memory consumption.
No complete or performance-qualified implementation is claimed until those results exist.
