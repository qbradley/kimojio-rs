# FSM performance tools

The [allocation probes](allocation-probes/README.md) count Rust allocator calls in the original applications.
The [comparison harness](../interop/perf/README.md) compares complete native applications with independent Go implementations.
The [final report](../docs/http1-fsm-report.md) separates these results from correctness evidence.

## CPU profiles

`profile_fsm.py` records the static server, HTTP wrapper, WebSocket chat, and HTTP client.
It uses frozen release binaries with debug information.
Each profile contains three seconds of userspace `cpu-clock` samples at 499 Hz.
Hardware events are not necessary.

The subject uses CPU 0.
The Go load uses CPUs 2, 4, and 6.
For the native client profile, the reference server uses CPU 2.
These are separate physical cores on the recorded host.
CPU affinity does not reserve those cores against other users.

Run the collector with immutable binary paths and their source revisions:

```sh
python3 -B perf/profile_fsm.py \
  --go-tool PATH_TO_FROZEN_GO_TOOL \
  --native-dir DIRECTORY_WITH_FROZEN_SERVERS \
  --native-client PATH_TO_FROZEN_NATIVE_CLIENT \
  --server-revision SERVER_SOURCE_REVISION \
  --client-revision CLIENT_SOURCE_REVISION \
  --output target/perf/new-profile-run
```

The server directory contains `http1-static`, `http1-wrapper-server`, and `websocket-chat`.
The native client is the wrapper's `bench_client` example.
The output directory must not already exist.
`--case` selects one subject and can appear more than once.
The recorded build commands describe the release builds used for this experiment.
Supplied binaries must use those commands and flags.

Each subject starts before the load.
The collector waits for server readiness and rejects an early load exit.
It captures a warmed interval and requires a successful, byte-exact workload.
Each successful case records its command, binary hash, revision, and load result beside `perf.data`.
A failed case does not produce the complete `profiles.json` publication.
The collector terminates remaining child processes on failure.

The load statistics include profiler overhead.
They are not comparative throughput results.
The profiles exclude kernel execution and allocation counts.
A sparse profile cannot support precise copy rankings.
The raw profiles and frozen executables remain under `target/perf`.
The report retains their identities and the derived sample evidence.

Run the outcome-parser tests:

```sh
python3 -B -m unittest discover -s perf -p 'test_profile_fsm.py'
```
