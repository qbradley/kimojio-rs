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
//! The crate currently contains the routing substrate: method/path matching,
//! nested routers, fallback handlers, state and extension extraction, bounded
//! body extraction, response conversion, protocol-neutral server adapters, and
//! HTTP/2 streaming integration.

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
pub use handler::{ExtractorHandler, Handler, HandlerFn, extractor_fn, handler_fn};
pub use rejection::{Rejection, RejectionKind};
pub use response::{IntoResponse, ResponseResult};
pub use router::{MethodRouter, RouteError, Router};
pub use runtime::{StackContext, StackRuntimeFamily, StealContext, StealRuntimeFamily};
pub use serve::{
    empty_trailers, read_h2_request_body, response_head, send_h2_streaming_response,
    serve_h2_buffered_requests, serve_h2_streaming_once, serve_http1_one, serve_one,
};
