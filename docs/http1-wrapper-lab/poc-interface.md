# PoC 3: eager full-body admission

## Question and initial finding

Can the core interface remove a transport boundary that a conventional wrapper cannot otherwise avoid?

The existing request and response commands accept a body length, but not ready payload storage.
The core queues a head write.
Producer demand remains ineligible until that write settles.
The wrapper boxes `OutgoingBody::full` as a one-item stream and waits for demand before it polls that stream.
Thus, a small full body needs separate head and payload writes even when both are immediately available.

This experiment starts from `90b7ff60`.
The final comparison requires the common base that the parent supplies.
Wrapper production changes and benchmark timings remain pending until that base and a timing slot are available.

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

For a client short write that completes metadata but not the payload, the core arms the existing upload deadline.
The normal progress and settlement paths refresh and clear that deadline.
The combined operation cannot reveal kernel progress before its completion.
The existing head deadline therefore guards an outstanding first write until the executor reports progress.

`source_finished` can precede the combined write.
It permits the wrapper to discard the producer, not the operation-owned payload.
`body_sent` returns storage only after settlement.
This also preserves the distinction required by forwarded receive leases.
The initial wrapper fast path is limited to owned `OutgoingBody::full` storage.

## Planned wrapper path

The wrapper retains full bytes as an explicit body representation instead of a boxed one-item stream.
After successful metadata submission, it attempts eager admission for that representation.
If admission rejects the command, the wrapper retains the returned buffer for ordinary demand.
It does not infer HTTP suppression or continue policy.

Custom streams remain boxed and demand-driven.
The wrapper still accepts forwarded frames through the established source path.
The PoC does not combine eager admission with a new forwarding or cancellation policy.

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

## Remaining gates

1. Rebase the core experiment onto the final common base.
2. Adapt the conventional wrapper full-body representation.
3. Run focused wrapper and core tests in debug and release.
4. Run formatting and both required workspace Clippy commands.
5. Run the unchanged shared benchmark during the assigned timing slot.
6. Record observed results, code size, and the final keep-or-reject decision.
