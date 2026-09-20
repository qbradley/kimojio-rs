# Independent HTTP/2 socket suites

These suites use python-h2 4.3.0 and Go `x/net/http2` Framer with HPACK.
They do not import the Rust engine.
The [adapter contract](ADAPTER.md) defines the native fixture interface.
The Python server omits its library's default ENABLE_PUSH setting.
RFC 9113 prohibits servers from sending that setting, including a zero value.
Each peer advertises its requested stream window in its first SETTINGS frame.
It does not shrink that window during the handshake while DATA can be in flight.
This is a stable test setup, not a Python-h2 or production handshake requirement.

**Peer self-tests are not evidence that the Rust engine passes.**
Every report identifies its command-adapter run or its peer-only run.
Missing dependencies, missing adapters, unknown cases, and unsupported actions fail.
There are no capability skips.

## Prerequisites

Use Python 3.12 or later and Go 1.24 or later.
Install the pinned Python dependencies in a project-local virtual environment:

```sh
python3 -m venv target/http2-program/peers
target/http2-program/peers/bin/python -m pip install -r interop/http2/requirements.txt
```

Build the Go peer:

```sh
mkdir -p target/http2-program/build-interop/go-work
export GOTMPDIR="$PWD/target/http2-program/build-interop/go-work"
export GOMAXPROCS=8
(cd interop/http2/gopeer && taskset -c 8-31 go build -o ../../../target/http2-program/build-interop/go-peer .)
```

The commands assume the repository root is the working directory.
In a detached worktree, the dependency interpreter can use an absolute path.

## Run the peer qualification

```sh
export PYTHONDONTWRITEBYTECODE=1
PYTHON=target/http2-program/peers/bin/python
taskset -c 8-31 "$PYTHON" -m unittest discover -s interop/http2 -p 'test_*.py' -v
(cd interop/http2/gopeer && taskset -c 8-31 go test ./...)
taskset -c 8-31 "$PYTHON" interop/http2/suite.py --selftest \
  --output target/http2-program/reports/flow
taskset -c 8-31 "$PYTHON" interop/http2/protocol_suite.py --selftest \
  --go-peer target/http2-program/build-interop/go-peer \
  --output target/http2-program/reports/protocol
```

Use a new output directory for each flow run.
Existing case directories fail rather than mix old and new evidence.
Protocol reports replace files only within their selected output directory.

## Run native adapters

Create client and server adapter JSON files under `target`.
Each file contains the command array from `ADAPTER.md`.
The native binary must exist before the suite starts.

```sh
taskset -c 8-31 "$PYTHON" interop/http2/suite.py \
  --client-adapter target/http2-program/client.json \
  --server-adapter target/http2-program/server.json \
  --output target/http2-program/reports/native-flow
taskset -c 8-31 "$PYTHON" interop/http2/protocol_suite.py \
  --client-adapter target/http2-program/client.json \
  --server-adapter target/http2-program/server.json \
  --go-peer target/http2-program/build-interop/go-peer \
  --output target/http2-program/reports/native-protocol
```

Either command accepts one adapter instead of both.
`--case NAME` selects a concrete case.
Repeated `--case` arguments select several cases.
A selection without a matching adapter role fails.
Reports contain only cases that the command actually ran.
Every final result requires explicit stream and connection terminal outcomes.
Normal success requires `stream.outcome == "complete"` and a `graceful` or `peer_closed` connection outcome.
`ended: true` records receive completion only.
It cannot hide a failed producer, incomplete retirement, transport failure, resource failure, or unprocessed request.
The `error` field contains only actual HTTP/2 wire codes.
Null wire errors do not make a failed terminal outcome successful.

## Diagnose an early successful response

This separate probe sends status 200 and response END_STREAM before the upload ends.
`--status 413` exercises the same behavior without an application cancellation action.
The independent server keeps its socket open and continues to consume the upload.
It returns stream and connection credit and never sends RST_STREAM or GOAWAY.

```sh
taskset -c 8-31 "$PYTHON" interop/http2/duplex_probe.py \
  --client-adapter target/http2-program/client.json \
  --output target/http2-program/reports/early-success
```

The report contains the bounded fixture trace, peer events, credit totals, reset frames, EOF, and the adapter result.
Its classification distinguishes peer errors, native resets, and native failure while the peer remains open.
The last classification does not, by itself, distinguish engine policy from fixture policy.
`--selftest` uses the independent reference client.
`--withhold-credit` is an explicit failing control, not successful interoperability evidence.
It stops the reference upload at exactly 65,535 bytes.

The main protocol suite's early-413 case differs from this probe.
That case withholds upload credit deliberately.
After the response END_STREAM, the peer sends a PING.
After its acknowledgment, the peer sends RST_STREAM(NO_ERROR) without closing the socket.
The protocol engine must not infer upload cancellation from END_STREAM or the status code alone.
This case requires the observed server reset, stream outcome `reset`, actual wire code 0, and preserved receive END_STREAM.
Its sibling must retire as `complete`, and the connection must close normally.
Global `connection_failed` outcomes cannot satisfy this stream-local termination.

For a fixture with a declared application-cancellation policy, `--early-response-policy application-cancel` selects a separate case contract.
The request explicitly selects stream 1 with the `cancel_upload_after_response` action.
The fixture must not cancel an upload based on status 413 alone.
The peer then sends neither a reset nor a barrier.
The oracle requires actual client CANCEL8, preserved response END_STREAM, `reset` retirement, successful sibling completion, and graceful connection closure.
The selected policy appears in the report.

The default `peer-reset` oracle still requires the server barrier and NO_ERROR reset, with no client reset.
Neither mode accepts `connection_failed` or ignores the actual wire reset code.
The no-action duplex probes require full uploads for both status 200 and status 413.

The reset-discard case keeps the sibling response open until the peer observes at least 33,792 bytes of connection refunds.
The connection must refund both the initial DATA and the late discarded DATA before graceful close.

## Explicit wrapper qualification profile

Both suites accept `--profile wrapper`.
The default remains `--profile canonical`, with unchanged cold-start and exact-list trailer oracles.
The wrapper profile needs the same client and server command adapters.
It adds no fixture action, readiness API, raw-frame observer, or timing delay.

```sh
python interop/http2/suite.py --profile wrapper \
  --client-adapter client.json --server-adapter server.json \
  --output target/wrapper-flow
python interop/http2/protocol_suite.py --profile wrapper \
  --client-adapter client.json --server-adapter server.json \
  --go-peer /absolute/path/to/go-peer --output target/wrapper-protocol
```

The profile runs 48 flow cells and 17 protocol cells.
For independent-peer validation, `--selftest` replaces both adapter arguments.
Those results remain peer selftests, not native wrapper qualification.

Coverage differs from the canonical profile in these ways:

* Reduced-window client uploads prepend one bodyless GET per concurrency slot.
  The peer completes these warmups only after its SETTINGS/PING barrier.
  Target uploads therefore start with the actual reduced window already applied.
* The early-response case prepends two warmups, with concurrency two.
  Each target request waits for a completed warmup to free its application slot.
  Stream 5 sends exactly 1,024 bytes, receives the complete 413 response, then receives peer RST0.
  Stream 7 remains the successful sibling.
* Trailer comparisons use case-insensitive names and ordered occurrences for each name.
  The client-response trailer case sends interleaved `x-list: one`, `x-other: marker`, and `x-list: two`.
  Different-name regrouping is permitted, but value sorting, combining, deletion, or duplication fails.
  Native-server routes retain their declared trailer contents.

Every warmup requires an exact empty-body hash and successful retirement.
Target hashes use their actual stream IDs, not renumbered IDs.
Credit equations retain all wire traffic, while workload-size checks exclude zero-body warmup streams.
The early-response peer returns no upload credit and rejects any post-barrier DATA beyond the 1,024-byte stream window.
Its witness includes the partial upload hash, both remaining credit balances, actual RST0, receive completion, and terminal outcomes.

Reports identify the selected profile, warmup count, startup method, and trailer comparison.
Synchronized cases report `canonical_cold_start: false`.
They never replace the separately identified cold-start diagnostics.
The no-action 200/413 duplex probes remain unchanged and still require full uploads.

## Focused wrapper startup diagnostics

`wrapper_startup_probe.py` diagnoses reduced-upload and early-response startup failures.
It does not run or qualify the full native suite.

```sh
python interop/http2/wrapper_startup_probe.py \
  --binary /absolute/path/to/frozen-wrapper-client \
  --go-peer /absolute/path/to/go-peer \
  --output target/wrapper-startup-diagnostic
```

The echo variants retain the strict initial window, withhold server bytes until DATA arrives, update the default window, or add a bodyless warmup.
The warmup response follows the peer's SETTINGS/PING barrier.
Application concurrency one then starts the upload after synchronization, without a fixture action or production frame parser.
The window-update variant uses public Python-h2 settings and consumption APIs.
Neither successful alternative qualifies the original cold-start scenario.

The strict Go early-response case remains unchanged.
A separate diagnostic permits bounded in-flight DATA under the original 65,535-byte allowance and still sends the explicit RST0 after its response barrier.
It never returns upload credit or treats a missing terminal outcome as success.
`H2_PEER_TRACE=1` records at most 128 incoming frame descriptions.
The Python recorder limits each direction to 4 MiB, 20,000 total frames, and 128 recorded descriptions.

[RFC 9113 sections 6.9.2–6.9.3](https://www.rfc-editor.org/rfc/rfc9113.html#section-6.9.2) permit default-window DATA before the client receives SETTINGS.
A reduced stream window can create negative credit.
The receiver can retain the in-flight bytes or reset the affected stream with FLOW_CONTROL_ERROR.
A test that requires a cold first upload to respect an unapplied 1,024-byte advertisement has an additional synchronization requirement.
These diagnostics do not establish a need for a public wrapper readiness API.

[RFC 9110 section 5.3](https://www.rfc-editor.org/rfc/rfc9110.html#section-5.3) makes same-name field value order significant, but not order across different names.
`trailer_values` compares case-insensitive names while retaining each name's exact occurrence sequence.
Its controls reject missing values and reversed same-name values.
This assessment does not change the canonical suite's exact-list trailer oracle.
Global wire-order fidelity is a separate contract from HTTP trailer semantics.

## Implemented flow and socket checks

The complete flow command runs 48 cases across these roles:

* Independent server against the command-driven client.
* Independent server echoes accepted client-upload fragments before request END_STREAM.
* Independent client downloads from the command-driven server.
* Independent client uploads to the command-driven server, which echoes each accepted fragment.

Nine workloads exercise receive credit in each role:

| Receive configuration | Single body | Serial aggregate | Eight concurrent streams |
|---|---:|---:|---:|
| Stream 1,024, connection 65,535 | 131,087 bytes | 320 × 512 bytes | 320 × 512 bytes |
| Stream 65,535, connection 65,535 | 131,087 bytes | 600 × 32,768 bytes | 600 × 32,768 bytes |
| Native defaults, without overrides | 16 MiB + 17 bytes | 600 × 32,768 bytes | 600 × 32,768 bytes |

The sender records actual SETTINGS and connection credit before its first DATA.
The fixture upload gate avoids the startup-window-reduction race permitted by RFC 9113 section 6.9.3.
An ungated client can legally send DATA under its current window before it receives server SETTINGS.
A later reduction can permit a stream FLOW_CONTROL_ERROR reset without a connection error.
The gate does not prescribe raw-frame parsing in production wrappers.
The single body must reach twice the larger actual balance plus 17 bytes.
Each aggregate body must be smaller than its actual stream window.
The aggregate total must exceed twice the actual connection window.
Default cases fail if either actual initial window exceeds 8 MiB.

`CreditSender` obeys both flow-control balances.
It sends one frame per ready stream in each bounded round.
It never bypasses flow control to make a workload finish.
The socket trace separately counts DATA payload, padding, and every WINDOW_UPDATE increment.
The report records initial credit, increments, DATA flow bytes, and final credit.
The connection equation must match the independent library's actual balance.
Closed streams can receive late updates that python-h2 ignores in its retired stream object.
The wire equation still includes those increments.
Queued refunds after peer EOF appear as `pending_connection_window_update`.
They do not count as transmitted WINDOW_UPDATE increments.
Python-h2 discards its queued output when it receives GOAWAY.
Generated refunds that this operation discards appear separately as `goaway_discarded_connection_window_update`.
They never count as transmitted credit, and missing refunds without GOAWAY fail the accounting check.

Every successful stream needs an exact byte count, SHA-256, status, declared content length, trailers, informational sequence, and END_STREAM.
HEAD must report its declared content length despite its empty body.
Bodies use the repeated byte `stream_id % 251`.
The peers calculate hashes incrementally without retaining complete bodies.
Serial aggregates exercise connection-credit refunds after END_STREAM.

The echo server sends response headers when request headers arrive.
It retains accepted body fragments only until the corresponding socket writes complete.
It then returns receive credit.
Its retained fragments cannot exceed its initial connection window or 4,096 items.
Independent-server reports contain fragment high-water counts.
The upload client stops after its first fragment until echo DATA arrives.
Thus, a server that waits for request END_STREAM cannot pass through a whole-body buffer.

Additional cases exercise:

* Response trailers and status 103 before the final response.
* HEAD and status 204 without body payload.
* Padded DATA, with padding included only in flow-control accounting.
* A paused consumer on stream 1 while stream 3 completes.
* 4,096 empty DATA frames on the paused stream while its sibling progresses.

Padding and empty-frame injection run only from the independent server.
The native server routes do not promise those exact frame layouts.
The empty-frame case proves bounded peer traffic and sibling progress.
It does **not** measure native fragment-descriptor capacity.
It permits full delivery or stream-local ENHANCE_YOUR_CALM on stream 1 with zero delivered body bytes.
The latter outcome requires the matching wire RST_STREAM, successful sibling 3, and no connection error.
It also requires a `reset` terminal outcome, not a hidden `connection_failed` outcome.
Reports distinguish `bounded-stream-rejection` from complete delivery.

## Implemented protocol checks

The complete protocol command runs 17 cases.
The Go peer avoids python-h2 limitations around classic CONNECT, pre-ACK push, and DATA after GOAWAY.

Against a command-driven client, it exercises:

* Classic CONNECT with tunnel DATA and exact pseudoheader rules.
* Active-stream completion after GOAWAY(NO_ERROR).
* Fragmented response HEADERS with a valid CONTINUATION.
* An interleaved PING during CONTINUATION, which requires connection PROTOCOL_ERROR.
* Disabled push before acknowledgment, including rejection and shared HPACK history.
* Disabled push after acknowledgment, which requires connection PROTOCOL_ERROR.
* RST_STREAM isolation while a sibling completes.
* Short content length, which requires a stream error while a sibling completes.
* Illegal DATA after status 204, which requires a stream error while a sibling completes.
* An early response while a 1,024-byte upload window blocks its producer.
* Late DATA after reset, with connection refunds and no new stream refunds.
* GOAWAY(NO_ERROR) before an explicit graceful socket close.
* Queued admission after `SETTINGS_MAX_CONCURRENT_STREAMS` changes from zero to one.

The `admission-recovery` case uses two bodyless requests with application concurrency one.
After request 1, the Go peer advertises a zero stream limit and sends a PING.
After its acknowledgment, the peer completes response 1 and sends a second PING while the limit remains zero.
Only after the second acknowledgment does the peer advertise a limit of one.
Any new request HEADERS before that advertisement fail the case.
Both requests must finish with status 200, zero response bytes, and `complete` retirement.

The witness records the exact order of both barriers, both advertisements, and both requests.
Request 2 must follow the positive SETTINGS acknowledgment, excluding HEADERS queued before the client receives the new limit.
The case saves bounded fixture trace output separately from its required wire evidence.
Independent Go socket controls reject premature HEADERS before either barrier.
Another control sends HEADERS after the second PING acknowledgment but before the positive SETTINGS acknowledgment.
They also reject a client that drops the queued request after admission reopens.

The selftest pairs this Go server with the Python-h2 client, which respects the current remote stream limit.

The late-DATA case deliberately sends frames after reset.
It retains the credit limits that existed before reset.
It is a discard scenario, not a substitute for compliant body-flow tests.

Against a command-driven server, the Go client exercises:

* Classic CONNECT.
* Fragmented request HEADERS with CONTINUATION.
* Shared HPACK history across two concurrent requests.
* Request trailers after a streaming upload.

The request-trailer case proves acceptance and complete body echo.
The echo route does not expose request-trailer values to the test.
The independent CONNECT client requires echo DATA for a non-final prefix before it sends the remaining tunnel DATA.

## Fault controls and bounds

The Python tests include no-consumption and no-credit server controls.
One socket control stops at exactly 1,024 payload bytes.
Another stops at exactly 65,535 bytes across eight small uploads.
Both controls fail by deadline before a response completes.
Another control rejects a server that waits for the complete upload before it sends echo DATA.
A socket test requires response headers before the first request DATA.
It then requires echo DATA before request END_STREAM.
An ownership test withholds input credit until the echo write settles.
Outcome controls reject `ended: true` and null wire errors when retirement or connection outcomes indicate failure.
The tests also reject workloads that are too small for 8 MiB windows.
Separate socket runs qualify both 8 MiB balances with large bodies and serial/concurrent aggregates.

Go tests reject a DATA write that exceeds cumulative stream or connection credit.
Another negative control fails HPACK decoding without the promised header block.
The valid path decodes that block before the indexed response.

The peers reuse `interop/http1` process ownership and scripted-listener helpers.
Python sockets use a 60-second absolute deadline, not a deadline per read.
Commands get a five-second cleanup allowance.
Each direction permits at most 128 MiB of wire data and 300,000 frames.
The write queue is at most 2 MiB.
The trace retains counters and one incomplete frame, not full transcripts.
JSON files are at most 2 MiB each.
Process output retention is at most 128 KiB, and output overflow fails the run.
The Go protocol peer uses a ten-second deadline, 16 MiB per direction, and 100,000 frames.
Its client upload producer is limited to 131,087 bytes.
It obeys both credit balances and continues after response END_STREAM unless an explicit action or peer reset stops it.
Go tests exercise early status 200 and 413 without a cancellation action.
Context managers close sockets and stop owned processes and threads on success or failure.

## Remaining qualification gates

Native-core and wrapper qualification require the parent-owned fixture binary and actual adapter runs.
No peer-only result substitutes for those runs.
The current cases do not prove internal descriptor bounds, operation ownership, ID exhaustion, or retry classification after GOAWAY.
Malformed-request error scope and late reset DATA at the server boundary need additional cases.
Full HPACK resource-limit and eviction qualification belongs to the codec tests.
These omissions are not capability skips or passing matrix entries.
