// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Stackful service and middleware foundations for `kimojio-stack`.
//!
//! `kimojio-stack-tower` provides the low-level service/layer contracts used by
//! higher-level stackful HTTP routers and middleware. It intentionally mirrors
//! the tower mental model while staying synchronous from the caller's point of
//! view: a service receives an explicit stackful runtime context and returns a
//! response directly, without exposing futures or requiring `async`/`await`.
//!
//! # Readiness and backpressure
//!
//! Stackful services expose readiness as an ordinary method. A service can
//! report immediate readiness, report a structured not-ready/overloaded state,
//! or park the current stackful coroutine using the provided runtime context
//! before returning. Middleware must make blocking points explicit instead of
//! silently blocking an OS worker thread.
//!
//! # Basic composition
//!
//! ```no_run
//! use kimojio_stack_tower::{Layer, Readiness, Service, service_fn};
//!
//! struct AddOne;
//!
//! impl<S> Layer<S> for AddOne {
//!     type Service = AddOneService<S>;
//!
//!     fn layer(&self, inner: S) -> Self::Service {
//!         AddOneService { inner }
//!     }
//! }
//!
//! struct AddOneService<S> {
//!     inner: S,
//! }
//!
//! impl<Cx, S> Service<Cx, u64> for AddOneService<S>
//! where
//!     S: Service<Cx, u64, Response = u64>,
//! {
//!     type Response = u64;
//!     type Error = S::Error;
//!
//!     fn ready(&mut self, cx: &Cx) -> Result<Readiness, Self::Error> {
//!         self.inner.ready(cx)
//!     }
//!
//!     fn call(&mut self, cx: &Cx, request: u64) -> Result<Self::Response, Self::Error> {
//!         self.inner.call(cx, request).map(|value| value + 1)
//!     }
//! }
//!
//! # fn example(cx: &()) -> Result<(), std::convert::Infallible> {
//! let mut service = AddOne.layer(service_fn(|_: &(), value: u64| {
//!     Ok::<u64, std::convert::Infallible>(value * 2)
//! }));
//! assert!(service.ready(cx)?.is_ready());
//! assert_eq!(service.call(cx, 3)?, 7);
//! # Ok(())
//! # }
//! ```
//!
//! This crate is currently the framework substrate. HTTP-specific routing and
//! axum-like extractors live in `kimojio-stack-router`.

pub mod error;
pub mod http;
pub mod middleware;
pub mod service;

pub use error::{BoxError, ServiceError};
pub use middleware::*;
pub use service::{
    BoxCloneService, BoxService, Layer, Readiness, Service, ServiceExt, ServiceFn, service_fn,
};
