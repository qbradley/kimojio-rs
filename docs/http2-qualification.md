# HTTP/2 qualification ledger

## Scope and status

This ledger records bounded evidence for the scope in [the composition design](http2-composition.md).
It does not claim an exhaustive RFC proof.
The core qualification gates are complete for the recorded scope and snapshots.
The metadata-admission repair passed independent review, socket qualification, and source-specific measurements.
Kimojio HTTP/2 wrapper implementation is in progress.
The native client and server have focused runtime coverage.
The explicit wrapper interoperability profile passed for native and generic transports.
Lifecycle evidence is recorded with explicit limits.
The runtime scope-retention repair passed independent review, socket qualification, and bounded retention measurements.
The wrapper performance baseline is measured.
The direct-read experiment was rejected for general integration. One final bounded allocation experiment remains in progress.

The current socket qualification covers the direct HTTP/2 engine.
Separate models cover the HTTP/1 plus HTTP/2 composite.
Socket interoperability does not establish runtime-wrapper cost or cancellation behavior.

## Qualified socket snapshot

| Item | Identity |
| --- | --- |
| Integrated source, composite, and fixture | `b3c6484b1cb664dca76b5c35399d2e7d1367ce87` |
| Core | `451c38e00d7e8e589668324c6af82dbc9e7a5a2c` |
| Independent peers | `f55c762813709598f8a1ad3844f7b9805ddb507f` |
| Fixture SHA-256 | `fda53641d06f2d4af76885e11ca3197d08614010777f00c4bb3a056815e3fac4` |
| Local evidence | `target/http2-program/reports/native-b3c6484b-summary.json`, revision 1 |

The peer suite checked the fixture hash before and after execution.
Both roles passed all 48 flow cases and 17 protocol cases.
The [peer documentation](../interop/http2/README.md) describes the scenarios and commands.
The local summary names each detailed report.

Healthy peers sent complete 200 and 413 responses before request completion.
Both no-action probes still received all 131,087 request bytes and a request END_STREAM.
The streams retired successfully.
Neither response status nor response END_STREAM implicitly cancelled an upload.

The blocked-upload case used a different, explicit protocol signal.
The peer sent a complete 413 response, waited for a PING acknowledgment, and sent RST_STREAM(NO_ERROR).
The fixture preserved the response, reported reset code zero, completed the sibling, and closed gracefully.
The client sent no reset in this case.

The earlier `28292978` reset-discard run recorded 33,829 connection-credit bytes and zero reset-stream credit.
Queued refunds that the peer discarded on GOAWAY did not count as transmitted credit.
The suite rejects missing terminal outcomes and rejects `aborted` as normal success.
This socket suite did not execute an explicit connection-abort scenario.

## Ownership and lifecycle evidence

Independent review exposed seven defects in earlier core snapshots.
The repaired behavior has focused regressions:

| Defect | Required behavior |
| --- | --- |
| Response completion removed an unfinished upload | Receive and transmit halves remain independent |
| Partial writes produced successful buffer receipts | Success requires full acceptance, with exact or uncertain progress preserved |
| Rejected output left private stream state active | Admission remains atomic, or failure resets the stream coherently |
| CONNECT retained an HTTP body-size cap | Tunnel bytes retain storage and flow bounds without the HTTP body cap |
| Informational HEAD response ended the stream | Only the final bodyless response ends the stream |
| Expected read cancellation discarded GOAWAY | Original settlement preserves required protocol output |
| Obsolete alarms left unbounded cancellation acknowledgments | The joined obligation bound is the configured cap plus three teardown cancellations |

The abort and alarm-failure review passed 19 focused tests and 60 additional independent schedules.
The focused models include 11,520 hard-abort schedules.
They cover original completions, cancellation acknowledgments, primary causes, partial writes, and uncertain progress.

An active alarm failure reports `IoFailed` without an invented timestamp.
An obsolete alarm failure only settles its original obligation.
The composite also exercises these rules before and after cancellation dispatch.
Its 16 tests include protocol selection, hard abort, wrong-owner recovery, late completions, and metadata-admission notification forwarding.

Transport close does not settle a held application body lease.
The independent probe retained readable body data after `closed`.
Its later release produced one stream retirement without new I/O or receive credit.
The [wrapper requirements](http2-wrapper-design.md) preserve this distinction.

## Resource progress and directional overlap

Prompt body release did not prevent the original control-queue failure.
Each release created connection and stream WINDOW_UPDATE frames while an original write remained outstanding.
The queued frames exhausted the outbound item cap.
The repair stores bounded pending credit rather than one frame per release.
It does not increase resource limits.

Successful byte totals then exposed a second weakness in the acceptance test.
Credit work still deferred response DATA until uploads finished.
The scheduler now gives eligible DATA a turn between credit batches.
Ordinary control order and partial-frame precedence remain intact.

The strict transport-level negative control fails all ten cohorts on `c08b7333`.
Its first response arrives after all 8,388,608 request bytes enter the transport.
With `18a8f392`, the first response arrives after only 49,152 to 65,536 request bytes.
The [overlap report](http2-performance/overlap/) retains both source identities and unchanged payload assertions.

An independent probe combined three simultaneous uploads, small windows, fragmented I/O, and headers that require CONTINUATION frames.
All streams showed overlap, header blocks remained uninterrupted, and both directions returned exact credit and buffer totals.
This is bounded scheduler evidence, not an exhaustive fairness proof.

## Allocation and performance boundaries

The isolated allocation probe passed 324 runs across 108 cells.
Three repetitions produced identical counts.
Its allocator is separate from the timing benchmark.
The [allocation report](http2-performance/allocations/) states the measurement boundaries and exclusions.

Initial warmed measurements still showed two reallocations and retained growth.
Debugger stacks attributed both reallocations to bounded private tombstone queues.
Each queue has a 1024-entry limit but retains capacity for 2048 slots.
The extended probe covered 6,030,000 exchanges over 10,000 cohorts per connection.
Storage reached a plateau, harness vectors stayed unchanged, and shutdown reclaimed all measured connection storage.
The [retention report](http2-performance/retention/) preserves the detailed evidence.

These counts measure requested storage, not allocator rounding or process RSS.
They do not establish zero-allocation requests.

The earlier `c08b7333` timing matrix passed all 945 trials across 189 cells.
It did not establish directional overlap.
The isolated forwarding experiment replaced a 144-byte move with a hot 232-byte copy.
All six paired timing intervals included equal performance, so the experiment remains unmerged.
The [experiment report](http2-performance/credit-coalescing/) records the rejected change.

The later run used frozen integration `5505efc4`.
All 945 timing trials, 324 allocation runs, and 12 retention runs passed.
Four fresh profiles separate client, server, and shared work.
For 128 concurrent empty-body exchanges without fragmentation, medians were 2.662 microseconds direct and 2.679 microseconds explicitly selected.
Some fragmented workloads showed approximately 14 percent composition overhead.
These are in-memory costs, not socket throughput or runtime-wrapper measurements.

The maximum reported requested-live storage was 610,546 bytes, not process RSS.
The [source-specific report](http2-performance/final/) records distributions, measurement boundaries, hashes, and allocation exclusions.
Its evidence remains unchanged.
The metadata-admission repair is not part of this measured source.

The admission-aware run measured `b3c6484b` against immutable `5505efc4`.
All 945 matrix runs, 486 paired runs, 324 allocation runs, and 12 retention runs passed.
The normalized storage counts match the earlier bounded baseline.
Four new profiles retain client, server, and shared attribution.

The admission-aware source has a measured common-path regression.
Paired median changes range from 0.60 to 3.84 percent slower, with a 2.04 percent median across 27 cells.
Nineteen bootstrap intervals are positive, and eight include zero.
These measurements do not identify an instruction-level cause.
Performance while metadata admission is blocked remains unmeasured.
The [admission report](http2-performance/admission/) preserves hashes, distributions, profiles, and the complete measurement scope.

## Metadata admission

Research found an interface defect in the earlier socket snapshot.
`Capacity` covered both temporary pressure and a command that exceeded the entire control-byte limit.
Peer concurrency zero instead produced `Message(InvalidFrame)`, despite its temporary nature.
No semantic callback reported when metadata admission changed.
The original socket cases did not establish correct wrapper retry behavior for these conditions.

The repair separates `Blocked` from permanent rejection and adds one coalesced admission-change notification.
Independent review passed 68 targeted tests and 14 additional schedules.
Repeated blocked attempts preserved exact wire output and HPACK state.
Partial writes retained their byte-capacity charge.
Terminal invalidation notified callers before held writes or body leases settled.
The observer retains no metadata and remains quiet without further changes.

The new socket case makes request admission stop at a zero peer-concurrency limit.
Two PING barriers separate the zero limit, the first response, and the later positive limit.
The peer observes the positive SETTINGS acknowledgment before the second request HEADERS.
Both requests retire successfully.
Four independent fault controls reject premature HEADERS or a lost request.

The [composition design](http2-composition.md#metadata-command-admission) records the retry contract.
The fixture retries only after the semantic notification.
The composite forwards that notification without protocol-state reconstruction.

## Lessons

Independent peers, explicit outcomes, and negative controls exposed failures that successful byte counts concealed.
The overlap regression demonstrated why eventual completion is not a substitute for bidirectional progress.
Long-lived allocation measurements distinguished bounded growth from a leak.
The unsuccessful copy experiment prevented an unsupported optimization claim.

Artifact coordination was less successful.
Repeated messages and mutable build paths caused redundant runs and confusion about source identity.
Later qualification used immutable paths, exact hashes, explicit source revisions, and one designated candidate.
Old failures remain negative controls rather than evidence against repaired code.

The runtime wrapper work now starts from the measured admission-aware baseline.
The small measured regression remains explicit rather than hidden by a claim of unchanged cost.
Runtime wrappers need their own ownership, cancellation, interoperability, and performance evidence.

## Runtime wrapper progress

The native client and shared runtime foundation are implemented in `b9bd24458f64ea852bfef633f3202c9aa42eb463`.
Integration `c9e213ec` includes that separate implementation change.
The [crate documentation](../kimojio-http2/README.md) records its API, bounds, completion distinctions, and current exclusions.

The phase-one suite passed 22 tests and one doctest in default and release configurations.
All-feature configurations passed 23 tests and one doctest, including virtual time.
The parent integration also passed the all-feature suite.
A separate source review found no significant issues.
That review did not independently inject kernel cancellation races or close failures.

| Wrapper phase | State |
| --- | --- |
| Shared ownership, native I/O, concurrent client | Implemented with focused runtime tests |
| Concurrent native server | Implemented with focused runtime tests |
| Generic established transports | Implemented with focused runtime tests |
| Independent wrapper socket fixture and peers | All 65 wrapper-profile cases passed per transport on `b3789dc6` |
| Wrapper performance and final review | Baseline measured; direct-read rejected; final allocation experiment in progress |

The current core socket results do not qualify the new wrapper.
The wrapper needs its own fixture, peer runs, and source-specific measurements.

### Native server and observation checkpoint

Native checkpoint `af7d2ad48f9a87e8c37abf75dd5b2a4c29307377` adds concurrent handlers, optional informational responses, and independent retirement reports.
Its separate implementation commits are `09b23260`, `8b08c240`, `41fdd37d`, and `af7d2ad4`.
The server drains unread request bodies without an implicit response reset.
Both roles share body ownership, metadata admission, scoped tasks, and native I/O.

`IncomingBody::retirement()` exposes the actual core outcome independently of a failed send receipt.
Optional `RequestObserver` callbacks expose admitted stream identities and retirement after failures before final response headers.
Client callbacks expose actual informational heads, and the server can submit informational heads before its final response.
The synchronized 200/413 reset-zero regression covers response preservation, sibling progress, and graceful close.

The parent integration passed 53 all-feature tests, four doctests, and the nine existing client fixture tests.
The implementation owner also passed default and release suites and both Clippy configurations.
Independent source review of `b9bd2445` through `af7d2ad4` found no significant issues.
It covered server orchestration, shared-client changes, informational admission and cancellation, observers, retirement reports, and associated tests.
The reviewer did not independently execute tests or fault-injection schedules.
This review excludes subsequent generic-transport changes.
The native fixture now uses these observation APIs, and the wrapper-profile socket suite passed.
Broader fault injection and wrapper performance remain incomplete.

### Generic transport checkpoint

Generic checkpoint `d8ca94b6c6ba8439085609b289ff1ddbd6698357` adds established `SplittableStream` transports for both roles.
The public APIs are `connect`, `Connection::run`, `serve_connection`, and `serve_connection_with_shutdown`.
Native operations retain their own reusable slots and exact write receipts.

A failed generic write reports a lower bound because the transport trait cannot expose its accepted prefix.
The adapter does not replay the buffer.
The driver preserves unexpected transport errors and combined transport/close errors.
It drops the settled read half before full transport close.
Held body leases remain independent of transport closure.

The parent integration passed 67 all-feature tests, five doctests, and 13 native fixture tests.
The implementation owner also passed default and release suites and both Clippy configurations.
Independent source review of `af7d2ad4` through `d8ca94b6` found no significant issues.
The reviewer also ran eight focused tests from existing compiled binaries.
Those executions covered partial-write errors, cancellation, late completion, close errors, held body leases, and virtual time.
The reviewer did not rebuild those binaries from the frozen revision, so those runs have weaker source provenance.
Independent TLS qualification and exhaustive custom-transport cancellation remain outside this review.
The generic socket fixture is implemented, and the wrapper-profile socket suite passed for both transports.

Fragmented forwarding can exhaust a retained-page bound without exceeding the wire window.
The fragmented duplex test uses a 2 MiB per-stream receive-capacity bound and unchanged receive windows.
An arbitrary non-native future that ignores cancellation can delay closure until its original operation settles.
Neither limit establishes a performance result or complete adversarial qualification.

### Native client/server fixture checkpoint

Fixture source `96e5280595251ec2957a4955e1e51f8668cd2113` uses the native client and server from `af7d2ad4`.
Its release binary SHA-256 is `4610ada3004603e363f18f4fca65dbc2d162251927df632587710324bc35c2bc`.
Local provenance and reports are in `target/http2-program/worktrees/http2-wrapper-fixture/target/wrapper-native-checkpoint/summary.json`.
No known public API gap prevents the current fixture observations.

The fixture owner passed 31 selected socket cases and five additional probes.
These cover both roles, informational responses, trailers, CONNECT, repeated exchanges, and large bodies.
The extra probes cover full uploads after early 200/413 responses and server responses before complete uploads.
A wide-window 200 response followed by reset-zero preserved the response and exposed the actual retirement outcome and failed-buffer receipt.
This probe does not replace the reduced-window early-response case.

The parent integration passed all 13 fixture tests.
The separate peer owner will run the full suite against this immutable artifact.
The reduced-window client cases need an explicit startup profile with distinct coverage claims.
The older `00005698` artifact remains unchanged.

### Unified native/generic fixture checkpoint

Fixture source `b3789dc6a1ec91efb1f295e9931d373946477852` uses wrapper source `d8ca94b6`.
Its release binary SHA-256 is `0e51d7c774196f398a0e88eb1c9ddcd79580667d0fa112abe4a304052e728fc0`.
It adds explicit `client-generic` and `server-generic` modes over established `OwnedFdStream` transports.
Native mode names remain unchanged.
Local provenance and reports are in `target/http2-program/worktrees/http2-wrapper-fixture/target/wrapper-generic-checkpoint/summary.json`.

The fixture owner passed 22 selected generic socket cases and two full-upload probes.
The parent integration passed all 17 fixture tests.
The independent peer assignment includes both native and generic modes on this artifact after the earlier native report.
This sequence keeps results for different source revisions separate.

Generic transport errors do not always expose a separate terminal connection outcome or close result.
For such errors, the fixture returns a failure with unknown outcome rather than inferred closure or success.
Per-stream retirement and inexact failed-write receipts remain observable.
Full peer qualification must distinguish a missing observation from a demonstrated protocol failure.

### Qualified dual-transport socket snapshot

The independent peer owner qualified the same immutable `b3789dc6` binary in native and generic modes.
Peer source `1df3ea1e81e679c3da51b8c4ff1ed10b8d9f8868` provides the explicit `wrapper` profile.
The peer owner checked the binary hash before and after the runs.
Local evidence is in `target/http2-program/reports/wrapper-b3789dc6-summary.json`.

| Transport | Flow cases | Protocol cases | No-action early-upload probes |
| --- | --- | --- | --- |
| Native | 48/48 | 17/17 | 2/2 |
| Generic | 48/48 | 17/17 | 2/2 |

Each mode covers both client and server roles.
The early-200 and early-413 probes each received all 131,087 upload bytes and complete retirement.
The synchronized reset-zero case preserved the 413 response, authoritative reset retirement, sibling completion, and graceful close.
No exercised case had a failure or unresolved observation.

Four cases per transport use bodyless warmups and peer SETTINGS/PING barriers.
These synchronized cases do not claim equivalent cold-start coverage.
The canonical profile remains separate and unchanged by default.
The wrapper profile compares trailer occurrences in order per case-insensitive name.
It does not discard, sort, or combine repeated values.

Strict profile and credit controls passed.
Without credit refunds, each transport stopped at 65,535 bytes and then reported an explicit watchdog abort.
The parent integration passed 41 Python tests, Go tests, and Go vet.
The earlier native-only `96e52805` report remains separate.

These results do not qualify ambiguous split/I/O/close error observations or every native cancellation race.
The lifecycle evidence matrix and performance measurements provide separate acceptance evidence.

### Lifecycle contract evidence

Checkpoint `59d32b778a3a1b48b6a89a65a7ddb6d88bc350b3` adds tests and a [12-contract matrix](../kimojio-http2/tests/CONTRACTS.md).
It changes no production wrapper, core, runtime, peer fixture, or benchmark behavior.
The parent integration passed 74 tests and five doctests in both debug and release all-feature configurations.
Formatting and both Clippy configurations also passed.

New cases cover nine response-EOF/drop schedules, full-budget queued cancellation, permanent metadata rejection, and cross-connection duplex forwarding.
Other cases cover release-only admission, terminal receive-page exhaustion, and virtual application deadlines.
Release-only progress restores stream-admission capacity.
It does not resume a connection after terminal receive-page exhaustion.
The independent peer suite separately covers zero-to-positive peer concurrency for both wrapper transports.

The matrix records exact tests, observable sequences, and limits rather than marking every broader contract complete.
Native close-failure injection, arbitrary kernel cancellation races, and externally scheduled cancellation-acknowledgment orders remain unqualified.
Other limits include internal HPACK equality, exact lease peaks, core deadline/error ties, and generic cross-connection forwarding.
No claim of exhaustive lifecycle proof follows from these results.

### Runtime benchmark preparation

Benchmark checkpoint `1d48669b56facb0e58f8600e005df18cc37b795e` adds real socket workloads and a separate allocation probe.
It uses wrapper source `d8ca94b6` through integration `c8f6014a`.
The [benchmark report](http2-performance/wrapper/) records frozen binary hashes, workload boundaries, commands, and smoke evidence.

All 64 smoke runs passed payload, retirement, overlap, and actual driver-close assertions.
The parent integration passed seven release/all-feature harness controls and both Clippy configurations.
These smoke durations do not establish performance.
No timing lease or statistical measurement followed from the smoke results.

Requested live storage increased during short warmed allocation windows.
For static 1 MiB duplex bodies at concurrency eight, native storage increased from 1,691,064 to 4,549,080 bytes.
The corresponding generic window increased from 1,296,845 to 2,892,885 bytes.
These values describe requested bytes, not RSS, and do not establish a leak or a plateau.
The following attribution identifies the runtime registry defect that now blocks long statistical runs.

### Runtime scope-retention defect

Attribution checkpoint `598dad5164c667fba4a50127b05a69c3227dcd6b` includes allocation-site traces and runtime-only controls.
The [retention report](http2-performance/wrapper/retention/) records immutable binaries, exact boundaries, resource limits, and resolved allocation stacks.
No production source changed in that checkpoint.

Both wrappers keep an `io_scope` open for the connection lifetime.
Its registries retain obsolete event waiters and completed I/O objects until scope exit.
The retained storage grows with operation history rather than live work.
Payload, overlap, stream retirement, and actual close assertions still pass, so those assertions alone missed this defect.

| Boundary, C8 duplex | Native requested live bytes | Generic requested live bytes |
| --- | ---: | ---: |
| Cohort 1 | 1,691,032 | 1,296,813 |
| Cohort 32 | 46,063,760 | 25,991,877 |
| After both drivers close | 627,572 | 38,493 |
| After runtime cleanup | 1,572 | 1,572 |

Runtime-only scoped NOP and event-wait controls reproduce the growth.
Equivalent unscoped controls remain flat.
Harness and wrapper allocation-site totals remain constant across the socket cohorts.
Core storage does not account for the large growth.
Runtime destruction releases the retained objects, so this is connection-lifetime retention rather than a permanent post-runtime leak.

The assigned correction must retire obsolete registrations while retaining genuine pending I/O and active waiters.
It must preserve nested scopes, cancellation, borrowed-buffer safety, and reentrant waker destruction.
Removing the connection scope or increasing limits is not an acceptable repair.
Independent review and repeated retention measurements must precede timing claims.

### Runtime repair candidate

Candidate `a4af94fd97c986877f1ba23188d923dbe31fae2d` replaces operation-history vectors with live-registration maps.
Waiter registration deduplicates repeated pending polls.
Original I/O retires on genuine completion, while a separate cancellation-target owner remains until the cancellation acknowledgment.
That separate owner prevents premature completion-address reuse.
Registry capacity shrinks geometrically, and callback-bearing destruction occurs outside runtime and registry borrows.

The parent integration passed 183 focused runtime tests, 81 HTTP/1 wrapper tests, and 74 HTTP/2 wrapper tests plus five doctests.
The implementation owner also ran debug/release combinations and 22 no-default-feature scope tests.
The repair adds regressions for scope retention, active cancellation, nested ownership, reentrant wakers, and borrowed-I/O panic settlement.
The acknowledgment-order regression controls owner release around real completions, not kernel CQE order.

Independent safety review found no significant issues in `59d32b77` through `a4af94fd`.
The reviewer built a clean detached checkout of `a4af94fd` with a private target.
All 22 scope/ownership tests passed in three configurations: debug with no default features plus virtual clock, debug all-features, and release all-features.
Nine event/waiter tests and the cross-task future-completion migration test also passed.
These runs do not force both kernel cancellation-acknowledgment orders or establish exhaustive race and unwind safety.
Separate allocation-site measurements followed this review.
The earlier socket and retention reports describe the old runtime, not this candidate.
Source review alone does not establish a retained-storage plateau or a performance result.

The rebuilt fixture source is `86da3f3f90c59a5895c116c689c991ae59887a08`.
Its release binary SHA-256 is `8e8550c0d771ea20a2a6b7c4eab24fb9e64f3f918a21e0c06df1bf44e2d9b80e`.
It includes only the runtime repair beyond `b3789dc6`, with unchanged wrapper and fixture logic.
The fixture owner passed 17 fixture tests, 12 socket smoke cases, and four server-close probes.
Local provenance is in `target/http2-program/worktrees/http2-wrapper-fixture/target/wrapper-runtime-candidate/summary.json`.
The independent peer owner repeated the complete wrapper profile for both transports on this candidate.
Native and generic modes each passed 48 flow cases, 17 protocol cases, and two complete early-200/413 upload probes.
Strict negative controls also passed, including the 65,535-byte upload stop without credit refunds.
The synchronized reset-zero case preserved the 413 response, actual reset retirement, sibling completion, and graceful close.
Four cases per mode remain synchronized rather than cold-start coverage.

The peer owner checked the same binary hash before and after both modes.
The authoritative report is `target/http2-program/reports/runtime-86da3f3f-summary.json`, with peer source `1df3ea1e`.
No exercised socket case had a failure or unresolved observation.
Ambiguous split/I/O/close error observations remain outside this qualification.
Safety review and socket qualification are complete for their recorded scope.
The following measurements provide the separate retention evidence.

### Retention repair acceptance

Checkpoint `6e00374698d40dedebd33b87d3fd183ebb0e2682` measures byte-identical runtime repair `a4af94fd`.
The [repair report](http2-performance/wrapper/retention-candidate-a4af94fd/) records rebuilt binaries, allocation sites, counters, and all 18 capped runs.
The measured retention gate passed.
Independent safety review and the rebuilt socket suite also passed for their recorded scope.

At duplex cohort 32, native requested live storage fell from 46,063,760 to 317,820 bytes.
Generic storage fell from 25,991,877 to 496,193 bytes.
Runtime allocation-site storage stopped increasing by cohort eight.
The remaining duplex growth belongs to bounded core storage, not completed runtime operation history.

The empty-body extension passed 32,768 exchanges on each connection.
Requested live storage, live allocation counts, and reallocation counts stayed unchanged from cohort 256 through cohort 4096.
The plateau was 258,314 bytes native and 338,451 bytes generic.
Runtime cleanup released all traced runtime, wrapper, and protocol storage.
These are workload-specific requested-byte results, not RSS or universal peak-memory limits.

The repair increases allocation activity in the measured duplex workload.
Native allocation calls increased from 556,667 to 767,418, approximately 38 percent.
Generic allocation calls increased from 546,854 to 1,010,075, approximately 85 percent.
Those counts do not establish a CPU regression.
The next phase measures runtime cost and profiles sampled bottlenecks before any optimization.

The integrated workspace formatting check and both required Clippy configurations passed.
Clippy retained the two existing `pipe.rs` warnings.
The all-feature workspace suite, excluding the runtime package, passed 794 tests across 77 result groups, with four ignored tests.
The runtime package has the separate 183-test focused run recorded earlier.
Filesystem-dependent and hardware-only runtime tests remain outside this regression run.
Logs are in `target/http2-program/reports/runtime-a4-parent/`.

### Measured wrapper baseline

Report `8ba74acb59a607d30fae614f2866c475860ec4dc` records measurements of source `6e003746`, with runtime repair `a4af94fd`.
The normal runtime binary SHA-256 is `0a362f7029ead27d25b1046df1ae0cdd4838b87f5cfc275c8f042096b7ec00ad`.
The [measured report](http2-performance/wrapper/measured/) preserves all 80 cells, five alternating trial groups per cell, and separate allocation evidence.
All workload assertions passed.
The normal timing binary uses no allocation instrumentation.

| Warmed workload, concurrency eight | Native wall time per exchange | Generic wall time per exchange |
| --- | ---: | ---: |
| Empty request, 128-byte response | 25.55 microseconds | 28.93 microseconds |
| Fixed 4 KiB in each direction | 35.45 microseconds | 40.96 microseconds |
| Gated 1 MiB duplex | 1,287.24 microseconds | 1,556.01 microseconds |

These times are inverse throughput, not individual request latency or pure wrapper overhead.
Both endpoints share a runtime thread and a local UNIX socketpair.
The workload includes payload assertions, application tasks, protocol work, runtime work, and kernel transport.
It excludes TCP connection establishment, TLS, DNS, and a remote network.
The five-group intervals have limited precision, and the report retains visible host variability.

The allocation probe reproduced the accepted long-connection plateau.
Its largest observed requested-byte peak across 80 cells was 2,323,215 bytes.
That is not RSS or a universal memory bound.
The repair's allocation-call increase remains explicit, without an unsupported before/after CPU claim.

Normal-binary profiles identified generic buffered-read and frame-assembly copies in 313 of 4,356 user-CPU samples.
A separate frame-pointer build supports endpoint attribution but does not supply primary timings.
Those samples justify an empty-buffer direct-read experiment, not a predicted 7.2 percent wall-time improvement.
The experiment must preserve borrowed-buffer settlement, buffered tails, cancellation, deadlines, and all existing outcome contracts.
It is not yet implemented or accepted in this baseline.

Four report-accounting and call-site attribution controls passed after parent integration.
CPU2 was released after the baseline measurements.

### Direct-read experiment

Candidate `458b2bcd2f371b234087f04d2e96e19b61837390` remains separate from the accepted integration.
It bypasses the internal buffer only when that buffer is empty and the caller offers at least 16 KiB.
It preserves the existing 16 KiB native read limit.
The existing native read helper retains the caller's destination until original-operation settlement.

An earlier destination-sized variant reproduced `Reset(11)` in the paused-consumer regression.
The core charges retained pages against the stream receive-capacity bound, so larger reads changed resource pressure.
The candidate does not change protocol budgets or test expectations to hide that failure.
The narrower experiment targets only the buffered copy, not the proposed frame-assembly reduction.

The implementation owner passed focused runtime and HTTP/1/HTTP/2 suites in debug/release and default/all-feature configurations.
Independent review found no significant issues in `a4af94fd` through `458b2bcd`.
From a clean detached checkout, 26 stream tests passed in both debug and release with no default features plus virtual clock.
The review covered buffered tails, refill transitions, partial reads, EOF, deadlines, cancellation, panic cleanup, and late successful completion.
It did not force kernel acknowledgment ordering or establish a speedup.
The rebuilt fixture source is `7e362b6e77d5ebab405f3f9f60f1aadc809ccfe3`.
Its binary SHA-256 is `c4e542b048082003cf2899e0ccc9c021e922558a38ceb9635f4b13699dd62625`.
The fixture owner passed 17 fixture tests, the paused-consumer regression, 12 socket smoke cases, and four actual-close probes.
Local provenance is in `target/http2-program/worktrees/http2-wrapper-fixture/target/wrapper-direct-read-candidate/summary.json`.
This artifact has not passed a new complete peer suite.
The [paired report](http2-performance/wrapper/direct-read-458b2bcd/) records nine trial groups and matching allocation controls.
Its commit is `2a65d8e4a690975564104f3a78c697448eaee8e0`.
All workload assertions passed, and buffered-copy samples fell from 350/8,791 to zero/8,574.
Connection-live and cleanup allocation counters and plateaus matched the baseline.

The generic large-body group improved by 1.52 percent.
The broader generic interval included no change, while cold generic C128 empty exchanges regressed by 2.93 percent.
Some unchanged native-path controls also showed nominal regressions, without an established cause.
The candidate is therefore rejected for general integration at this checkpoint.
The accepted source retains the measured baseline, and the candidate and its evidence remain available separately.
CPU2 was released after the experiment.

This result distinguishes removing a sampled copy from establishing a useful overall improvement.
It does not justify weaker workload assertions, larger resource budgets, or extra trials selected to obtain a favorable result.

### Final bounded allocation experiment

The next experiment targets the common single reverse scope membership, with a general multi-scope fallback.
The measured ledger attributes 341,115 native and 329,414 generic allocations to the current reverse-membership vectors at duplex cohort 32.
Growth and deallocation account for about 2.0 percent native and 1.6 percent generic user samples.
Those costs are smaller than the complete waiter path and do not include all registry hashing.

The experiment starts from accepted runtime repair `a4af94fd`, without the rejected direct-read change.
It must preserve weak ownership, cancellation generations, deduplication, migration, and destruction outside runtime borrows.
Candidate `cd81114e8dd8596ca709106271bfbd21bb542e74` implements empty, inline-one, and vector-backed multiple memberships.
It remains separate from the accepted integration and excludes the rejected direct-read change.
Retirement detaches the membership storage before callbacks.
Hashing, scope creation, completion settlement, and cancellation-target ownership remain unchanged.

On the measured x86_64 target, the new enum and old vector both occupy 24 bytes.
Waiter and completion object sizes remain unchanged in debug and release configurations.
Controlled scope/wait tests observed zero/one/two memberships in 27/4,623/1 retired waiter generations.
A mixed native/generic wrapper test observed 609 zero-member and 16,786 single-member generations.
These diagnostics are absent from the final runtime code and do not establish a production traffic distribution.

The implementation owner passed focused runtime, HTTP/1, and HTTP/2 suites in debug and release.
Independent review found no significant issues in `a4af94fd` through `cd81114e`.
From a clean detached checkout, all 35 focused scope/wait tests passed in debug without default features plus virtual clock and release all-features.
The review covered multi-scope growth, weak ownership, deduplication, generation isolation, migration, reentrant destruction, and pending originals.
It did not measure performance or force kernel cancellation-acknowledgment order.
The rebuilt fixture source is `c9b9ecbc07e40f0804fd3a1e5b2c6fe11c014e30`.
Its binary SHA-256 is `4f2b8aa2eadde6b9bc36b4ed03b0da5fbb4fc39259815c71a136bdba0c289a1a`.
The fixture owner passed 17 fixture tests, the paused-consumer regression, 12 socket smoke cases, and four actual-close probes.
Local provenance is in `target/http2-program/worktrees/http2-membership-fixture/target/membership-fixture-evidence/summary.json`.
This artifact remains separate and has not passed a complete new peer suite.
Paired performance measurements remain in progress.
The new performance and fixture worktrees start from the accepted baseline, without the rejected direct-read change.
CPU2 is leased only to the measurement owner until this experiment completes.
No broader registry rewrite or third optimization is part of this bounded follow-up.

### Initial wrapper client fixture

The client fixture uses the public native wrapper, not the direct core executor.
Its source is `00005698dcbd2f15d1fed3e2fb0e94b4f3db1862`, with wrapper source `b9bd24458f64ea852bfef633f3202c9aa42eb463`.
The release binary SHA-256 is `b82bfd11e269bd087c61edb4af1fcdbce1e1dbb100a2068b3a416f0e087b44d6`.
Local evidence is in `target/http2-program/worktrees/http2-wrapper-fixture/target/wrapper-fixture-evidence/summary.json`.

Seventeen selected flow and protocol cases passed.
Separate early-200 and early-413 probes received the full upload without application cancellation.
The nine fixture tests also passed after parent integration.
These results do not qualify the server or the complete client contract.

The reduced-window upload and canonical early-response cases failed before response delivery.
The wrapper reported `Error::Send` with `ConnectionFailed`.
The peer diagnosis identified strict startup-window enforcement, not a demonstrated wrapper flow-control defect.
The fixture does not parse raw SETTINGS frames to reconstruct readiness.

The public API also limits the fixture observations.
Informational responses are not observable, and send errors can hide the independent retirement outcome.
A failed `Client::send` exposes neither an admitted stream identity nor a separate retirement handle.
`HeaderMap` does not preserve global trailer occurrence order across distinct names.
The fixture rejects unsupported observations instead of inventing successful outcomes.
The API and peer contract need separate assessment before full qualification.

### Wrapper startup diagnosis

Peer commit `9dfd57a8b9ded9035a73b4101e38e87ba10bd6c3` records the bounded diagnosis.
Local evidence is in `target/http2-program/reports/wrapper-00005698-startup/summary.json`.
The trace captured 49,152 upload bytes before any server bytes, within the default 65,535-byte credit.
The Python peer rejected these bytes against its reduced window.
The strict Go peer separately rejected 16,384 bytes before acknowledgment of its 1,024-byte window.
Neither failure demonstrates that the wrapper exceeded known credit.

A bodyless warmup synchronized a later upload with the reduced window.
A separate Python peer used public SETTINGS-update APIs to account for data already in transit.
Both variants completed exact 131,087-byte echoes and graceful closure.
Neither variant is the unchanged cold-first-request case.
This evidence does not justify a new production readiness API.

After synchronized startup, the reset-zero diagnostic preserved the 413 response, receive completion, sibling completion, and graceful closure.
The wrapper still hid the authoritative retirement outcome behind `Error::Send` with `Reset(0)`.
That observation remains a qualification failure, not successful full retirement.

RFC 9110 section 5.3 makes field order across distinct names insignificant.
Trailer comparisons must preserve every occurrence and the value order for each case-insensitive name.
Global wire order is a stronger contract than HTTP requires.
The peer diagnostic includes negative controls for missing values and reordered same-name values.
Fixture repair `ba0310d687bb990292ddc11d5e604eef8fd330ae` removes the rejection of multi-name trailers.
Its regression preserves repeated identical values, per-name order, and literal commas through outgoing construction and incoming projection.
The frozen `00005698` binary remains unchanged. This repair has focused test coverage, not a new complete peer qualification.
