# HTTP/1 composition evidence

The [assessment](../http1-fsm-report.md) interprets these results and states their limits.
These files retain measured outcomes instead of links to temporary agent worktrees.

| Directory | Contents | Scope |
| --- | --- | --- |
| `correctness` | Independent release publications and individual wire-suite results | HTTP 93/93, WebSocket 30/30 |
| `allocations` | Complete allocator-probe matrix with commands and binary identities | Whole-process Rust allocator calls, not ordinary throughput or RSS |
| `profiles` | Exact profile subjects and derived sample analysis | Warmed userspace CPU samples, not kernel cost |
| `performance` | Comparative harness publications and measurements | Per-workload assumptions and error gates apply |

A publication names the source and binary used for that experiment.
Later source revisions do not change the experiment's identity.
Absolute paths record the original artifact locations.
The recorded commands and hashes distinguish those artifacts from later builds.

Some publication status fields describe an earlier stage of the work.
For example, the correctness publication predates performance measurements.
Those historical fields do not override the final assessment.
Parent-reported checks and independent wire outcomes remain separate evidence categories.

The large `perf.data` files and frozen executables remain under `target/perf`, outside version control.
Their hashes, build commands, and derived sample counts remain here.
The repository does not contain every raw profiler sample or executable.
Reproduction requires the documented toolchain, Linux interfaces, and workload configuration.
