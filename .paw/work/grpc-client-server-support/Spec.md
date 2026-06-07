# Feature Specification: gRPC Client Server Support

**Branch**: feature/grpc-client-server-support  |  **Created**: 2026-06-07  |  **Status**: Draft
**Input Brief**: Add low-overhead HTTP/1.1, HTTP/2, and unary gRPC client/server support for the stackful runtime with tokio-ecosystem interoperability validation.

## Overview

Kimojio users need first-class HTTP and gRPC client/server capabilities that fit the stackful runtime model without inheriting a tokio dependency or an async-runtime-centered programming model. The new capabilities should let applications communicate with common HTTP and gRPC peers while retaining the repository's emphasis on low overhead, predictable latency, explicit control, and minimal allocation in hot paths.

The HTTP layer should provide a reusable transport foundation for both direct HTTP applications and the gRPC layer. It should support core production-usable HTTP/1.1 and HTTP/2 request/response client and server behavior, including TLS where applicable and plaintext support when it does not add disproportionate complexity. Advanced HTTP features are intentionally outside the initial scope unless required for unary gRPC correctness.

The gRPC layer should initially prioritize unary request/response calls. It should expose usable low-level APIs compatible with prost-style message encoding and decoding, while avoiding a large generated-stub framework until the transport and service model are proven. The design should not block future streaming support, but client streaming, server streaming, and bidirectional streaming are not required in this initial workflow.

Compatibility is essential: the kimojio-stack HTTP and gRPC clients and servers should be validated against tokio-based ecosystem counterparts. This ensures that lower-level, performance-oriented APIs still interoperate with existing deployed clients and servers.

## Objectives

- Provide stackful HTTP client and server capabilities for core HTTP/1.1 request/response usage.
- Provide stackful HTTP client and server capabilities for core HTTP/2 request/response usage.
- Support TLS-enabled operation for HTTP and gRPC where peer expectations require it, while allowing plaintext where useful for local and integration testing.
- Provide unary gRPC client and server support with prost-compatible message payloads.
- Preserve kimojio-style control over latency, allocation, buffering, and scheduling rather than hiding costs behind convenience abstractions.
- Validate interoperability in both directions against tokio-based clients and servers.
- Establish transport and API boundaries that allow future streaming gRPC support without redesigning the entire stack.

## User Scenarios & Testing

### User Story P1 - Stackful HTTP Client Requests

Narrative: A kimojio-stack application opens an HTTP connection, sends a request, and receives a response using stackful blocking-style control flow without depending on a tokio runtime.

Independent Test: A stackful HTTP client sends requests to tokio-based HTTP/1.1 and HTTP/2 servers and receives correct status, headers, body, and end-of-message handling.

Acceptance Scenarios:
1. Given a tokio-based HTTP/1.1 server, When the stackful HTTP client sends a request with headers and a body, Then it receives the complete response with correct status, headers, and body.
2. Given a tokio-based HTTP/2 server, When the stackful HTTP client sends a request with headers and a body, Then it receives the complete response with correct status, headers, body, and stream completion.
3. Given a TLS-enabled HTTP peer, When the stackful HTTP client connects with compatible TLS settings, Then the request/response exchange completes successfully.

### User Story P2 - Stackful HTTP Server Handling

Narrative: A kimojio-stack application accepts HTTP connections and serves requests using explicit stackful request handling, enabling predictable low-latency services without a tokio runtime.

Independent Test: Tokio-based HTTP/1.1 and HTTP/2 clients send requests to a stackful HTTP server and receive correct responses.

Acceptance Scenarios:
1. Given a stackful HTTP/1.1 server, When a tokio-based client sends a request with headers and a body, Then the server handler receives the full request and returns the expected response.
2. Given a stackful HTTP/2 server, When a tokio-based client sends a request with headers and a body, Then the server handler receives the full request and returns the expected response on the correct stream.
3. Given malformed or unsupported HTTP input, When the stackful server detects the condition, Then it reports an explicit protocol error or sends an appropriate error response without corrupting subsequent connection state.

### User Story P3 - Unary gRPC Client Calls

Narrative: A kimojio-stack application calls unary gRPC methods on existing gRPC services using prost-compatible request and response messages.

Independent Test: A stackful gRPC client calls unary methods on a tokio-based gRPC server and verifies request metadata, response metadata, message payload, status, and trailers.

Acceptance Scenarios:
1. Given a tokio-based unary gRPC server, When the stackful gRPC client sends a prost-compatible request message, Then it receives the expected response message and successful gRPC status.
2. Given a tokio-based unary gRPC server that returns an application error, When the stackful gRPC client calls the method, Then it exposes the gRPC status code, message, and metadata to the caller.
3. Given a request with metadata, When the stackful gRPC client sends the call, Then the server observes the expected metadata and the client receives response metadata and trailers.

### User Story P4 - Unary gRPC Server Methods

Narrative: A kimojio-stack service exposes unary gRPC methods that existing tokio-based gRPC clients can call.

Independent Test: A tokio-based gRPC client calls unary methods on a stackful gRPC server and verifies request decoding, response encoding, metadata, status, and trailers.

Acceptance Scenarios:
1. Given a stackful unary gRPC server, When a tokio-based client sends a valid request message, Then the server handler receives the decoded request and returns the expected encoded response.
2. Given a stackful unary gRPC server handler that returns an error status, When a tokio-based client calls the method, Then the client receives the expected gRPC status code, message, and trailers.
3. Given a malformed gRPC frame or unsupported content type, When the stackful server receives the request, Then it reports a protocol error through valid HTTP/2/gRPC error semantics.

### User Story P5 - Predictable Low-Overhead Operation

Narrative: A performance-sensitive service author can reason about when data is copied, when memory is allocated, and when operations block or yield.

Independent Test: Benchmarks and allocation-focused tests compare representative HTTP and unary gRPC paths after warmup and document allocation counts, latency distribution, and throughput.

Acceptance Scenarios:
1. Given a warmed stackful HTTP request/response loop, When representative client and server exchanges run repeatedly, Then allocation counts are measured and any hot-path allocations are either eliminated or explicitly justified.
2. Given a warmed unary gRPC request/response loop, When representative client and server exchanges run repeatedly, Then latency and allocation behavior are measured for both plaintext and TLS modes where supported.
3. Given backpressure or partial I/O progress, When an HTTP or gRPC operation cannot complete immediately, Then the operation yields through the stackful runtime rather than spinning or blocking an OS thread.

### Edge Cases

- HTTP peers close connections cleanly while a request or response is in progress: callers receive explicit EOF or protocol-completion results without use-after-close behavior.
- HTTP peers send malformed headers, invalid frame lengths, invalid pseudo-headers, invalid content length, or unsupported transfer semantics: clients and servers surface protocol errors predictably.
- HTTP/2 flow-control windows are exhausted: senders stop writing new data until progress is possible rather than buffering unbounded data.
- HTTP/2 streams complete out of order or concurrently: responses are associated with the correct stream.
- TLS handshake failure, certificate verification failure, or close-notify behavior occurs: callers receive explicit errors and resources are cleaned up.
- gRPC peers omit required gRPC status trailers, send invalid compressed flags, send malformed message lengths, or exceed configured message limits: clients and servers surface gRPC protocol errors.
- Request or response body exceeds configured limits: operation fails with an explicit size/limit error rather than allocating unbounded memory.
- Handler returns an application error: server encodes it as a gRPC status without panicking or corrupting the connection.
- Tokio-based peer uses common legal header casing, metadata, or HTTP/2 settings variations: stackful implementation remains interoperable.

## Requirements

### Functional Requirements

- FR-001: Provide a stackful HTTP client capable of sending core HTTP/1.1 requests and receiving complete responses. (Stories: P1)
- FR-002: Provide a stackful HTTP server capable of accepting core HTTP/1.1 requests and sending complete responses. (Stories: P2)
- FR-003: Provide a stackful HTTP client capable of sending core HTTP/2 requests and receiving complete responses. (Stories: P1)
- FR-004: Provide a stackful HTTP server capable of accepting core HTTP/2 requests and sending complete responses. (Stories: P2)
- FR-005: Support TLS-enabled HTTP client and server operation where required for interoperability, plus plaintext operation for local and integration use. (Stories: P1,P2,P3,P4)
- FR-006: Provide unary gRPC client calls using prost-compatible request and response message payloads. (Stories: P3)
- FR-007: Provide unary gRPC server method handling using prost-compatible request and response message payloads. (Stories: P4)
- FR-008: Preserve and expose gRPC metadata, response metadata, status code, status message, and trailers for unary calls. (Stories: P3,P4)
- FR-009: Enforce configurable request, response, header, and gRPC message size limits. (Stories: P1,P2,P3,P4,P5)
- FR-010: Surface HTTP, TLS, I/O, and gRPC protocol failures as explicit typed errors or error categories that callers can inspect. (Stories: P1,P2,P3,P4)
- FR-011: Support bounded buffering and HTTP/2 flow-control behavior that avoids unbounded memory growth under backpressure. (Stories: P2,P5)
- FR-012: Validate stackful HTTP client interoperability with tokio-based HTTP/1.1 and HTTP/2 servers. (Stories: P1)
- FR-013: Validate stackful HTTP server interoperability with tokio-based HTTP/1.1 and HTTP/2 clients. (Stories: P2)
- FR-014: Validate stackful gRPC client interoperability with tokio-based unary gRPC servers. (Stories: P3)
- FR-015: Validate stackful gRPC server interoperability with tokio-based unary gRPC clients. (Stories: P4)
- FR-016: Provide measurement coverage for representative latency, throughput, and allocation behavior. (Stories: P5)
- FR-017: Keep initial APIs low-level enough for predictable performance while remaining usable without generated stubs. (Stories: P3,P4,P5)
- FR-018: Keep the transport and handler model compatible with future streaming gRPC support, without requiring streaming call shapes in this initial workflow. (Stories: P3,P4)

### Key Entities

- HTTP Client: A stackful caller-side component that manages a connection and performs request/response exchanges.
- HTTP Server: A stackful acceptor/connection component that receives requests and invokes user-provided handling logic.
- HTTP Request: Method, target, headers, optional body, and completion state.
- HTTP Response: Status, headers, optional body, and completion state.
- HTTP/2 Stream: A request/response exchange multiplexed over an HTTP/2 connection with stream-local state and flow control.
- gRPC Client: A caller-side component that sends unary method requests and receives unary responses plus status metadata.
- gRPC Server: A service-side component that dispatches unary method requests to user handlers and encodes responses or statuses.
- gRPC Method: A named unary operation with prost-compatible request and response message types.
- Metadata: HTTP/2/gRPC headers and trailers exposed to callers and handlers.
- Message Limits: Configurable bounds for headers, body buffers, and gRPC message frames.

### Cross-Cutting / Non-Functional

- NFR-001: Hot paths should avoid avoidable heap allocation after connection and handler setup; any remaining hot-path allocation must be measured and justified. (Stories: P5)
- NFR-002: APIs should favor explicit buffers, limits, and error handling over hidden background work. (Stories: P5)
- NFR-003: Backpressure should prioritize bounded memory and low latency over maximizing buffered throughput. (Stories: P5)
- NFR-004: No public HTTP or gRPC runtime dependency may require tokio or another async runtime. (Stories: P1,P2,P3,P4)
- NFR-005: The implementation should reuse runtime-agnostic protocol, header, protobuf, or framing crates when doing so does not compromise latency, allocation control, or stackful scheduling. (Stories: P1,P2,P3,P4,P5)

## Success Criteria

- SC-001: Stackful HTTP client integration tests pass against tokio-based HTTP/1.1 and HTTP/2 servers for request/response exchanges with headers and bodies. (FR-001,FR-003,FR-012)
- SC-002: Stackful HTTP server integration tests pass against tokio-based HTTP/1.1 and HTTP/2 clients for request/response exchanges with headers and bodies. (FR-002,FR-004,FR-013)
- SC-003: Stackful unary gRPC client integration tests pass against a tokio-based unary gRPC server for success, error status, metadata, and trailers. (FR-006,FR-008,FR-014)
- SC-004: Stackful unary gRPC server integration tests pass against a tokio-based unary gRPC client for success, error status, metadata, and trailers. (FR-007,FR-008,FR-015)
- SC-005: TLS-enabled integration tests pass for at least one HTTP client path and one HTTP server path, and the gRPC path works over the selected HTTP/2 transport mode. (FR-005)
- SC-006: Malformed HTTP and gRPC inputs produce explicit protocol errors or valid peer-visible error responses in focused tests. (FR-010)
- SC-007: Configured size limits are enforced in tests without unbounded allocation. (FR-009,FR-011)
- SC-008: Benchmark or measurement output exists for representative HTTP and unary gRPC request/response loops, including latency and allocation observations after warmup. (FR-016,NFR-001)
- SC-009: Public APIs for the initial gRPC layer can perform unary calls and register unary handlers without generated stubs. (FR-017)
- SC-010: Planning or design documentation identifies how streaming support can be added without invalidating the initial unary APIs. (FR-018)

## Assumptions

- Initial gRPC scope is unary-only; streaming call shapes are deferred but must remain architecturally possible.
- Initial service APIs are low-level and prost-compatible; tonic-like generated client/server stubs are deferred.
- Core production-usable HTTP excludes advanced HTTP features that are not required for unary gRPC correctness, such as proxying, WebSocket upgrades, HTTP/2 server push, and broad compression negotiation.
- Tokio-based interoperability tests may use existing ecosystem crates for the peer side because those crates are test dependencies rather than public runtime dependencies of the new stackful crates.
- Runtime-agnostic crates for shared concepts such as HTTP types, HPACK/H2 framing helpers, protobuf message encoding, and byte buffers may be reused if code research confirms they do not bake in an async runtime dependency or unacceptable overhead.
- Plaintext support is in scope when it shares most of the TLS transport path or materially improves integration testing without compromising the TLS path.

## Scope

In Scope:
- A stackful HTTP crate providing core HTTP/1.1 client support.
- A stackful HTTP crate providing core HTTP/1.1 server support.
- A stackful HTTP crate providing core HTTP/2 client support.
- A stackful HTTP crate providing core HTTP/2 server support.
- TLS-enabled HTTP operation and plaintext operation when practical.
- A stackful gRPC crate layered on the HTTP/2 support for unary client calls.
- A stackful gRPC crate layered on the HTTP/2 support for unary server handlers.
- Prost-compatible message payload support.
- gRPC metadata, status, and trailers for unary calls.
- Integration tests pairing stackful clients with tokio-based servers and tokio-based clients with stackful servers.
- Performance and allocation measurement for representative paths.

Out of Scope:
- Client streaming, server streaming, and bidirectional streaming gRPC call shapes in the initial implementation.
- Tonic-like generated client and server stubs in the initial implementation.
- Full replacement of mature general-purpose HTTP clients or servers.
- Advanced HTTP proxy behavior, WebSocket upgrades, HTTP/2 server push, CONNECT tunneling, and broad content-compression negotiation.
- Automatic load balancing, retry policy engines, service discovery, or name resolution beyond explicit target configuration.
- Browser-focused HTTP features or HTTP/3.

## Dependencies

- Existing stackful runtime and I/O capabilities.
- Existing stackful TLS capability or equivalent TLS transport.
- Runtime-agnostic protocol/type crates selected during code research.
- Tokio-based ecosystem crates used only for interoperability test peers.
- Protobuf message encoding compatible with prost-style messages.

## Risks & Mitigations

- Risk: HTTP/2 flow control and multiplexing introduce hidden buffering or scheduling complexity. Mitigation: keep explicit limits, test backpressure, and measure allocation behavior.
- Risk: Reusing mature protocol crates may introduce async-runtime assumptions or opaque allocation behavior. Mitigation: research dependencies before planning and wrap only runtime-agnostic pieces.
- Risk: Unary-first APIs could accidentally block future streaming support. Mitigation: include a success criterion requiring documented streaming extensibility before finalizing the design.
- Risk: Tokio interoperability tests may pass only for narrow happy paths. Mitigation: include success, error, metadata, trailers, TLS, malformed-input, and limit tests.
- Risk: Performance goals may conflict with convenient APIs. Mitigation: prefer explicit low-level APIs for the initial scope and defer convenience layers until measured overhead is understood.
- Risk: TLS and plaintext transport paths diverge. Mitigation: keep common request/response behavior independent of transport mode where possible and test both modes.

## References

- User brief: "Create a PAW workflow to implement gRPC client and server support..."
- User clarification: "Unary-first MVP; design for streaming later"
- User clarification: "Low-level prost-compatible APIs; defer generated stubs"
- User clarification: "Core production-usable HTTP; defer advanced HTTP features"
