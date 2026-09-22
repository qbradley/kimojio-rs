# Remaining FSM/runtime architecture alternatives

These are proposals for review, not implemented APIs or measured gains. The
implemented wrapper improvements and counterexamples are in [README.md](README.md).

## What is actually inherent?

HTTP framing/validation, bounded buffering, partial-write accounting, backpressure,
and original-operation settlement are necessary. Allocation per notification,
a kernel timeout per logical refresh, a fresh event waiter after unrelated work,
and turning every core callback into a large enum are not inherently required.
The compatible changes delivered ~26–28% native improvements without changing the
FSM. Direct transport callbacks were legal with the existing Ports interface but
were not a substantial win in their tested form. Removing ownership guarantees
or replacing enums with dynamic closures does not by itself remove required work.

### 1. Clock-aware bounded drive / separate wake hints from semantic events

**Problem demonstrated:** observe_time only advances the core's clock. The caller
must separately learn and expire its deadline. A pending initial deadline can
already be due before the caller observes DeadlineChanged. Suppressing that yield
issued an illegal read in the tested PoC. A getter before next is insufficient
by itself: consuming the first byte of a reused head can create a new zero-time
head deadline *inside* next.

**Candidate contract:** `drive_at(now, budget, ports)` applies current authoritative
expiry before selecting each internal transition, including newly created due
phases. Return a compact progress summary / next desired wake separately from
application notifications. The external timer merely supplies a wake; it does not
own the authority to expire an old deadline identity. Keep standalone completion
APIs for existing callers, or add an explicit completion/command batch with a
well-defined time/expiry ordering.

Benefits to test: safe coalescing of wake hints, fewer wrapper turns, fewer clock
reads and event moves. Required decisions: precedence of a completion versus an
already-due deadline, exact first-byte timing, informational/continue/upload
policies, and command-batch atomicity. Late positive write progress must still be
accounted and original storage returned even if timeout wins logically. Preserve
log ordering and explicit cooperation budgets. This is a contract change, not an
unreviewed implementation fast path.

A runtime alternative is a per-thread indexed timer queue and one kernel wake,
with generation-tagged cancellation and bounded live storage. It can amortize
many connections' timers, but needs I/O-scope cancellation semantics, wrapped
wakers, virtual-clock agreement, and no lazy-deletion history growth. The local
wrapper timer delivered the largest gain with far less scope; build/measure a
central service only when many-connection evidence justifies it.

### 2. Connection-local readiness inbox rather than N disposable selectors

Use bounded per-lane slots and a readiness bitset, with one driver notification
and stable per-lane wake state. Native I/O retains original operations in its
existing pinned slots. Commands, credit returns, handler/source readiness, and
shutdown update the inbox rather than reconstructing many wait futures.

This can reduce both registration churn and idle-lane scans. Preserve rotated
fairness, source eligibility after core notifications drain, one poll per custom
future per pass, and no lost wakes at suspension. Kimojio's yield futures explicitly
wake their Context waker, which is necessary for wrapped per-lane wakers. Native
wake_task has a direct-vtable optimization; wrapped wakers take a more expensive
fallback, so a readiness mask is not automatically faster.

Possible implementations: wrapper-owned mailbox using existing events; an additive
runtime owned receive/registration API; or a runtime-provided local wake adapter.
Do not substitute a plain foreign channel that bypasses I/O-scope cancellation.
Keep callbacks/destructors outside registry/TaskState borrows. The ready-only
second-scan PoC and global waiter pool both illustrate why operation-count and
allocation-count reductions require end-to-end evaluation.

### 3. Compact receive/forward leases before general operation boxing

OutgoingData's forwarded-body variant embeds a BodyChunk, which embeds a full
BodyOp. Even ordinary Vec output pays for that largest variant in WriteOp,
WriteResult, and other envelope layouts. This is a representation cost, not HTTP
framing work.

Candidates: compact internal accepted-body metadata with one authoritative
connection identity; a unique reusable receive-storage lease shared across read,
body delivery, and forwarding phases; or boxing only a cold/large variant.
A receive lease might provide more leverage than boxing every outgoing operation,
because it also shrinks forwarding and release messages. Per-frame boxing may
instead add allocator work. A stable receiving slot must outlive the source core
when forwarded elsewhere and may only recycle after the original destination
write receipt returns it. Rejected commands, stale IDs, uncertain acceptance,
queued final heads, and cancellation cannot duplicate or reclaim it early.

The preceding write-slot experiment did not establish a speedup. It is evidence
against assuming indirection wins merely from smaller handles, not a proof that
all compact lease representations are unhelpful. Measure actual wrapper envelope
sizes and moves, not just the core's borrowed-slice instantiation.

### 4. Transactional credit return and bounded write batches

A body release followed immediately by a grant repeats state/identity access and
creates a wrapper handoff. A core command that returns the lease and replenishes
credit can share validation. Specify partial success: returning storage cannot be
undone merely because credit is no longer legal after cancellation/retirement.
Zero consumption must still withhold credit; aggregate credit remains bounded.

For output, a bounded batch of frame descriptors can amortize demand, callbacks,
and native writev while retaining zero-copy slices. It must preserve per-buffer
receipts and distribute partial/unknown acceptance across framing and payload
prefixes. Producer polling must not run ahead of revocation or configured memory
limits. Streaming trailers/source completion and receive-lease forwarding cannot
be treated like a known full body. Existing coalescing is deliberately opt-in
because it changes timeout progress granularity; do not silently generalize it.

## Proposed order if further work is approved

1. Establish many-connection and latency-sensitive workload baselines in addition
   to the current single pair; validate the retained empty-case tradeoff.
2. Specify and test clock-aware drive and release-with-credit contracts against
   independent step/reference traces, including due deadlines and late results.
3. Prototype a connection-local readiness inbox on the native backend only; retain
   generic stream workers and their write-all/unknown-progress behavior as control.
4. Reprofile before choosing compact input leases or bounded write batches. Gate
   each by byte/identity/cancellation oracles, live-memory bounds, and paired timing.

No proposal requires abandoning sans-I/O composition. What needs improvement is
clarity of authority and granularity at the adapter boundary, not the mere fact
that the protocol is a state machine.
