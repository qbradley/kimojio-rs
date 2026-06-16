# kimojio-stack-steal TODO

Deferred follow-up work from the runtime-agnostic downstream migration workflow
and final review. Some items touch sibling crates because `kimojio-stack-steal`
is the companion runtime that drives the shared runtime/I/O contract.

## Recommended pickup order

1. Tighten stack-steal shared-ring cancellation and ownership behavior.
2. Harden the runtime-neutral result/wait API contract.
3. Add missing TLS write-timeout and allocation/performance coverage.
4. Continue downstream generic migration once HTTP/1+TLS is stable.

## Stack-steal and runtime API hardening

### 1. [done] Replace shared-buffer fd-clone retirement polling

**Why:** `SharedBufferOperation::request_cancel` still waits for fd clone
retirement with zero-duration runtime sleeps plus `thread::sleep`. This can block
worker OS threads and can surface `FdInUse`/generic runtime errors after HTTP has
already decided a deadline won.

**Pickup notes:**

- Start in `kimojio-stack-steal/src/lib.rs`:
  - `SharedBufferOperation::request_cancel`
  - `wait_for_fd_clone_release`
  - shared operation state/cancel waiters near `SharedOpState`
- Replace wall-clock polling with either:
  - a runtime-neutral waiter that wakes when fd clone retirement is observable, or
  - a deterministic ownership result that does not block workers and is documented
    as the caller-visible terminal outcome.
- Preserve the current HTTP contract: once a deadline wins, HTTP should normally
  return `Errno::TIME` and still be able to close/reclaim the transport.

**Validation to add:**

- Delay shared-operation retirement past the old one-second polling window and
  assert HTTP still returns the documented timeout/cancellation outcome.
- Stress many stalled stack-steal HTTP/TLS connections with short deadlines and
  assert bounded cancellation latency and worker availability.

### 2. [done] Avoid fresh shared-ring core allocation for each root socket adapter call

**Why:** Root/non-worker `SocketIoRuntime` adapter calls choose `RingMode::Shared`
and currently create a fresh shared ring for each read/write/async/shutdown/close
operation. That is expensive and can create avoidable cache traffic and queue
allocation on hot root/shared paths.

**Pickup notes:**

- Start in `kimojio-stack-steal/src/runtime_api.rs`:
  - `RuntimeContext::socket_ring`
  - `SocketIoRuntime` read/write/read_async/write_async/shutdown/close impls
- Design options:
  - cache a shared ring in the socket/transport adapter;
  - cache one root shared ring per runtime context or worker owner;
  - make hot HTTP/TLS root use explicit/erroring and require worker-local
    execution for latency-sensitive paths.
- Keep costs visible. Avoid hidden helper threads or ambient runtime lookup.

**Validation to add:**

- Allocation-count tests for stack-steal runtime-agnostic HTTP/1+TLS and timeout
  cases.
- A regression that shared-ring core creation is bounded near setup, not
  proportional to read/write calls or deadline waits.

### 3. [done] Remove stack-core `Waitable` from runtime-neutral result traits

**Why:** `RuntimeReadResult` and `RuntimeWriteResult` are runtime-neutral in name
but still inherit stack-core `Waitable`, forcing non-stack runtimes to implement
stack-core waiter compatibility.

**Pickup notes:**

- Start in `kimojio-stack/src/runtime_api.rs`:
  - `RuntimeReadResult`
  - `RuntimeWriteResult`
  - `RuntimeWaitableAdapter`
- Make generic HTTP/TLS code wait through `RuntimeWaitable` only.
- Keep stack-core compatibility behind `RuntimeWaitableAdapter` or stack-specific
  wrapper impls.
- Audit `kimojio-stack-http`, `kimojio-stack-tls`, and `kimojio-stack-steal` for
  direct `Waitable` bounds that should become `RuntimeWaitable`.

**Validation to add:**

- A fake-runtime compile/test case where read/write results implement
  `RuntimeWaitable` but not stack-core `Waitable`.
- HTTP/TLS runtime-generic tests proving waits still work on stack-core and
  stack-steal.

### 4. [done] Clarify or fix direct result `cancel()` semantics

**Why:** The shared result trait comments say cancellation is non-consuming, but
direct stack-core `IoResult::cancel` and stack-steal `RingIoResult::cancel`
consume/detach state. The safer non-consuming path currently lives on
`SocketIoRuntime::cancel_read` / `cancel_write`.

**Pickup notes:**

- Start in:
  - `kimojio-stack/src/runtime_api.rs` trait comments/default impls
  - `kimojio-stack/src/lib.rs` `IoResult::cancel`
  - `kimojio-stack-steal/src/lib.rs` `RingIoResult::cancel`
  - `kimojio-stack-steal/src/runtime_api.rs` `cancel_read` / `cancel_write`
- Decide one of:
  - make direct result cancellation truly non-consuming and drainable; or
  - rename/reword direct cancellation as consuming/detaching and document that
    non-consuming cancellation requires `SocketIoRuntime::cancel_*`.

**Validation to add:**

- Stack-core and stack-steal contract tests that call direct result `cancel()`,
  then verify one deterministic drain/close behavior.
- Tests for immediate socket close/reclaim after direct cancellation.

### 5. [done] Clarify TLS async cancellation API ownership

**Why:** `TlsReadResult::cancel(&self, _cx: &R)` and
`TlsWriteResult::cancel(&self, _cx: &R)` accept a runtime context but use the
runtime captured when the async handle was created.

**Pickup notes:**

- Start in `kimojio-stack-tls/src/lib.rs`:
  - `TlsReadResult::cancel`
  - `TlsWriteResult::cancel`
  - `TlsPendingIo::cancel`
- Pick one API story:
  - remove the unused context parameter;
  - keep it but assert/enforce same-runtime semantics;
  - document it as compatibility-only until the alpha API stabilizes.
- Update HTTP call sites in `kimojio-stack-http/src/transport.rs` if signatures
  change.

**Validation to add:**

- Rustdoc or compile tests that show the intended cancellation call shape.
- A regression that cancellation still poisons TLS streams and drains private
  socket I/O on both runtime families.

### 6. [done] Add TLS write-deadline cancellation coverage

**Why:** Current active HTTP TLS deadline tests cover read timeouts. Write
timeouts have distinct cancellation, drain, poisoning, and close paths.

**Pickup notes:**

- Start in `kimojio-stack-http/tests/tls_interop.rs`.
- Induce a pending TLS write with a peer that completes handshake but then stops
  reading, ideally with small socket send/receive buffers via rustix sockopts.
- Cover both stack-core and stack-steal runtime-generic transports.
- Consider adding a direct generic `TlsWriteResult` cancellation test in
  `kimojio-stack-tls/src/lib.rs` tests.

**Validation to add:**

- Assert HTTP returns `Errno::TIME`.
- Assert the TLS stream is poisoned/non-reusable after timeout.
- Assert `transport.close(cx)` succeeds after the timeout path.

### 7. Add an alpha runtime API release note / no-publish decision

**Why:** The runtime API is alpha but public within the workspace. The migration
added `RuntimeCapability::SocketIo`, runtime-neutral timers/waits, and generic
TLS/HTTP surfaces without a consumer-facing release note or explicit no-publish
decision.

**Pickup notes:**

- Start with `kimojio-stack/src/runtime_api.rs` rustdoc and docs that already say
  release notes must call out breaking alpha changes.
- Add a repository release note, changelog entry, or explicit "workspace-private;
  no publish for this change" note.
- Include:
  - `RuntimeCapability::SocketIo`;
  - runtime-neutral wait/timer handles;
  - generic TLS async handles;
  - HTTP/1+TLS runtime-generic surfaces;
  - preserved stack-core aliases;
  - deferred HTTP/2/gRPC/storage/OpenTelemetry boundaries.

**Validation to add:**

- Run the package/publish dry-run or equivalent release gate after the
  version/no-publish decision is made.

### 8. [done] Decide what to do with `RuntimeSocket::as_stack_io_fd`

**Why:** The runtime-neutral socket trait exposes a stack-core escape hatch. It is
currently useful for compatibility assertions, but it could let production
generic code branch on concrete stack-core sockets.

**Pickup notes:**

- Start in `kimojio-stack/src/runtime_api.rs` at `RuntimeSocket::as_stack_io_fd`.
- Options:
  - move it to a stack-core extension trait;
  - make it test-only;
  - keep it and document the exact compatibility use case.

**Validation to add:**

- Re-run HTTP/TLS and stack-steal runtime API tests after moving/removing it.
- Search downstream crates to ensure production paths are not branching on this
  method.

### 9. Preserve structured runtime diagnostics where callers need them

**Why:** Several stack-steal errors collapse into `RuntimeIoError::Other` static
strings or generic categories, losing wrong-worker, wrong-runtime, queue-full,
fd-in-use, and no-stackful-context structure.

**Pickup notes:**

- Start in:
  - `kimojio-stack/src/runtime_api.rs` `RuntimeIoError`
  - `kimojio-stack-steal/src/runtime_api.rs` `runtime_io_error`
  - HTTP/TLS error mapping in `kimojio-stack-http/src/transport.rs` and
    `kimojio-stack-tls/src/lib.rs`
- Decide whether callers need branching on structured categories or whether
  allocation-free static strings are an intentional alpha constraint.

**Validation to add:**

- Focused tests for wrong-runtime, wrong-worker, queue-full, fd-in-use,
  no-stackful-context, and true cancellation.
- Assert each maps to a documented caller-visible category.

### 10. Measure and reduce boxed waiter allocation on runtime-neutral waits

**Why:** `RuntimeWaitable::add_stackful_waiter(Box<dyn StackfulWaiter>)` can
allocate per park. HTTP deadlines now use runtime-neutral `wait_any_stackful`, so
this may become a real hot-path cost.

**Pickup notes:**

- Start in `kimojio-stack/src/runtime_api.rs`:
  - `StackfulWaiter`
  - `StackfulWaitRegistration`
  - `StackfulWaitContext::wait_all_stackful`
  - `StackfulWaitContext::wait_any_stackful`
- Also inspect stack-steal wait registration wrappers in
  `kimojio-stack-steal/src/lib.rs`.
- Measure before redesigning. Candidate redesigns include waiter pooling,
  stack-owned registration slots, or an allocation-free typed waiter handle.

**Validation to add:**

- Allocation instrumentation for TLS async waits/drains on stack-core and
  stack-steal.
- If allocation-free waits are required, add a regression for blocked HTTP/TLS
  deadline waits.

### 11. Make portability-overhead tests less brittle

**Why:** Source substring checks and unexplained allocation tolerances can
false-positive or hide the actual portability budget.

**Pickup notes:**

- Start in `kimojio-stack-http/src/lib.rs`:
  - allocation tolerance assertions around the runtime-agnostic HTTP/TLS tests;
  - `runtime_agnostic_portability_surface_avoids_dyn_heap_and_helper_threads`.
- Replace magic constants with named constants and rationale tied to the runtime
  migration success criteria.
- Factor source scanning into helpers with documented false-positive limits.

**Validation to add:**

- Clear failure messages and fixtures/comments documenting what the source scan
  does and does not prove.

## Deferred downstream migration work

### 12. Fully migrate HTTP/2 and protocol-neutral HTTP APIs

**Why:** The current runtime-generic slice covers HTTP/1.1 plaintext/TLS. HTTP/2
and protocol-neutral top-level client/server enums remain stack-core concrete.

**Pickup notes:**

- Start in `kimojio-stack-http/src/h2/` and protocol-neutral
  `client`/`server` wrappers.
- Reuse `RuntimeStackTransport<S>` and the `HttpRuntime<S>` pattern from HTTP/1.
- Keep stack-core aliases for compatibility.

**Validation to add:**

- HTTP/2 request/response tests on both stack-core and stack-steal.
- gRPC smoke tests after generic HTTP/2 wrappers exist.

### 13. Fully migrate gRPC client/server APIs to generic runtime contexts

**Why:** gRPC layers on HTTP/2 and still owns concrete stack-core HTTP/2
connections.

**Pickup notes:**

- Start in `kimojio-stack-grpc/src/client.rs` and
  `kimojio-stack-grpc/src/server.rs`.
- Wait for generic HTTP/2 connection types, then parameterize `UnaryClient`,
  server dispatch, and streaming response types over the runtime/socket handle.
- Preserve current stack-core aliases and constructors.

**Validation to add:**

- Unary and server-streaming gRPC tests on both runtime families.
- Existing tonic/hyper interop tests should still pass for stack-core aliases.

### 14. Fully migrate storage transports and clients to generic runtime contexts

**Why:** Storage still depends on a concrete `StackHttpTransport` over stack-core
HTTP clients.

**Pickup notes:**

- Start in `kimojio-stack-storage/src/transport.rs`.
- Wait for generic HTTP client wrappers, then make `StackHttpTransport` generic
  or add a generic sibling while preserving current names as aliases.
- Keep storage-facing `Transport` domain semantics stable.

**Validation to add:**

- Storage unit tests with stack-core aliases.
- Runtime-generic HTTP transport tests once generic HTTP clients exist.
- Emulator/real-storage tests remain environment-gated.

### 15. Fully migrate OpenTelemetry exporters to generic runtime contexts

**Why:** OpenTelemetry exporters wrap gRPC unary clients, so they inherit the
current stack-core runtime boundary.

**Pickup notes:**

- Start in `kimojio-stack-opentelemetry/src/client.rs`,
  `logs.rs`, and `metrics.rs`.
- Wait for generic gRPC APIs, then parameterize exporters over the generic gRPC
  client or transport.
- Keep disabled instrumentation free of allocation, background work, and helper
  threads.

**Validation to add:**

- Existing log/metric unit tests.
- Collector interop tests gated by `KIMOJIO_STACK_OTEL_RECEIVER_TESTS=1`.

### 16. Add runtime-agnostic TLS and HTTP read/write benchmarks

**Why:** The first vertical slice now passes correctness gates, but comparative
runtime costs need explicit labels and measurements.

**Pickup notes:**

- Start in:
  - `kimojio-stack-http/benches/http_request_response.rs`
  - `kimojio-stack-steal/benches/runtime_baseline.rs`
  - possible new TLS-focused benchmark under `kimojio-stack-tls/benches/`
- Label paths explicitly:
  - stack-core;
  - stack-steal worker-local;
  - stack-steal shared/root;
  - plaintext vs TLS;
  - read/write/deadline paths.
- Include tail-latency and allocation-sensitive measurements where practical.

**Validation to add:**

- Benchmark smoke mode in tests, mirroring existing Criterion `--test` coverage.
- Document benchmark commands and expected interpretation in `docs/stack-steal.md`
  and `docs/stack-http-grpc.md`.
