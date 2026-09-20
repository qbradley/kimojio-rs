# HTTP/1 wrapper optimization program

## Objective

The priority order is performance, a useful conventional API, then maintainability.
The production wrapper must retain HTTP correctness, duplex progress, cancellation safety, and bounded ownership.
The core remains synchronous and runtime-independent.

The starting revision is `05545524` (`rsvqkltq`).
Experiments must not change existing unrelated worktrees or bookmarks.
Each prerequisite, PoC, and final implementation has a separate jj change.
Git commits in isolated worktrees supply independently reviewable jj revisions.

## Acceptance criteria

- Repeated requests use the same established connection without replay or reconnect.
- The benchmark checks complete payloads, exchange counts, reuse, and normal shutdown.
- Cancellation works through conventional future combinators and preserves original I/O settlement.
- Outgoing storage can retain an incoming lease instead of requiring a payload copy.
- An explicit duplex policy permits safe reuse after both message directions complete.
- Early rejection retains its existing abandonment and cancellation behavior.
- All four PoCs use equivalent workloads and disclose API or scope differences.
- The final design follows measurements, not source size or predicted optimization.
- Correctness tests, native interoperability, formatting, and both Clippy modes cover the final implementation.

## Sequence

1. Establish an end-to-end keep-alive benchmark and freeze the starting implementation.
2. Correct runtime cancellation compatibility, add explicit duplex policy, and add lease forwarding in independent changes.
3. Integrate the common prerequisites and establish a second shared baseline.
4. Build four independent PoCs from that baseline.
5. Compare the PoCs, review their failure paths, and record the selected design and implementation plan.
6. Implement the selected wrapper in a separate change.
7. Repeat correctness and performance measurements and publish the evidence and remaining limits.

## PoCs

| Experiment | Main question |
| --- | --- |
| Minimum operation overhead | Can direct operation slots and one-shot transport remove avoidable work from each read, write, and body frame? |
| Minimum code | What is the smallest complete wrapper using existing primitives and the current protocol contract? |
| FSM interface | Which interface changes remove adapter coordination or improve direct callback execution without moving protocol policy outward? |
| Profile-guided iteration | Which measured costs dominate the existing wrapper, and which independent changes actually reduce them? |

Each experiment records its hypothesis, implementation, supported surface, results, and rejected alternatives.
No experiment can improve a result by omitting validation, shortening a response, reconnecting silently, or abandoning cleanup.
An intentionally narrower PoC must report that limit rather than claim production equivalence.

## Measurement controls

Each revision uses a separate `CARGO_TARGET_DIR`.
Only Criterion result data can move between build directories.
Timing runs are serialized and use fixed CPU affinity.
Build and test processes use CPUs 8-31 when timing or profiling is active.
Affinity does not reserve CPUs on this shared host.

The initial benchmark uses a connected native socket pair and a client/server pair in one Kimojio runtime.
It isolates wrapper and runtime overhead from TCP connection setup and external load-generator behavior.
Network interoperability and TCP measurements remain separate acceptance checks.
Workloads include empty and small responses, larger bodies, and streaming forwarding.
Connection setup, warmup, measured exchanges, and shutdown have separate boundaries.

Measured outputs record the source revision, compiler, workload, counts, elapsed time, and failures.
Allocation instrumentation and profiling are separate from timing.
Profile findings refer to the exact frozen binary, not addresses from a later build.
Final comparisons include both the original implementation and the integrated prerequisite baseline.

## Design risks

Raw operations must preserve stable kernel-visible storage until original completion.
An incoming lease held by a write must not prevent the completion that releases that lease.
Callback suspension remains separate from operation completion.
Cancellation can lose a race to successful partial progress.
Explicit duplex reuse must not reinterpret incomplete or abandoned uploads as complete.
Fewer lines or allocations do not establish lower latency.
Root scheduling budgets and bounded queues must remain effective under continuously ready work.

## Completion

All four PoCs remain separate changes.
The [comparison](COMPARISON.md) records the measured selection and compatibility constraints.
The selected implementation combines native slots, runnable probes, and ready bodies.
Full-body coalescing remains an explicit opt-in.
The [assessment](REPORT.md) records the accepted source, measurements, correctness evidence, and remaining limits.
The [final profile](FINAL-PROFILE.md) includes one additional measured inlining change and deferred alternatives.
