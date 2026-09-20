# HTTP/2 qualification ledger

## Scope and status

This ledger records bounded evidence for the scope in [the composition design](http2-composition.md).
It does not claim an exhaustive RFC proof.
The Kimojio HTTP/2 wrappers are not implemented.
Final performance measurements and wrapper command-admission research remain in progress.

The current socket qualification covers the direct HTTP/2 engine.
Separate models cover the HTTP/1 plus HTTP/2 composite.
Socket interoperability does not establish runtime-wrapper cost or cancellation behavior.

## Qualified socket snapshot

| Item | Identity |
| --- | --- |
| Integrated source | `2829297806a6d7326ff5d5bc2e5b4b8a71264ae5` |
| Core | `64a884e6a95c9b2937b463d1861fd3a526eb8cdb` |
| Composite and fixture adaptation | `b724e43cff9dd75409afbece6ffda1f984065be0` |
| Independent peers | `82c24b76043d3cc38addc4acd312f132bf115d01` |
| Fixture SHA-256 | `16faaf7f1cb1bacbfc217a86a2e79be4b2f42e2fde4f3bcb72da0f10a269be63` |
| Local evidence | `target/http2-program/reports/native-28292978-summary.json`, revision 1 |

The peer suite checked the fixture hash before and after execution.
Both roles passed all 48 flow cases and 16 protocol cases.
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

The reset-discard case observed 33,829 connection-credit bytes and zero reset-stream credit.
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
Its 15 tests include protocol selection, hard abort, wrong-owner recovery, and late completions.

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

The final timing and profile run uses frozen integration `5505efc4`.
Its results are pending.
Earlier timings must not be presented as measurements of that later source.

## Lessons

Independent peers, explicit outcomes, and negative controls exposed failures that successful byte counts concealed.
The overlap regression demonstrated why eventual completion is not a substitute for bidirectional progress.
Long-lived allocation measurements distinguished bounded growth from a leak.
The unsuccessful copy experiment prevented an unsupported optimization claim.

Artifact coordination was less successful.
Repeated messages and mutable build paths caused redundant runs and confusion about source identity.
Later qualification used immutable paths, exact hashes, explicit source revisions, and one designated candidate.
Old failures remain negative controls rather than evidence against repaired code.

The next implementation gate requires final performance evidence and unambiguous command-admission behavior for wrapper queues.
Runtime wrappers then need their own ownership, cancellation, interoperability, and performance evidence.
