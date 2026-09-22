# HTTP/1 optimization session: retained changes and lessons

## Status at recording

This record was written with `eff194a590eb` (receive replay) as the parent of the
working change. The retained history includes the four changes below. Callback-
driver prototyping and promotion also occurred during the session, but their
revisions are **not ancestors of this checkout**. The current root API still uses
`next`. Their absence does not establish why the branch was changed or whether
that architecture would be faster.

Const-generic client/server specialization, semantic-boundary metadata batching,
the explicit awaiting-request receive phase, and observation facilities were
already present at the start of this investigation. Do not count their historical
measurements as new gains from this session.

For application to other FSMs, use the [reusable performance checklist](../fsm-performance-lessons.md).

## Retained improvements

All performance figures are workload-specific reductions in core benchmark elapsed
time, measured against each change's immediate baseline—not network speedups.
Do not add these percentages or treat them as a measured combined improvement.

| Change | Implementation | Observed result and limits |
| --- | --- | --- |
| `7c101976` — Avoid unnecessary header scratch initialization | Move header descriptors out of the common metadata path. Use standard `MaybeUninit` with httparse's safe request/response APIs; initialize descriptors only for standalone trailer parsing. | About **10–14% lower time for 1 MiB chunked workloads**. Small-message results were mixed/near unchanged. No new dependency or application-side unsafe code. |
| `cd4899b8` — Bulk metadata scanning | Scalar delimiter scanning plus one bounded span append, rather than per-byte Vec mutation and completion checks. | **22–31% lower small fixed-message time** across the tested role/callback/timer configurations. Large chunked results were roughly unchanged. |
| `aec4e26e` — Count reuse and single-allocation heads | Reuse the request field count; return serialized byte totals from existing header validation; compute checked exact head lengths and allocate once. | Count reuse alone saved about **3% on small clients with timers**. Combined changes saved **9–13% on small workloads with timers**. Heads use one allocation and zero reallocations. Large-body results were mixed, with a small regression in one incremental comparison. |
| `eff194a5` — Preloaded receive replay | Add opt-in replay fixtures and a benchmark-only equal-length buffer-swap helper. Preserve operation identities, ranges, read fragmentation, ownership, and callback ordering. | Removes incoming transport payload copies from timing. A preliminary run measured roughly **23–25 us replay vs 37–42 us copy-in** for large chunked exchanges. This is a better-isolated benchmark, not a production networking optimization. |

### 1. Header scratch: phase-local work, not a different collection

The common `process_metadata` path initialized 128 header descriptors (4 KiB on
the measured target), including for chunk-size lines and chunk delimiters. A large
chunked exchange visited 131 metadata sections; 129 did not parse header fields.
Disassembly retained the initialization loop, and profiling identified its cost.

An empty ArrayVec would not directly satisfy the old initialized-slice parser API.
httparse 1.10.1 already supplied safe uninitialized-storage APIs for heads, so no
new container dependency was needed. Trailer parsing retained initialized storage.
Tests cover zero, exact-capacity, and over-capacity header counts and parsed contents.

The session's [scratch timing evidence](evidence/header-scratch-comparison.json)
is archived here so the result does not depend solely on `/tmp` files.

### 2. Bulk scanning: preserve semantic and error boundaries

The retained scanner handles split CR/LF pairs and empty trailer sections, preserves
pending-deadline boundaries, stops before bodies/following messages, and retains
exact error precedence and cursor advancement. Differential tests compare it with
a simple byte-at-a-time oracle over short exhaustive inputs and long/binary fragments.

A `memchr` candidate was also measured. It slightly improved small-message results
but regressed several chunked cases by about 2–3%, so scalar was retained.
Long/many-field and adversarial-fragmentation performance was not comprehensively
qualified, even though those inputs have correctness coverage.

See [bulk scanning](bulk-metadata-prototype.md) and its linked raw samples.

### 3. Head construction: reuse existing knowledge

Request budget validation and accepted-state accounting now use one computed field
count. Validation's existing serialized-byte total is reused for exact head sizing.
Checked arithmetic includes generated framing, Expect, connection fields, and CRLFs.
Formatting and per-write defensive bounds checks remain; there is still one request
field scan and no connection-level output-buffer cache.

Allocation probes confirmed zero head growth reallocations, versus the preceding
client's three and server's two. A chunked exchange still allocates a separate
5-byte terminator, and informational/final-head coalescing is separate work.
Tests cover size boundaries, overflow, validation precedence, generated fields,
suppressed bodies, and rejection without accepting state changes.

See [head construction](head-construction.md) for separate count-only, combined,
and incremental comparisons, including the regression/uncertainty details.

### 4. Replay: isolate the FSM without changing its workload

The copy-in driver remains as a transport-inclusive control. Replay preloads fixed-
capacity buffers outside timing and restores them to their fixture slots after use.
Both modes retain Vec input storage, read counts/sizes, credit, short writes, and
exchange reuse. Tests compare bytes, statistics, and ordered core events, including
fragmentation and 1,024 reused exchanges. No copying/allocation/reconstruction of
receive fixtures happens in the timed replay loop.

Replay has a different working set/cache pattern and potentially large setup memory
for tiny read limits. Never use its swap hook with outstanding real I/O or registered
buffers, replace segmentation with one giant read, or subtract modes to claim an
exact transport copy cost. Re-establish baselines when the harness changes.

Commands and caveats: [FSM CPU benchmarks](../../kimojio-fsm-http1/README.md#fsm-cpu-benchmarks).

## What the investigation established

- There is no universally dominant transition. Large fixed bodies spread work
  across several actions; chunked traffic repeats metadata/body cycles; short
  writes favor Write; advancing timers can make Deadline notifications most frequent.
- Transition regularity is workload/scheduling dependent. Deterministic synthetic
  sequences do not justify unconditional continuations across I/O or credit boundaries.
- `next`/`advance` was very hot, but inline/assembly attribution showed much of its
  cost was metadata work rather than dispatch. At an intermediate revision, small
  metadata accumulation alone accounted for roughly 27–34% of sampled CPU.
- A final transition depends on much more than lifecycle, timer notification, and
  body receipt. Simply recomputing it in setters can move or multiply the work.
  No net cache/boxed-executor speedup was demonstrated.
- Large-body profiles included a dominant copy in the **benchmark driver**. That
  led to a separate replay mode, not an unjustified rewrite of production ownership.

[The profiling report](next-opportunities.md) records measurements at its stated
revision. Its hotspot percentages are historical, not a fresh profile of the final
retained combination.

## Callback-driver experiments: explored, not currently retained

Session checkpoints:

- `vuzkrron` / `2878f3ca`: optional command-driven callback API prototype.
- `kqvwrvyu` / `f4c31604`: separate promotion experiment, after `jj new`.

The prototype exposed construction, update/update_with batches, and resume instead
of next. It used nine coalescing priority work flags, preserved Option<Output>
suspension, and reused the existing action executor. The promotion made that API
the default, moved the old selector into a test-only oracle, routed legacy adapters
through a compat facade using the same scheduler, and migrated the benchmarks.
Correctness and build checks passed in those experiments, including the 474-schedule
ownership model in additional callback modes. **No performance improvement was
measured for the architectural change.** Neither revision is retained in the
checkout recorded at the top of this document.

Preserve the design lessons even without adopting that architecture:

- Batch eager head/body admission and related completions before callbacks.
- Make adapter context available before dispatch so accepted IDs can be registered.
- Preserve pending work before yielding; notification-only outputs still need resume.
- A partly successful batch can return both an error and an owned callback output.
- Keep priorities, original completion ownership, and terminal handoff authority.
- Avoid recursive dispatch and queued borrowed metadata; audit every wake dependency.
- Distinguish a compat facade, a test oracle, actual adapter migration, and the path
  benchmarks really exercise. An API rename or successful test run is not a speedup.

## Follow-up candidates, not completed optimizations

Reprofile the retained code in both replay and copy-in modes before choosing the
next change. Candidates include eliminating the remaining serialized-field scan,
specialized stack integer/chunk-prefix encoding, cheap early guards in
`continue_due`, and local `PrepareBody -> Write` fusion. Each needs its own paired
measurement and ordering/ownership tests. Longer headers, tiny fragments/chunks,
advancing deadlines, and actual adapter/runtime workloads remain important controls.
