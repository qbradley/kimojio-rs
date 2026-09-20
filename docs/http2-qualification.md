# HTTP/2 qualification ledger

## Scope and status

This ledger records bounded evidence for the scope in [the composition design](http2-composition.md).
It does not claim an exhaustive RFC proof.
The core qualification gates are complete for the recorded scope and snapshots.
The metadata-admission repair passed independent review, socket qualification, and source-specific measurements.
Kimojio HTTP/2 wrapper implementation is in progress.
The native client and server have focused runtime coverage.
Selected wrapper interoperability cases passed. Full interoperability and wrapper performance qualification remain pending.

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
| Independent wrapper socket fixture and peers | Client/server fixture implemented, full suite and explicit startup profile in progress |
| Wrapper performance and final review | Pending |

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
The native fixture now uses these observation APIs. Full independent peer qualification remains in progress.
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
Generic socket fixture implementation remains in progress.

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
