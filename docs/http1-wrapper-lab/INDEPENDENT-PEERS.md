# Independent wrapper peers

## Prerequisite result

Both wrapper backends passed 45 independent cases each.
These results precede production optimization.
They establish the behavior that the selected implementation must retain.

The accepted implementation subsequently passed 180 cases across both backends and both coalescing policies.
That repeat uses source `5068d6582b18db490192a16041e6a3d465b9c5d6`.
The [accepted publication](evidence/accepted-independent-peers.json) retains its exact commands, binary hashes, and per-case outcomes.
The prerequisite results in this document remain a separate historical record.

| Suite | Generic stream | Native descriptor |
| --- | ---: | ---: |
| Server fixture, including Go clients | 20/20 | 20/20 |
| Scripted client framing and reuse | 17/17 | 17/17 |
| Large upload after early response headers | 4/4 | 4/4 |
| Gated duplex exchanges on one socket | 4/4 | 4/4 |

Each reusable duplex case sends three different requests through one connection.
The server must return headers before the first upload fragment.
Each fragment must produce matching response bytes before the next fragment arrives.
The cases cover fixed-length and chunked input, both with and without `Expect: 100-continue`.
Chunked cases require exact response trailers.
Only the third exchange requests connection close.

The examples keep `/echo` and `/early` behavior unchanged.
The separate `/duplex` route selects `continue_request_body()`.
Both examples select the exact descriptor backend with `--native`.
The existing client adapter passes that flag without changing request or result formats.

The 46 independent harness self-tests passed.
The seven benchmark-runner tests also passed.
The runner rejects stale output, incorrect measurement scope, and the wrong backend or forwarding mode.
Both required Clippy modes completed with existing unrelated warnings only.

## Source and binaries

The implementation is change `mnnlnwyt`, commit `612a0949cd77a52b2518c25e16fec080bdc69f2c`.
It descends from common baseline `0bd3e950f4b0aedb552fbfd8fab96139ef399bb9`.
The examples were built at snapshot `8d997599e3744082ab8c021c6a98d09a95630155`.
The final commit retains identical Rust sources and adds only benchmark-runner safeguards after that build.

The build used Rust 1.98.1 and LLVM 22.1.8:

```sh
taskset -c 8-31 env CARGO_PROFILE_RELEASE_DEBUG=2 \
  CARGO_TARGET_DIR=target/wrapper-lab/build-interop \
  cargo build --release -p kimojio-http1 --examples --offline
```

The binary directory is `target/wrapper-lab/build-interop/release/examples`.

| Binary | SHA-256 |
| --- | --- |
| `client` | `329a0dd54fab8ed398bc6a1bea9c102995ce10fd46c62c556fc1de2610caaf17` |
| `server` | `78a85bcddf9899ff9cf9df73bad5acb30b250993dc4416aea55718761fbb7dad` |

## Reproduction

Run the harness self-tests:

```sh
taskset -c 8-31 python3 -B -m unittest discover -s interop/http1 -p 'test_*.py' -v
```

Run the native server suites:

```sh
taskset -c 8-31 python3 -B interop/http1/fixture_server_suite.py \
  --server-command '["target/wrapper-lab/build-interop/release/examples/server","--native","--bind","{bind}"]' \
  --go-peer target/interop/http1-peer
taskset -c 8-31 python3 -B interop/http1/duplex_suite.py \
  --server-command '["target/wrapper-lab/build-interop/release/examples/server","--native","--bind","{bind}"]' \
  --path /duplex --reuse
```

For the generic backend, remove `--native` from each server argument array.
For client suites, copy `interop/http1/kimojio-adapter.json` to the artifact directory.
Set its client path to the release binary.
For the native backend, add `--native` to its Python adapter arguments.

```sh
taskset -c 8-31 python3 -B interop/http1/client_suite.py \
  --adapter target/wrapper-lab/interop-native-adapter.json
taskset -c 8-31 python3 -B interop/http1/duplex_suite.py \
  --client-adapter target/wrapper-lab/interop-native-adapter.json
```

## Raw evidence

The individual reports remain under `target/interop`.
They retain commands, outcomes, request traces, peer observations, and unsupported capabilities.

| Backend and suite | Report path below `target/interop` |
| --- | --- |
| Generic server | `fixture-server-de11f4cfcb714af09813257a47d27820.json` |
| Native server | `fixture-server-6eef92798c2c4e93ace1c58fa06bd500.json` |
| Generic client | `client-96a06bf8d35542f6a37f9f1176fdfaa5/results.json` |
| Native client | `client-17d41b5bd70349aa84ee2ac83584a2dd/results.json` |
| Generic duplex client | `duplex-d49354ecfe314ed5b22d6fc653e841f2/results.json` |
| Native duplex client | `duplex-de0ca880d513489fa0ebae764d00e73e/results.json` |
| Generic reusable server | `duplex-076a8a227220419ebf7523b531133cb8/results.json` |
| Native reusable server | `duplex-d3974ddf47e34bba8319942117d5a666/results.json` |

These cases do not replace deterministic cancellation or partial-write tests.
They do not prove every kernel completion schedule.
They do prove application progress and repeated connection use against independent Python and Go peers.
