// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{Request, Response, StatusCode};
use kimojio_stack_http::Body;

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

use super::empty_response;

/// Request validation layer.
#[derive(Clone)]
pub struct ValidateRequestLayer<P> {
    predicate: P,
    relaxed: bool,
}

impl<P> ValidateRequestLayer<P> {
    /// Creates a request validation layer.
    pub fn new(predicate: P) -> Self {
        Self {
            predicate,
            relaxed: false,
        }
    }

    /// Explicitly bypasses request validation.
    pub fn unsafe_bypass(predicate: P) -> Self {
        Self {
            predicate,
            relaxed: true,
        }
    }
}

impl<S, P> Layer<S> for ValidateRequestLayer<P>
where
    P: Clone,
{
    type Service = ValidateRequest<S, P>;

    fn layer(&self, inner: S) -> Self::Service {
        ValidateRequest {
            inner,
            predicate: self.predicate.clone(),
            relaxed: self.relaxed,
        }
    }
}

/// Request validation middleware.
pub struct ValidateRequest<S, P> {
    inner: S,
    predicate: P,
    relaxed: bool,
}

impl<Cx, S, P> Service<Cx, Request<Body>> for ValidateRequest<S, P>
where
    S: Service<Cx, Request<Body>, Response = Response<Body>>,
    S::Error: Into<BoxError>,
    P: Fn(&Request<Body>) -> bool,
{
    type Response = Response<Body>;
    type Error = ServiceError;

    fn ready(&mut self, cx: &Cx) -> Result<Readiness, Self::Error> {
        self.inner
            .ready(cx)
            .map_err(|error| ServiceError::Inner(error.into()))
    }

    fn call(&mut self, cx: &Cx, request: Request<Body>) -> Result<Self::Response, Self::Error> {
        if !self.relaxed && !(self.predicate)(&request) {
            return Ok(empty_response(StatusCode::BAD_REQUEST));
        }
        self.inner
            .call(cx, request)
            .map_err(|error| ServiceError::Inner(error.into()))
    }
}
