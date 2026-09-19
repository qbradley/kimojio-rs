# HTTP/1 composition implementation plan

## Goal and status

This plan implements the [family-wide FSM pattern](fsm-composition.md) through HTTP/1.
HTTP/2 is deliberately outside this work.
Its complexity must not obscure the first concrete ownership and composition contracts.

The work supplies a standalone HTTP/1 client/server FSM and two independent consumers.
A static-file application FSM uses a direct `rustix-uring` driver.
A conventional Kimojio wrapper uses the same HTTP/1 implementation.
After the core ownership review, a WebSocket FSM and broadcast-chat example extend the composition.
Their implementation can overlap final wrapper debugging and review.

The plan is an implementation commitment, not a claim of completed functionality.
The final report records actual results, changes to the plan, and incomplete work.
Existing HTTP/2 changes and alternative adapter experiments remain intact.
The [implementation assessment](http1-fsm-report.md) records the resulting changes and evidence.

## Change topology

```text
main
  |
  +-- HTTP/1 FSM foundation
      Includes fsm-composition.md and this plan
      |
      +-- Static-file application FSM and rustix-uring driver
      |
      +-- Conventional Kimojio HTTP/1 wrapper
      |
      +-- WebSocket FSM and broadcast-chat example

Integration change
  Parents: completed static-file, Kimojio, and WebSocket branch tips
```

The foundation starts directly on `main`.
The three consumers are siblings, not a linear chain.
The WebSocket branch can start after the core ownership review and initial consumer evidence.
The user approved this overlap with remaining wrapper debugging.
The HTTP/1 design checkpoint still blocks final integration acceptance and performance work.
The integration change joins the completed branches and contains cross-branch evidence and the final report.

Each branch can contain additional focused changes where review requires them.
Corrections to shared HTTP behavior belong in the foundation.
Consumer branches receive those corrections without duplicate protocol patches.
The integration must preserve one shared HTTP/1 implementation.

## Architecture constraints

1. The core contains no async operations, runtime tasks, system calls, sockets, or clock reads.
2. Callback methods return `Option<caller output>` and specialize behavior for each consumer.
3. Callback suspension is independent of acceptance, input consumption, and operation completion.
4. Outstanding operations own resources independently of a mutable borrow of the complete machine.
5. Read and write progress can overlap without concurrent mutation of protocol state.
6. The FSM owns framing, partial-write cursors, protocol deadlines, reuse decisions, and exchange retirement.
7. Application-specific operations retain their semantic types through composition.
8. The static-file FSM requests file operations without performing them.
9. The outer driver executes unresolved file and transport operations and reports their results.
10. Bounded state and explicit cancellation protect resources without introducing a second protocol engine.
11. Upgrade transfers preserve pending output and unread input.
12. The Kimojio wrapper exposes ordinary async APIs without reconstructing HTTP policy.

The core can reuse existing HTTP/1 parsing and serialization code.
It must not acquire HTTP/2, HPACK, or historical orchestration dependencies through that reuse.
The design remains specific enough to implement without a universal effect framework.

## Phase 1: Research and contract review

Research covers reusable HTTP/1 code, existing Rust conventions, native stream interfaces, and the direct `rustix-uring` API.
Independent work can proceed in isolated worktrees.
The shared API review precedes dependent implementation.

The review resolves:

- Client and server commands, callbacks, and completion types.
- Buffer ownership during partial I/O and application suspension.
- Operation identity, stale completions, and resource-preserving rejection.
- Input consumption, body end, trailers, and EOF.
- Handler/body-source demand and consumption credits.
- Timers, cancellation, graceful shutdown, and close.
- HTTP upgrade and ownership transfer to a later protocol.
- Bounded admission and memory accounting.

Runtime checks include actual ring creation and operation completion.
A disabled or unavailable kernel interface is a blocker for direct-driver evidence.
A fake backend must not substitute for the required `rustix-uring` driver.

### Exit criteria

The API is concrete enough for independent core and consumer implementation.
Ownership and progress have explicit contracts.
The environment can run the required driver, or the report identifies the exact blocker.

## Phase 2: HTTP/1 foundation

The initial change contains the family design, this plan, and the standalone HTTP/1 crate.
It supplies client and server machines over explicit transport operations.
Configuration defines resource limits and time policy.
The caller supplies monotonic time observations.

The supported surface includes ordinary HTTP/1.1 request/response exchanges, fixed-length and chunked bodies, and applicable EOF-delimited responses.
It includes persistence, close semantics, HEAD and bodyless responses, informational responses, and early final responses during uploads.
Unsupported features must fail explicitly or have a documented handoff contract.

Core tests cover fragmented input, short writes, empty bodies, limits, malformed framing, cancellation, and lifecycle transitions.
They assert complete observable sequences and forbidden output.
Existing semantic tests must not disappear merely because the calling convention changes.

### Exit criteria

Client and server use the same ownership and progress conventions.
The core has no runtime or HTTP/2 dependency.
The smallest real exchange and streaming exchange work through typed callbacks and completions.
Required formatting, Clippy, and targeted tests pass.

## Phase 3A: Static-file application FSM

This branch starts from the foundation.
The application composes with the HTTP server FSM.
HTTP callbacks select application work, which can request file operations from the outer driver.

The application supports GET and HEAD beneath an explicit document root.
It handles missing files, unsupported methods, empty files, file metadata, and bounded file reads.
Its path and symlink policy must prevent escape from the document root.
Files that change or end early must produce an explicit outcome without corrupting a subsequent response.

The driver uses `rustix-uring` directly for asynchronous file and transport work.
It does not route these operations through Kimojio or another async runtime.
Synchronous setup operations and unsupported kernel operations must be documented precisely.
They must not hide blocking work inside the FSM.

File handles, socket handles, buffers, and completion identifiers have explicit ownership.
The driver must preserve them until cancellation or completion permits release.
Slow clients must not cause unbounded reads, response buffering, or connection growth.

### Exit criteria

A real client can obtain files through the direct driver.
File open/read/close effects visibly cross the composition boundary.
Keep-alive, partial writes, disconnects, and file failures preserve resource accounting.
The application FSM itself performs no system calls.

## Phase 3B: Kimojio HTTP/1 wrapper

This sibling branch also starts from the foundation.
It supplies an ordinary client API and a server API over explicit connected transports.
The wrapper uses native Kimojio I/O and conventional async handlers and body sources.

The callback implementation differs from the static-file composition.
It can retain operations for asynchronous execution or return caller-defined work.
It must use the same HTTP/1 engine rather than reproduce protocol state.

The initial wrapper includes streaming bodies, keep-alive, limits, cancellation, and explicit transport shutdown.
Pooling, redirects, automatic replay, and a general application framework are outside the initial scope.
Existing DNS and TLS facilities can supply established transports without entering the core.

### Exit criteria

Runnable client and server examples use the wrapper without exposing FSM mechanics to application callers.
Pending writes do not prevent permitted read progress.
An early final response can stop an upload.
Errors, deadlines, and dropped application futures do not strand unrelated work or resources.

## Phase 4: HTTP/1 design checkpoint

Review the actual core, static-file application, and Kimojio wrapper together.
The two consumers must exercise different callback behavior against the same protocol machinery.

The review checks for:

- Adapter-owned framing, lifecycle, or readiness decisions.
- Whole-machine borrows that block unrelated completions.
- Repeated operations after callbacks return `None`.
- Lost internal work before a composite reports quiescence.
- Mandatory queues, copies, or boxed futures at synchronous layer boundaries.
- Hidden unbounded state and ambiguous completion milestones.
- Upgrade limitations that force a second HTTP parser or transport owner.

Correct shared defects in the foundation.
Propagate those corrections to every consumer branch, including an active WebSocket branch.
Amend the family design and this plan when concrete evidence changes the contract.
Record deviations instead of retaining a design that the code cannot support.

### Exit criteria

Both consumers are usable and the ownership model survives their combined requirements.
The WebSocket layer must not conceal an unresolved foundational defect.
Parallel implementation does not waive this checkpoint or the later correctness gate.

## Phase 5: WebSocket FSM and broadcast chat

This branch starts from the HTTP/1 foundation, not from either consumer branch.
It adds the HTTP upgrade composition and an RFC 6455 server FSM.
Extensions are excluded unless separately justified and documented.

The protocol surface includes masking rules, lengths, fragmentation, text validation, binary messages, ping/pong, close, and bounded messages.
Upgrade must preserve bytes received after the HTTP header boundary.
Handshake output and WebSocket output must remain correctly ordered.

The chat example broadcasts every complete application message to all connected clients, including its sender.
Client admission, message sizes, and outbound storage have explicit bounds.
The slow-consumer policy must be documented and tested.
One stalled client must not indefinitely block eligible peers.

The example uses Kimojio raw I/O without the sibling HTTP/1 wrapper.
HTTP, WebSocket, and chat logic remain synchronous FSMs.
Only the outer executor contains futures and performs I/O.
The static-file example still uses `rustix-uring` directly.
This choice preserves the sibling topology without another copy of the direct driver.

The first WebSocket API accepts complete outbound messages and delivers incoming chunks with explicit release.
The chat FSM assembles each incoming message before broadcast.
The protocol FSM accepts caller-owned outbound storage without mandatory reference counting.
The chat FSM shares immutable storage between recipients without a payload copy for each recipient.
Streaming outbound producers and extensions remain outside this first API.

The chat configuration bounds clients, message size, per-client backlog, and aggregate retained storage.
The aggregate bound includes incomplete message assemblies, allocated capacities, queued messages, and in-flight messages.
Queue entries have separate accounting because shared payloads still require per-client metadata.
The implementation must define explicit admission and close outcomes for each limit.
It must not silently discard messages while a client remains an active recipient.

All protocol machines retain the same `next(&mut ports) -> Option<P::Output>` convention.
Finite buffers and operation slots bound internal work.
The root controls cooperative scheduling without a second protocol progress API.

### Exit criteria

Independent WebSocket clients can connect, exchange messages, and close.
The broadcast example works with concurrent clients.
The protocol core remains usable without the example or its runtime.

## Phase 6: Integration and deep correctness

Create the integration change with completed sibling branch tips as parents.
Resolve manifests, shared documentation, and cross-branch behavior explicitly.
The integrated tree must build without patches hidden in external worktrees.

Correctness precedes performance conclusions.
Tests include:

| Area | Required evidence |
| --- | --- |
| HTTP framing | Fragmentation, conflicting lengths, transfer coding, chunk extensions, trailers, EOF, bodyless responses |
| HTTP lifecycle | Reuse, pipelining boundaries, Expect behavior, early final responses, cancellation, shutdown |
| File server | Document-root boundaries, missing files, HEAD, changing files, short reads, resource cleanup |
| WebSocket | Upgrade leftovers, masking, fragmentation, interleaved control frames, invalid text, lengths, close races |
| Composition | Selective suspension, pending operations after `None`, sibling progress, typed completion routing |
| Resource bounds | Slow readers/writers, stalled sources, finite admission, retained-buffer accounting |
| Interoperability | Independent client and server implementations in other languages |

Tests must include both sides of supported HTTP exchanges.
An implementation talking only to itself is insufficient interoperability evidence.
Negative cases must check exact outcomes and the absence of forbidden frames or bytes.
All hanging probes require finite outer deadlines.

Required repository checks include `cargo fmt`, `cargo clippy`, and `cargo clippy --all-targets --all-features`.
Targeted tests precede wider workspace checks.
Relevant feature combinations, examples, and documentation receive explicit coverage.

### Exit criteria

No known correctness defect blocks the documented supported surface.
Every remaining limitation has an explicit scope and consequence.
The integrated tree passes the required checks.

## Phase 7: Comparative performance and bottlenecks

Performance work starts after the correctness gate.
The benchmark plan fixes workloads and records environment details before interpreting results.
Comparisons must use equivalent payloads, concurrency, persistence, and resource limits.

Representative workloads include:

- Small HTTP responses with and without connection reuse.
- Large fixed-length and chunked transfers.
- Static files at several sizes, with cache conditions recorded.
- Concurrent clients and deliberately slow consumers.
- WebSocket echo/broadcast with different message sizes and fan-out.
- Cancellation and shutdown under load.

Measurements include throughput, latency distributions, allocations, retained memory, CPU cost, and runtime or kernel scheduling activity.
Native composition and the conventional wrapper receive separate results.
Independent implementations provide comparison points where their workload and semantics match.

Profiles identify actual hot paths before optimization.
Optimizations require repeatable before/after measurements and retained correctness coverage.
No callback microbenchmark or test count substitutes for an integrated result.
Unavailable profilers, noisy hosts, or incomparable peers must be recorded as evidence limitations.

### Exit criteria

The report contains reproducible commands, workload definitions, raw-result locations, and measurement limitations.
Claims distinguish measured improvements from hypotheses.
Known bottlenecks have supporting profiles or bounded experiments.

## Phase 8: Final assessment

The integration change contains `docs/http1-fsm-report.md`.
The report includes:

- Final jj topology and component locations.
- Supported functionality and explicit exclusions.
- Changes to the original design and their reasons.
- Correctness and interoperability evidence.
- Performance comparisons and bottlenecks.
- Resource ownership and cancellation assessment.
- Successes, failures, unresolved issues, and recommended next work.

The report must assess both native composition and conventional Kimojio reuse.
It must state whether the evidence supports extending the pattern to HTTP/2.
The next step is not automatically another protocol or a general framework.

## Parallel execution and ownership

Core design, direct-driver research, and interoperability preparation can proceed independently.
After the contract review, consumer implementation can proceed in isolated worktrees.
Shared API changes require coordination before consumers adopt them.

The main repository owns jj topology, plan updates, integration, and the final report.
Each implementation worktree has a bounded component owner.
Changes return to their intended jj branch rather than accumulating in one unrelated working change.
Temporary processes and generated experiment files require explicit cleanup.
