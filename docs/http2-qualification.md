# HTTP/2 qualification ledger

## Scope and status

This ledger records bounded evidence for the scope in [the composition design](http2-composition.md).
It does not claim an exhaustive RFC proof.
The core qualification gates are complete for the recorded scope and snapshots.
The metadata-admission repair passed independent review, socket qualification, and source-specific measurements.
Kimojio HTTP/2 wrapper implementation is in progress.
No wrapper qualification or performance result is claimed yet.

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
