# FSM HTTP Server

`kimojio-fsm-http` includes runtime-neutral HTTP server primitives for HTTP/1.1
and HTTP/2 protocol progress. The HTTP/2 layer owns shared frame validation,
header-block/HPACK state, stream lifecycle, control-plane policy, flow-control
primitives, resource limits, and diagnostics. `kimojio-fsm-static-file-server`
is a std TCP proof server that owns sockets and files while driving those FSMs.

## Usage

```sh
cargo run -p kimojio-fsm-static-file-server -- --addr 127.0.0.1:8080 --root ./public
curl http://127.0.0.1:8080/index.html
```

HTTP/1.1 GET and HEAD requests are supported by the proof server. HTTP/2
prior-knowledge clients can exercise the HTTP/2 path by sending the client
preface, SETTINGS, HEADERS, DATA, and control frames. The FSM HTTP/2 engine is
runtime-neutral: callers feed input and drain outbound frame bytes, while
adapters own sockets, TLS, timers, wakeups, and application delivery.

HTTPS is enabled by providing a PEM certificate chain and private key:

```sh
cargo run -p kimojio-fsm-static-file-server -- \
  --addr 127.0.0.1:8443 \
  --root ./public \
  --tls-cert ./localhost.crt \
  --tls-key ./localhost.key
```

The TLS path advertises `h2` and `http/1.1` with ALPN. If the client negotiates
`h2`, the proof server uses the HTTP/2 FSM; otherwise it uses HTTP/1.1.

HTTP/2 endpoints support explicit local flow-control advertisement through the
FSM HTTP/2 constructors used by gRPC. The implementation emits
`SETTINGS_INITIAL_WINDOW_SIZE` for local stream receive windows and a stream-0
WINDOW_UPDATE for local connection receive windows above the HTTP/2 default.
Outbound DATA remains governed by peer SETTINGS and WINDOW_UPDATE. Control-frame
budgets are interval-refilled so rapid floods are rejected without treating
spaced idle keepalive PINGs as lifetime budget exhaustion.

The HTTP/2 layer tolerates unknown extension frames, enforces negotiated frame
size and SETTINGS bounds, assembles HEADERS plus CONTINUATION under header-list
and frame-count limits, keeps HPACK state synchronized for discarded header
blocks, validates stream IDs, trailers, pseudo-headers, `content-length`, reset
tombstones, GOAWAY cutoffs, and flow-control overflow, and exposes typed
stream-vs-connection errors for newer adapters.

### Outbound header handoff migration

HTTP/2 frame-return helpers now return `H2OutboundCommit` (or a stream ID plus
that commit) instead of freely owned header bytes. Call
`next_outbound_block()` and verify `block.commit() == commit` before assigning
its complete bytes to one ordered transport queue. A mismatch means an older
FIFO block must be handed off first; do not assign or acknowledge the newer
receipt. After successful assignment, call
`acknowledge_outbound_block(commit)`. The
[checked outbound handoff](hpack-header-representation.md#checked-outbound-handoff)
is the canonical migration example, including mismatch and assignment-failure
handling. The affected helpers are
`H2Server::{response_frames,response_frames_with_headers,response_headers_frame,trailers_frame}`
and
`H2Client::{open_stream,trailers_frame}`. They require `&mut self` because one
outbound HPACK history belongs to the connection. Code that previously stored
or reordered returned `Vec<u8>` values must migrate to this commit/acknowledge
sequence; dropping or reversing connection-dependent blocks is no longer a
supported API.

### HPACK headers, limits, and diagnostics

`H2Server` and `H2Client` each own one inbound decoder and one outbound encoder.
Their dynamic tables, SETTINGS changes, terminal state, and diagnostics are
directional and advance in wire order. A layered connection that owns those
histories must use `for_external_hpack_adapter` and
`accept_external_header_fields` so the FSM does not create a second active
codec pair.

Use `H2HeaderField` and the `H2ByteStreamEvent`/`H2ByteClientEvent` families
when bytes, duplicates, global order, or sensitivity must survive forwarding.
A sensitive occurrence is always encoded as never indexed and is not inserted
into the dynamic table. Text events and `project_h2_header_fields*` are atomic
convenience projections: non-text input is rejected, and typed pseudo-fields
cannot be forwarded later as ordinary occurrences.

`H2Limits` independently bounds encoded header-block bytes, decoded
field-section size, and table capacity. The encoded and decoded defaults are
64 KiB, the table defaults to 4,096 octets, and table capacity is capped at
1 MiB. Compression errors poison the connection decoder; a decoded-list limit
failure preserves required table synchronization when no later compression
error occurs. `H2HpackError` distinguishes compression, limit, poison,
overflow, and allocation categories.

`inbound_hpack_diagnostics` and `outbound_hpack_diagnostics` return
`H2HpackDiagnosticsSnapshot` values with content-free counters for blocks,
bytes, representations, Huffman use, table activity, and failures.
`effectiveness()` reports an exact reduced wire/field-octet fraction when its
operands are usable. See the
[HPACK and Header Representation Reference](hpack-header-representation.md) for
the complete API, adapter, failure, and default/all-feature compatibility
contract.

Reusable HTTP/2 configuration and diagnostics are exported through types such as
`H2Limits`, `H2ControlDiagnostics`, `H2TimerIntent`, `H2ShutdownIntent`,
`H2ReceiveWindow`, `H2ReceiveWindowDiagnostics`, `H2SendCapacity`,
`H2FlowDiagnostics`, `H2FlowDiagnosticsSnapshot`,
`H2FairStreamScheduler`, `H2FairStreamSchedulerDiagnostics`, and
`H2BackpressureState`. Defaults are bounded for header blocks, continuation
sequences, settings entries, queued control frames, queued DATA bytes, and
closed-stream tombstones, with a default network-facing active-stream cap of 100
and a default decoded header-list cap of 64 KiB. Explicit higher active-stream
limits remain valid. The fair scheduler uses randomized hashing for
peer-selected stream IDs and bounds its initial reservation at the default cap,
growing only when an owner deliberately configures and reaches a higher limit.
`H2ReceiveWindow::with_adaptive_growth` uses fast consumed-byte turnovers,
measured exhaustion delay, and an RTT/BDP proxy to grow receive credit in bounded
steps. The initial window remains the floor and the configured maximum remains
the memory cap. The internal control-frame abuse budget uses a monotonic clock
for idle refill; externally visible timers remain adapter-owned.

`H2FlowDiagnosticsSnapshot` reports mutually exclusive stream- and
connection-window stall reasons per failed flush pass, post-startup
WINDOW_UPDATE refund counts and bytes accepted by the caller-owned downstream
output layer, the current number of queued streams blocked on send capacity,
and fair-scheduler distribution. `consume_outbound(amount)` establishes that
ownership transfer: adapters call it only after their output queue or TLS layer
accepts exactly `amount` bytes, and retain accepted bytes until write completion
or terminal connection failure. Partial TLS acceptance consumes only the
accepted prefix. Connection-window exhaustion wins when both windows are
blocked, and repeated failed passes are counted repeatedly. Startup stream-0
credit advertisement is configuration, not a receive refund, and is excluded.
A stream WINDOW_UPDATE removed before ownership transfer, such as by a GOAWAY
cutoff, is also excluded. An open-idle flush records `NoStreamOutput` as the
latest gRPC flush boundary rather than retaining a historical stall reason.
`kimojio_fsm_grpc::ServerEvent::Goaway` carries the monotonic `received_at`
timestamp from the receive call that emitted it. Owners must pass that value
unchanged to `GrpcServer::discard_outbound_streams_above_at`; using the time of
a later receive can distort adaptive receive-window retirement.

Fair scheduler selections are successful `next_ready` turns only.
`immediate_dispatches` counts direct flush invocations that emit at least one
DATA frame; it is neither a frame count nor a fair-turn-equivalent denominator.
Blocked direct attempts affect neither population. Scheduler turns are grouped
by inclusive upper bounds `[0, 1, 3, 7, 15, 31, 63, u64::MAX]`. The zero bucket
is connection-lifetime history for active and retired streams that received no
fair turn; it does not diagnose current blockage. Use
`pending_capacity_streams` for the instantaneous blocked gauge, and interpret
histogram deltas only with connection-lifecycle context. Active stream counts
are materialized by scanning the connection-local stream table only when
diagnostics are requested. Snapshot collection is therefore O(tracked streams)
and intended for control-plane sampling. The intrusive ready queue has one
physical entry per queued stream; selection, drain, and retirement do not
allocate or retain stale tokens.

One scheduler belongs to one HTTP/2 connection. `register` enrolls new work,
`mark_drained` temporarily unlinks an open stream, and idempotent `remove`
retires terminal state exactly once; valid HTTP/2 stream IDs are never reused
after removal. `GrpcServer::h2_flow_diagnostics` and
`GrpcClient::h2_flow_diagnostics` return the native snapshot while preserving
the existing flattened gRPC counters as projections of the same accumulator.
`GrpcDiagnosticsSnapshot` is non-exhaustive; downstream code should obtain it
from a client/server or start with `Default`, then inspect selected fields
instead of constructing or destructuring every field.

The diagnostics API change is an intentional source-incompatible migration.
Downstream code that constructs `GrpcDiagnosticsSnapshot` with a struct literal
must use `GrpcDiagnosticsSnapshot::default()` and then mutate or read supported
public fields, or consume a runtime-produced snapshot; exhaustive destructuring
must include `..`. Callers of the removed
`GrpcDiagnosticsSnapshot::record_flush_stop` and
`GrpcDiagnosticsSnapshot::record_window_update` mutators must use
runtime-produced snapshots and native flow diagnostics instead. `H2FlowStall`
remains intentionally exhaustive.

Low-level stream closure is intentionally asymmetric. `H2Server::close_stream`
abandons server state and records reset tolerance when its tombstone is retained.
`H2Client::close_stream` leaves a normally completed tombstone unchanged and
marks only an active abandoned stream as reset-tolerant; unknown IDs are a
no-op. Reset-tolerant late DATA is exposed as
`H2StreamEvent::DiscardedData` or `H2ClientEvent::DiscardedData`. Owners must
charge its full, padding-inclusive `flow_control_len` to the connection receive
window even though no application payload is delivered. Refunding that credit
after safely discarding the payload is owner-specific policy, not part of the
event contract. These variants are source-incompatible additions for exhaustive
matches, so downstream matches over either event enum must add an explicit
`DiscardedData` arm.

Adapters that strip DATA padding or otherwise normalize a frame before passing
it to the FSM must retain the decoder-local, padding-inclusive wire length and
use that value for connection accounting; the normalized event cannot recover
removed padding. The request-only `H2Server::accept` compatibility wrapper
continues to tolerate late DATA after `close_stream` without delivering a
request. Accounting-aware owners must use `accept_event_ref` or `accept_event`
instead, where `DiscardedData` is explicit.

When migrating low-level code, use `H2FrameType::as_u8()` rather than enum casts
and include `H2FrameType::Unknown(_)` in exhaustive matches.
Use `H2FrameRef::decode` when the frame payload is consumed before the input
buffer is reused; its payload aliases the input without allocation or copying.
`H2Server::accept_event_ref` and `H2Client::accept_ref` likewise return
`H2StreamEventRef` and `H2ClientEventRef` with borrowed DATA payloads for
synchronous consumers. Use `H2Frame::decode`, `H2Server::accept_event`, and
`H2Client::accept` when payloads must be retained as owned `Vec<u8>` values.
Stack HTTP validation and FSM gRPC consume the borrowed events directly; adapters
that retain bodies copy once into their final owned buffer.

FSM gRPC classifies outbound protocol buffers as DATA, headers, WINDOW_UPDATE,
or control output. Adjacent WINDOW_UPDATE buffers and adjacent control-response
buffers are coalesced immediately up to 16 KiB, without waiting to fill a batch.
Coalescing never crosses DATA or header boundaries, so fair DATA scheduling and
header/trailer ordering remain unchanged. PING and SETTINGS acknowledgements,
RST_STREAM, and GOAWAY-class output remain immediately pollable rather than
waiting on a batching timer.

The proof server infers `content-type` for common HTML application assets such as
HTML, CSS, JavaScript, JSON, text, SVG, PNG, JPEG, GIF, WebP, icons, fonts,
WASM, PDF, audio, and video files. Each served request is logged to stderr with
peer address, protocol, method, target, status, byte count, and content type.

## Validation

```sh
cargo test -p kimojio-fsm-http
cargo test -p kimojio-fsm-http corpus
cargo test -p kimojio-fsm-http flow
cargo test -p kimojio-fsm-static-file-server
cargo test -p kimojio-stack-http
cargo test -p kimojio-fsm-grpc
cargo build -p kimojio-fsm-static-file-server
```

## Current limitations

- The FSM HTTP/2 engine is a protocol component, not a complete web server.
- TLS server and ALPN negotiation are included for proof usage.
- The proof server is not a production static file server.
- Release acceptance remains blocked on `TEST-004B` in
  `kimojio-fsm-http/TODO.md`: final-revision, same-session end-to-end workloads
  must satisfy the 5% throughput and 10% per-request p99 regression bounds.
  Existing scheduler batch percentiles are local throughput-stability evidence,
  not operation or request p99.
- Server push, RFC 9218 priority scheduling, h2c upgrade, and other optional
  features listed in `kimojio-fsm-http/TODO.md` remain future work.
