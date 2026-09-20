# HTTP/2 socket adapter, schema 1

This interface separates the native fixture from the independent peer.
All connections use cleartext HTTP/2 prior knowledge on loopback.
No capability flags or silent skips exist.

## Client command

The adapter file contains:

```json
{"schema":1,"command":["/absolute/fixture","client","{request_file}","{result_file}"]}
```

The harness replaces both placeholders without a shell.
The fixture reads one request file and writes one result file.
Both files use UTF-8 JSON.
The request file contains:

```json
{
  "schema":1,
  "host":"127.0.0.1",
  "port":12345,
  "timeout_ms":60000,
  "config":{"stream_window":1024,"connection_window":65535},
  "request_count":2,
  "concurrency":2,
  "requests":[
    {"method":"GET","path":"/bytes/131087","body_bytes":0,"trailers":[]},
    {"method":"GET","path":"/trailers/37","body_bytes":0,"trailers":[]}
  ],
  "actions":[]
}
```

The fixture uses one connection for all requests.
For these strict-success upload scenarios, the fixture waits for the peer's initial SETTINGS before it starts body DATA.
It must not wait for acknowledgment of its own SETTINGS before it sends request headers.
That distinction permits the legal pre-ACK push scenario.
This upload gate is a test-scenario choice, not a production handshake requirement or a general Python-h2 requirement.
It is not a recipe for raw-frame parsing in production wrappers.
Request array order determines stream IDs: 1, 3, 5, and subsequent odd IDs.
`concurrency` limits active requests, not lifetime requests.
Upload bytes repeat the byte `stream_id % 251`.
The fixture must not retain the complete body.
`config.stream_window` controls the advertised initial receive window.
`config.connection_window` controls the initial connection receive credit.
Values larger than 65,535 require an initial connection WINDOW_UPDATE.
An absent key selects the fixture default without an override.
Default cases use an empty `config` object and measure both actual balances.

### Startup window changes

A client can send request DATA before server SETTINGS, within its current credit.
When an initial-window reduction affects DATA already in flight, RFC 9113 section 6.9.3 permits a stream FLOW_CONTROL_ERROR reset.
That stream reset does not, by itself, require a connection error or GOAWAY.
The sender must retain any negative window balance and remain blocked until new credit makes the balance positive.

The normal flow cases avoid that startup race so they can require complete, successful streams.
They still measure actual SETTINGS and connection credit.
No ungated-startup case is currently part of these suites.
Such a case must distinguish a permitted wire RST_STREAM(3) from successful, credit-compliant delivery.
It must not label that permitted stream reset as a new protocol defect.

The result file contains:

```json
{
  "schema":1,
  "streams":[
    {
      "stream_id":1,
      "status":200,
      "content_length":131087,
      "bytes":131087,
      "sha256":"lowercase hexadecimal SHA-256",
      "trailers":[],
      "informational":[],
      "ended":true,
      "outcome":"complete",
      "error":null
    },
    {
      "stream_id":3,
      "status":200,
      "content_length":37,
      "bytes":37,
      "sha256":"lowercase hexadecimal SHA-256",
      "trailers":[["x-end","done"]],
      "informational":[],
      "ended":true,
      "outcome":"complete",
      "error":null
    }
  ],
  "connection":{"error":null,"outcome":"graceful","closed":true}
}
```

Every issued stream needs exactly one result.
`stream.outcome` records full stream retirement, separately from receive completion.
Its values are `complete`, `reset`, `unprocessed`, `connection_failed`, and `deadline`.
It remains null until retirement.
`ended` records normal receive END_STREAM, not successful transmission or full retirement.
An unexpected producer failure must remain a non-complete outcome, even after a successful response.

`connection.outcome` records the terminal connection result.
Its values are `graceful`, `peer_closed`, `io_failed`, `protocol`, and `resource_exhausted`.
Normal cases require every stream outcome to be `complete`.
They also require a `graceful` or `peer_closed` connection outcome and an actual socket close.
Missing or null terminal outcomes cannot qualify a completed case.

`content_length` is the declared response length, or null when the response has no Content-Length field.
HEAD reports the declared length even though `bytes` is zero.
`trailers` contains ordered string pairs, such as `[["x-end","done"]]`.
`informational` contains status integers, such as `[103]`.
`error` is null or `{"scope":"stream","code":1}`.
A connection error uses `{"scope":"connection","code":1}`.
The `connection.error` field reports terminal connection errors.
Pending streams retain null errors when a connection error prevents their completion.
Codes are HTTP/2 wire error codes.
Transport, source, resource, and unprocessed failures must not invent HTTP/2 error codes.
Their non-complete terminal outcomes remain mandatory even when `error` is null.
`closed` means that the fixture explicitly closed its socket.
It does not mean that the fixture observed EOF from the server.
Successful commands return zero, including expected protocol-error scenarios.
Setup errors return nonzero.

## Server command

The server adapter file contains:

```json
{"schema":1,"command":["/absolute/fixture","server","{request_file}"]}
```

The process binds an ephemeral loopback port.
Its request file contains `schema`, `config`, and `timeout_ms`, with the same meanings as the client file.
A server with a flag interface can instead use:

```json
{"schema":1,"command":["/absolute/fixture","server","--stream-window","{stream_window}","--connection-window","{connection_window}"]}
```

Both window placeholders are mandatory for this form.
An absent configuration field becomes the literal argument `default`.
The fixture must preserve its default for that argument.
The harness still records the requested configuration in its report directory.

It prints and flushes `LISTEN 127.0.0.1:PORT`.
The harness owns process termination after each case.
Each connection supports multiple requests.
Routes are:

| Route | Response |
|---|---|
| `/bytes/N` | Status 200, N copies of byte `stream_id % 251`, exact content length |
| `/echo` | Status 200 at request headers, then the exact accepted request fragments as streaming DATA |
| `/trailers/N` | The same body as `/bytes/N`, followed by `x-end: done` |
| `/informational/N` | Status 103, then the same final response as `/bytes/N` |
| `/no-content` | Status 204 and no DATA payload |
| `/early` | Status 413 with END_STREAM before request END_STREAM |

HEAD returns the route's content length but no DATA payload.
Classic CONNECT uses `:method` and `:authority`, without `:scheme` or `:path`.
The fixture accepts CONNECT with status 200 and echoes tunnel DATA until END_STREAM.
CONNECT is bidirectional and does not wait for the complete request body.
The `/echo` response ends only after request END_STREAM and all accepted fragments reach their output write completions.
The fixture retains each accepted fragment until its echo write completes.
Only then does it release that fragment and return its receive credit.
It does not collect the complete upload before it starts the response.
The client fixture must process responses while its upload producer remains active.
Echo responses can omit Content-Length, especially for requests without a declared length.

## Initial concrete cases

Both directions require a 131,087-byte body with stream windows 1,024 and 65,535.
These explicit cases select connection credit 65,535.
Default cases use 16 MiB plus 17 bytes, without receive-window overrides.
The sender must first record the actual SETTINGS and connection credit.
A case fails unless its body is at least twice the larger actual initial balance plus 17 bytes.
Default cases also fail if either initial balance exceeds 8 MiB.
Senders obey both balances and every WINDOW_UPDATE increment.

Reduced-window serial and concurrent cases use 320 bodies of 512 bytes.
Other serial and concurrent cases use 600 bodies of 32,768 bytes.
Each body is smaller than the advertised stream window.
Their total exceeds twice the actual initial connection window.
Every body needs exact byte counts, SHA-256, END_STREAM, and expected trailers.
The peer records all WINDOW_UPDATE increments and final send balances.
Those values must satisfy the stream and connection credit equations.
Independent upload clients stop after their first fragment until echo DATA arrives.
This explicit duplex probe rejects a server that waits for the complete request body.

The initial semantic cases cover trailers, informational responses, HEAD, and status 204.
Fault controls include a receiver that withholds consumption and a server that never returns upload credit.

## Actions exercised by socket suites

The independent reference peers implement these actions.
The native fixture needs each action for the corresponding case.

* `{"action":"pause","stream_id":1,"until_stream_ended":3}` retains stream 1 body fragments until stream 3 ends.
* `{"action":"reset","stream_id":1,"after_bytes":1024,"code":8}` cancels stream 1 after that many response bytes.
* `{"action":"graceful_close","after_streams":2}` starts graceful shutdown after two completed streams.
* `{"action":"cancel_upload_after_response","stream_id":1}` cancels that unfinished upload after normal receive END_STREAM.

The harness must name every executed case in its report.
Unsupported actions fail rather than produce a skip.
Further protocol cases can use fixed independent-peer scenarios without new fixture routes.

The reset case reports status 200, 1,024 delivered bytes, no END_STREAM, and a stream error with code 8.
The peer sends late DATA after reset to exercise connection-credit refunds.
These late frames remain within the credit that existed before reset.
The test prohibits stream credit for the discarded DATA.
The peer withholds the sibling's END_STREAM until connection refunds cover the initial 1,024 bytes and all 32,768 discarded bytes.
This barrier prevents graceful close from discarding pending refunds before the test observes them.

The graceful-close case requires GOAWAY(NO_ERROR) on the wire before actual socket close.
The content-length error case sends 37 bytes, then an empty END_STREAM after a PING barrier.
Its declared content length is 38.
It requires stream PROTOCOL_ERROR without a connection error.

The empty-DATA pressure case permits stream 1 to fail with code 11 and zero delivered bytes.
That outcome requires a matching RST_STREAM, no connection error, and a complete response on sibling stream 3.
The report distinguishes this bounded resource rejection from full body delivery.
The rejected stream outcome must be `reset`, while the sibling outcome must be `complete`.

## Early response with an explicit peer abort signal

Response END_STREAM alone never instructs the protocol engine to cancel the upload.
This is true for status 200 and status 413.
The withheld-credit case uses no fixture application action.
The independent server sends a complete status-413 response after it receives 1,024 request bytes.
It then sends a PING after response END_STREAM.
After the matching PING acknowledgment, it sends RST_STREAM(NO_ERROR) and keeps the socket open.
This barrier proves that response END_STREAM preceded the reset in the received frame sequence.

The client preserves the completed response and lets its sibling finish.
The rejected stream reports `ended: true`, `outcome: "reset"`, and the actual stream reset code 0.
The connection requires `outcome: "graceful"`, a null wire error, and an actual close.
`connection_failed` is not an acceptable substitute for this peer-requested stream termination.

Without a reset or application cancellation, the duplex probe requires the complete request body and request END_STREAM.
It can send status 200 or 413 while it continues to consume the upload and return credit.

RFC 9113 section 8.1 permits a server to send RST_STREAM(NO_ERROR) after a complete response.
The client must not discard that completed response.
Explicit application cancellation is another valid policy, but this case does not require an additional fixture action.

### Declared application-cancellation variant

`protocol_suite.py --early-response-policy application-cancel` selects a distinct fixture-application contract.
The request includes `{"action":"cancel_upload_after_response","stream_id":1}`.
After normal receive END_STREAM, the fixture cancels only the selected stream's unfinished upload.
The action does not depend on response status.
Without the action, neither status 200 nor status 413 authorizes cancellation.

The independent server withholds credit and keeps its socket open.
It does not send the PING barrier or a reset in this variant.
The client must send exactly RST_STREAM(CANCEL) for stream 1 after it accepts the completed response.
That stream reports `ended: true`, `outcome: "reset"`, and actual wire code 8.
Its sibling must complete, and the connection must close gracefully without error.
The report records the selected policy.

The default remains `peer-reset`, with strict code-0 and barrier assertions.
The oracle never accepts either reset code indiscriminately or excuses `connection_failed`.
This application policy is not inferred by the pure protocol engine.
The no-action status-200 and status-413 full-upload probes remain mandatory for either policy.
