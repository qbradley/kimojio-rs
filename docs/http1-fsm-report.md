# HTTP/1 and WebSocket composition assessment

## Outcome

The HTTP/1 foundation supports both requested execution models.
The static-file application composes synchronous FSMs through a direct `rustix-uring` executor.
The conventional wrapper supplies async client and server APIs through the same HTTP/1 machine.
The WebSocket and chat branch adds a third consumer without either sibling dependency.

Independent integrated release runs passed 93 HTTP cases and 30 WebSocket cases.
These results exercise the documented protocol surface.
They are not exhaustive protocol certification.
Allocation measurements show substantial costs in the conventional wrapper.
The callback interface alone does not establish maximum performance.

The comparison contains 180 valid measured rows, with no request errors.
Final repository checks also passed.
The architecture is usable, but the wrapper's small-response and large-streaming costs remain performance work.

## Location and jj topology

Stable jj change IDs identify the work even after descendant rebases change commit hashes.
The foundation starts directly on `main` at `ad1b7bee8e8f5d73e5e852dd0af22067f8e27d62`.
No existing HTTP/2 experiment forms part of this ancestry.

| Change | Parent | Contents |
| --- | --- | --- |
| `rklsoomzukyl` | `main` | `kimojio-fsm-http1`, family design, implementation plan |
| `nlsrqplwpylu` | Foundation | `examples/http1-static` |
| `stzrupowmmtz` | Foundation | `kimojio-http1`, runnable examples, native benchmark client |
| `nyrxrxlpnvpm` | Foundation | `kimojio-fsm-websocket`, `examples/websocket-chat` |
| `qozoolpmtnqkvpnnvtlomznmkxnnrkmn` | All three sibling tips | Integration, cross-language harnesses, performance tools, evidence, this report |

The [family design](fsm-composition.md) and [implementation plan](http1-fsm-plan.md) remain the architectural references.
Shared protocol corrections belong in the foundation.
The integration contains one shared HTTP/1 implementation, not consumer-specific forks.

## Implemented surface

| Component | Supported behavior | Deliberate exclusions |
| --- | --- | --- |
| HTTP/1 core | Client/server, HTTP/1.0 and 1.1, fixed/chunked/close-delimited bodies, trailers, HEAD, informational responses, Expect, reuse, deadlines, cancellation, upgrade/CONNECT handoff | HTTP/2, transfer codings other than chunked, concurrent client pipelining, automatic replay |
| Static application | GET/HEAD, opened-file metadata, bounded positioned reads, keep-alive, root-relative `OpenAt2`, explicit file and socket settlement | TLS, ranges, directory listings, file snapshots, signal-driven graceful shutdown |
| Kimojio wrapper | Connected transports, ordinary async handlers, streaming sources, incoming leases, duplex I/O, shutdown, virtual time | Pooling, redirects, automatic replay, application framework, informational-response API |
| WebSocket core | Server handshake, masking, fragmentation, incremental UTF-8, binary messages, ping/pong, close, owned operations, deadlines | Client role, extensions/compression, selected subprotocols, outgoing producer streams |
| Chat | Complete-message broadcast including sender, one publication order, shared payload storage, bounded admission/backlogs, slow-peer removal | TLS, authentication, path/origin restrictions, distributed state |

The protocol and application FSMs perform no I/O or clock reads.
Their allocation behavior remains separate from that contract.
Native examples require Linux and a working io_uring environment.
The static driver also requires the documented `OpenAt2` behavior.

The chat example accepts valid handshakes on every path and origin.
Its default bind address is loopback.
It is not a production access-control boundary.
The static document root remains administrator-controlled.
Its no-symlink policy does not exclude hard links or mount points.

## Design results

### One interface supports different callers

Every machine uses `next(&mut ports) -> Option<P::Output>`.
Callbacks can resolve internal work without suspension or return caller-selected output.
The protocol does not require a universal action enum, task, or channel between layers.
Composites retain internal ready work before they report quiescence.

The static composite forwards file open, metadata, read, and close operations to the root.
The HTTP wrapper instead connects application futures and native transport workers.
Chat resolves protocol and application callbacks synchronously, then exposes unresolved transport work.
These are different consumers of the same contract, not three copies of HTTP policy.

### The machine owns protocol decisions

The shared machine now chooses the response wire version.
It owns automatic 100/417 behavior, including body-credit authorization.
It accepts one final response behind pending informational output.
Consumers no longer reconstruct these decisions or retry final-response admission.

The added `source_finished` notification separates producer lifetime from transport ownership.
A consumer can release an unused producer while an admitted write still owns its payload.
`incoming_finished`, exchange retirement, payload receipts, and connection closure retain distinct meanings.
Collapsing these events caused incorrect ownership assumptions during implementation.

### Outstanding operations own their resources

Typed operations own storage without borrowing the complete machine.
One read and one write can remain outstanding together.
Rejected commands and completions return their original resources.
Cancellation acknowledgement never permits early release of kernel-accessible storage.

Write-all failures need an explicit unknown-progress result.
They cannot safely imply zero accepted bytes or authorize replay.
Persistent cancellation must also cover continuation operations after a positive short write.
The wrapper now cancels new native submissions after each pending poll of the original future.

### Upgrade transfers authority, not only bytes

HTTP retains authority until the complete handshake output and original operations settle.
The handoff preserves unread input and transfers the receive buffer.
It does not transfer a socket that the FSM never owned.
Abort, timeout, or shutdown before transfer revokes pending handoff authority.
After transfer, HTTP cannot request transport close.

### Bounds need explicit scope

The HTTP core bounds metadata, payload credit, operation slots, and retained buffers.
Chat accounts for shared backing capacity once, plus recipient metadata and retained references.
Growth includes both old and new allocation capacities.
Its public constructor rejects incompatible message and outgoing-buffer limits.

These bounds are not whole-process RSS limits.
They exclude allocator overhead, kernel buffers, executable pages, and undeclared runtime storage.
A conventional body source must also bound its own application state.

## Defects found and corrected

| Defect | Correction and evidence |
| --- | --- |
| Terminal HTTP state still permitted upgrade handoff | Recheck authority and outstanding slots at transfer. Lifecycle tests cover abort, timeout, shutdown, and late completions. |
| A late static-file read assumed earlier body demand still guaranteed admission | Return the rejected buffer and settle file ownership after HTTP revokes demand. |
| Buffered unsolicited client response bytes could become a later request's response | Preserve the valid first response, but disable reuse and later admission. |
| A wrapper source could return late output after an early 413 and replace that response with a limit error | Drain core notifications before source polling and retain explicit rejected-buffer ownership. |
| A write-all continuation could escape one-shot cancellation | Maintain cancellation across pending polls. Native tests include a positive short write and unrelated connection progress. |
| A public chat configuration admitted a message larger than the WebSocket outgoing-buffer limit | Reject the configuration explicitly. Public-composition and native-wire cases cover 20,000 bytes through a 16-KiB receive buffer. |
| Benchmark CPU timing used a nominal boundary that differed from actual work admission | Publish one actual measurement boundary and align the CPU interval with completion cleanup. |
| A benchmark with successful requests followed by an error still reported numeric throughput | Report invalid status, null throughput, and a nonzero exit for any phase or cleanup error. |

Initial small-message interoperability did not expose the duplex defects.
The final duplex cases transfer exactly 8 MiB and require response progress before upload completion.
Intermediate binaries do not supply final release evidence.
The retained publications identify exact sources and binary hashes.

The default chat close timeout is one second.
A slow peer can observe EOF before the close frame completes.
The strict slow-peer close-code case uses an explicit five-second timeout.
This is a documented deadline consequence, not a guarantee of complete close frames under every backpressure condition.

## Correctness evidence

The [correctness publication](http1-fsm-evidence/correctness/publication.json) records the complete source and binary identities.
The adjacent JSON reports retain individual outcomes.
The HTTP release source is `df5c5c193639f4305650c8cb2870ba67eda60e71`.
The final WebSocket release source is `b75b160c61fb44bc52e6115a9998226babe714eb`.
Later benchmark and documentation additions do not replace these immutable test subjects.

| Independent integrated suite | Result | Important scope |
| --- | --- | --- |
| Static HTTP wire cases | 35/35 | Strict framing, request policies, file responses |
| Static recovery cases | 13/13 | Descriptor baseline, all samples, and final count: 6 |
| Wrapper server | 20/20 | Independent request peers and response contracts |
| Wrapper client | 17/17 | Independent server behaviors |
| Duplex | 8/8 | All four client variants complete 8-MiB transfers |
| WebSocket raw wire | 19/19 | Handshake, lengths, masking, fragmentation, control and close behavior |
| WebSocket application | 11/11 | Python, Node, and Go peers, concurrent publication, slow peers, shutdown |

The WebSocket recovery cases retain a descriptor baseline of 10 across six samples.
Exact native completion-order cases belong to unit and native-operation tests.
TCP wire tests cannot deterministically choose kernel cancellation-completion order.
Descriptor observations are not an exhaustive leak proof.

The final integrated run passed 681 workspace tests, including doctests.
Four pre-existing doctests remain ignored.
The benchmark example passed six additional tests.
The Python harnesses passed 44 HTTP, 24 WebSocket, 12 performance, and four profile-outcome tests.
The performance tool passed ten Go tests.

`cargo fmt`, `cargo clippy`, and `cargo clippy --all-targets --all-features` completed without warnings.
Go vet and Node syntax checks also passed.
The final HTTP and WebSocket production source directories match the independently tested release revisions.
The [final-check ledger](http1-fsm-evidence/final-checks.json) records command scope and source identities.

## Allocation measurements

The [raw allocation matrix](http1-fsm-evidence/allocations/whole-process.json) contains 12 successful runs.
Each run includes process startup, successful work, and normal shutdown before the allocator report.
The [probe implementation](../perf/allocation-probes/README.md) wraps the Rust allocator around the original executable entry points.
It does not measure native allocations outside Rust or kernel storage.
Its atomic counters change timing, so instrumented throughput is not a comparison result.

The table shows the observed increase from 32 to 128 successful operations.
Calls include allocation, zeroed allocation, and reallocation.
One chat operation publishes to four recipients, including its sender.
These are two-point slopes, not universal constant costs.

| Application | Payload | Additional allocator calls per operation | Peak live requested bytes, 128-operation run |
| --- | ---: | ---: | ---: |
| Static | 128 B | 19.00 | 75,264 |
| Static | 65,536 B | 31.00 | 75,264 |
| Wrapper server | 128 B | 139.00 | 113,718 |
| Wrapper server | 65,536 B | 256.00 | 128,896 |
| Chat, fanout 4 | 128 B | 28.81 | 169,796 |
| Chat, fanout 4 | 65,536 B | 113.00 | 261,868 |

The wrapper has a substantial per-exchange allocation cost.
The interface does not require all of those allocations, but this adapter currently performs them.
Chat's count also depends on scheduling and actual I/O fragmentation.
Neither the final live-byte count nor the peak is a leak count or RSS.

An initial large chat run reached its configured five-second lifetime before all requested messages completed.
The completed matrix uses an explicit fifteen-second lifetime.
Normal 1001 shutdown in the earlier run was not a data-corruption result.

The [client allocation runs](http1-fsm-evidence/allocations/client-whole-process.json) use the corrected benchmark entry point and one connection.
They compare 250-ms and 1000-ms admission windows, with zero internal warmup.
Operation totals include any work before the published measurement boundary.
All four runs completed without errors.
Their two-point slopes are 155.35 allocator calls per 128-byte response and 361.87 per 64-KiB response.
Whole-process peak live requested storage reached 170,463 B.
These results include benchmark reporting and do not describe pure protocol allocations.
Their instrumented rates do not enter the throughput comparison.

## Comparative performance

The [comparative report](http1-fsm-evidence/PERFORMANCE.md) contains all workload definitions, trial ranges, source mappings, and reproduction commands.
Compressed raw reports and their checksums remain in the repository.
The primary matrix has 126 rows, the one-core client control has 30, and the large-chunked supplement has 24.
All 180 rows and their warmups passed.
No failed measured row was replaced or excluded.
Six separate resource-challenge groups also passed.

The table contains medians of three trials.
HTTP rates count requests, WebSocket rates count complete sender-inclusive publications, and streaming rates count payload bytes in each direction.

| Workload | Native | Go reference | Interpretation |
| --- | ---: | ---: | --- |
| Wrapper, 128 B, persistent, concurrency 16 | 28,851 requests/s | 66,780 requests/s | The small-response wrapper path needs improvement. |
| Wrapper, 64 KiB, persistent, concurrency 16 | 11,566 requests/s | 12,585 requests/s | Rates are closer. Median per-trial p99 is 3.60 versus 4.85 ms. |
| WebSocket, 4 KiB, four recipients | 9,396 publications/s | 8,463 publications/s | This complete-broadcast workload favors native. It is not a saturation result. |
| Chunked echo, 1 MiB, concurrency 16 | 628 MiB/s per direction | 1,424 MiB/s per direction | Native approaches one server CPU core. Both targets use fresh connections. |
| Client, 128 B, persistent, concurrency 16, one CPU | 31,758 requests/s | 47,403 requests/s | Includes different byte-validation strategies, not isolated client-library cost. |

Large chunked echo uses the existing native `/echo` route.
Four independent progress probes require response prefixes before later upload chunks.
They distinguish streaming from whole-body collection.
The timed supplement requires chunked transfer in both directions, with exact payloads and no response `Content-Length`.

Static-file results reverse with size and connection mode.
Native persistent 128-byte responses and Go persistent 64-KiB responses show roughly 40-50 ms latency plateaus.
Both 64-KiB WebSocket implementations also show these plateaus.
Their low CPU use points away from a simple CPU-copy explanation.
The experiments did not establish the transport cause.
These results remain visible in the full tables rather than disappear as outliers.

Median sampled peak RSS across server configurations was 2.49-4.88 MiB for native and 20.97-26.41 MiB for Go.
These are process and runtime comparisons, not protocol-state sizes.
The Go tool includes both reference and load implementations in one executable.
The static reference can also use sendfile, unlike the native file-read path.

Servers use one physical CPU.
The primary clients can use three, and the additional client control uses one.
CPU affinity does not reserve a shared host.
Three two-second trials provide ranges, not statistical confidence intervals.
Histograms have approximately 3.125% bucket width, and native quantiles were not pooled.
No throughput claim uses allocation instrumentation, profiling overhead, or an errorful workload.

Resource challenges include intentional resets and slow recipients outside successful throughput counts.
Native static returns to six descriptors, and native wrapper/chat return to ten.
Go references return to six, sometimes after a short delayed cleanup.
Both chat targets preserve healthy recipients and close the slow recipient with 1008 under an explicit five-second close timeout.

## CPU profiles and copy costs

Four error-free workloads produced release profiles with debug information.
The [profile publication](http1-fsm-evidence/profiles/publication.json) records exact binaries, source revisions, and commands.
Each profile covers three warmed seconds with `cpu-clock:u` at 499 Hz.
Kernel execution is outside this sample set.
Hardware cycle events were unavailable.

The server binaries come from `284812f1f9b59256907524a8b3ef5529e56379d2`.
The corrected benchmark client comes from `295a956cb34f1717bc9768824570cce904786671`.
The source tree now includes later evidence and tools.
Assembly addresses belong to those frozen binaries, not the current working tree.

The [copy analysis](http1-fsm-evidence/profiles/analysis.md) includes instruction-level evidence and lifetime requirements for proposed changes.

| Profile | Samples | Main observation |
| --- | ---: | --- |
| Static server | 204 | Header scanning contributes measurable work. Stable-operation copies are not established as major costs. |
| Wrapper server | 554 | Runtime wait/poll helpers and allocation contribute more recovered samples than the identified state move. |
| WebSocket server | 184 | Masking accounts for 40 samples. An 80-byte deadline iterator move has six samples. |
| Native client | 953 | The benchmark's ASCII comparison loop accounts for 483 samples. Receive staging memcpy has 19 samples. |

The client payload comparison represents about half of its userspace samples.
Client throughput therefore measures the complete validated workload, not isolated HTTP execution.
Different validation algorithms can change the comparison even with the same byte-exact contract.
The comparison harness must retain that qualification.

The first copy candidates are empty receive-staging bypass, an intermediate 264-byte wrapper state move, and local WebSocket deadline staging.
Each candidate has a specific ownership or lifetime condition.
No production optimization or predicted speedup follows from these short recordings.
Large 16.4-KiB read-state and 1.6-KiB upgrade-state moves have zero nearby samples here.
Their size alone does not justify priority.

Recovered stacks are incomplete in substantial portions of every profile.
The shared category includes unknown ownership, not only code proven common to client and server.
Kernel work and off-CPU delays need separate evidence.

## Assessment and next work

The composition experiment succeeds at separating execution from protocol and application state.
The same HTTP implementation supports the direct driver, conventional wrapper, and WebSocket upgrade.
The two execution models remain independent.
The implementation exposed useful ownership milestones before HTTP/2 added multiplexing complexity.

The experiment does not establish allocation-free adapters or maximum throughput.
The wrapper allocation counts are a concrete weakness.
The small-response and large-chunked comparisons also show a material performance gap.
Lower resident memory does not cancel those costs.
The native runtime also retains the documented cancellation-scope limitation with wrapped `FuturesUnordered` wakers.
The examples avoid that combinator rather than claim that this work corrected the runtime.

The design evidence supports an HTTP/2 prototype, not immediate migration of the historical engine.
That prototype needs concurrent stream identities, connection-level flow control, and stream-local cancellation through the same ownership model.
It must also demonstrate one blocked stream alongside another stream with progress.
HTTP/1 cannot prove those properties.

| HTTP/2 question | Required next evidence |
| --- | --- |
| A stream retains received data | Another eligible stream still receives data within an explicit connection storage bound. |
| One producer remains pending | Other streams and control frames retain progress without a task between protocol layers. |
| A stream resets during a partial transport write | The current frame retains storage and wire integrity while unused producer work stops. |
| Stream and connection credit differ | The machine exposes admission and completion without adapter-owned flow-control calculations. |

The HTTP/1 single-buffer receive lease is not a complete HTTP/2 receive architecture.
WebSocket close ordering supplies a useful partial-frame example, but not a multiplexed-stream proof.
These cases belong in the first small HTTP/2 composition experiment.

The next performance work must follow the measured hot paths.
Any optimization needs a new frozen binary, matching workload, and retained correctness coverage.
The current evidence puts wrapper allocation and the sampled copy sites ahead of cold connection-setup moves.
The transport plateaus need a separate controlled socket-policy experiment.
That experiment must retain both default-policy results and any tuned results.
