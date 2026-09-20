# HTTP/2 implementation and wrapper assessment

## Result

The approved HTTP/2 core, HTTP/1 plus HTTP/2 composition, and conventional Kimojio client/server wrappers are implemented.
Native and generic transports passed the recorded correctness, interoperability, retention, and performance gates.
This report closes the implementation sequence, not every possible protocol extension or fault-injection scenario.

HTTP/2 remains a sibling of HTTP/1, not an extension of its framing.
The composite reuses the complete HTTP/1 child.
Direct HTTP/2 connections bypass that child.
The protocol core remains synchronous and sans-I/O.
The wrappers supply runtime integration.

| Component | Location |
| --- | --- |
| Family design and ownership rules | [FSM composition](fsm-composition.md) |
| HTTP/2 scope, reuse decision, and contracts | [Composition design](http2-composition.md) |
| Core and HTTP/1 plus HTTP/2 composite | [`kimojio-fsm-http2`](../kimojio-fsm-http2/) |
| Conventional client/server APIs | [`kimojio-http2`](../kimojio-http2/README.md) |
| Wrapper implementation requirements | [Wrapper design](http2-wrapper-design.md) |
| Exact lifecycle tests and exclusions | [Contract matrix](../kimojio-http2/tests/CONTRACTS.md) |
| Source-specific evidence and history | [Qualification ledger](http2-qualification.md) |

The earlier static-server and chat coordinator changes remain separate.
Their ownership model supports composed applications without requiring conventional async HTTP APIs.

## Delivered behavior

Both HTTP/2 roles support concurrent streams, streaming bodies, informational responses, trailers, classic CONNECT, cancellation, GOAWAY, and graceful shutdown.
The core implements RFC 9113 and RFC 7541 behavior within the approved scope, including disabled-push acknowledgment handling.
The wrappers accept established native descriptors or `SplittableStream` transports.
They do not implement DNS, socket establishment, TLS negotiation, pooling, or automatic retries.

Metadata commands remain transactional.
Only `CommandError::Blocked` waits for the authoritative admission-change signal.
The adapters do not parse SETTINGS or reconstruct flow-control arithmetic.

Receive EOF, upload failure, failed-buffer progress, stream retirement, and transport close remain distinct.
Optional observers expose admitted identities and retirement even when a request fails before final response headers.
An early response does not silently cancel its upload.
Native write receipts are exact.
Failed generic write-all receipts preserve a lower bound.

Handlers and producers have separate cancellation scopes.
Unread request bodies drain without resetting a valid response prematurely.
Held body chunks remain readable after transport close and delay retirement until release.

## Final source and evidence

The accepted wrapper checkpoint is `d8ca94b6`.
Runtime repair `a4af94fd` removes connection-lifetime retention of completed work.
Optimization `cd81114e` stores one weak scope membership inline, with a general multi-scope fallback.
Rejected direct-read candidate `458b2bcd` is absent from the accepted source.

The integrated implementation and evidence are in jj change `kwrnspym`, commit `f1118067`.
This assessment is in its child, `zvyyonpq`.
At completion, the `fsm-composition` bookmark remained at `yuzmtmyv` / `c5aecb11`.
No bookmark moved and no change was pushed.

| Evidence | Identity |
| --- | --- |
| Final socket fixture | `c9b9ecbc07e40f0804fd3a1e5b2c6fe11c014e30` |
| Socket binary SHA-256 | `4f2b8aa2eadde6b9bc36b4ed03b0da5fbb4fc39259815c71a136bdba0c289a1a` |
| Independent peers | `1df3ea1e81e679c3da51b8c4ff1ed10b8d9f8868` |
| Measured source | `a2bbb0666c5a3adcf740569243b20efa875a9223` |
| Normal benchmark SHA-256 | `670dba3077c072839cf2d45a9afbb2936c86e98f9d0166cc87607b0f9eb42d84` |
| Final measurement report | [`be427381`](http2-performance/wrapper/membership-cd81114e/) |

The final fixture passed 48 flow cases and 17 protocol cases for each transport: 130 cases total.
Four additional early-200/413 probes completed full uploads, actual retirement, and transport close.
Strict credit controls and peer-suite unit tests also passed.
Four cases per transport use explicit startup synchronization and do not claim cold-start qualification.
The [final socket report](http2-performance/wrapper/membership-cd81114e/final-socket-summary.json) preserves all case results.

The final integrated all-feature workspace suite, excluding the runtime package, passed 794 tests with four ignored tests.
A separate focused runtime suite passed 187 tests.
Release HTTP/1 and HTTP/2 suites passed 160 tests and doctests.
Fixture and benchmark controls passed 24 tests.
These suites overlap and must not be added as a count of unique tests.
Formatting and both Clippy configurations passed, with the two existing `pipe.rs` warnings unchanged.
Filesystem-dependent and hardware-only runtime tests were not part of this regression run.

Independent reviews covered the core, native wrapper, generic transport, runtime repair, and final membership optimization.
The final runtime reviews used clean frozen-source checkouts and recorded their execution limits.
No unresolved correctness finding remains from those reviews.

## Performance and storage

The final normal-allocator binary passed all 80 workload cells and five trial groups per cell.
Both endpoints share one runtime thread and a local UNIX socketpair.
The workloads include full payload comparisons, application tasks, framing, runtime scheduling, and kernel I/O.
They exclude TCP establishment, TLS, DNS, a NIC, and a remote peer.

Fresh warmed results at concurrency eight:

| Workload | Native microseconds/exchange | Generic microseconds/exchange |
| --- | ---: | ---: |
| Empty request, 128-byte response | 24.903 | 28.516 |
| Fixed 4 KiB in each direction | 35.235 | 39.993 |
| Gated 1 MiB in each direction | 1,254.993 | 1,531.983 |

These figures are inverse throughput, not individual request latency or pure wrapper overhead.
The duplex result corresponds to approximately 1,594 MiB/s native and 1,306 MiB/s generic, counting both payload directions.
It proves progress under the explicit overlap gate, not universal scheduler fairness.

Nine paired trial groups support a 2.59 percent native and 1.93 percent generic steady improvement from inline membership.
The equal-weight combined improvement is 2.24 percent, with a nominal interval of 1.94 to 3.00 percent.
Individual cells and sensitivities retain uncertainty.
This is not a universal non-regression guarantee.
The paired experiment supplies the improvement claim, not subtraction of separately collected full-matrix medians.

The allocation investigation found a more important defect than a copy cost.
Long-lived runtime scopes retained completed I/O and obsolete waiters until connection close.
At duplex cohort 32, the old implementation retained 46,063,760 native and 25,991,877 generic requested bytes.
The repair changed registry lifetime from completed history to live work, without removing cancellation protection.

After the final optimization, the empty-request workload stayed at 258,090 native and 338,227 generic requested bytes through 32,768 exchanges.
Application and driver allocation origins returned to zero after runtime cleanup.
At duplex cohort 32, inline membership removed 341,115 native and 329,414 generic vector allocations.
Waiter object size remained unchanged on the measured compiler and target.
These are requested heap bytes and workload-specific plateaus, not RSS or universal peak bounds.

## Assessment

**What worked:** Explicit ownership and separate milestones survived both pure-FSM composition and conventional async wrappers.
Independent peers and lifecycle schedules found defects that ordinary successful exchanges concealed.
Allocation-site traces distinguished runtime retention from payload storage and bounded protocol caches.
Separate frozen binaries and reports made accepted and rejected experiments reproducible.

**What did not work initially:** The first metadata API hid retryable pressure.
The first wrapper API hid authoritative retirement behind send errors.
The runtime scope implementation retained operation history despite correct payload and close results.
These failures required API and runtime corrections, not adapter workarounds.

The direct-read experiment removed its targeted copy but did not establish a useful general improvement.
Its generic large-body gain was 1.52 percent, while a cold small-message case regressed by 2.93 percent.
It remains separate and unmerged.
The larger-read variant also changed receive-page pressure and failed a paused-consumer test.
No resource limit or correctness assertion was weakened to accept either result.

**Remaining costs:** The shared wrapper driver still coordinates substantial ownership, task, and cancellation state.
Semantic callbacks avoid duplicate protocol policy.
They do not eliminate adapter complexity.
Small-message allocation counts and waiter hashing remain material costs.
The measured baseline does not establish superiority over another runtime or HTTP implementation.

The long implementation sequence produced many snapshots and report directories.
This assessment provides an entry point without changing historical results.
New measurements need explicit source hashes, frozen binaries, and an exclusive timing lease.

## Remaining limits and next work

Extended CONNECT, RFC 9218 scheduling, and legacy h2c Upgrade remain outside the approved scope.
TLS/ALPN integration, connection pools, retry policy, gRPC, and other service FSMs remain separate work.
The current design supports those layers but does not implement them.

Qualification does not enumerate every kernel cancellation order, native close failure, deadline/error tie, or arbitrary custom-transport failure.
Some generic transport errors lack a separate public connection outcome and close report.
Fixtures reject those unknown outcomes rather than treating them as success.
A custom non-native future that ignores cancellation can delay closure until its original operation settles.

Retained-page accounting can reach a resource limit before visible payload reaches the flow-control window.
Fragmented forwarding therefore needs explicit capacity configuration.
The contract matrix distinguishes release-driven admission progress from terminal receive-page exhaustion.

The most useful next integration is a small gRPC or protocol-neutral consumer of these boundaries.
That work can expose layering requirements without reopening the HTTP engines speculatively.
Further optimization needs a separate measured objective, including TCP/TLS or another implementation only with matched workload contracts.
No third optimization experiment is part of this completed implementation sequence.
