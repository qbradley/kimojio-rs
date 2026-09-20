# HTTP/2 engine

## Extraction checkpoint

The private protocol components come from `ae1c3402b12a338d321246596aed33025fbc3b91`.
They include HPACK, compact fields, frames, flow arithmetic, and separate client and server stream halves.
They do not include the mixed HTTP driver or an HTTP/1 parser.
The only external dependency is `rustc-hash`.

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

## Unqualified gates

Stage two adds classic CONNECT, disabled-push transitions, owned operations, and an in-memory example.
Its focused tests include frame-credit sequences and bounded completion-order cases.
Release qualification, wider resource models, and the final support audit remain pending.
Independent peers and performance qualification remain separate acceptance work.

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

`BodyOp::bytes()` exposes one retained fragment.
`BodyOp::release()` consumes the fragment and creates its release receipt.
The engine does not refund application DATA credit before that receipt arrives.
Padding and discarded DATA have separate immediate credit paths.
Release consumes a whole fragment, not the whole connection buffer.

Every operation carries an unforgeable machine identity and a nonwrapping sequence.
Cancellation acknowledgment does not settle the original operation.
No callback has a silent default implementation.
`reschedule` means that the drive budget ended with runnable work.

## Source demand and retained capacity

`send_ready` transfers one single-use permit for one stream.
The permit reserves bounded application storage, not current wire credit.
The engine schedules the admitted buffer against both wire windows.
Applications do not poll `send` or calculate window balances.
One admitted buffer can span several DATA frames.

The permit states separate maximum byte length and retained capacity.
`Vec<u8>` reports its allocation capacity.
`BodyOp` reports the complete page capacity, even for a one-byte range.
Custom buffers must report all backing allocations that their ownership retains.
The engine returns both the permit and the buffer after an admission error.

Reset, excluded GOAWAY streams, and connection failure revoke outstanding permits.
`send_stopped` reports source revocation independently of outstanding write settlement.
`sent` returns the original buffer after its last transport obligation settles.
A successful complete write remains successful after a concurrent stream reset.
Its stream outcome can still report that reset.

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
Each subsequent drive transition checks one recorded sender.
SETTINGS window changes preflight all affected windows before commitment.

## Alarms and settlement

`WakeOp` is a deadline alarm, not an I/O-readiness request.
The core maintains separate SETTINGS, stream, and shutdown deadlines.
Each alarm has a distinct operation token.
An earlier replacement deadline causes cancellation of the old alarm.
The caller must complete both the cancellation request and the original alarm.

An invalidated alarm completion releases its token without advancing protocol time.
It cannot trigger a replacement deadline.
Transport close waits for outstanding read, write, cancellation, and alarm operations.
Application body receipts can remain outstanding after transport close.
Stream retirement still waits for those receipts.

The role wrappers expose shared commands through `Deref<Target = Connection<B>>`.
The shared type has no public constructor and no role-specific request or response command.
This arrangement avoids duplicate forwarding code without a universal operation interface.

## Executable API example

`examples/memory.rs` drives both roles with seven-byte I/O completions.
`examples/support/mod.rs` supplies explicit callback and completion wiring.
`examples/interop.rs` belongs to the parent integration work and is not part of this implementation.
`src/http/**` and `tests/http_composition.rs` also belong to the parent.
The pure engine does not create or drive an HTTP/1 parser.
