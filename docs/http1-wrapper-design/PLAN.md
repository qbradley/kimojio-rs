# Wrapper architecture experiments

Baseline: `478820c8`. Benchmark source and policies remain unchanged for comparisons.
Timing excludes instrumentation; freezes use optimized + debug=2 builds with no
extra runtime features. Shared host, CPU13, no exclusivity claim. Keep baseline,
individual PoCs, combined candidate, allocation slopes, and rejected results.

## Runtime investigation

- Native I/O already uses two reusable pinned future slots and driver-local cancel
  flags. It has no per-I/O worker channels or token allocations to remove.
- A RingFuture allocates/rents an Rc<Completion> at construction, submits only on
  first poll, and updates its waker on pending polls. Completion recycling requires
  exclusive ownership; cancellation ACKs keep a target owner until their CQE.
- SleepFuture owns a boxed timespec. Replacing a submitted sleep creates timeout
  and cancel SQEs and eventual CQEs, even when only a later logical deadline is
  required. Owned timer storage permits nonblocking drop; borrowed I/O may require
  synchronous drain on drop. Do not conflate either with ordinary callback cost.
- AsyncChannel is a one-item Rc-owned slot, not an OS channel. WaitFuture lazily
  allocates Rc<WaitData> on a pending poll. Drop unregisters it and retires all I/O
  scope memberships. Reset and set can wake tasks. Persistent/reused registrations
  must preserve cancellation generations and wrapped-waker behavior.
- next_input reconstructs waits every call and scans ten rotating lanes. The
  runnable try_recv fast path already exists. A blocked scan can register losing
  waits before finding a ready completion. Previous worker-backend two-pass and
  shutdown-only persistent-wait experiments were mixed/rejected; remeasure rather
  than treating fewer allocations as proof of improvement.
- The core Ports API permits direct delivery, but the wrapper turns each callback
  into a large Event, then moves it to State::event. Similarly, input selection
  returns a large Input. These are wrapper choices, not inherently required by FSM.

## Alternative proposals / PoCs

A. Logical/physical timer separation: defer allocation until poll; reuse an earlier
   physical wake for a later logical deadline. Immediately replace a later wake
   when the logical deadline moves earlier. Recheck/rearm on obsolete early wake.
   No changed expiration time or core deadline identity. Test real and virtual clocks.

B. Ready-first selection: in the existing rotation probe native completions and
   channels without registering unrelated waits; only register after a complete
   no-ready pass. Never double-poll producer/handler futures. Compare cost of the
   extra pass with allocation/registration reductions on the current native backend.

C. Reusable receive/wait lanes (or runtime waiter pooling): retain pending waits
   across unrelated input selections instead of merely probing twice. More state
   and lifetime complexity; no raw pointers or leaked registrations. Test canceled
   scopes, channel closure, tasks/wakers, and inactive exchange replacement. Only
   retain if timing, not just allocation counts, supports it.

D. Direct transport dispatch through Ports: accept read/write capabilities straight
   into IoDriver from the core callback while yielding the same logical boundary.
   Avoid a giant operation-containing Event on that path without changing callback
   ordering, completion order, core ownership, or batching policy.

E. Explicit I/O batching: preserve the coalesce_full_bodies opt-in and measure it
   with A–D. New streaming batching needs producer/receipt/deadline contracts; do
   not silently poll sources ahead, conflate partial acceptances, or change defaults.
   A shared immutable body API / returned-buffer recycling can avoid Vec source
   copies but is a distinct public-API proposal, not a benchmark fixture trick.

## Selection rule

Retain a small compatible combination only after per-PoC tests and paired timings.
Reprofile the combination; re-evaluate remaining costs. All benchmarks must validate
payload, both endpoints, no reconnects, cancellation/shutdown settlement, and feature
identity. Document assumptions falsified as well as supported. If necessary propose
new FSM contracts separately rather than hide behavior changes in a fast path.
