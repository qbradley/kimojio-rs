# Composable sans-I/O state machines

## Status and purpose

This document records the family-wide design pattern for Kimojio state machines.
It guides future APIs and reviews.
It does not claim that existing crates implement every part of the pattern.
The example signatures describe contracts, not a released API.

The same protocol implementation must support native FSM applications and conventional async frameworks.
The core contains no async operations, runtime tasks, system calls, or clock reads.
The caller supplies external observations and executes external operations.
Allocator behavior is outside the no-system-call contract.

The design must preserve a path to maximum performance in both execution models.
It must not require a task, queue, allocation, payload copy, or dynamic dispatch at every software layer.
Actual performance requires measurements of the complete implementation.

**Each layer requests the capabilities it needs.
Composition resolves those requests without imposing a scheduling boundary.**

## The pattern

The pattern combines:

1. Domain-specific callback ports with optional caller-defined output.
2. One progress mechanism for each machine.
3. Typed operations and completions for work that can remain outstanding.
4. Explicit ownership, consumption, and credit contracts.
5. Composites that expose the same kind of interface as individual FSMs.
6. Interchangeable outer executors for native event loops, async runtimes, existing frameworks, and deterministic tests.

These are shared conventions, not one universal trait or operation enum.
A parser, HTTP connection, gRPC stream, and application server need different semantic interfaces.

## Semantic layers and execution ownership

Protocol relationships remain conventional:

```text
Application semantics
        |
       gRPC
        |
      HTTP/2
        |
       TLS
        |
     Byte stream
```

Commands, data, and results can move in both directions.
Execution ownership sits outside the synchronous stack:

```text
Native event loop                     Async framework
        |                                    |
        |                             Conventional facade
        |                                    |
        +--------------+---------------------+
                       |
                Composite service FSM
                +-------------------+
                | Application FSM   |
                | gRPC FSM          |
                | HTTP FSM          |
                | Optional TLS FSM  |
                +-------------------+
                       |
              Unresolved operations
             read / write / time / ...
```

A composite handles operations that another component can perform.
Only unresolved operations reach its caller.
The outer executor does not need to understand every protocol layer.

| FSM | Representative requested capabilities |
| --- | --- |
| Application | Invoke an RPC, query storage, publish an application result |
| gRPC | Open an HTTP stream, send headers/body/trailers, deliver an RPC message |
| HTTP | Read/write a byte stream, deliver HTTP data, request application work |
| TLS | Read/write encrypted bytes, request external certificate decisions |
| Composite | Expose only capabilities not supplied by its components |

An HTTP byte stream is not necessarily a socket.
A TLS FSM can implement that port and expose encrypted transport operations.
A conventional TLS implementation can supply the same capability through an adapter.

## Selective callback suspension

A callback trait specializes behavior for its caller.
Each callback returns `Option<Self::Output>`.
The caller chooses `Output`.

Illustrative signatures:

```rust
pub trait HttpPorts {
    type Output;

    fn read(&mut self, op: ReadOp) -> Option<Self::Output>;
    fn write(&mut self, op: WriteOp) -> Option<Self::Output>;
    fn request(&mut self, op: RequestOp) -> Option<Self::Output>;
    fn body(&mut self, op: BodyDeliveryOp) -> Option<Self::Output>;
    fn exchange_finished(
        &mut self,
        result: ExchangeResult,
    ) -> Option<Self::Output>;
}

impl HttpConnection {
    pub fn next<P: HttpPorts>(
        &mut self,
        ports: &mut P,
    ) -> Option<P::Output>;
}
```

| Signal | Meaning |
| --- | --- |
| Callback returns `None` | Continue without returning a value to the machine's caller |
| Callback returns `Some(value)` | Suspend at this point and return the caller-defined value |
| Typed completion arrives | Apply the observed result of an outstanding operation |
| Machine returns `None` | No immediately runnable work remains within its documented drive scope |

**A control yield is not an operation completion.**

A callback can register pending work and return `None`.
The machine can then issue unrelated work.
The pending operation completes through its separate completion contract.

A callback can also perform synchronous work and return `Some(value)`.
This gives its caller an iterator-like interface.
Different callbacks can always yield, never yield, or yield conditionally within the same implementation.

Neither `None` nor `Some` means rejection or permission to repeat an issued operation.
Capacity rejection requires an explicit contract that preserves the operation and its resources.
Backpressure must not rely on an ambiguous callback return.

### `Infallible` and unit output

`Output = Infallible` means that the callback cannot produce a suspension value.
`Option<Infallible>` has only the `None` case.
Generic specialization can remove the suspension checks and inline the callback work.

`Output = ()` permits `Some(())`.
It fits a callback that requests suspension without additional data.
The optimizer can also simplify this case, but the type does not forbid suspension.

`Infallible` does not mean that no asynchronous work exists.
A callback can register work with an executor without returning a suspension value.

The library does not force a materialized `Action` enum.
A caller can choose a small enum, completion value, operation, or future as its own output.

### Suspension state

A machine commits the relevant suspension state before it invokes a callback.
A scanner advances past the reported token.
An effectful machine records operation issuance.

Issuance does not imply successful completion.
A write transaction advances only at its defined completion boundary.
Repeated drive calls must not repeat the same outstanding operation.

## Composition

**A collection of connected FSMs must expose the same kind of interface as one FSM.**

A composite accepts commands and completions.
Its drive method exposes caller-defined output from unresolved operations or notifications.
Its caller does not need to know how many machines it contains.

A connector implements one machine's callback port through another machine's commands.
For example, a gRPC body operation can become an HTTP body command.
The connector marks HTTP ready and returns `None`.
HTTP completion later advances the gRPC operation.

One operation can expand into several operations in another layer.
Several operations can also share one transport write.
The component that performs this mapping owns its completion accounting.
The runtime adapter must not reconstruct it.

### Direct synchronous composition

A parser callback can call a decoder, which calls an application visitor.
These components can run in one ordinary call chain.
An optional caller output can propagate through the chain.
Callbacks that return `None` permit continued synchronous work.

This form fits parsing, validation, framing, and immediate application decisions.
It requires no message queue or runtime wakeup between components.

### Bidirectional composition

HTTP, gRPC, and application state also have bidirectional dependencies.
An HTTP event can make gRPC runnable.
gRPC can then produce more HTTP work.

A local ready set handles these dependencies without recursive re-entry.
Commands and completions mark the affected components ready.
The composite drives those components rather than scanning every exchange.

A child returning `None` must not make the composite sleep while a sibling remains runnable.
Internal connectors can return `None` and still create work for another component.
The ready set records that work independently of callback output.

In Rust, a composite can own its machines in separate fields.
A connector borrows only the disjoint fields it needs.
It must not re-enter a machine whose drive call is active.
Literal recursive ownership is not required.

The composite owns wiring, correlation, and readiness.
It must not duplicate protocol tables, transport cursors, or lifecycle policy already owned by a component.

### Immediate completion and fairness

A synchronous executor must not call back into the currently borrowed machine.
It can return a completion as `Some(output)` for application after the drive call.
A composite can instead retain ready completions in bounded local slots.

A cooperative root needs a bounded turn budget.
Budget exhaustion requests rescheduling of runnable work.
It must not look like quiescence or a need for new I/O.
A root that yields through callback output must support that output type.
`Infallible` fits non-yielding connectors, not a root that requires this form of cooperative suspension.

## Operations, ownership, and accounting

### Typed operations and completions

An operation that can remain outstanding needs stable identity and a typed result.
Its identity is scoped to the owning machine instance and operation generation.
It must not silently wrap or alias a live operation.

The machine records outstanding state before it hands the operation to a callback.
The receiver accepts responsibility for eventual completion or explicit cancellation settlement.
A pending operation is not an instruction to block unrelated work.

Wrong-owner, stale, duplicate, and mismatched completions have explicit handling.
Invalid input must not mutate a valid outstanding operation.
Rejected completions must preserve transferred resources.
Expected late completions can release resources without reviving a retired exchange.

Synchronous observations do not all need operation tokens.
Tokens belong where identity must survive deferred completion or concurrent work.
The family does not require a universal token table for every parser callback.

### Borrow now, retain through ownership

Synchronous callbacks can inspect borrowed data without copying it.
Work that survives the callback needs independently valid storage.
An owned buffer, pool lease, or explicitly supported external lifetime can provide that storage.

An operation must not borrow the entire machine across an async suspension.
Such a borrow prevents unrelated completions from entering that machine.
Narrowly scoped borrows remain useful where their lifetimes permit progress.

The interface must not force `Vec<u8>`, an atomic reference count, or concatenated output.
Exclusive leases can move through multiple layers without payload copies.
Sharing, splitting, and retained subranges require an explicit storage contract.
They are not automatically free.

### Partial consumption

A child can suspend after it consumes only part of an offered buffer.
The remaining bytes need an owner and an exact cursor.
The parent must not infer full consumption from callback invocation.

Two valid contracts are exact consumption accounting and ownership transfer of the offered chunk.
With ownership transfer, the child retains the remainder across suspension.
The optional caller output does not replace either contract.

### Distinct milestones and credits

These milestones are not interchangeable:

- A component accepted a command.
- A consumer released input storage.
- A component stopped requesting data from a producer.
- The transport accepted output bytes.
- The application operation finished.

Each API must define which milestone its completion represents.
A transport write completion does not prove peer receipt or peer processing.
Retry decisions must preserve that distinction.

A producer can retain resources that other work needs.
The machine must report when it no longer needs that producer.
This notification does not imply that outstanding writes finished or that their storage is reusable.
For example, an HTTP response can suppress a body while its producer still holds a request-body lease.
Waiting for exchange retirement before releasing that producer can prevent retirement itself.

Completion types must distinguish exact progress from a known lower bound.
A write-all adapter can fail after hidden partial writes.
That failure must not report zero progress or permit replay of bytes whose acceptance is unknown.
The machine retains earlier exact progress and applies its failure policy.

Flow-control credit follows the documented release or bounded-storage contract.
An enqueue into an unbounded channel is not consumption.
A gRPC decoder that retains an incomplete message still owns memory.
Encoded-byte budgets and decoded-message budgets are separate resources.

Per-stream backpressure must preserve sibling progress within the connection budget.
No design can promise unlimited progress after the aggregate budget is exhausted.
Limits must include retained data, pending operations, and framing storage.

### Bounded deferred control work

A resource release can create a protocol obligation before the transport can accept its output.
One queued frame per release can exhaust a control budget even when the consumer promptly releases every body lease.
Bounded body storage alone does not prevent this failure.

Equivalent obligations can accumulate in bounded counters with at most one pending entry per live resource.
The machine creates wire output only when the output slot and framing budget permit it.
Counter bounds, resource retirement, and reset rules remain explicit.
Obligations that require distinct ordering or acknowledgments cannot use this combination without a separate correctness argument.

Consumer release, pending credit, and published credit remain distinct milestones.
The protocol changes its advertised allowance at its defined publication boundary, not at consumer release alone.
Deferred output must preserve partial frames and compression order.
The HTTP/2 credit repair applies this pattern to connection and stream WINDOW_UPDATE obligations.

### Bounded executor registrations

Protocol storage bounds do not bound the executor's cancellation and wakeup registries.
A long-lived connection scope must retain live obligations, not the history of completed operations and obsolete waiters.
Duplicate pending polls must not create duplicate retained registrations.
Registry capacity can include bounded or amortized slack, with explicit growth and shrink rules.

Original-operation settlement and cancellation acknowledgment remain separate ownership obligations.
Retiring a completed registration must not permit premature reuse of an identity still referenced by cancellation.
Callbacks and final resource destruction must occur outside mutable runtime or registry borrows.
Weak reverse memberships must preserve migration across multiple live scopes.

Allocation evidence needs connection-live, post-close, and post-runtime boundaries.
Successful retirement and actual transport close do not prove bounded storage during a long connection.
The [HTTP/2 wrapper assessment](http2-wrapper-report.md) records a runtime-only reproduction and a repair that preserves cancellation safety.

## Cancellation, time, and failure

Cancellation follows semantic ownership, not unconditional recursive transport cancellation.
Canceling one RPC on a shared HTTP/2 connection does not normally cancel its socket read.
gRPC requests stream cancellation.
HTTP decides how this affects queued frames, outstanding writes, and RST_STREAM.

The outer executor cancels only the concrete operations exposed for cancellation.
A cancellation request is not proof that kernel I/O stopped.
Resources remain valid until the original operation reaches a final outcome.
Application completion and resource cleanup can occur at different times.

Cancellation must also cover continuation operations inside an adapter.
A write-all future can submit another native write after a partial completion wins a cancellation race.
Canceling only the native operations that exist at one instant does not cancel that complete future.
The adapter must prevent uncanceled continuation I/O and retain resources until settlement.
A full successful result that wins the race remains successful.

Failures retain their scope.
A failed RPC is not automatically a failed HTTP connection.
A connection failure must produce the required outcomes for dependent operations without adapter-invented lifecycle rules.

Time comes from one caller-supplied monotonic domain.
Each component owns its deadline policy.
A composite can expose the earliest unresolved deadline through one wakeup.
It retains the owning component and generation for completion routing.
The runtime adapter executes the wakeup without interpreting its protocol purpose.

## Maintainable state and coordination

Valid state representation and correct coordination are separate requirements.
An enum can remove contradictory local states without making a global priority scan easy to review.
These rules apply to new machines and to refactors of existing machines.
They do not require a shared implementation crate or a universal transition enum.

### State decomposition

Each fact has one authoritative representation.
Mutually exclusive alternatives belong in an enum.
State-specific identities and resources belong with the state that requires them.
For example, awaiting transport close contains its original close identity.
A terminal connection cannot also have an independent "close pending" flag.

Independent progress belongs in separate components.
Receive parsing, transmit ownership, external I/O, protocol lifecycle, and timers can advance independently.
A product of those components preserves duplex progress without a flattened enum for every combination.
Independent facts can remain booleans, but each boolean needs a meaning that does not depend on check order.

Protocol shutdown and transport settlement are distinct.
A WebSocket close frame is not a transport close.
A producer end is not a completed write.
A cancellation request is not an operation completion.
The state model must retain these distinctions and the original resources.

Local enums cannot exclude every invalid combination across components.
Cross-component invariants need explicit guards and assertions at command, completion, and drive boundaries.
Assertions support transition contracts. They do not replace them.

### Selection, transition, and scheduling

A coordinator has three responsibilities with separate contracts:

| Responsibility | Contract |
| --- | --- |
| Selection | Inspect state without mutation and determine eligible work |
| Transition | Commit the selected state and ownership changes before a callback |
| Scheduling | Choose among eligible transitions using documented dependencies |

Lifecycle dispatch restricts the available transitions before local eligibility checks run.
Normal producer demand must not remain reachable through a transport-termination driver.
Bounded protocol error output needs its own permitted path.
A closed machine permits only its pending terminal notifications.

Local receive and transmit drivers own their eligibility rules.
The coordinator owns cross-component policy, not parser details or duplicate transport cursors.
Long conjunctions are acceptable when each condition has a local resource or protocol justification.
Extracting a predicate name does not remove a hidden ordering dependency.

For each transition, the design records its prerequisites, state changes, resource transfers, callbacks, and effects on other eligible work.
Eligibility checks must not consume notifications or take ownership.
The selected transition performs those mutations.
No external command or completion can intervene between selection and commitment in the synchronous drive.

Semantic boundaries trigger cross-component policy.
Examples include incoming-message completion, final output settlement, peer close, and cancellation settlement.
When drive-time ordering matters, a bounded pending obligation can defer that policy until the next drive.
Such an obligation must name its cause and define coalescing and invalidation.
It must not become a second, contradictory copy of protocol state.

A composite commits the effects of a child command only when the child accepts that command.
A rejected failure notification for a retired exchange does not put a reusable child into termination.
The composite must preserve that distinction when delayed cleanup notifications cross an exchange-retirement boundary.

Not every pair of transitions needs a fixed order.
Independent transitions can commute.
Observable or safety-sensitive dependencies need explicit reasons.
For example, a partial frame must finish before close bytes can enter the stream.
Each machine documents its actual notification order rather than assuming one universal family-wide order.

### Progress and cost

A selected transition that produces no caller output still represents progress.
It must consume work or advance a defined state.
A callback returning `None` resumes selection, including after terminal or handoff notifications.
Only the absence of eligible work permits a blocked return.
Internal cycles require a termination argument or an explicit rescheduling contract.

Private unit-sized transition labels are control flow, not a public operation representation.
They must not contain owned operations, borrowed metadata, or large buffers.
Direct calls and static specialization preserve the callback architecture.
The refactor must not introduce heap queues, payload copies, dynamic dispatch, or repeated resource scans without a demonstrated need.
Performance claims require equivalent workloads and measurements, not shorter functions.

### Review and refactoring procedure

1. Inventory facts, resource owners, public commands, completions, and observable callback sequences.
2. Separate exclusive alternatives from independent progress and independent policy.
3. Record local invariants and cross-component settlement conditions.
4. Define lifecycle-specific transition sets and pure eligibility rules.
5. Record semantic boundaries and correctness-sensitive scheduling dependencies.
6. Separate transition effects from selection without changing public ownership contracts.
7. Exercise competing enabled transitions and each relevant completion order.
8. Compare the resulting implementation with the contracts, not only with the old control flow.

Necessary bug fixes need explicit regression cases.
The previous implementation is compatibility evidence, not the sole specification.
Refactors must distinguish intended callback ordering from incidental source ordering.

A bounded executable model can explore ownership joins, cancellation races, deadlines, and callback yields without reproducing the parser.
Its expected behavior must come from independent contracts, not a copy of the production selector.
The evidence includes exact resource returns, notification counts, forbidden effects, quiescence, and eventual settlement under stated external-completion assumptions.
Comparisons between yielding and continuing callbacks use the same external input schedule.
Schedules must include several external completions between drive calls, not only one completion followed by one drive.
They must also include queued notifications that arrive after logical retirement and a subsequent exchange on the same connection.
The static-server refactor exposed a stale source-failure transition only when file-close failure and final-write success occurred in one batch.
Resource models must include repeated consumer releases while an original write remains outstanding.
The HTTP/2 credit repair exposed control-queue exhaustion under this schedule, despite bounded retained body storage.
Parser corpora, native integration, and independent peers complement this model.
The report states the explored bounds and does not claim a complete proof.

The HTTP/1 [coordination contracts](../kimojio-fsm-http1/README.md#coordination-contracts) provide one concrete implementation.
Other protocols retain their own lifecycle and ordering rules.

## Conventional and mixed integration

An async facade implements ports and exposes conventional methods, futures, and body streams.
It drives the same synchronous machines as a native event loop.
The protocol implementation does not change with the facade.

| Arrangement | Use | Required boundary |
| --- | --- | --- |
| Pure FSM application | Application, gRPC, HTTP, and optional TLS machines | Native executor handles unresolved effects |
| Conventional application | Async methods over a protocol composite | Facade converts commands and completions |
| Mixed stack | gRPC FSM over an existing HTTP client | Semantic HTTP adapter executes HTTP operations |

A single-owner facade can drive the stack inline with concrete futures.
It does not require a separate task or channel.
A shared driver with concurrent handles needs communication across its ownership boundary.
That boundary can use bounded channels without adding channels between every internal FSM.

The design must permit concurrent reads and writes.
It must not require a task per frame, boxed futures, or repeated polling through every protocol layer.
Cross-thread execution can add `Send` constraints at the actual transfer boundary.
Local composition does not require those constraints.

## Alternatives and tradeoffs

| Pattern | Strength | Limitation |
| --- | --- | --- |
| Imperative commands and byte/event pumps | Small and efficient primitives | Integrators can inherit scheduling and lifecycle policy |
| Universal OS-operation interface for every FSM | Uniform outer executor | Higher layers absorb lower layers or lose semantic interoperability |
| Actors or queues at every layer | Isolation and dynamic composition | Scheduling and ownership costs at every boundary |
| Async interfaces throughout | Familiar composition and potentially allocation-free static futures | Runtime polling and cancellation contracts enter every layer |
| Typed ports with selective yields | Synchronous composition and optional suspension | Requires precise ownership and progress contracts |

Typed ports with selective yields are the preferred default.
Actors and async facades remain useful at intentional isolation boundaries.
Async code is not inherently slower.
The design avoids mandatory overhead rather than claiming universal superiority over futures.

Static specialization can increase compile time and code size.
Inlining every function can also hurt instruction-cache behavior.
Concrete callback types preserve optimization opportunities, but measurements must guide actual inlining and storage choices.

## Historical lessons and evidence

The HTTP orchestration experiment at jj change `wxupqrpmkost` added operation-level control without removing enough adapter orchestration.
Its production performance gate failed despite favorable callback microbenchmarks.
The thin-wrapper effort from `xosvtxrrpwyu` through `kwttmxkk` then explored ownership and executor mechanics with fake machines.
These efforts show why both responsibility removal and real protocol integration matter.

The HTTP connection design at `mzkupwtx` and `ypwrwxtr` established callback-driven progress and direct protocol ownership.
This family-wide pattern extends that direction to semantic composition.
It does not assert that the current HTTP API already supplies these contracts.

The JSON tokenizer provides an existing example of selective callback suspension.
Its visitor can use `Infallible`, always yield, or yield selectively.
The scanner advances before each callback and resumes without repeating the event.

A design-time prototype composed an application, record framer, and partial-write machine.
It covered selective yields, generation-checked completions, resource-preserving rejection, and an async facade with pending work.
The measured section allocated no memory after setup and preserved payload addresses.
An optimized `Infallible` fold contained no callback calls or optional-output dispatch.

That prototype was exploratory, not a maintained repository test.
It used a simulated transport and did not establish HTTP/gRPC throughput or production cancellation correctness.
The generated fold was not byte-for-byte identical to a direct loop.

## Review and acceptance criteria

A new family member needs evidence for both isolated behavior and composition.
Review criteria:

1. Each fact has one authoritative owner.
2. Each callback documents acceptance, suspension, ownership, and any later completion.
3. `None` never conflates rejection, completion, and quiescence.
4. A composite preserves progress after internal callbacks return `None`.
5. Suspension preserves exact input cursors and operation identity.
6. Invalid completions preserve valid state and transferred resources.
7. Cancellation and deadlines retain their semantic scope.
8. Partial I/O and stalled consumers do not prevent permitted sibling progress.
9. Resource limits cover all retained storage and outstanding operations.
10. The adapter executes operations without reconstructing protocol policy.

Performance acceptance requires real protocol workloads through native and async execution.
Both paths must use the same protocol machines and equivalent resource limits.
Measurements include throughput, latency, allocations, retained memory, and scheduler activity.
Callback microbenchmarks alone do not establish acceptance.

The [initial implementation plan](http1-fsm-plan.md) uses HTTP/1 before HTTP/2.
A static-file application FSM and a conventional Kimojio wrapper exercise different callback behavior.
A WebSocket layer then challenges upgrade, duplex progress, and bounded multi-client delivery.
Independent peers must complement tests that use the same FSM at both ends.

Streaming gRPC over real HTTP/2 remains a later composition proof.
It must include fragmented messages, trailers, receive-window replenishment, stalled consumers, and cancellation during partial writes.
It is not a prerequisite for the first HTTP/1 implementation.

The remaining design work includes concrete buffer leases, efficient readiness storage, and cross-layer consumption contracts.
This document fixes the architectural direction, not every type representation.

## References

- [JSON tokenizer visitor and drive contract](../kimojio-json/src/tokenizer.rs)
- [Tokenizer inspiration: Azure/kimojio-rs#46](https://github.com/Azure/kimojio-rs/pull/46)
- [HTTP/1 implementation plan](http1-fsm-plan.md)
- Historical HTTP connection design at `ypwrwxtr`: `kimojio-fsm-http/DESIGN.md`
- Historical orchestration results at `wxupqrpmkost`: `.paw/work/http-io-orchestration-fsm/ExperimentResults.md`
- Historical wrapper rationale at `xosvtxrrpwyu`: `.paw/work/http1-wrapper-poc/WorkShaping.md`
- Historical async bridge at `kwttmxkk`: `.paw/work/http-async-bridge/Docs.md`
