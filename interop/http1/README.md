# Independent HTTP/1 peers

This directory contains correctness tools, not production protocol code.
Python and Go use only their standard libraries.
The tools require Linux, Python 3.11 or later, and Go 1.21 or later.
Missing required tools cause failures, not skipped tests.

## Run the harness self-tests

From the workspace root, run:

```sh
python3 -B -m unittest discover -s interop/http1 -p 'test_*.py' -v
```

The tests build `target/interop/http1-peer` without network dependencies.
Go caches and build scratch files stay under `target/interop/`.
The `-B` flag prevents Python bytecode files outside that directory.

The self-tests cover:

- Every two-part input split for the JSON framing corpus.
- Bytewise input, informational responses, trailers, and exact response sequences.
- EOF at every position in fixed-length and chunked response fixtures.
- HEAD, 204, 304, upgrades, and CONNECT ownership of trailing bytes.
- Invalid lengths, chunk syntax, headers, and bounded input.
- Child readiness, startup failure, timeouts, output limits, and forced cleanup.
- Static responses from Go, Python clients, and Go clients.
- Go echo requests with lengths, chunks, trailers, and `100-continue`.
- An early final response before an expected upload.
- A Go client against a scripted Python server with two responses on one connection.

These tests establish harness behavior against independent peers.
They do not establish correctness of the new Rust binaries.

## Run a static-server suite

After the self-tests build the Go peer, run the reference suite:

```sh
python3 -B interop/http1/server_suite.py \
  --profile portable \
  --go-peer target/interop/http1-peer \
  -- target/interop/http1-peer --bind '{bind}' --root '{root}'
```

For each new static-server binary, use the strict profile:

```sh
python3 -B interop/http1/server_suite.py \
  --profile strict \
  --go-peer target/interop/http1-peer \
  -- /absolute/path/to/server --bind '{bind}' --root '{root}'
```

A JSON argument array is an alternative to the arguments after `--`:

```sh
python3 -B interop/http1/server_suite.py \
  --server-command '["/absolute/path/to/server","--bind","{bind}","--root","{root}"]'
```

The harness replaces `{bind}` with `127.0.0.1:0`.
It replaces `{root}` with an absolute fixture path under `target/interop/`.
It does not interpret a shell command.
Relative command paths resolve from the workspace root.

The server must bind the address and then flush this line to standard output:

```text
LISTEN 127.0.0.1:PORT
```

The port must be the actual nonzero listener port.
Diagnostic output can use either output stream.
The harness drains both streams and keeps bounded diagnostics.

The harness owns the server process group.
It terminates the group and reaps the server after success or failure.
Startup has a five-second deadline.
Each raw connection has a three-second deadline.
Forced process cleanup has bounded waits.

Each run creates a fixture directory and `results.json` under `target/interop/run-ID/`.
The report includes the command, profile, peers, fixture hash, and individual results.
The command returns a nonzero status for a failed case or startup failure.
The fixture directory remains available for failure analysis.
The self-tests delete their own fixture directories.

## Server contracts

The portable profile has 27 cases.
The strict profile adds six policy cases.
The optional `--go-peer` adds two independent client cases.
The report lists the selected peers explicitly.
An invalid supplied Go peer path fails those cases.

Both profiles require these static-server behaviors:

- GET returns exact binary file content and its `Content-Length`.
- HEAD returns the same length without body bytes.
- Empty files, nested paths, and percent-encoded spaces work.
- Missing files return 404.
- Sequential and pipelined requests retain response order.
- GET bodies with lengths, chunks, and trailers do not corrupt the next request.
- `Connection: close` and HTTP/1.0 without persistence end the connection.
- Response versions match request versions, and HTTP/1.0 responses do not use transfer coding.
- Unsupported HTTP/1.1 expectations receive 417 without an upload.
- Malformed request syntax and invalid lengths produce an exact 400 response.
- Rejected malformed requests do not permit another response on that connection.
- Parent-directory paths cannot expose the synthetic file outside the root.

Traversal cases permit redirects or rejection.
The clients do not follow redirects.
Normal close cases require EOF without extra bytes.
Malformed-request cases also permit a reset after the complete error response.

The strict policy additionally requires:

- Rejection of `Transfer-Encoding` together with `Content-Length`.
- Rejection of folded header fields.
- Rejection of bare-LF request headers.
- Rejection of bare-LF and bare-CR prefixes before complete headers or peer EOF.
- Rejection of symlinks to files outside the root.

These are explicit application policies, not claims that every HTTP server must reject those forms.
Go accepts the three complete HTTP forms, and its file server follows symlinks.
Go also waits for more bytes on the two incomplete line prefixes.
The self-tests require the strict suite to detect those six differences.
A portable Go result is not a strict correctness result.

The prefix cases send no valid pipeline suffix and do not half-close the socket.
They require a complete 400 response and termination within one second.
A 408 timeout response does not count as malformed-request rejection.
The other malformed cases retain their pipeline suffix to detect unintended reuse.
The v2 contract added exact 400 and eager-prefix checks.
The current `application-boundary-v3` contract also checks response versions and automatic 417 responses.
The unsupported-Expect probe requests `Connection: close` and withholds the body.
The complete strict suite has 35 cases with the Go peer enabled.

Static applications can choose different request-body or path policies.
Such differences require an explicit suite contract change, not silent case removal.

## Static transport recovery

The transport suite owns one server process and uses the same synthetic file fixtures:

```sh
python3 -B interop/http1/static_transport_suite.py \
  --server-command '["/absolute/path/to/http1-static","--bind","{bind}","--root","{root}","--max-connections","1","--timeout-ms","1000"]'
```

It repeats three reset cases four times:

- A reset during incomplete headers.
- A reset during an incomplete request body.
- A reset after a response starts, with an 8 MiB file and a constrained receive window.

Each reset must permit a subsequent exact file response within three seconds.
The suite samples descriptors only for its own child process.
Each sample must stay within two descriptors of the warm baseline.
The report retains exact counts instead of claiming exhaustive leak detection.

A final case requires a complete 408 response for an incomplete request that remains open.
The example command sets a one-second target timeout.
The peer permits three seconds for that response and subsequent termination.
The complete suite has 13 cases.

The explicit `--without-timeout` profile selects only the 12 reset/recovery cases.
Go reference self-tests use that profile because Go does not promise the same 408 policy.
They also use a separate peer with `--max-connections 1` to match the target admission bound.
The Go peer disables this admission limit by default.
The report names the selected profile.
These are correctness probes, not throughput benchmarks or deterministic kernel-receipt injection.

## Wrapper fixture routes

The conventional wrapper example is not a static-file server.
It has no `--root` argument.
Its separate suite uses the documented fixture routes:

```sh
python3 -B interop/http1/fixture_server_suite.py \
  --server-command '["target/debug/examples/server","--bind","{bind}","--connections","32"]' \
  --go-peer target/interop/http1-peer
```

The suite has 18 raw-wire cases and two optional Go cases.
It checks `/`, `/echo`, `/trailers`, `/early`, and `/bytes/N`.
It checks exact response bodies, response trailers, reuse, pipelining, request framing, and Expect decisions.
Malformed-request cases require an exact 400 response before termination.
The two incomplete-prefix cases use the same one-second, no-EOF contract.
HTTP/1.0 version selection and unsupported-Expect rejection are explicit v3 cases.
The `/early` fixture must return 413 without first issuing 100.
That requirement is an explicit application policy, not a ban on every 100 followed by 413.

The Rust client and server examples also accept `--native` for the exact one-shot descriptor backend.
The `kimojio_adapter.py` command accepts the same flag and passes it to the client.
Without that flag, the examples retain the generic stream backend.

The server's `/duplex` route selects explicit reusable forwarding.
The existing `/echo` and `/early` policies do not change.
The duplex suite can require three gated exchanges on one socket:

```sh
python3 -B interop/http1/duplex_suite.py --path /duplex --reuse \
  --server-command '["target/debug/examples/server","--native","--bind","{bind}"]'
```

Each exchange must return its head before the upload starts.
Each uploaded fragment must produce matching output before the next fragment arrives.
The suite covers fixed-length and chunked requests, with and without `Expect: 100-continue`.
It requires exact trailers and permits EOF only after the final exchange.

Each report preserves base64 request intents and received bytes.
A send intent does not prove that the peer accepted every byte.
A failed fragmented send records an error instead of claiming complete delivery.
Connection failures remain individual report entries.
They do not discard results from earlier cases.

## Reusable helpers

`harness.py` provides:

- `PeerProcess(command, cwd=...)` for bounded readiness and process ownership.
- `run_command(command, cwd=..., timeout=...)` for arbitrary client commands.
- `Wire` for bounded socket operations and strict response framing.
- `Response.wire` for the exact consumed response bytes.
- `Wire.buffer` for bytes that belong to the next message or upgraded protocol.

`scripted.py` provides `ScriptedPeer(handler, connections=..., timeout=...)`.
It binds an ephemeral loopback port.
The handler receives a `Wire` connection and its connection index.
Worker failures propagate to the caller.
Context exit closes active sockets and joins the worker.

The response oracle supports content lengths, plain `chunked`, and close-delimited bodies.
It rejects other transfer codings rather than decoding them.
It bounds heads at 64 KiB, decoded bodies at 8 MiB, and wire messages at 16 MiB.
Its length representation has a 20-digit limit.
It is a bounded test oracle, not a replacement HTTP implementation.

`framing_cases.json` contains transport-independent fixtures.
Each `wire` string maps to bytes through Latin-1, not UTF-8.
Accepted fixtures give the status, body, trailers, and unconsumed suffix.
Error fixtures require rejection.
The `requires_eof` field marks close-delimited completion.

The Go server also exposes:

| Path | Behavior |
| --- | --- |
| `/__peer/echo` | Read up to 8 MiB, then return the exact body |
| `/__peer/chunked` | Return three flushed chunks and `X-Peer-End: done` |
| `/__peer/reject` | Return 417 without an expected upload |
| `/__peer/no-content` | Return 204 |
| `/__peer/duplex` | Flush a response head, then echo request fragments before request completion |

The Go client supports GET, HEAD, known-length uploads, and chunked uploads.
Its `--body-file` argument preserves arbitrary request bytes.
It emits one JSON line per completed request.
Each line includes an index, status, headers, and a base64 body.
The line also includes informational status codes and response trailers.
It disables redirects, proxy discovery, and automatic compression.
Its target must use `http://127.0.0.1`.

## Run the independent client suite

After the self-tests build the Go peer, run:

```sh
python3 -B interop/http1/client_suite.py \
  --adapter interop/http1/go-adapter.json
```

This command runs 16 client cases against scripted Python servers.
Each case has a separate loopback listener and an eight-second outer deadline.
The cases cover lengths, chunks, interim responses, HEAD, 204, 304, EOF, malformed responses, reuse, uploads, and Expect.
The peer checks exact request lines, body bytes, message counts, and connection reuse.

An adapter manifest supplies a command array and explicit capabilities:

```json
{
  "schema": 1,
  "command": ["actual-client-bridge", "{request_file}", "{result_file}"],
  "capabilities": ["basic", "reuse", "upload", "chunked-upload", "interim", "trailers", "expect-continue"]
}
```

The harness writes each request file under `target/interop/client-ID/`.
It replaces the two placeholders with absolute paths.
The command adapter maps this protocol to the actual client CLI.
The protocol does not prescribe the production CLI.

The request file contains:

```json
{
  "schema": 1,
  "url": "http://127.0.0.1:PORT/fixture",
  "method": "GET",
  "headers": [],
  "body_base64": "",
  "body_mode": "known-length",
  "count": 2,
  "connection": "reuse",
  "timeout_ms": 6000
}
```

`body_mode` is `known-length` or `chunked`.
`connection` is `single` or `reuse`.
Reuse means that all requests use one TCP connection.
Expect cases supply `headers: [["expect", "100-continue"]]`.
The other current fixtures supply no extra request headers.

The adapter writes a result file:

```json
{
  "schema": 1,
  "exchanges": [
    {
      "index": 0,
      "status": 200,
      "body_base64": "b25ldHdv",
      "interim_statuses": [],
      "trailers": {}
    }
  ],
  "failure": null
}
```

Trailer values use arrays, such as `{"X-End": ["done"]}`.
The `interim` and `trailers` capabilities enable exact checks for those observations.
Without those capabilities, the report lists those observations as uncovered.

A protocol failure uses `failure: {"kind": "protocol", "detail": "description"}`.
Other failure kinds are `io`, `rejected`, and `timeout`.
Malformed-response cases accept the first three kinds, but never a timeout.
An adapter returns zero after it writes a valid outcome, including an expected protocol failure.
A nonzero exit means that the adapter itself failed.

The required `basic` capability selects 11 cases.
The `reuse`, `upload`, and `chunked-upload` capabilities each add one case.
The `strict-framing` capability adds rejection of a response with both TE and CL.
The `expect-continue` capability adds two cases.
One supplies 100 before the upload.
The other rejects the expectation with `Connection: close` and requires no upload bytes.
The Go reference omits that capability because Go accepts that response form.
Reports name absent capabilities rather than treating them as passed coverage.

### Native Kimojio client bridge

`kimojio_adapter.py` maps the protocol to the example client.
Its manifest is `kimojio-adapter.json`.
The manifest uses `target/debug/examples/client` after workspace integration.

Before use, make sure that the binary supplies these machine-mode arguments:

```text
--connect ADDRESS --method METHOD --path PATH
--body-file FILE --result-file FILE --count N
--chunked --expect-continue
```

The last two arguments are conditional.
The bridge does not parse the ambiguous human-readable stdout format.
It reads one native NDJSON record per completed response:

```json
{"status":200,"body_base64":"b25ldHdv","headers":[],"trailers":[["X-End","ZG9uZQ=="]],"error":null}
```

Field values use base64 to preserve arbitrary bytes.
The bridge preserves repeated fields.
It supplies exchange indexes from record order.

A native failure record is `{"error":"description"}`, with a nonzero process exit.
The bridge rejects missing results, malformed records, mismatched exits, signals, and Rust panic exits.
Those failures cannot count as expected protocol rejection.
The normalization self-tests use synthetic records, not a Rust binary.

After the owner confirms a compatible binary, run:

```sh
python3 -B interop/http1/client_suite.py \
  --adapter interop/http1/kimojio-adapter.json
```

This manifest requests strict response framing and Expect coverage.
It does not request interim-event observations because the native record lacks that field.

## Independent duplex progress

The ordinary echo cases send the complete request before they read the response.
They do not establish full duplex behavior.
The separate duplex suite requires progress before request completion:

```sh
python3 -B interop/http1/duplex_suite.py \
  --server-command '["target/debug/examples/server","--bind","{bind}","--connections","4"]' \
  --client-adapter interop/http1/kimojio-adapter.json
```

Either target argument can run alone.
The server path defaults to `/echo`.
The Go reference path is `/__peer/duplex`.
The reference uses `http.ResponseController.EnableFullDuplex` and explicit flushes.

The four server cases use known-length and chunked bodies, with and without Expect.
The peer sends only headers, then waits for a 200 response head.
Expect cases require 100 before that 200 head.
It sends the first three body bytes, then waits for three response bytes.
Only then does it send the remaining request bytes.
The chunked case also requires request trailers to reach the response.
A server that collects the request before its response cannot pass these barriers.

The four client cases use an 8 MiB binary upload.
The independent peer sends a 200 response head and body prefix before it reads the complete upload.
Expect cases send 100 immediately before the early 200.
The already-authorized upload must continue after that successful final head.
It constrains the receive window to limit queued request data.
Then it requires the complete, exact upload before it sends the final response chunk and trailer.
The client must continue the upload after the successful early response head.
Rejecting early responses remain separate cases with different expectations.

The result includes verified request-byte counts and exact final response checks.
The body prefix reader has deterministic fragmentation tests.
The eight positive reference cases use Go on one side and Python on the other.
The controlled Go fixture explicitly emits 100 before its early final response for Expect cases.
Known pre-duplex Rust snapshots also serve as explicit negative controls during preparation.
Those control results do not describe a newer target binary.

The server barriers establish application-level progress ordering.
The client case is a bounded transport probe, not deterministic injection of a particular pending kernel receipt.
The client adapter must advertise `upload`, `chunked-upload`, and `expect-continue` capabilities.
All files remain under `target/interop/`.

## Required interface for the new client

No new-client flag names are assumed.
A command adapter must expose these actions and observations:

| Action | Required observation |
| --- | --- |
| Select URL, method, headers, and body | Final status, exact body, and trailers |
| Send a known-length or unknown-length body | Exact request bytes at the independent peer |
| Make two exchanges on one connection | Exchange order and connection identity |
| Send `Expect: 100-continue` | Interim responses and upload-release decisions |
| Receive a final response during upload | Upload completion or cancellation, without a deadlock |
| Cancel before or during I/O | Terminal exchange outcome and connection-reuse decision |
| Apply limits and deadlines | Named failure and bounded completion |
| Upgrade or establish a CONNECT tunnel | Exact ownership of protocol bytes after the response |

The process command can use any CLI that supplies these capabilities.
The adapter must return nonzero for unexpected failures.
It must provide structured results or exact output files under `target/interop/`.

## Remaining integration

The new server binaries and client adapter are not part of these self-tests.
Configured limits, cancellation, and stalled uploads need target-specific controls.
Static-file responses cannot expose every upload or expectation policy.
Those cases need an echo or fixture wrapper around the core.

TCP writes do not guarantee corresponding peer read boundaries.
Memory-socket fixtures establish deterministic parser fragmentation only.
Deterministic short writes and completion races need the core or driver test API.
The current socket suite does not prove cancellation safety or memory bounds in either driver.
Socket writes also cannot guarantee fully buffered core input or an occupied informational-output slot.
Core boundary tests cover conditional 100 suppression and final-response admission behind informational output.

WebSocket cases belong in `interop/websocket/` after the HTTP interface is stable.
Performance tools remain separate and start only after the correctness gate.
