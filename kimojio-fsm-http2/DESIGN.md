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

The public engine will expose direct `Client` and `Server` types.
Each type will use `next(&mut ports) -> Option<P::Output>`.
Ports will receive owned read, write, body, and close operations.
Read and write operations will have separate outstanding slots.
Completions will return the original operation and its storage.
Wrong-owner completions will return an error with the original completion.

Receive pages will use bounded reusable `Rc` storage.
Body fragments will retain a page range and an explicit release receipt.
Capacity accounting will include full retained pages, not only body ranges.
Read operations will fill exclusive pages without a mandatory DATA payload copy.
Fragmented frames can require bounded assembly.

Outbound DATA will retain the original application buffer.
Each write will expose a frame header and a payload slice.
Exact partial progress will advance a cursor across both slices.
Unknown progress will terminate the transport without replay.

Protocol state, application leases, and transport settlement will remain separate.
Normal selection will use recorded readiness.
Connection-wide SETTINGS work can visit all affected streams.

## Unqualified gates

The extraction does not yet fix classic CONNECT or disabled-push transitions.
The owned-operation engine, bounded ownership models, and direct examples remain pending.
Independent peers and performance qualification belong to later acceptance work.

## Public API checkpoint

These signatures define the next implementation stage.
They are not present in the extraction commit.
`B: AsRef<[u8]>` is the application-owned send buffer type.
The default buffer type is `Vec<u8>`.
The core does not require that applications allocate a `Vec` for each body chunk.

```rust,ignore
pub struct Client<B = Vec<u8>> { /* private */ }
pub struct Server<B = Vec<u8>> { /* private */ }

pub trait Ports<B: AsRef<[u8]>> {
    type Output;
    fn read(&mut self, op: ReadOp) -> Option<Self::Output>;
    fn write(&mut self, op: WriteOp<B>) -> Option<Self::Output>;
    fn headers(&mut self, head: Head<'_>) -> Option<Self::Output>;
    fn body(&mut self, op: BodyOp) -> Option<Self::Output>;
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
fn new(config: Config, now: Duration) -> Result<Self, ConfigError>;
fn next<P: Ports<B>>(&mut self, ports: &mut P) -> Option<P::Output>;
fn advance_time(&mut self, now: Duration) -> Result<(), TimeError>;
fn send(&mut self, stream: StreamId, buffer: B, end: bool)
    -> Result<(), Rejected<B>>;
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

// Client:
fn request(&mut self, fields: &[H2HeaderField], end: bool)
    -> Result<StreamId, CommandError>;

// Server:
fn respond(&mut self, stream: StreamId, fields: &[H2HeaderField], end: bool)
    -> Result<(), CommandError>;
```

Request and response commands accept complete pseudoheader sections.
This avoids a separate CONNECT constructor and permits sensitive regular fields.
Header callbacks borrow a compact, validated field section for the callback duration.
Applications can copy selected metadata if they need to retain it.
Body callbacks transfer an owned page range instead.

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
