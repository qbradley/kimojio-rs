# Kimojio HTTP/2 wrapper requirements

This document defines the wrapper boundary, not a completed implementation.
The core correctness and performance gates in [the composition design](http2-composition.md) precede implementation.
Those core gates are complete for the snapshots in the [qualification ledger](http2-qualification.md).
The shared runtime foundation, native transports, and generic transports are implemented for both roles.
Complete wrapper qualification and performance measurements remain in progress.

## Connection-level API

The wrapper accepts an established transport.
Socket creation, DNS, TLS, ALPN, pooling, and retries remain separate services.
The initial public metadata types use the existing `http` crate.

A client handle submits requests to one connection driver.
The caller keeps that driver polled.
A server connection invokes a handler for each admitted request.
Both roles expose streaming bodies and trailers.

The default native path accepts an owned descriptor.
A generic stream path has a separate transport implementation.
The generic path must not reduce an unknown partial write to a false exact receipt.
The driver also needs explicit hard-abort and failed-alarm inputs.
It must not fabricate a read failure or a future timestamp to report another integration failure.

The wrapper uses the direct HTTP/2 engine.
The protocol composite remains available to pure FSM applications and a future protocol-neutral facade.
The HTTP/2 wrapper must not instantiate an HTTP/1 parser for each connection or request.

## Driving and I/O

One connection driver owns the protocol engine.
It uses semantic ports and typed completions.
It does not duplicate frame parsing, stream state, window arithmetic, or shutdown policy.

Native read and write slots remain independent.
Each slot retains its original future until completion.
The implementation starts from the reusable-slot pattern in `kimojio-http1`, including its cancellation and late-success behavior.
Native write futures create borrowed slices only after they occupy their pinned slot.
Those borrows remain valid until the original operation settles, including cancellation and late success.
This requirement includes inline frame headers and inline application buffers, not only heap-backed payloads.

Transport callbacks can install operations directly into those slots.
They do not require a large intermediate event value.
Application commands that need mutable access to the engine run after its callback returns.
Ports must not reenter the engine.

A blocked write must not stop response processing.
A runnable turn must not repeatedly allocate a wait registration for an empty channel.
A blocked turn must register every wake source that can make work ready.
Bounded cooperative turns preserve command, cancellation, timer, and sibling progress.

## Admission and producers

The wrapper retains metadata until its synchronous command succeeds.
Only `CommandError::Blocked` permits a queued retry.
The driver retries after the coalesced `admission_changed` notification, not after a guessed peer limit or a body permit.
The notification is not a reservation, so another attempt can still return `Blocked`.
Permanent rejection and connection-terminal rejection complete the queued operation with an error.
An accepted command is never resubmitted.

The engine's `SendPermit` is the authoritative buffer-admission signal.
The wrapper must not reconstruct stream or connection wire credit.
It obeys both the visible-byte limit and the retained-capacity limit.
It must not parse SETTINGS frames to implement producer admission.
Any required handshake signal belongs to the engine interface.

Source revocation and original write settlement are separate.
Reset or GOAWAY can revoke a permit while a producer future is pending.
A rejected late submission returns its storage and must not abort an unrelated stream.

Empty and full bodies need no boxed stream solely to represent their data.
Custom producers can suspend.
Their wakeups must identify eligible work without an unconditional scan of every stream on every transport completion.

The producer strategy needs measurement.
Persistent native tasks and ready-set futures have different allocation and cancellation costs.
An optimization must preserve stream-local I/O scopes and the behavior of ordinary Kimojio combinators.

## Input bodies and retention

An incoming chunk owns a core body lease.
Its destructor returns the lease through an existing Kimojio channel.
The driver must wake when a release restores capacity, even when no new application command arrives.

Queues remain bounded by items and retained storage.
Visible payload length alone is not a memory bound.
An empty DATA frame must not create unbounded queued work.

Native channels replace private queue-plus-event mailboxes.
The channel choice must account for its receiver-drop and close semantics.
Closing a channel does not necessarily discard its already queued values.
The wrapper must release every queued body lease during stream teardown.

Forwarding retains the incoming lease through the original outgoing write receipt.
It does not release a page when a write is merely queued or cancellation is requested.
The returned receipt retains the exact accepted count and any failure of the unsent remainder.

Per-stream pressure must preserve siblings until the configured aggregate bound is exhausted.
Retained pages and externally held chunks must remain explicit resource obligations.
The close contract must distinguish descriptor settlement from the lifetime of read-only application data.
The core can close the transport while an application still holds a read-only body lease.
That lease remains valid, and release still precedes stream retirement.
The driver continues to process releases and pending retirements after transport closure.
It must not discard the release channel and strand retirement waiters.

## Completion and errors

Response headers, response END_STREAM, upload completion, and stream retirement are different milestones.
The request future can return a response before the upload finishes.
Receiving a complete response does not, by itself, require the core to cancel the upload.

RFC 9113 section 8.1 permits a server to send `RST_STREAM(NO_ERROR)` after a complete response to request upload termination.
The client must not discard that response because of this reset.
A wrapper policy that abandons an upload must issue an explicit stream-local command.
Such a policy must not become an implicit core framing rule.

A queued request that is dropped before admission must not reach the wire.
Cancelling an admitted request must not cancel a sibling's shared transport operations.
The wrapper preserves protocol, transport, deadline, application, and retryability distinctions.

Receive completion and full stream outcome remain observable separately.
A valid response followed by an upload failure must not become an unqualified full-stream success.
Conversely, a later reset must not erase an already complete valid response.

`IncomingBody::retirement()` returns the authoritative `StreamReport`, separate from the convenience completion result.
The report preserves the core outcome, receive outcome, failed-buffer receipt, and contextual error.
Optional `RequestObserver` callbacks expose admission and retirement even when a request fails before final response headers.
Ordinary requests do not require an observer queue.

Optional client callbacks expose received informational heads.
The server obtains an informational sender from the request body.
Its send result means metadata acceptance, not transport acknowledgment.
Informational metadata must precede final metadata without an extra queue for ordinary requests.

The driver uses `kimojio::clock_now` for its clock domain.
It must not mix that clock with wall-clock `Instant::now` when virtual time is active.
Final success follows actual transport close and original-operation settlement.

## Acceptance work

The wrapper needs native and generic transport coverage.
Both paths need repeated connections, concurrent streams, streaming bodies, trailers, early responses, and CONNECT.
Cancellation tests include dropped queued requests, late successful operations, and capacity-release wakeups.

Independent peers must exceed the actual advertised windows.
They must also exercise cumulative small streams, retained consumers, padding, reset DATA, and zero-length frames.
Core outcomes must remain visible in the fixture results rather than disappear behind `error: null`.

Performance evidence separates protocol cost from wrapper cost.
Repeated keep-alive workloads include small messages, large bodies, and concurrent streams.
Profiles use frozen binaries and distinguish client, server, and shared stacks.
Source changes require new measurements before any performance claim.

If measurements identify serial header and DATA writes as a bottleneck, batching belongs in the core's owned write operation.
The wrapper must not acknowledge queue insertion as successful transport acceptance to obtain another frame.
Any batching change retains exact partial progress, compression order, body ownership, and documented deadline boundaries.

## Implementation sequence

Each implementation phase has a separate change.
The core qualification gates precede the first phase.

1. Implement shared body ownership, metadata conversion, native I/O slots, and the native client in `kimojio-http2`.
2. Add concurrent server handlers over the shared driver and body types.
3. Add established generic streams without weakening exact native receipts or unknown generic progress.
4. Exercise cancellation, virtual time, admission pressure, late completions, retained chunks, and simultaneous upload/download at the wrapper boundary.
5. Run independent peers against the wrapper, not the existing synchronous core fixture.
6. Measure repeated native exchanges, concurrent streams, streaming bodies, allocation, and source-specific profiles before accepting optimizations.

The public connection functions retain the established HTTP/1 wrapper naming where the semantics agree.
They include `connect_native`, a caller-polled `NativeConnection::run`, and `serve_connection_native`.
HTTP/2 client handles support concurrent submissions rather than the HTTP/1 single-exchange restriction.
Ready empty/full bodies avoid producer tasks solely for immediate data.
Custom producers and handlers must preserve Kimojio I/O-scope and cancellation behavior.

Common runtime mechanics have one implementation owner.
Client and server changes use a fixed shared-interface checkpoint before parallel edits.
Protocol fixes remain in the core change rather than duplicated adapter workarounds.
The wrapper report distinguishes completed phases from unsupported or unmeasured behavior.
