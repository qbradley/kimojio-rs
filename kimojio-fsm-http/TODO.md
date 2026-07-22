# kimojio-fsm-http TODO

This file lists only known remaining work. Implemented HTTP/1.1 and HTTP/2
framing, HPACK, stream lifecycle, validation, flow control, control-plane,
diagnostics, conformance, and characterization work is omitted. The completed
HPACK design is described in the
[HPACK and Header Representation Reference](../docs/hpack-header-representation.md).

The crate is a workspace member consumed by the optional `kimojio` `http`
feature. Gaps found while building that adapter are tracked under
[Message semantics and connection reuse](#message-semantics-and-connection-reuse)
and [Driver and adapter contract](#driver-and-adapter-contract).

## Sorting and labels

- **Importance**: P0 = correctness, security, or interop blocker; P1 =
  performance, operability, or broader compatibility; P2 = optional or currently
  out-of-scope.
- **Risk**: High = touches core wire state, flow-control, memory, or shutdown
  semantics; Medium = localized but protocol-visible; Low = additive,
  diagnostic, or documentation-heavy.
- Items are grouped by category and sorted within each category by importance,
  then by risk.

## Message semantics and connection reuse

These gaps were identified by reviewing the `kimojio::http` adapter, the first
production consumer of this crate. Each one currently forces the adapter to
either refuse a capability or hard-code a conservative policy.

| ID | Importance | Risk | TODO / code to implement |
| --- | --- | --- | --- |

## Driver and adapter contract

| ID | Importance | Risk | TODO / code to implement |
| --- | --- | --- | --- |

## Stream lifecycle and message extensions

| ID | Importance | Risk | TODO / code to implement |
| --- | --- | --- | --- |
| STREAM-007 | P1 | Medium | Add extended CONNECT support if a non-gRPC caller needs it. This requires `SETTINGS_ENABLE_CONNECT_PROTOCOL`, `:protocol` validation, and CONNECT-specific pseudo-header rules. Keep disabled until there is a concrete caller. |
| STREAM-008 | P1 | Medium | Add explicit connection-rotation/retry guidance around stream-ID exhaustion. Local stream opening rejects overflow, but production adapters still need documented retry/drain behavior when the usable stream-ID space is exhausted. |
| STREAM-009 | P1 | Medium | Add early-response request-body cancellation. When a server sends a complete response before consuming a request body, optionally send RST_STREAM(NO_ERROR) or an equivalent drain/cancel signal so the peer stops uploading unused DATA without treating the response as failed. |

## Control-plane policy and timers

| ID | Importance | Risk | TODO / code to implement |
| --- | --- | --- | --- |
| CTRL-004 / SEC-004 | P1 | Medium | Complete graceful-shutdown timer orchestration. FSM HTTP exposes drain/close intent and `kimojio::http` now provides a reference adapter with I/O and shutdown deadlines, but the crate still owes the common two-stage GOAWAY sequence, RTT/PING wait policy, and deterministic virtual-clock hooks so every adapter does not re-derive them. |
| CTRL-005 / CTRL-006 / SEC-001 / SEC-003 | P1 | Medium | Make PING/RST/control-frame abuse policy configurable. Current fixed control budgets and diagnostics are bounded, but production deployments may need configurable ping/reset/window-update/priority budgets, timeout behavior, optional GOAWAY debug labels, and separate idle-keepalive versus suspicious-flood classification. |
| SEC-008 | P1 | Medium | Gate HTTP/2 inbound flow control on consumer demand. The adapter advertises a receive window far larger than its one-slot streaming handoff and cannot withhold a per-stream `WINDOW_UPDATE`, so a peer may legitimately send more than a paused consumer can take. The adapter now retires just that exchange rather than failing the connection, but a slow consumer can still lose its stream. Exposing per-stream window control would let demand drive the window and stop the situation arising. |
| DRIVER-005 | P2 | Low | Give HTTP/2 client multiplexing an operational surface. Connection topology now collapses from many connections to roughly one per origin, which concentrates blast radius and reconnect load, and nothing lets an operator see stream counts or attribute a failure to a stream. `Limits::max_active_streams` set to 1 works as a kill switch but is not documented as one, and the HTTP/2 streaming-body-drop semantics change is recorded only in rustdoc. |
| DRIVER-006 | P2 | Low | Verify HTTP/2 client failure isolation against a third-party peer. Reset, cancellation, and GOAWAY isolation are exercised mainly against this crate's own `H2Server`, which shares its frame and HPACK code, so a shared misconception would pass both sides. Drive the same cases from a real peer that can inject RST_STREAM and graceful GOAWAY. |
| SEC-009 | P1 | Medium | Stop gating adapter writes on inbound request demand. The HTTP/2 pump parks before its next `step` whenever some streaming consumer holds an undrained chunk and no stream has outstanding demand, which also withholds responses already prepared for every other stream on that connection. An application handler that neither completes nor reads its request body therefore stalls the whole connection; ordinary handlers only escape because completing removes their control. Inbound backpressure should suspend reading, not writing. |
| SEC-005 | P1 | Medium | Add connection-age and reconnect-jitter policy hooks. Expose drain intents and timer metadata so runtime adapters can implement max-connection-age rotation without embedding runtime ownership in FSM HTTP. |

## Optional or deprecated HTTP/2 features

| ID | Importance | Risk | TODO / code to implement |
| --- | --- | --- | --- |
| OPT-001 / OPT-004 | P2 | Medium | Evaluate server push and `PUSH_PROMISE` only if a future HTTP workload needs it. Full support adds reserved stream states, cache semantics, and security risk; keep disabled by default. |
| OPT-003 | P2 | Medium | Evaluate RFC 9218 `PRIORITY_UPDATE` and priority scheduling only for browser/proxy workloads. Current object-gateway and gRPC paths do not need it. |
| OPT-005 | P2 | Low | Evaluate h2c Upgrade support only for compatibility tests. Prior-knowledge cleartext and TLS ALPN cover current repository use cases. |

## Interop, benchmarks, and release evidence

| ID | Importance | Risk | TODO / code to implement |
| --- | --- | --- | --- |
| TEST-002 | P1 | Medium | Extend the completed direct-h2, h2spec, Python `h2`, and nghttpd coverage with accountable Go `x/net/http2` and grpc-go client/server peers. Focus the remaining matrix on streaming, trailers, PING, GOAWAY, RST_STREAM, malformed-peer behavior, and default versus large-window configurations; do not duplicate cases already closed by the retained 21-case external parent. |
| TEST-004 | P1 | Medium | Add isolated protocol microbenchmarks not covered by the completed 1 KiB echo qualification and 920-row end-to-end characterization: HEADERS encode/decode, DATA split/coalesce, WINDOW_UPDATE batching, and scheduler selection. Report copy counts, allocations, and throughput for 1 KiB and 256 KiB streams. |
| TEST-004B | P1 | High | Decide whether a future public release needs a stricter performance gate than the completed private qualification. If required, run at least 10 independent same-session trials over unary health, small upload/download, listing, and 256 KiB streaming upload/download at 1, 16, and 64 clients. Require the relative-regression confidence interval to remain wholly within 5% for throughput and 10% for per-request p99, retaining request-level distributions plus allocation, copy, flow-stall, fairness, queued-byte, revision, binary, profile, command, and host-placement evidence. |
| TEST-006 | P2 | Low | Add a feature-coverage table comparing `kimojio-fsm-http`, `kimojio-stack-http`, Tokio h2, Go `x/net/http2`, and grpc-go. Update the table when optional features or performance work land. |
| TEST-007 | P2 | Low | The http2jp/hpack-test-case interop corpus runs on demand only: `scripts/vendor-hpack-interop.py` generates it and `KIMOJIO_HPACK_INTEROP_CORPUS_V1` points `tests/hpack_interop.rs` at the result. Measured against 18 injected decoder faults it uniquely caught 3, all static table entry corruption, which `tests/hpack_static_table.rs` now catches outright, so the 386 KiB asset is not committed. Run it after changes to HPACK decoding internals, and extend the generator to stories 20-31 if dynamic table eviction depth ever needs the 646-case sequences. |
| TEST-009 | P2 | Low | The `fuzz/` crate carries coverage-guided targets for the HPACK decoder, HPACK round trip, HTTP/2 server accept loop, HTTP/1 connection decoder, and HTTP/1 chunked body; `fuzz/README.md` documents each surface. The HTTP/2 accept target found the oversized header field panic pinned by `tests/h2_header_field_bounds.rs`. Corpora are not committed, so re-run the targets after changes to decoding internals rather than relying on a stored corpus, and pin any finding as a regression test instead of checking in the artifact. |
