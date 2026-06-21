// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Handler conversion utilities.

use std::marker::PhantomData;

use http::{Request, Response};
use kimojio_stack_http::Body;

use crate::{FromRequest, IntoResponse, Rejection};

/// Stackful request handler object used by routers.
pub trait Handler<Cx>: Send {
    /// Handles one request.
    fn call(&mut self, cx: &Cx, request: Request<Body>) -> Result<Response<Body>, Rejection>;
}

/// Creates a raw request handler from a closure.
pub fn handler_fn<F>(f: F) -> HandlerFn<F> {
    HandlerFn { f }
}

/// Raw request handler created by [`handler_fn`].
pub struct HandlerFn<F> {
    f: F,
}

impl<Cx, F, R> Handler<Cx> for HandlerFn<F>
where
    F: FnMut(&Cx, Request<Body>) -> Result<R, Rejection> + Send + 'static,
    R: IntoResponse,
{
    fn call(&mut self, cx: &Cx, request: Request<Body>) -> Result<Response<Body>, Rejection> {
        (self.f)(cx, request).map(IntoResponse::into_response)
    }
}

/// Creates a one-extractor handler from a closure.
pub fn extractor_fn<E, F>(f: F) -> ExtractorHandler<E, F> {
    ExtractorHandler {
        f,
        _extractor: PhantomData,
    }
}

/// Handler created by [`extractor_fn`].
pub struct ExtractorHandler<E, F> {
    f: F,
    _extractor: PhantomData<fn() -> E>,
}

impl<Cx, E, F, R> Handler<Cx> for ExtractorHandler<E, F>
where
    E: FromRequest<Cx> + Send + 'static,
    F: FnMut(&Cx, E) -> Result<R, Rejection> + Send + 'static,
    R: IntoResponse,
{
    fn call(&mut self, cx: &Cx, mut request: Request<Body>) -> Result<Response<Body>, Rejection> {
        let extracted = E::from_request(cx, &mut request)?;
        (self.f)(cx, extracted).map(IntoResponse::into_response)
    }
}
