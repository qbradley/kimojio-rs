# Independent WebSocket correctness checks

These files are test tools, not a production WebSocket implementation.
They do not depend on the new Rust FSM.
All generated reports belong under `target/interop/`.
No performance tests run here.

## Run the self-tests

From the workspace root, run:

```sh
python3 -B -m unittest discover -s interop/websocket -p 'test_*.py' -v
```

The complete suite requires Python, Go, and Node with a built-in `WebSocket`.
Node 24 supplies that API without npm packages.
The Go peer uses only standard-library packages.
Its self-tests build the executable and keep the Go cache under `target/interop/`.
A missing required peer fails its test instead of silently skipping it.

The tests cover:

- RFC 6455 handshake and masked-frame examples.
- Every two-part split of small wire fixtures.
- Length boundaries at 125, 126, 65535, and 65536 bytes.
- EOF at every position in a masked frame.
- Strict masking direction and RSV, opcode, length, and control rules.
- Text fragments across UTF-8 boundaries.
- Ping and pong between fragments.
- Close codes, close reasons, message limits, and exact upgrade leftovers.
- Raw clients against an independent Python broadcast reference.
- Node against a Python peer that fragments text and sends a ping.
- Go against Python peers with fragments, binary messages, ping, close, and an invalid masked server frame.
- Chat assertions against Python references, including concurrent publishers and invalid-message suppression.

The Node case checks masking, automatic pong, text reconstruction, and a clean close.
The Python reference runs both sender-included and sender-excluded broadcast policies.
These results establish harness behavior, not Rust target correctness.

## Run the command-driven suite

To exercise the harness against its bounded reference, run:

```sh
python3 -B interop/websocket/ws_suite.py \
  --server-command '["python3","-B","interop/websocket/reference.py"]'
```

For a chat target, use:

```sh
python3 -B interop/websocket/ws_suite.py \
  --server-command '["/absolute/path/to/chat-server","--bind","{bind}"]'
```

If the target excludes the sender from broadcasts, add `--exclude-sender`.
If the upgrade path differs from `/chat`, supply `--path`.

The harness replaces `{bind}` with `127.0.0.1:0`.
The target must flush `LISTEN 127.0.0.1:PORT` after it starts its listener.
The harness uses the shared HTTP process helper for bounded output and child cleanup.
Each raw WebSocket connection has a five-second deadline.
The Python reference also has a ten-second process lifetime by default.

The socket suite has 19 cases:

- Three-client broadcast with exact order and counts.
- Fragmented UTF-8 with an interleaved ping.
- A complete HTTP upgrade and first WebSocket frame in one write.
- Twelve invalid-frame or invalid-message cases.
- Four invalid HTTP handshake cases.

The invalid-message cases require close code 1002 or 1007 as specified by the fixture.
Normal close permits an empty close payload or status 1000.
Both paths require connection termination without extra bytes.
Invalid handshakes require 400, 405, or 426 followed by connection termination.

These response policies are explicit expectations for the generic chat suite.
They are not assertions that every WebSocket application must use identical failure responses.
The report identifies the selected sender policy.

## Run the native chat contract suite

First, run the self-tests to build the required Go peer.
Then run the application checks:

```sh
python3 -B interop/websocket/chat_suite.py \
  --server-command '["/absolute/path/to/websocket-chat"]' \
  --publication target/interop/chat-publication.json
```

The optional publication file contains the owner source mapping and `binary_sha256`.
The harness compares that hash with the executable before the run.
It also compares the executable hash before and after the run.
The report includes the publication file content and each actual command.

The command array is an executable prefix, without bind or limit flags.
The suite appends `--bind 127.0.0.1:0` and the configuration for each case.
Each case owns a separate bounded child process.
The target must support the published native chat CLI.
This suite requires sender-included broadcasts and the `/chat` path.
It does not replace the 19-case raw suite.

The native suite has 11 cases:

- Exact handshake failures: unsupported version returns 426 with `Sec-WebSocket-Version: 13`. The other three malformed handshakes return 400.
- Exact binary broadcasts at lengths 0, 1, 125, 126, 65535, and 65536. Empty text retains its text opcode.
- Invalid fragmented text closes with 1007. No partial or invalid message reaches another recipient.
- Two concurrent publishers produce one common order at all three recipients. Each publisher retains its own message order.
- Bytewise HTTP upgrade and frame writes retain the exact payload.
- The Node built-in client completes a UTF-8 exchange and a clean close.
- The Go peer completes text, 65,792-byte binary, ping, and close exchanges.
- A 1024-byte message succeeds at the configured limit. Single-frame and fragmented overflows close with 1009.
- Six resets cover partial HTTP handshakes and partial WebSocket frames. Each reset precedes a healthy exchange and a descriptor sample.
- A slow recipient closes with 1008. Two healthy recipients receive all twelve exact messages.
- Timed shutdown sends a normal close and exits successfully within the process deadline.

The slow-recipient case selects a two-message queue and a 4096-byte send buffer.
It uses a five-second close timeout so the small receive window can drain before forced termination.
The close code assertion remains exactly 1008.
A one-second close timeout can terminate a blocked frame before the peer receives a complete close frame.
The report records this configuration instead of claiming that every blocked transport completes a WebSocket close.

The reset case permits at most two descriptors beyond the warm baseline.
Descriptor samples do not prove the absence of every leak.
Socket writes and receive windows do not force exact kernel completion order.
The suite does not inject native cancellation races or measure process memory.

## Files and scope

`ws_wire.py` provides handshake checks, frame encoding, bounded frame decoding, and message assertions.
It does not negotiate extensions, compression, or subprotocols.
It preserves bytes after the HTTP headers.
It limits frame and message payloads to 1 MiB by default.

`frame_cases.json` contains language-neutral wire fixtures.
The `hex` field is the complete frame or deliberately invalid prefix.
Accepted fixtures specify their opcode, payload, FIN bit, and masking direction.
Rejected fixtures specify their protocol close code.
The RFC examples independently constrain the encoder and decoder.

`reference.py` is a Python-only broadcast reference for harness self-tests.
Its threads, sockets, message sizes, and accepted connection count have limits.
Context exit closes its sockets and joins its threads.
It is not an application server or a performance reference.

`node_peer.js` uses only the Node built-in WebSocket API.
It supports one positive echo exchange followed by a close handshake.
Its own deadline is five seconds.

`peer.go` supplies a separate Go wire encoder and decoder.
It uses standard HTTP parsing for the upgrade and retains buffered bytes.
Its deadline is five seconds.
Its limits are 64 KiB for the upgrade read, 1 MiB per frame or message, and 128 frames per message.
Its data reader also has a 4 MiB limit after the upgrade.
`test_go_peer.py` builds it as `target/interop/websocket-peer`.
The native suite accepts another path through `--go-peer`.
Missing Node or Go peers fail the selected case instead of silently skipping it.

Native cancellation and write-completion races still require driver controls.
TCP chunking does not prove deterministic short reads or short writes.
The memory fixtures establish deterministic parser boundaries only.
