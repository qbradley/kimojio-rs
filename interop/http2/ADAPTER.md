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
  "timeout_ms":6000,
  "config":{"stream_window":1024},
  "request_count":2,
  "concurrency":2,
  "requests":[
    {"method":"GET","path":"/bytes/131071","body_bytes":0,"trailers":[]},
    {"method":"GET","path":"/trailers/37","body_bytes":0,"trailers":[]}
  ],
  "actions":[]
}
```

The fixture uses one connection for all requests.
Request array order determines stream IDs: 1, 3, 5, and subsequent odd IDs.
`concurrency` limits active requests, not lifetime requests.
Upload bytes repeat the byte `stream_id % 251`.
The fixture must not retain the complete body.
`config.stream_window` controls the advertised initial receive window.
The initial connection window remains 65,535, unless the fixture explicitly reports another value.

The result file contains:

```json
{
  "schema":1,
  "streams":[
    {
      "stream_id":1,
      "status":200,
      "bytes":131071,
      "sha256":"lowercase hexadecimal SHA-256",
      "trailers":[],
      "informational":[],
      "ended":true,
      "error":null
    }
  ],
  "connection":{"error":null,"closed":true}
}
```

Every issued stream needs exactly one result.
`trailers` contains ordered string pairs, such as `[["x-end","done"]]`.
`informational` contains status integers, such as `[103]`.
`error` is null or `{"scope":"stream","code":1}`.
A connection error uses `{"scope":"connection","code":1}`.
Codes are HTTP/2 wire error codes.
`closed` means that the fixture explicitly closed its socket.
It does not mean that the fixture observed EOF from the server.
Successful commands return zero, including expected protocol-error scenarios.
Setup errors return nonzero.

## Server command

The server adapter file contains:

```json
{"schema":1,"command":["/absolute/fixture","server","--stream-window","{stream_window}"]}
```

The process binds an ephemeral loopback port.
It prints and flushes `LISTEN 127.0.0.1:PORT`.
The harness owns process termination after each case.
Each connection supports multiple requests.
Routes are:

| Route | Response |
|---|---|
| `/bytes/N` | Status 200, N copies of byte `stream_id % 251`, exact content length |
| `/echo` | Status 200, the exact request body, after request END_STREAM |
| `/trailers/N` | The same body as `/bytes/N`, followed by `x-end: done` |
| `/informational/N` | Status 103, then the same final response as `/bytes/N` |
| `/no-content` | Status 204 and no DATA payload |
| `/early` | Status 413 with END_STREAM before request END_STREAM |

HEAD returns the route's content length but no DATA payload.
Classic CONNECT uses `:method` and `:authority`, without `:scheme` or `:path`.
The fixture accepts CONNECT with status 200 and echoes tunnel DATA until END_STREAM.
The fixture consumes ordinary uploads as it receives them.

## Initial concrete cases

Both directions require a 131,071-byte body with stream windows 1,024 and 65,535.
The sender must first record the actual SETTINGS and connection credit.
A case fails if the body does not exceed both actual initial balances.
Senders obey both balances and every WINDOW_UPDATE increment.

Serial and concurrent cases use 160 bodies of 512 bytes.
Each body is smaller than the advertised stream window.
Their total exceeds the actual initial connection window.
Every body needs exact byte counts, SHA-256, END_STREAM, and expected trailers.
The peer records all WINDOW_UPDATE increments and final send balances.
Those values must satisfy the stream and connection credit equations.

The initial semantic cases cover trailers, informational responses, HEAD, and status 204.
Fault controls include a receiver that withholds consumption and a server that never returns upload credit.

## Follow-up action interface

These actions reserve explicit fixture behavior for later cases.
They are not claims of implemented test coverage.

* `{"action":"pause","stream_id":1,"until_stream_ended":3}` retains stream 1 body fragments until stream 3 ends.
* `{"action":"reset","stream_id":1,"after_bytes":1024,"code":8}` cancels stream 1 after that many response bytes.
* `{"action":"graceful_close","after_streams":2}` starts graceful shutdown after two completed streams.

The harness must name every executed case in its report.
Unsupported actions fail rather than produce a skip.
Further protocol cases can use fixed independent-peer scenarios without new fixture routes.
