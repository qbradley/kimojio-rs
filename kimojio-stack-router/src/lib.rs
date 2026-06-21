// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Stackful HTTP routing foundations for `kimojio-stack`.
//!
//! `kimojio-stack-router` provides axum-like routing, extraction, rejection,
//! and response conversion for stackful services. It builds on
//! `kimojio-stack-tower` and `kimojio-stack-http`, so handlers receive an
//! explicit runtime context and return responses directly without futures or
//! `async`/`await`.
//!
//! # Model
//!
//! [`Router`] implements `kimojio_stack_tower::Service<Cx, Request<Body>>`.
//! `Cx` is the runtime context supplied by the caller, such as
//! `kimojio_stack::RuntimeContext` or `kimojio_stack_steal::RuntimeContext`.
//! The router never chooses a runtime for you.
//!
//! The routing surface includes method/path matching, terminal catch-all
//! wildcard segments such as `/account/container/*blob`, nested routers, router
//! merging, fallback handlers, state and extension extraction, bounded body
//! extraction, response conversion, generic service server adapters, and an
//! explicit HTTP/2 streaming adapter. The `Router` service response path is
//! buffered; use `serve_h2_streaming_once` for end-to-end streaming chunks.
//!
//! # Basic routing
//!
//! ```no_run
//! use http::{Method, Request, StatusCode};
//! use kimojio_stack_http::Body;
//! use kimojio_stack_router::{PathParams, Rejection, Router, extractor_fn};
//! use kimojio_stack_tower::Service;
//!
//! # fn example(cx: &()) -> Result<(), Rejection> {
//! let mut router = Router::new()
//!     .route(
//!         "/users/:id",
//!         Method::GET,
//!         extractor_fn::<PathParams, _>(|_: &(), params: PathParams| {
//!             Ok::<_, Rejection>(format!("user {}", params.get("id").unwrap()))
//!         }),
//!     )
//!     .unwrap();
//!
//! let response = router.call(
//!     cx,
//!     Request::builder()
//!         .method(Method::GET)
//!         .uri("/users/42")
//!         .body(Body::empty())
//!         .unwrap(),
//! )?;
//! assert_eq!(response.status(), StatusCode::OK);
//! # Ok(())
//! # }
//! ```
//!
//! # Feature coverage
//!
//! Supported compatibility features include method routes, nested routers,
//! merging, fallbacks, wildcard path captures, path/query/body/state/extension
//! extractors, direct handler conversion, response conversion, route
//! registration errors, buffered generic service serving, and HTTP/2 streaming
//! serving.
//! Nested and merged routers currently import route tables only; child state and
//! fallback scopes are deferred compatibility work.
//! Use [`stack_handler_fn`] and [`steal_handler_fn`] when a closure needs the
//! concrete runtime context lifetime inside `Runtime::block_on`.
//!
//! # Limitations
//!
//! The first compatibility surface intentionally omits axum proc macros,
//! tuple/serde extractor conveniences, custom rejection mappers, broad response
//! tuple forms, router-service streaming response preservation, and HTTP/1.1
//! streaming response serving. Those are documented as deferred compatibility
//! work in the project guide.

pub mod body;
pub mod extract;
pub mod handler;
pub mod rejection;
pub mod response;
pub mod router;
pub mod runtime;
pub mod serve;

pub use body::{StreamingBody, StreamingChunk};
pub use extract::{BodyBytes, Extension, FromRequest, PathParams, QueryParams, State, path_params};
pub use handler::{
    ExtractorHandler, Handler, HandlerFn, StackHandlerFn, StealHandlerFn, extractor_fn, handler_fn,
    stack_handler_fn, steal_handler_fn,
};
pub use rejection::{Rejection, RejectionKind};
pub use response::{IntoResponse, ResponseResult};
pub use router::{MethodRouter, RouteError, Router};
pub use runtime::{StackContext, StackRuntimeFamily, StealContext, StealRuntimeFamily};
pub use serve::{
    empty_trailers, read_h2_request_body, response_head, send_h2_streaming_response,
    serve_h2_buffered_requests, serve_h2_streaming_once, serve_http1_one, serve_one,
};
