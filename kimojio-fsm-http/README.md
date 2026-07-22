# Kimojio FSM HTTP

`kimojio-fsm-http` provides I/O-independent HTTP/1.1 and HTTP/2 protocol state
machines. It owns protocol parsing, serialization, validation, flow control,
HPACK state, stream lifecycle, bounded resource policy, and content-free
diagnostics. It does not own sockets, TLS, timers, tasks, or a runtime.

The crate is a low-level support crate intended for same-version runtime
adapters. Applications should use a runtime adapter, such as
`kimojio::http`, instead of depending on this crate directly.

## Responsibilities

The crate provides:

- incremental HTTP/1.1 request and response decoding, including fixed,
  chunked, EOF-delimited, informational, CONNECT, and protocol-switch
  boundaries;
- HTTP/2 frame parsing and encoding, SETTINGS synchronization, stream state,
  DATA flow control, trailers, PING, RST_STREAM, GOAWAY, limits, and
  deterministic terminal errors;
- connection-owned HPACK encoders and decoders with byte-preserving,
  duplicate-preserving, sensitivity-aware header fields;
- bounded receive windows, send-capacity calculations, fair stream
  scheduling, backpressure state, shutdown intent, timer intent, and
  diagnostics;
- borrowed frame and DATA event paths for adapters that must avoid
  payload-sized copies;
- protocol-neutral `ServerConnection` and `ClientConnection` drivers that
  collapse HTTP/1 and HTTP/2 into callback-scoped read/write/event progress.

Runtime adapters remain responsible for:

- reading and writing the transport;
- TLS and ALPN;
- polling and wake ownership;
- monotonic timers and timeout policy;
- queueing outbound bytes in wire order;
- application request/response dispatch;
- connection pooling, retries, DNS, routing, and reconnect policy.

## Runtime-independent progress model

Callers repeatedly:

1. read bytes from their transport;
2. pass those bytes to the appropriate state machine;
3. handle the returned event and consumed-byte count;
4. queue returned control/output bytes without reordering them;
5. acknowledge outbound header transactions only after the transport queue
   accepts the complete transaction.

The FSM never performs I/O or spawns work. One adapter must own connection
progress and preserve the lifetime of borrowed input until the returned event
has been consumed.

## Connection driver entry points

`ServerConnection` detects the HTTP/2 client preface (including partial
prefaces) and otherwise selects HTTP/1. `ClientConnection` is created with an
explicit `HttpProtocol`. Both expose `Step::NeedInput`, `Step::Write`,
`Step::Event`, and `Step::Done` through a higher-ranked callback, so borrowed
headers and body chunks cannot escape into an async transport operation.

After handling a step, remove `connection.consumed()` bytes from the caller's
input and acknowledge them with `connection.consume(amount)`. For writes,
complete the transport operation using `pending_write()` before acknowledging
the step. Outbound bodies use `prepare_body_chunk`, `body_chunk`, and
`commit_body_chunk`; the framing header and caller-owned payload can be passed
to vectored I/O without copying the payload.

`ClientConnection` returns an `ExchangeId` for each prepared request. Response
events and request-body operations use this identifier. HTTP/2 supports
concurrent exchanges and schedules their request bodies fairly. HTTP/1 permits
one active exchange until the caller retires it.

For pooled HTTP/2 connections, `ClientConnection::process_idle_input` accepts
connection-level `SETTINGS`, `PING`, and `WINDOW_UPDATE` frames between
exchanges. Adapters must write its returned acknowledgement bytes before reuse
and discard the connection for every other verdict or error.

The protocol-specific APIs below remain available for specialized adapters.

## HTTP/1.1 entry points

`Http1ConnectionDecoder` is the role-neutral incremental decoder used by newer
adapters:

```rust
use kimojio_fsm_http::{
    Http1ConnectionDecoder, Http1ConnectionEvent, Http1HeaderScratch,
};

let mut decoder = Http1ConnectionDecoder::request(4 * 1024 * 1024);
let mut scratch = Http1HeaderScratch::new(32);
let input = b"GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n";

let consumed = scratch.with_input(input, |input, headers| {
    match decoder
        .next_event(input, headers)
        .expect("complete request head")
    {
        Http1ConnectionEvent::Head { head, consumed, .. } => {
            // Process or copy borrowed fields before this callback returns.
            let _ = head;
            consumed
        }
        event => {
            // Continue feeding input for body, trailers, switch, or completion.
            panic!("unexpected event: {event:?}");
        }
    }
});
let _ = consumed; // Compact input only after the callback returns.
```

Use `Http1ConnectionDecoder::response(method, max_body_bytes)` for responses.
Call `finish_eof` only after transport EOF when decoding an EOF-delimited
message. `HttpClient` and `Http1Server` (`Http1Codec`) remain available as
lower-level compatibility APIs.

## HTTP/2 entry points

`H2Server` and `H2Client` own connection protocol state. Their borrowed event
methods avoid copying DATA payloads:

```rust
use kimojio_fsm_http::{H2Server, H2StreamEventRef};

let mut server = H2Server::default();
let input = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
let (event, consumed, control_output) =
    server.accept_event_ref(input).expect("valid HTTP/2 preface");

if let Some(H2StreamEventRef::Data { stream_id, payload, .. }) = event {
    // `payload` borrows `input`.
    let _ = (stream_id, payload);
}
let _ = (consumed, control_output);
```

`H2FrameRef` is the lower-level borrowed frame parser.
`H2ByteStreamEventRef` and `H2ByteClientEventRef` preserve raw field bytes and
DATA. Owned event APIs remain compatibility fallbacks.

Configure protocol and memory bounds with `H2Limits`. Flow-control and
operability surfaces include `H2ReceiveWindow`, `H2SendCapacity`,
`H2FairStreamScheduler`, `H2BackpressureState`, `H2FlowDiagnosticsSnapshot`,
`H2ControlDiagnostics`, `H2TimerIntent`, and `H2ShutdownIntent`.

Outbound DATA uses `H2Client::prepare_data_frame` or
`H2Server::prepare_data_frame`. Each plan is bounded by the peer's connection
window, stream window, and maximum frame size. Adapters can write the plan's
header and a borrowed payload prefix with vectored I/O, then call
`commit_data_frame` after successful handoff. Incoming SETTINGS and
WINDOW_UPDATE frames update that connection-owned capacity; adapters must keep
feeding control frames while a plan is unavailable.

## HPACK ownership and outbound handoff

One physical HTTP/2 connection has one inbound decoder and one outbound
encoder per endpoint. Reuse those histories in wire order. Do not create a
fresh codec per header block.

`H2HeaderField` is the canonical owned header occurrence. It preserves raw
bytes, duplicates, order, and a per-occurrence sensitivity bit. Sensitive
fields are encoded as never indexed and are not inserted into the dynamic
table.

Header-producing helpers return an `H2OutboundCommit`. Before handing bytes to
the transport, compare that receipt with the FIFO front:

```rust
use kimojio_fsm_http::{H2OutboundCommit, H2Server, ServerError};

fn handoff(
    server: &mut H2Server,
    commit: H2OutboundCommit,
    assign: impl FnOnce(&[u8]) -> Result<(), ServerError>,
) -> Result<(), ServerError> {
    let block = server
        .next_outbound_block()
        .ok_or(ServerError::InvalidFrame)?;
    if block.commit() != commit {
        return Err(ServerError::InvalidFrame);
    }
    assign(block.bytes())?;
    server.acknowledge_outbound_block(commit)
}
```

An assignment failure must not acknowledge the block. After bytes have been
accepted by an ordered transport queue, failure is connection-terminal because
peer-visible HPACK history cannot be rolled back or reordered.

Adapters that deliberately own the HPACK pair use
`for_external_hpack_adapter` and feed canonical fields through
`accept_external_header_fields`; this prevents two codec histories from
claiming the same connection direction.

## Runtime adapters

Runtime adapters own transport I/O, timers, task progress, queueing, and
application dispatch. The optional `kimojio::http` module is the public
Kimojio adapter. It drives `ServerConnection` and `ClientConnection`, preserves
ordered HPACK handoff, and keeps this crate private.

## Features

The default `hpack-test-support` feature exposes deterministic codec
instrumentation and allocation-failure controls used by the crate's HPACK
validation suite. Runtime consumers that do not need those test hooks should
use:

```toml
kimojio-fsm-http = { workspace = true, default-features = false }
```

The production HPACK codec itself is always available. The third-party
`hpack` crate is a development-only secondary oracle.

## Limitations

The current scope intentionally excludes or defers:

- server push and `PUSH_PROMISE`;
- RFC 9218 `PRIORITY_UPDATE` scheduling;
- extended CONNECT and `:protocol`;
- h2c Upgrade handling;
- adapter policy for stream-ID exhaustion, max connection age, reconnect
  jitter, and two-stage graceful shutdown;
- configurable deployment-specific PING/RST/WINDOW_UPDATE abuse policy;
- automatic early-response cancellation of an unused request body;
- sockets, TLS, DNS, retries, pooling, routing, and application protocols.

See [`TODO.md`](TODO.md) for the remaining implementation and release work.

## Testing

From the workspace root:

```sh
cargo test -p kimojio-fsm-http
cargo test -p kimojio-fsm-http --no-default-features
```

The test suite covers HPACK RFC vectors and state synchronization, frame and
message validation, flow control, stream lifecycle, resource bounds,
diagnostics, borrowed-event allocation behavior, mutation controls, and
compatibility closure.
