# HTTP/1 and HTTP/2 composition

This module selects a child protocol and drives that child through semantic callback ports.
It reuses `kimojio-fsm-http1` rather than a second HTTP/1 parser.
The direct crate-level HTTP/2 `Client` and `Server` do not use this selector.

## Interface choice

A port set implements both child port traits with the same `Output` type.
The composite retains `next(&mut ports) -> Option<Output>`.
Each child retains its own commands, operation types, errors, and metadata.
The composite does not force these values through a common action enum.

This choice preserves static callback dispatch and protocol-specific capabilities.
It does require two port implementations.
Applications can forward both metadata callbacks to a shared handler.
They must not infer framing, credit, or stream lifecycle in that handler.

`http1_mut()` and `http2_mut()` expose the selected child for commands and ordinary completions.
Only the selected accessor returns `Some`.
The caller drives the composite, not the child, while the composite owns protocol selection and prefetched input.
HTTP/1 upgrade handoff and HTTP/2 stream control remain available through the child interfaces.

An explicitly selected client uses `Client::http1(child)` or `Client::http2(child)`.
The caller supplies the protocol choice, including an external ALPN result.
The corresponding server constructors also skip detection.

## Server detection

`Server::detect` recognizes the complete HTTP/2 prior-knowledge preface.
A mismatch selects HTTP/1.
The detector preserves all bytes from each read, including bytes after a mismatch.
It never passes a complete HTTP/2 preface to the HTTP/1 parser.

```rust
use std::time::Duration;
use kimojio_fsm_http2::{Config, http::{self, http1}};

let http1_config = http1::Config::default();
let config = http::DetectionConfig {
    http1_connection: http1::ConnectionId { slot: 7, generation: 1 },
    http1_buffer: vec![0; http1_config.max_buffer_bytes],
    http1_config,
    http2_config: Config::default(),
    timeout: Duration::from_secs(2),
};
let server = http::Server::<Vec<u8>>::detect(config, Duration::ZERO).unwrap();
assert_eq!(server.protocol(), None);
```

Detection uses the HTTP/2 port types for its read, alarm, cancellation, and close operations.
These operations have a separate owner identity from either child.
The composite's `complete_read`, `complete_wake`, `complete_cancel`, and `complete_close` methods route those completions.
The same methods accept the selected HTTP/2 child's corresponding completions.
HTTP/1 completions go to the selected HTTP/1 child.

`ServerPorts::detection_closed` reports a failed detection after its original operations and transport close settle.
It preserves the primary failure and the close result separately.
The detector emits no HTTP/1 or HTTP/2 protocol output on this path.

The detector cancels its alarm after it selects a protocol.
Activation waits for both the original alarm and any issued cancellation acknowledgment.
A late alarm cannot reverse selection or advance the replacement child's clock.
A timeout or shutdown also waits for an outstanding read, including a late successful read.
An active alarm failure ends detection with `DetectionFailure::Transport`.
An obsolete alarm failure only settles its original obligation.
It cannot advance time, reverse selection, or replace a primary failure.

Both composites expose `abort()` for hard termination through the selected child.
The server also accepts this command during detection and prefetched-input replay.
Abort preserves original-operation and cancellation joins without a graceful-shutdown wait.
`Server::shutdown()` retains its existing graceful behavior after protocol selection.

The selected child receives the prefetched bytes through its own typed read completion.
This cold path retains the child's ownership and parser contracts.
After that prefix drains, drive calls delegate directly to the child.

## Time and resource costs

The caller supplies one monotonic `Duration` time origin.
HTTP/1 receives its checked nanosecond representation.
Values outside that representation return `Error::TimeRange` without a partial clock update.
Both child time origins include time spent in detection.

Detection checks both configurations at construction and retains both candidate children until selection.
The HTTP/1 receive buffer comes from the caller.
The HTTP/2 child owns its configured storage.
The unselected child is dropped before normal protocol processing.

The selector boxes child state to avoid large moves when its lifecycle changes.
Explicit selection adds one box and one protocol branch per drive call.
Detection also boxes its coordination state.
Detection adds a bounded 24-byte prefix and cold-path read storage.
These are implementation costs, not measured performance claims.
Applications that require a direct HTTP/2 path can use the crate-level engine without these costs.

The composite adds no body queue, stream table, flow-control calculation, or payload copy.
It does not unify HTTP/1 connection upgrades with HTTP/2 CONNECT streams.
Those operations have different ownership and protocol meanings.

## Evidence boundary

The composition tests compare exact response bytes with explicitly selected child paths.
They include persistent HTTP/1 requests, concurrent HTTP/2 requests, fragmented input, and yielding or continuing callbacks.
The detector model covers both original-completion orders, batched completions, and both cancellation-join orders.
It also covers EOF, shutdown, wrong-owner completions, invalid counts, close failure, and invalidated alarms.
Failure schedules include active and obsolete alarm failures, hard abort, and primary-cause preservation across late completions.

These checks qualify composition behavior.
They do not establish independent HTTP/2 interoperability or runtime-wrapper performance.
