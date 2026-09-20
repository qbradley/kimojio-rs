# Wrapper experiments and design constraints

## Separate experiments

All four experiments descend from common baseline `0bd3e950f4b0aedb552fbfd8fab96139ef399bb9`.
Each experiment retains its own jj change.
The production decision requires matched measurements, not independent timing claims.

| Experiment | Source commit | Mechanism | Main limit |
| --- | --- | --- | --- |
| Minimum operation overhead | `4eeb643addb60b387120fc62350165d1e5f97577` | Two reusable pinned native operation slots replace transport workers and channels | Generic streams still need their existing worker backend |
| Minimum code | `06eb49be439e42b2f5e126b93d82339209e4b52b` | A buffered transaction directly drives native futures and the FSM | Not equivalent to the streaming wrapper |
| FSM interface | `451f61b503fd60348e5cffedf33a26351533f46a` | Eager owned bodies share one scatter write with queued metadata | Automatic fusion changes timeout boundaries |
| Profile-guided scheduling | `a75433ce021b8908a1f9c482284281406bacd2e0` | Runnable turns inspect channels and cancellation state without temporary waits | Blocking turns still need wake registration |

The direct-slot experiment keeps original operations alive in reusable `Pin<Box<Option<F>>>` storage.
Each future owns its operation and descriptor reference.
Cancellation requests do not destroy the future.
Completion releases the descriptor reference before actual async close.
The implementation adds no unsafe code or dynamic dispatch.

The minimum-code experiment has 660 production-path lines, including shared metadata helpers.
It removes streaming delivery, application channels, body-demand queues, and lease-return queues.
Incoming bodies accumulate into a new complete-body vector for each exchange.
Applications receive complete transactions, not response heads or incremental leases.
Outgoing payload production still copies bounded fragments on FSM demand.
Whole-body buffering replaces part of the coordination work rather than eliminating equivalent work.

That smaller API lacks application streaming, duplex, custom transports, outgoing Expect support, and independent server shutdown.
Its deadlines do not run between client calls or during handlers.
Descriptor destruction does not report close errors.
Its benchmark rows remain useful lower-scope comparisons, not evidence of production equivalence.

The profile experiment preserves input rotation, turn budgets, deadline polling, and source-capacity gates.
A runnable turn cannot suspend.
A blocking turn registers its ordinary wake sources before suspension.
Two additional scheduling experiments reduced allocations but did not improve timing consistently.
The experiment therefore retains only the measured runnable probes.

## A scatter write changes observable progress

Eager admission retains separate owned metadata and payload buffers in one operation.
The core still owns framing, Expect policy, limits, byte counts, and receipts.
Rejected admission returns the original payload for ordinary demand.
Forwarded and fallible stream sources remain demand-driven.

Combining writes changes more than syscall count.
The client upload deadline starts at eager admission rather than after a separate head completion.
The server also loses a separately observable head completion that previously refreshed its body deadline.
Generic write-all transports report completion only after the entire combined write finishes.

For example, a server deadline at tick 10 previously refreshed when its head completed at tick 9.
A payload completion at tick 11 then succeeded.
With a combined generic write, no completion arrives before tick 10, so the same schedule can time out.
Gating client fusion alone does not preserve server behavior.

Production adoption therefore requires an explicit opt-in for full-body coalescing.
The default must retain the existing head/body completion boundaries.
An unboxed ready-body representation can remain independent of that opt-in.
The core's explicit eager command does not justify silently changing the wrapper's default timeout contract.

The independent review found no confirmed premature receipt, payload lifetime, suppression, or duplex-abandonment defect.
It required additional late-positive-completion and pre-submission cancellation cases.
It also required eager duplex output with independently held or abandoned input leases.
The interface-contract follow-up retains the original experimental source for comparison.

## Build and measurement discipline

The parent freezes candidate binaries before comparison.
It rebuilds interface, profile, and direct-slot candidates with the same explicit release settings.
Each revision has a separate target directory.
The original frozen executable remains unchanged.

A revision alone does not identify an executable.
Feature unification can change runtime code even when wrapper sources do not change.
The profile experiment caught this contamination and repeated its affected comparisons.
The parent records build commands, compiler identity, and binary hashes.
Allocation-instrumented and profile-instrumented elapsed times never enter ordinary timing summaries.

The complete matched matrix is `target/wrapper-lab/all-pocs-comparison.json`.
It contains every run, command, binary identity, payload count, failure, median, and range.
The shared host remains a source of variability even with fixed CPU affinity.
The results describe complete client/server exchanges on one Unix socket pair, not production TCP throughput.

## Matched results

The complete matrix passed all 300 executions: ten configurations, six workloads, and five trials.
The table gives median microseconds per complete client/server exchange.
The JSON report retains every trial and its range.

| Configuration | Empty | Small | Both small | Large | Chunked | Fragmented |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Original stream | 28.039 | 41.668 | 52.323 | 134.698 | 2063.572 | 360.891 |
| Common stream | 29.233 | 46.818 | 53.570 | 139.774 | 2154.573 | 421.747 |
| Common native | 32.671 | 43.270 | 57.273 | 129.135 | 2114.199 | 367.649 |
| Minimum-code native | 14.567 | 19.770 | 24.909 | 75.242 | 1207.223 | 199.486 |
| Interface stream | 30.048 | 39.833 | 39.088 | 145.711 | 2129.684 | 419.901 |
| Interface native | 30.680 | 32.572 | 37.314 | 128.172 | 2201.169 | 357.915 |
| Profile stream | 27.554 | 38.905 | 52.392 | 123.979 | 1898.649 | 327.519 |
| Profile native | 28.615 | 40.077 | 46.191 | 113.411 | 1776.501 | 313.412 |
| Direct-slot stream | 29.810 | 47.206 | 58.474 | 149.121 | 2168.281 | 422.442 |
| Direct-slot native | 24.395 | 34.917 | 46.056 | 112.984 | 1721.284 | 285.456 |

The direct-slot native path improves every median against common native.
Runnable probes also improve every native median and every generic median in this matrix.
These changes remove different mechanisms, but their gains need not add.
The generic direct-slot configuration retains workers and shows no consistent improvement.

Eager interface changes mainly improve fixed small bodies.
Their streaming results do not show a general improvement.
The native small-response median falls from 43.270 to 32.572 microseconds.
The bidirectional small-body median falls from 57.273 to 37.314 microseconds.
These results use the original experimental timeout policy, not the conservative production default.

The buffered API is fastest in this matrix.
Its narrower contract prevents replacement of the streaming wrapper.
Its result demonstrates the remaining cost of independent driver progress, leases, incremental delivery, and cancellation surfaces.
It does not prove that each remaining cost is necessary.

Some trial ranges are wide.
For example, original empty exchanges range from 28.026 to 43.619 microseconds.
Common native empty exchanges range from 28.275 to 39.321 microseconds.
The preceding 240-run comparison supports the same broad mechanisms, but individual rankings vary.
These medians support candidate selection, not precise causal percentages.

## Selected design and implementation plan

The production candidate combines reusable native slots, runnable probes, and unboxed ready bodies.
Full-body coalescing remains an explicit opt-in.
The buffered minimum-code API remains an experiment, not a production export.

The interface-contract follow-up is `4129a197dfc04fdaddd7335d2e5f5046ba089b78`.
It defaults `Config::coalesce_full_bodies` to false for both roles.
It adds deterministic deadline matrices and the ownership cases required by independent review.
Existing callers that use `Config::new` retain their previous deadline boundaries.
External structure literals need the new configuration field.

Implementation proceeds in a separate change:

1. Combine direct native slots with backend-aware runnable probes.
2. Retain ordinary blocking registration and input rotation for all backends.
3. Integrate the hardened ready-body representation and explicit coalescing policy.
4. Add an explicit benchmark flag and configuration metadata for coalescing.
5. Repeat ownership, cancellation, deadline, duplex, and generic-transport regressions.
6. Run the independent peer suites against the combined implementation.
7. Compare default and coalesced modes against the original, common, and separate experimental controls.
8. Measure allocation slopes and a fresh profile of the final frozen binary.
9. Review the combined source and revise any change that lacks measured benefit or preserves the wrong contract.

The largest integration risk is treating native operation slots as channels.
Native slots already retain their operation futures.
They need direct polling, not channel probes or additional temporary registrations.
Application channels still benefit from runnable probes.
Neither path can suspend without the wake sources that its pending work requires.

This selection is a measured implementation plan.
The combined wrapper still needs its own evidence before acceptance.
