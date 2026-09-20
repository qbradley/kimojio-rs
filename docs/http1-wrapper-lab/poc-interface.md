# PoC 3: eager full-body admission

## Question and initial finding

Can the core interface remove a transport boundary that a conventional wrapper cannot otherwise avoid?

The existing request and response commands accept a body length, but not ready payload storage.
The core queues a head write.
Producer demand remains ineligible until that write settles.
The wrapper boxes `OutgoingBody::full` as a one-item stream and waits for demand before it polls that stream.
Thus, a small full body needs separate head and payload writes even when both are immediately available.

The independent core experiment started from `90b7ff60`.
Its delta now sits on the frozen common base `0bd3e950f4b0aedb552fbfd8fab96139ef399bb9`.
The wrapper adaptation preserves that base's native transport, explicit duplex selection, and lease-aware abandonment policy.
Benchmark timing remains pending until the parent assigns a timing slot or runs the matched comparison.

## Alternatives

| Alternative | Decision |
| --- | --- |
| Concatenate payload into the encoded head | Reject. This copies payload and replaces exclusive storage ownership. |
| Issue two writes from the adapter without core admission | Reject. This bypasses demand, continue gates, receipts, and cancellation policy. |
| Poll arbitrary streams before demand | Reject. A fallible stream can suspend or fail, and suppression must not poll it. |
| Add full request and response APIs with body payloads | Defer. This duplicates metadata commands, duplex selection, and transactional validation. |
| Expand ordinary `send_body` admission silently | Reject. Existing callers can rely on its capacity rejection during head output. |
| Add `send_body_eager(SendBody<W>)` after the existing metadata command | Select. It is optional, returns rejected storage, and shares normal payload admission checks. |
| Generalize head, chunks, and trailers into a universal output action | Reject. The existing typed operation is sufficient. |

The selected method accepts a complete, nonempty, fixed-length body.
It requires `end: true`, idle write ownership, and a final head with no accepted prefix.
The existing `send_body` path retains framing, range, capacity, byte-limit, sequence, and continue-gate checks.
On rejection, the core restores the original head without a callback or external mutation.

The narrow fixed-length scope is deliberate.
Chunked bodies and trailers retain the established demand path.
Unknown and fallible streams do not gain speculative polls.
The application still selects conservative or duplex server responses through their existing methods.

## Operation representation

`WriteStorage::HeadBody` owns encoded metadata, `SendBody<W>`, and its `BodyId`.
It does not concatenate payload with metadata.
The existing three-slice view exposes metadata, payload, and an empty third slice.
The operation owns both allocations for the complete I/O lifetime.

The existing body variant already stores a command, identity, inline chunk prefix, prefix length, and chunk flag.
The new variant fits inside that storage layout.
A unit test compares the original and experimental enum sizes for `Vec<u8>` and borrowed slices.
The public operation fields and completion types do not change.
The change adds no core heap queue or executor operation.

## Invariants and failure paths

- Metadata starts through the existing request or response command.
- Eager admission transfers exactly one complete body into exactly one queued write.
- The core never invokes a port between removal and restoration of the head during admission.
- A rejected command retains the original buffer and range.
- HEAD and bodyless suppression reject eager attachment through core framing state.
- A client continue gate rejects eager attachment until the regular path permits upload.
- Source completion remains distinct from storage return.
- The combined cursor counts wire bytes.
- A body receipt subtracts the metadata prefix and clamps the accepted count to the payload length.
- Unknown-progress failures retain lower-bound acceptance.
- Cancellation retains the original operation until its completion.
- No cancelled prefix is replayed.
- Successful retirement still waits for both directions and external ownership.

At eager client admission, the core arms the existing upload deadline.
The normal progress and settlement paths refresh and clear that deadline.
The combined operation cannot reveal kernel progress before its completion.
Thus, the upload deadline includes the combined metadata instead of waiting for a separate head completion.
This is stricter than ordinary admission, but avoids an unguarded payload write when the generic write-all transport hides partial progress.
The existing head deadline remains independent.

`source_finished` can precede the combined write.
It permits the wrapper to discard the producer, not the operation-owned payload.
`body_sent` returns storage only after settlement.
This also preserves the distinction required by forwarded receive leases.
The initial wrapper fast path is limited to owned `OutgoingBody::full` storage.

## Implemented wrapper path

`OutgoingSource::Ready` retains full bytes as an explicit body representation instead of a boxed one-item stream.
`OutgoingSource::Stream` retains the existing boxed custom stream.
Empty bodies use `Ready(None)`.
After successful metadata submission, it attempts eager admission for that representation.
If admission rejects the command, the wrapper retains the returned buffer for ordinary demand.
It does not infer HTTP suppression or continue policy.

The driver checks retained allocation capacity before the eager attempt.
An oversized allocation stays on the ordinary path.
This preserves suppression without an early adapter error.
If normal demand reaches that allocation, the existing frame-limit check rejects it.

Custom streams remain boxed and demand-driven.
The wrapper still accepts forwarded frames through the established source path.
The PoC does not combine eager admission with a new forwarding or cancellation policy.
The `continue_request_body` marker stays unchanged on the body.
Both metadata commands and their duplex choice complete before eager admission.

On successful admission, the active exchange needs no outgoing source.
The core owns the payload in the combined write.
The existing `source_finished` handler therefore discards no live payload.
The existing `body_sent` handler receives the original storage.

## Expected cost change

For each eligible full body, the fast path can remove:

- One one-item stream allocation.
- One producer-demand callback and source-poll boundary.
- One separate head write operation and completion.

The core still encodes metadata into its existing allocation.
The payload retains its original allocation.
The wrapper gains a body-representation enum and an eager-admission branch.
The core gains one method, one storage variant, and shared body-settlement logic.

These are operation-count expectations, not measured latency claims.
The shared `keepalive_bench` must establish the performance result without changes to its validation or fixture.
Large, chunked, and fallible streams serve as compatibility controls rather than expected beneficiaries.

## Observed wrapper effects

A wrapper unit test submits `OutgoingBody::full` through the actual eager-admission helper.
It observes exactly one write callback, zero producer-demand callbacks, and the original payload pointer in the second scatter slice.
Source completion precedes that write, and the body receipt follows settlement.

The fallback test retains the original allocation across Expect rejection and the retained-capacity check.
It also proves that eager selection never polls a custom stream.
The body representation test distinguishes unboxed full storage from a boxed fallible stream.

The new socket integration test runs three requests on one connection for each transport backend.
It covers an 8192-byte full request and response, HEAD suppression, and an Expect request that uses normal demand.
The socket send buffer is 4096 bytes.
The complete existing wrapper suite also passes, including native transport, duplex ownership, forwarding, cancellation, and settlement cases.

No latency or throughput improvement is claimed yet.
The parent owns the final matched comparison.
The benchmark fixture and its CLI remain byte-for-byte unchanged from the common base.
The release executable accepts the common `--native`, `--duplex`, and `--copy-forward` flags.

## Added code and tradeoffs

The core production files add 153 lines and remove 23 lines relative to the common base.
These counts include a 32-line layout regression module in `operations.rs`.
The wrapper production files add 172 lines and remove 10 lines.
These counts include the adapter unit tests in `driver.rs` and representation assertions in `body.rs`.
Standalone regression files and documentation are additional.

The public wrapper API does not change.
The core adds one optional command on each role rather than separate eager request, response, and duplex-response constructors.
The two-command sequence requires submission before the next drive, but a rejection preserves normal fallback.
This is simpler than duplicating metadata acceptance and error ownership across atomic full-message constructors.

The representation enum increases the local outgoing-body representation.
It does not increase transport operation storage.
That local cost also applies to stream-backed bodies, so the matched streaming controls remain important.
The extra enum branch replaces dynamic dispatch for full bodies, but does not remove dynamic dispatch from custom streams.

The experiment remains a candidate, not a recommendation to merge.
The final decision must consider small full-body gains alongside large-stream and duplex regressions.

## Core regression evidence

The initial focused command uses CPUs 8-31:

```sh
CARGO_TARGET_DIR=/workspace/kimojio-rs/target/wrapper-lab/build-interface-poc \
taskset -c 8-31 cargo test -p kimojio-fsm-http1 \
  --lib --test eager_body --test source_lifecycle
```

Result: 17 unit tests, five eager-body tests, and four source-lifecycle tests passed.
The eager cases cover every partial-write boundary, both exact and lower-bound cancellation receipts, and a second request on the same connection.
They also cover HEAD/bodyless suppression, chunked fallback, Expect gating, upload timeout, early rejection, pointer identity, and producer completion before storage return.

The complete core suite passed in debug and release: 105 tests and four doctests per profile.
Formatting and both workspace Clippy commands passed.
Workspace Clippy reported only the existing example `question_mark` warning and two `byte_char_slices` warnings in runtime tests.
Core Clippy passed for all targets and features with `-D warnings`.
No benchmark timing ran during this independent core phase.

## Final-base validation

All builds and tests use CPUs 8-31 with this fresh target directory:

```text
/workspace/kimojio-rs/target/wrapper-lab/build-interface-poc-final-0bd3e950
```

| Command | Result |
| --- | --- |
| `cargo fmt --all` | Passed |
| `cargo test -p kimojio-http1 -p kimojio-fsm-http1 --quiet` | Passed: 106 core tests, four core doctests, 61 wrapper tests |
| `cargo test -p kimojio-http1 -p kimojio-fsm-http1 --release --quiet` | Same tests passed |
| `cargo test -p kimojio-http1 --all-features --quiet` | Passed: 63 wrapper tests, including virtual-clock deadlines |
| `cargo test -p kimojio-http1 --example keepalive_bench --quiet` | Four fixture tests passed |
| `cargo build -p kimojio-http1 --example keepalive_bench --release --quiet` | Passed |
| `cargo clippy --quiet` | Passed with the existing example warning |
| `cargo clippy --all-targets --all-features --quiet` | Passed with the existing example and two runtime-test warnings |
| `cargo clippy -p kimojio-http1 -p kimojio-fsm-http1 --all-targets --all-features --quiet -- -D warnings` | Passed without warnings |

The benchmark executable is `release/examples/keepalive_bench` in that target directory.
Its SHA-256 is `906eb65d817dcc4d842336b213212044d63c9695cd0ea6e18db326a97147d032`.
These checks did not run a timed benchmark.

## Remaining gates

1. Run the unchanged shared benchmark during the assigned timing slot, or use the parent's matched comparison.
2. Record measured latency and throughput with the frozen source and executable hashes.
3. Make the final keep-or-reject decision.
