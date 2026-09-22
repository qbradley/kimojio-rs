# HTTP/1 no-copy replay: perf follow-up

## Scope and method

Measured the tree at `cfb7aa75` (working child `2fbf7d01` was empty): all three
previous optimizations are present. No production or benchmark source was changed
for this investigation. New files in this directory are evidence and analysis.

Environment: Intel Xeon Platinum 8370C VM, rustc 1.98.1 (48a229cea), perf
6.6.139.1. Timing and profiling processes were pinned to logical CPU 8, **without
an exclusive CPU reservation**. Hardware cycles/instructions are not exposed by
this VM; `cpu-clock:u` works. Thus this report makes no IPC, cache-miss, or branch-
misprediction claims. See `perf-capabilities.txt` and `binaries.sha256`.

Timing (uninstrumented default optimized bench profile, metrics disabled):

```sh
taskset -c 8 cargo bench -p kimojio-fsm-http1 --bench roundtrip \
  --features bench-internals -- http1_replay --noplot \
  --save-baseline perf-followup
# Repeat with --save-baseline perf-followup-repeat.
```

Both runs used Criterion's 3-second warmup, 5-second measurement, and 100 samples.
The replay fixture swaps prefilled receive buffers; payload copying and full
wire/payload validation are outside the timed loop. Outgoing payloads are borrowed
scatter/gather slices. Core metadata copies, owned-operation moves, and simulated
executor bookkeeping still occur inside timing. This is not a network bandwidth
benchmark. Qualification tests passed: six tests in `workload-tests.txt`.

### Fresh timing results

Central estimates, microseconds per exchange (each size is per direction):

| Case | First run | Repeat |
| --- | ---: | ---: |
| fixed 128 B / client / continue | 0.87675 | 0.87408 |
| fixed 128 B / client / yield | 0.86276 | 0.86533 |
| fixed 128 B / server / continue | 0.90922 | 0.90830 |
| fixed 128 B / server / yield | 0.85051 | 0.85540 |
| chunked 1 MiB / client / continue | 22.584 | 20.270 |
| chunked 1 MiB / client / yield | 21.861 | 19.536 |
| chunked 1 MiB / server / continue | 22.649 | 20.482 |
| chunked 1 MiB / server / yield | 21.674 | 19.788 |

The first chunked run had wide intervals and many outliers, so it was repeated.
Both runs are retained in `benchmarks.txt` and `benchmarks-repeat.txt`; do not
interpret the difference as a code change. The repeat chunked intervals are
[20.164,20.402], [19.438,19.662], [20.376,20.613], [19.662,19.942] us respectively.

### Profiles

Built with only debug information added to the optimized benchmark:

```sh
CARGO_PROFILE_BENCH_DEBUG=2 cargo bench -p kimojio-fsm-http1 \
  --bench roundtrip --features bench-internals --no-run

# Repeat for all eight exact replay case names; sequential, not concurrent.
perf record -e cpu-clock:u -F 997 --call-graph dwarf,16384 --delay 1500 \
  -o /tmp/http1-perf-followup/CASE.data -- taskset -c 8 \
  target/release/deps/roundtrip-f8b1e690de642ca9 \
  --bench http1_replay/chunked_1mib/server/continue --profile-time 12

perf report --stdio --no-children --percent-limit 0.5 --sort symbol \
  --call-graph none -i /tmp/http1-perf-followup/CASE.data | rustfilt
perf script -i /tmp/http1-perf-followup/CASE.data \
  -F time,period,ip,sym,symoff,dso --inline | rustfilt
```

There are 9,331–10,706 samples per case, with zero reported lost samples. Samples
start 1.5 seconds after launch to exclude fixture initialization and untimed
copy/replay validation. Percentages below are weighted by recorded sampling
period. They describe CPU attribution, **not guaranteed recoverable time**.

DWARF unwinding in this build often stopped after the immediate caller or produced
invalid older frames. Self-PC and inline attribution are usable; do not treat
these as complete inclusive call graphs. Separate corroborating builds used
`RUSTFLAGS='-C force-frame-pointers=yes' CARGO_PROFILE_BENCH_DEBUG=2`:

- `fp-chunk-server`: frame-pointer recording of chunked/server/continue.
- `fp-fixed-client`: DWARF recording of fixed/client/continue with frame pointers.

Those corroborate the paths but change code generation; their percentages are
not mixed into the main table. The fixed-client corroborating profile places
about 21% inclusively under request encoding (including validation and appends).

`*.self.txt` files contain demangled physical self-symbol reports. The
`*.stacks.summary.txt` files break down inline regions and immediate copy callers;
`sample-summary.json` contains the table data. Main self regions are disjoint in
the following table; libc-copy includes both core and driver calls.

| Case | Selector | Metadata scanner | httparse | Header semantics | Outgoing field validation | libc copy |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| fixed/client/continue | 7.92% | 8.23% | 11.68% | 7.83% | 7.33% | 12.04% |
| fixed/client/yield | 4.71% | 9.10% | 12.09% | 7.24% | 7.42% | 11.79% |
| fixed/server/continue | 10.25% | 11.11% | 13.31% | 7.64% | 4.74% | 7.77% |
| fixed/server/yield | 5.19% | 12.86% | 13.54% | 7.67% | 5.55% | 9.71% |
| chunked/client/continue | 14.27% | 2.63% | 0.67% | 0.29% | 0.27% | 9.83% |
| chunked/client/yield | 9.98% | 2.88% | 0.83% | 0.30% | 0.28% | 11.52% |
| chunked/server/continue | 15.74% | 3.12% | 0.71% | 0.28% | 0.18% | 8.97% |
| chunked/server/yield | 10.93% | 3.10% | 0.74% | 0.23% | 0.27% | 10.55% |

Header semantics groups `framing`, `host`, `has_token`, `expect_continue`,
`upgrade_protocols`, `connection_fields`, and `field_present`; some serve both
input and output. Scanner means `metadata_span`, not the removed `strict_lines`
release scan. We did not attribute the whole inlined `Endpoint::next` symbol to
scheduling: it also contains metadata processing and body delivery.

## Top three opportunities

Ranked as engineering priorities, not as one workload-independent sum of CPU time.

### 1. Stop materializing and moving large operation objects repeatedly

This is the clearest new finding from perf plus assembly. The replay excludes
payload copying, but 9–12% of chunked samples are still in libc's copy routine.
`prepare_body` itself is another 3.9–6.5% (mostly constructing/moving state, not
hexadecimal conversion; `encode_chunk_size` is only 1.0–1.5%).

Sizes for the actual benchmark types (`operation-sizes.txt`):

- `WriteOp<&[u8]>`: 168 B; `WriteCompletion<&[u8]>`: 184 B.
- `BodyOp<Vec<u8>>`: 104 B; `BodyCompletion<Vec<u8>>`: 112 B.
- `SendBody<&[u8]>`: 64 B, including a 24 B ExchangeId.
- OperationId is 32 B and BodyId is 24 B; each qualified ID repeats ConnectionId.

`issue-write.asm` shows a 167 B memcpy while taking/unwrapping `self.output`, then
another 168 B memcpy for the port call. The two call sites account for 2.06% and
2.76% of **all** chunked/server/continue samples (4.82% combined). Other copies
occur in completion transport, read/body operation movement, and metadata.
`copy-call-sites.txt` maps them to coordinator.rs:539 and :543.

The libc PCs initially appear as numeric/unknown symbols. `memcpy` and `memmove`
resolve via IFUNC to libc offset 0x1697c0 on this host; disassembly of that range
contains the sampled copy instructions. This is not DNS/NSS work despite a
stripped-symbol fallback sometimes naming the region `__nss_database_lookup`.

**Actions:**

1. Extend the existing *control-flow* PrepareBody→Write fusion into *data-flow*
   fusion: construct the outgoing operation once, avoiding pending_body→output→
   stack→port round trips when immediately writable. Retain queued storage for
   readiness, partial writes, errors, and cancellation.
2. Use compact internal accepted-body metadata rather than carrying the complete
   public SendBody and several duplicated fully qualified IDs. Reconstruct IDs
   from one authoritative connection identity plus sequences when needed; keep
   cross-connection and stale-operation validation unchanged.
3. Inspect whether targeted inlining/lifetime restructuring eliminates remaining
   ABI copies before redesigning public operations. Do not add per-chunk boxing.

Sequence exhaustion must still return every admitted buffer, and uncertain/partial
write receipts must retain exact semantics. Some sampled copies are in the bench
executor; do not claim all 9–12% as a core-only win or remove its correctness checks.

### 2. Give metadata a contiguous-input fast path and classify fields once

For small exchanges the remaining work is dominated by multiple stages of header
processing: scalar boundary scanning (8–13%), httparse (12–14%), semantic header
checks (7–8%), and outgoing field validation (5–7%). Those are real scans/checks,
not the previously removed header_count. HeadWriter::append still has 3–5% self
cost, plus calls into memcpy for short header fragments.

`receive_metadata` always scans and accumulates into `self.head`, even when the
entire section is already contiguous in the read buffer. It then parses it, and
several codec helpers independently traverse/classify the same fields. For chunks,
size lines and two-byte CRLFs use this general machinery too: receive_metadata's
inlined work is 5.6–6.1%, metadata_span 2.6–3.1%, and chunk_size 2.8–3.3%.

**Actions:**

1. When no prefix is buffered and a full section is present, parse the validated
   span directly from owned receive storage. Keep accumulation for fragmentation;
   explicitly preserve buffer ownership and borrowed-head callback lifetimes.
2. Specialize chunk CRLF and ordinary hexadecimal size lines; keep an extension/
   fragmentation slow path. Avoid staging tiny complete lines through the Vec.
3. Classify headers once and derive framing, persistence, Expect, upgrade, and
   Connection-nominated fields together, preserving validation/error precedence.
   For outgoing headers, consider table-based token validation and an explicit
   prevalidated-header representation for repeated static fields (never cache an
   arbitrary borrowed pointer as proof of validation).
4. Try vectorized delimiter search only above a measured head-size threshold.
   Earlier work found blanket memchr regressed tiny chunk metadata; retain the
   scalar/dedicated short-section paths.

Do not remove strict CRLF validation just because httparse accepts an input.
Malformed framing, duplicate lengths, byte/field limits, first-byte deadlines,
and unread body/pipelined suffix boundaries remain mandatory.

### 3. Amortize scheduler/credit work across streaming fragments

Even after PrepareBody→Write fusion, selection alone costs 14–16% in chunked
continue mode and 10–11% in yield mode. For chunked/server/continue, leaf/inline
breakdown includes 3.22% source_transition, 1.65% continue_due, 1.37%
receive_transition, and 1.19% transmit_transition, besides common selector work.
These costs recur even when most of those conditions cannot have changed.

The unchanged 1 MiB replay produces 65 reads, 66 writes, **128 body deliveries**,
and 64 receipts. Header/chunk overhead misaligns 64 payload chunks with 16 KiB
reads. Every delivery causes release and separate credit grant commands; core
body-delivery and release work are each approximately 4–5% of chunked samples.
See `operation-counts.txt` for freshly executed, validated counts.

**Actions:**

1. Add narrowly guarded streaming continuations that skip full priority selection
   only while its higher-priority dependencies cannot change. ChunkCrlf→Size is
   one candidate internal sequence; stop at every actual callback/deadline/policy
   boundary. No caching of a stale final transition.
2. Consider an optional atomic release-and-replenish-credit command to share
   identity/range validation and state updates without a public callback change.
3. Separate ready/notified facts at authoritative completion boundaries rather
   than re-evaluating all source/continue/I/O predicates after every local step.
   Preserve notification ordering, including early-response upload revocation.

Do not coalesce payloads by copying or change benchmark read/chunk sizes to hide
fragment overhead. The harness round_trip self cost (12–17% in chunked mode) is
not itself a library hotspot; changing its assertions is not an optimization of
HTTP/1.

## What not to prioritize now

- The previous general-formatting and header-recount work is already gone.
- Allocation is not the dominant observed cost: libc copying is mostly object/
  small-metadata movement, not evidence for a new allocator or blanket pooling.
- Sampling shares are not additive potential speedups. Reprofile and perform
  matched A/B trials for each candidate; preserve the replay/normal driver trace
  and byte oracles, both callback policies, and error/ownership tests.

Raw perf recordings, stacks, the summary script, and additional disassembly are
under `/tmp/http1-perf-followup/` (large binaries are not checked into the repo).
The preliminary `test.data` recording used an older executable solely to test
perf access and is **excluded** from every reported result. All eight main
profiles use the current `roundtrip-f8b1e690de642ca9` hash recorded here.
