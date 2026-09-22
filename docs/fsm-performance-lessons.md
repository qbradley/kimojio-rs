# FSM performance lessons from the HTTP/1 study

Use this checklist before optimizing another FSM. The [HTTP/1 session record](http1-core-study/session-summary.md)
separates retained changes, measured results, rejected alternatives, and control-flow
experiments. HTTP/1 measurements are evidence for HTTP/1 workloads, not speedup
predictions for HTTP/2, WebSocket, or their adapters.

**Main lesson:** remove repeated work inside hot actions before replacing the FSM's
control architecture. Separate transport cost from protocol cost, and make ordering
and ownership contracts explicit enough to test every optimization against them.

## 1. Measure the real work, not just the function name

- Trace transition counts, adjacent pairs, and repeating sequences by workload.
  Record suspension/completion boundaries and selector returns of `None` too.
- Do not treat unit-test frequencies as production frequencies. Error/ownership
  tests intentionally overrepresent exceptional cases.
- Count frequency is not CPU cost. A cheap notification can be frequent while a
  less frequent parser/encoder consumes much more CPU.
- Resolve inline stacks and sampled instruction addresses. A hot `next`/`advance`
  symbol may contain parsing, array initialization, copies, and user callbacks.
  It does not establish that the enum match or selector is the bottleneck.
- Distinguish inclusive from exclusive attribution; do not add overlapping totals.
  A sample on a branch instruction does not prove branch misprediction.
- Verify profiler output, not just its exit status. Two collection attempts here
  returned successfully without usable experiments and had to be rerun.
- If hardware profiling is unavailable, userspace clock sampling can still locate
  work. Record that limitation. Requested sample periods may be quantized: do not
  infer CPU descheduling from weighted profiler seconds without independent CPU
  and wall-time measurements.

In HTTP/1, shallow attribution suggested a dispatch problem. Inline/assembly
inspection instead exposed a 4 KiB scratch initialization on tiny chunk metadata,
a per-byte accumulation loop, repeated output-buffer growth, and header rescans.

## 2. Dispatch by semantic phase before preparing scratch storage

Ask whether the shared entry point prepares data needed by only some branches.
HTTP/1 initialized 128 header descriptors even for chunk-size lines and CRLFs.
Moving scratch into the head/trailer branches removed that work from chunk parsing.

When a parser supports uninitialized output storage, prefer its **safe API**:
HTTP/1 uses `[MaybeUninit<Header>; 128]` with httparse's uninitialized-head APIs.
The parser initializes and exposes only the populated prefix. Trailer parsing
retains initialized storage because its public parser API still requires it.

- `ArrayVec` is not a drop-in solution for a parser accepting `&mut [T]`: an empty
  collection exposes zero initialized elements, not its unused capacity.
- Filling placeholders to satisfy that API recreates the initialization cost.
- Do not manufacture initialized slices/references to uninitialized elements or
  use unchecked `set_len` to bypass the parser contract.
- This optimization does **not** permit uninitialized transport input: the FSM's
  Buffer contract still requires initialized bytes and consistent slice views.
- Verify the initialization loop disappeared from the intended path in optimized
  code. Uninitialized scratch can remove stores without removing stack capacity.

## 3. Batch work within a semantic boundary

Bulk accumulation can win even with a scalar scanner. HTTP/1 scans delimiter
positions and appends an accepted span once, rather than updating a Vec and
checking completion after every byte. No SIMD, zero-copy ownership redesign, or
new dependency was necessary for the retained improvement.

Before batching, specify all stop conditions:

- Section/frame/message completion and callbacks that expose metadata.
- Exhausted input, credit, or an outstanding body lease.
- Pending notification boundaries, including first-byte timer changes.
- Malformed input and exact resource limits.
- Cross-direction policy that can revoke further output/demand.

Preserve not only success bytes, but the **first error, consumed cursor, and
retained prefix**. HTTP/1 checks CRLF validity before its byte limit; the bulk
scanner may inspect the first over-limit byte to preserve that precedence, but
must not scan arbitrarily farther. Never consume a following body/message just
because it is already buffered.

A repeated sequence is not automatically fusible. `PrepareBody -> Write` is local;
`Read -> Body -> Read` crosses external completion/credit boundaries. Model those
boundaries explicitly before constructing a specialized continuation.

## 4. Reuse validation/accounting work and allocate for actual output

- Look for sizes and counts computed during validation and then discarded.
- Reuse an accepted count for both budget validation and state accounting.
- Prefer a checked wire-size calculation plus one allocation over repeated small
  growths, when the exact size can be obtained cheaply.
- Count generated fields and framing, not just caller-supplied headers. Include
  suppressed bodies, informational responses, connection rules, and trailers as
  appropriate to the protocol.
- Check overflow and limits **before allocation**, without changing validation
  error precedence or accepting a command before its checks finish.
- Keep defensive writer bounds checks and test predicted size against actual
  output. Avoid allocating each connection's configured maximum by default.
- Measure allocation/reallocation counts separately from timing. No allocator
  instrumentation should be in the performance-comparison binary.

HTTP/1 retained one field scan per request, but eliminated its duplicate scan.
It also reused the field byte total from validation to size heads exactly.
Steady-state head construction became one allocation with zero reallocations.
That is not a zero-allocation exchange: chunk termination and informational-head
coalescing are separate operations and were not eliminated.

## 5. Treat scalar/vectorized and static/dynamic designs as experiments

`memchr` improved some small HTTP/1 cases slightly beyond scalar bulk scanning,
but regressed several chunked cases by about 2–3%. Scalar was retained. Tiny
sections, setup cost, code layout, and common-case lengths matter.

Similarly, a boxed closure/function pointer does not precompute runtime state.
Installing it once need not allocate on every call, but indirect dispatch can
inhibit inlining. Compile-time specialization works best for a small stable domain
such as endpoint role—not arbitrary limits or every combination of runtime state.
Role specialization already existed at the start of this session; it was not a
new speedup delivered here.

## 6. Caching a decision is not the same as eliminating its computation

List the complete dependency set before caching a final transition. Lifecycle,
timer notifications, and receipts only selected the top-level path in HTTP/1.
The final decision also depended on credit, storage ownership, buffered bytes,
producer state, I/O slots, exchange notifications, EOF, and cross-direction policy.

If reads of a selector number R and cache refreshes number U, moving the same
calculation to setters changes roughly `R * selection_cost` into
`U * selection_cost + cache read/write costs`. It wins only if recomputation is
avoided or incremental updates are cheaper. Multiple mutations between drive
calls can make eager refreshes more expensive, not less.

Try local, measured changes first: cheap common-case guards before expensive
checks, appropriate inlining of tiny helpers, or guaranteed local successors.
Readiness bits should be maintained at authoritative semantic boundaries, not as
unreviewed duplicates of many predicates. Repeated polling while unchanged is a
different case from advancing the FSM between every selection.

## 7. Keep two transport models in core benchmarks

Provide both a transport-inclusive mode and a preloaded replay mode.

For replay:

- Build fixtures and allocate storage outside timing.
- Keep the original buffer type, addressable length, read segmentation, valid-byte
  counts, operation IDs, credit, and completion ordering.
- Recycle each buffer into its proper fixture slot; preserve the active slot
  across reused exchanges and prefetched next reads.
- Keep byte-validation, statistics, and ordered-event oracles. Compare copy and
  replay over multiple exchanges and fragmented reads/writes.
- Restrict buffer exchange to a benchmark-only hook. Never change storage backing
  real outstanding I/O or registered buffers.

Do not simply delete a copy and report a successful read of stale bytes. Do not
replace many small reads with one giant buffer and call that the same workload.
Replay changes working set/cache behavior; its faster score is not a production
networking optimization or an exact measurement of copy cost. Keep both modes,
compare revisions within a mode, and re-establish baselines after harness changes.

Preloading full-capacity buffers can be expensive for one-byte reads. HTTP/1's
fixture storage is approximately `ceil(wire_bytes / read_limit) * buffer_capacity`
when the limit is below capacity. Keep adversarial replay fixtures small.

## 8. Preserve these contracts in any event/callback-driver experiment

Callback-driver work was prototyped and separately promoted during the session,
but neither change is in the retained replay-based checkout recorded by the
session summary. No speedup was established. The design lessons remain useful:

- Use explicit command batches when eager admission or joined completions must
  happen before callbacks. Driving after every field assignment exposes unstable
  intermediate state and can change early-response policy.
- Returning `Some(output)` must preserve the continuation. A notification may
  require no new command, so an explicit `resume` (or equivalent) remains needed.
- Lend adapter context to the batch when needed: the accepted exchange ID may
  have to be registered **before** the first write callback, not after dispatch.
- A batch can partially succeed. Do not hide pending owned output behind the
  error from a later command; handle result and output channels independently.
- Prefer bounded/coalescing readiness work over queues of owned operations or
  borrowed metadata. FIFO alone does not preserve protocol priorities.
- Revalidate deferred work after higher-priority actions. Deadlines, receipts,
  cancellation, early responses, and lifecycle changes can invalidate it.
- Install continuations before returning a suspension; use iterative dispatch,
  not unbounded recursive callbacks. Keep reentrant mutation out of callbacks.
- Cancellation requests do not settle the original operation. Body leases and
  writes retain their ownership until the original completion/return arrives.
- Handoff must revoke HTTP authority: no later callback can revive it or close
  the transferred transport.
- A compat facade using a new scheduler is not the same as migrating an adapter
  to the new event API. Test-only oracles, production engines, public API defaults,
  and actual benchmark paths must be distinguished and documented separately.

## 9. Validation and measurement checklist

For the next FSM optimization:

1. Record revision, frozen executable hash, compiler/settings, buffer/port types,
   features, timer policy, workload, and benchmark mode.
2. Cover both directions/roles, small and large units, continue/yield callbacks,
   enabled advancing timers, reuse, fragmentation, and short writes.
3. Use a simple reference implementation or independent model. Compare traces,
   returned storage, identities, exact error positions, and quiescence—not just
   final output. Include cancellation/late completions, deadlines, and handoff.
4. Validate bytes outside timing; retain cheap success/reuse/count checks inside.
5. Measure one change at a time, with paired trials and balanced execution order.
   Check unaffected control workloads for noise/layout effects.
6. Reprofile accepted changes. Hotspot shares change when another cost disappears.
7. Do not add successive percentage savings or infer a total gain without a
   matched end-to-end comparison. Inclusive percentages are not additive either.
8. Report regressions and uncertainty. A shared-host core benchmark is not a
   production latency distribution. Correctness coverage of an input does not
   establish its comparative performance.
9. Preserve results, assumptions, rejected alternatives, and current disposition
   in repository documentation, not only ephemeral `/tmp` artifacts. Check the
   actual checkout before calling an experiment integrated.

## Where to apply this next (hypotheses, not measured findings)

| FSM / area | First investigation | Protocol-specific obligations |
| --- | --- | --- |
| WebSocket coordinator and framing | Profile timer refresh/notification work; audit header/control scratch setup; separate payload masking/UTF-8 work from dispatch | Mask position across fragments, incremental UTF-8, frame/message boundaries, ping/pong/close ordering, leased buffers, original completions |
| HTTP/2 engine, head encoding, HPACK | Profile actual frame/stream paths; audit repeated serialization/counting and scratch setup; use replay controls; measure ready-stream scheduling before redesigning it | Connection and stream windows, CONTINUATION/header-block boundaries, HPACK state ordering, RST/GOAWAY, fairness, per-stream cancellation and ownership |
| Application/composite FSMs | Separate child-machine work from queues, copies, logging, and adapter scheduling | Child quiescence does not imply parent idleness; preserve sibling-ready work and return completions to their original owner |

Start with the measurement and invariant list. Do not transplant HTTP/1's CRLF
scanner, nine-work-item scheduler, or measured percentages into another protocol.
