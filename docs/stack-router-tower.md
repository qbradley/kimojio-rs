# Stack Router and Tower

`kimojio-stack-router` and `kimojio-stack-tower` provide an axum/tower-style
framework layer for stackful applications. They keep routing, extraction,
services, layers, and middleware familiar while avoiding futures: handlers and
services receive an explicit stackful runtime context and return responses
directly.

The crates target both `kimojio-stack` and `kimojio-stack-steal`. They build on
`kimojio-stack-http` for HTTP bodies and transports.

## Crates

| Crate | Purpose |
|-------|---------|
| `kimojio-stack-router` | Method/path routing, nesting, fallback, extraction, response conversion, and server adapters |
| `kimojio-stack-tower` | Stackful `Service`/`Layer` traits, readiness, type erasure, and tower/tower-http-style middleware |

## Quick start

```rust
use http::{Method, Request};
use kimojio_stack_http::Body;
use kimojio_stack_router::{PathParams, Rejection, Router, extractor_fn};
use kimojio_stack_tower::Service;

let mut router = Router::new()
    .route(
        "/users/:id",
        Method::GET,
        extractor_fn::<PathParams, _>(|_: &(), params: PathParams| {
            Ok::<_, Rejection>(format!("user {}", params.get("id").unwrap()))
        }),
    )
    .unwrap();

let response = router
    .call(
        &(),
        Request::builder()
            .method(Method::GET)
            .uri("/users/42")
            .body(Body::empty())
            .unwrap(),
    )
    .unwrap();

assert_eq!(response.status(), http::StatusCode::OK);
```

In an application, replace `&()` with the context passed to
`kimojio_stack::Runtime::block_on` or `kimojio_stack_steal::Runtime::block_on`.

## Routing and extraction

Routers support:

- method routes with `Router::route`
- nested routers with `Router::nest`
- router merging with `Router::merge`
- fallback handlers
- path parameters using `:name`
- shared state with `State<T>`
- extensions with `Extension<T>`
- query extraction with `QueryParams`
- bounded body extraction with `BodyBytes`

Ambiguous same-shape routes are rejected at registration time. During dispatch,
literal segments outrank parameter segments at the earliest differing segment.
Nested routers and merged routers currently import route tables only; child
router state and fallback scopes are not preserved.

## Services and middleware

`kimojio-stack-tower` uses a direct stackful service trait:

```text
fn ready(&mut self, cx: &Cx) -> Result<Readiness, Error>
fn call(&mut self, cx: &Cx, request: Request) -> Result<Response, Error>
```

`Readiness` is explicit: a service reports `Ready`, `NotReady`, or
`Overloaded`. Middleware can reject, retry, shed, or wait through the provided
runtime context.

Layer ordering follows `ServiceExt::layer`: each call wraps the service built so
far. The final `.layer(...)` in a chain is the outermost request-side middleware
and observes the response/error last.

Example composition:

```rust
use std::time::Duration;
use http::{Request, Response};
use kimojio_stack_http::Body;
use kimojio_stack_tower::{
    ConcurrencyLimitLayer, RetryLayer, Service, ServiceError, ServiceExt,
    TimeoutLayer, service_fn,
};

let mut service = service_fn(|_: &(), _request: Request<Body>| {
    Ok::<_, ServiceError>(Response::new(Body::empty()))
})
.layer(RetryLayer::new(2))
.layer(ConcurrencyLimitLayer::new(128))
.layer(TimeoutLayer::new(Duration::from_secs(1)));

let response = service
    .call(
        &(),
        Request::builder().uri("/").body(Body::empty()).unwrap(),
    )
    .unwrap();
assert_eq!(response.status(), http::StatusCode::OK);
```

Middleware families include core tower-style filters, limits, load-shed, retry,
steer, timeout, discovery, balance, reconnect, buffer, spawn-ready, and hedge,
plus HTTP middleware for auth, CORS, CSRF, request IDs, trace hooks, header
updates, compression/decompression, redirects, sessions, cache, and
governor-style rate limiting.

## Stackful differences from axum/tower

The API is intentionally close to axum/tower concepts, but not source
compatible:

- handlers are direct stackful calls, not async functions
- `Service::ready` replaces async `poll_ready`
- retry and hedge middleware clone/replay requests, so use them only for
  replay-safe operations or with an explicit policy/guard for mutating handlers
- timeout is cooperative/post-call and does not preempt synchronous work
- buffer, spawn-ready, and hedge provide cooperative compatibility behavior, not
  detached background workers yet
- HTTP/2 has an explicit streaming adapter, while the router `Service` response
  path buffers `StreamingBody` through `IntoResponse`; HTTP/1.1 serving is
  currently buffered
- `MemorySessionStore` is bounded and uses opaque random IDs, but production
  deployments should supply persistent stores and any required signing,
  encryption, or stricter cookie attributes
- `CacheLayer` is a conservative bounded response cache. It bypasses
  authorization, cookie, set-cookie, vary, private, no-cache, and no-store cases
  by default; validator/revalidation and full `Vary` keying are deferred
- proc macros and tuple/serde extractor conveniences are deferred

See the
[Stack Router/Tower Compatibility Matrix](stack-router-tower-compatibility.md)
for feature-by-feature status and test references.

## Porting examples

The `examples` crate contains small ports of common axum/tower patterns:

| Example module | Demonstrates |
|----------------|--------------|
| `stack_router_ported_routes` | route trees, path/query/state extraction, fallback |
| `stack_router_ported_middleware` | auth, CORS/CSRF, request IDs, trace, header middleware |
| `stack_router_ported_stateful` | sessions, cache, and governor-style rate limiting |
| `stack_router_ported_full_stack` | full middleware stack with success and failure behavior |

Run their smoke tests with:

```sh
cargo test -p examples stack_router_porting --all-targets
```

## Runtime selection

Use the same router or service shape with either runtime:

```rust
let mut runtime = kimojio_stack::Runtime::new();
runtime.block_on(|cx| {
    // call router/service with cx
});

let mut runtime = kimojio_stack_steal::Runtime::new();
runtime.block_on(|cx| {
    // call the same router/service shape with cx
});
```

The framework crates do not choose worker counts, stack sizes, listener loops,
or io_uring resources. Keep those as explicit application/runtime choices.

## Validation and benchmarks

Useful validation commands:

```sh
cargo test -p kimojio-stack-router
cargo test -p kimojio-stack-tower
cargo test -p examples stack_router_porting --all-targets
cargo test -p examples stack_router_workload --all-targets
cargo test --doc
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps -p kimojio-stack-router -p kimojio-stack-tower
```

Benchmark smoke commands:

```sh
cargo test -p kimojio-stack-router --bench router_baseline -- --test
cargo test -p kimojio-stack-tower --bench middleware_baseline -- --test
```

The workload binary emits stable key-value output for routed,
middleware-heavy, and failure profiles:

```sh
cargo run -q -p examples --bin stack_router_workload -- \
  --runtime stack-steal --workload failure
```

The failure profile reports timeout, retry, load-shed, buffer-reject, and
cancellation-cleanup counters separately as a smoke signal on both runtime
families. It is not a substitute for a long-running scheduler-progress or
steady-state benchmark.

## Limitations

- True concurrent background implementations for buffer, spawn-ready, and hedge
  are deferred until the service core has a safe owned stackful task handle.
- Production external stores for sessions, cache, and discovery are future work;
  the traits are already pluggable.
- Session mutation commits, cache validator/revalidation hooks, and full
  header-varying HTTP cache keys are deferred compatibility items.
- Nested router state/fallback scopes and serving middleware-wrapped routers via
  the router-specific adapters are deferred compatibility items.
- Proc macros, derived extractors, tuple extractor conveniences, and broader
  response tuple forms are not part of the first compatibility surface.
- HTTP/1.1 streaming responses are buffered; use HTTP/2 streaming adapters where
  streaming response chunks are required outside the router `Service` path.
- Local Criterion numbers are smoke baselines, not release pass/fail budgets.
