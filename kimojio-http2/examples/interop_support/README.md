# Wrapper socket fixture

Native modes use `kimojio_http2::Client`, `NativeConnection::run`, and `serve_connection_native_with_shutdown`.
It does not drive the core or implement socket reads and writes.
Both roles use native Kimojio descriptors and I/O scopes.

```text
interop client REQUEST_JSON RESULT_JSON
interop server REQUEST_JSON
interop client-generic REQUEST_JSON RESULT_JSON
interop server-generic REQUEST_JSON
```

The `client` and `server` names retain their native behavior.
Generic modes wrap the established descriptor in `OwnedFdStream`.
They call `connect` with `Connection::run`, or `serve_connection_with_shutdown`.
Both transports share the JSON interface, application logic, routes, bounds, and result rules.
The generic adapter is part of the wrapper, not a fixture socket executor.

The JSON interface follows `interop/http2/ADAPTER.md`.
The server request contains `schema: 1`, `timeout_ms`, and an optional `config` object.
The server binds an ephemeral loopback port and flushes `LISTEN 127.0.0.1:PORT`.
It handles connections sequentially, with concurrent requests inside each connection.

## Client observations

The fixture polls each send future once, in request order, before it schedules concurrent response work.
The admission callback checks each actual stream ID against that order.
Requests without admission do not produce synthetic stream results.
Uploads use lazy frames of at most 16 KiB.
Receive chunks return to the wrapper before the fixture waits for full retirement.

Only an explicit reset action calls `IncomingBody::cancel`.
Neither status 200 nor status 413 cancels an upload.

`send_with_observer` reports actual informational heads, admission, receive completion, and retirement.
The observer remains available after a failure before final headers.
`IncomingBody::retirement` supplies the authoritative report after response frames finish.
The fixture does not use `completion()` errors to infer retirement.
The `ended` field records receive completion, separately from the full stream outcome.

An optional `send_failure` object preserves the failed buffer receipt: `accepted`, `exact`, `reason`, and an optional wire reset error.
The accepted count belongs to that buffer, not the complete upload.
An optional `wrapper_error` preserves additional error context.
Neither field replaces the authoritative retirement outcome.
A reset with wire code zero remains a reset, even after normal response END_STREAM.

The selected connection driver remains active alongside the application.
The report records a confirmed close only after the driver returns a documented terminal connection result.
A watchdog requests abort and permits three seconds for driver settlement.
A missing close result stays unconfirmed.
An admitted stream without a retirement report causes an explicit fixture error.

## Generic transport errors

A successful generic write covers its complete offered range.
A failed started write can preserve only a lower-bound receipt, with `exact: false`.
The fixture preserves that distinction and does not replay the failed buffer.
Cancellation does not settle an original operation or release its buffers early.

The generic driver drops its settled read half before it awaits the full transport `close`.
The fixture does not substitute write-side shutdown for full close.
`Error::Transport` can represent a split, I/O, or close error without a separate terminal connection report.
`Error::TransportAndClose` preserves both errors but does not establish successful close.
These errors produce a nonzero fixture result, with the diagnostic intact and closure unconfirmed.
The fixture does not infer a terminal core outcome or confirmed close from an errno.

## Server routes

| Route | Response |
| --- | --- |
| `/bytes/N` | Status 200, exact length, and repeated byte `stream_id % 251` |
| `/trailers/N` | The same body, followed by `x-end: done` |
| `/informational/N` | Status 103, then the same final response as `/bytes/N` |
| `/no-content` | Status 204 without a body or Content-Length |
| `/early` | Status 413 with an empty body, without a wait for request END_STREAM |
| `/echo` | Status 200 at request headers, followed by streamed request data |
| Classic CONNECT | Status 200, followed by bidirectional streamed data |

HEAD returns the declared route length without DATA.
Unknown routes and invalid lengths return status 404 with an empty body.
Echo forwards body leases through `OutgoingFrame::Forward`, without complete-body collection or payload copies.
The wrapper retains each forwarded lease through its original write receipt.
Echo consumes request trailers but does not include them in the response.
Other routes drop the unread request body, which makes the wrapper discard and refund subsequent upload data.

The informational sender accepts metadata before the handler returns its final response.
Its successful result is not a socket write acknowledgment.
Only the peer can establish receipt of those headers.

## Startup profile

The production wrapper permits upload DATA before peer SETTINGS, within the current default credit.
A smaller initial peer window can therefore affect DATA already in flight.
The fixture has no readiness API, raw-frame observer, or delay that pretends to establish peer readiness.
Strict reduced-window success cases require a separately synchronized peer profile.
A permitted startup stream reset is not a wrapper defect or a successful upload.
Unknown or failed retirement never becomes success because receive END_STREAM was present.

## Trailer representation

Incoming trailer output groups occurrences using `HeaderMap::iter`.
The fixture preserves each name and value, including the order of repeated values for the same name.
It does not reconstruct original wire order across different names.
Peer comparisons must use per-name semantics, as described by [RFC 9110 section 5.3](https://www.rfc-editor.org/rfc/rfc9110.html#section-5.3).

## Bounds

The request file limit is 2 MiB.
The fixture permits at most 4,096 requests and 64 active requests.
Each generated body has a 1 GiB logical limit, independent of retained storage.
Paused leases have an 8 MiB capacity limit and an 8,192-fragment limit.
Response trailer metadata has an 8 MiB aggregate limit.
Each stream permits at most 64 informational responses.
Absent window fields preserve wrapper defaults.
Server connections have the same watchdog and a three-second abort settlement limit.

The focused tests cover submission order, producer bounds, schema fields, server routes, informational callbacks, and retirement before response headers.
They also preserve reset code zero independently of upload failure context.
Socket reports qualify only the cases run against this wrapper executable.
They do not establish complete RFC coverage or performance.
