// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{Request, Response, StatusCode};
use kimojio_stack_http::Body;

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

use super::empty_response;

/// Authentication/authorization layer.
#[derive(Clone)]
pub struct AuthLayer<P> {
    predicate: P,
    relaxed: bool,
}

impl<P> AuthLayer<P> {
    /// Creates an auth layer with conservative predicate enforcement.
    pub fn new(predicate: P) -> Self {
        Self {
            predicate,
            relaxed: false,
        }
    }

    /// Explicitly relaxes authentication checks.
    pub fn relaxed_allow_all(predicate: P) -> Self {
        Self {
            predicate,
            relaxed: true,
        }
    }
}

impl<S, P> Layer<S> for AuthLayer<P>
where
    P: Clone,
{
    type Service = Auth<S, P>;

    fn layer(&self, inner: S) -> Self::Service {
        Auth {
            inner,
            predicate: self.predicate.clone(),
            relaxed: self.relaxed,
        }
    }
}

/// Auth middleware.
pub struct Auth<S, P> {
    inner: S,
    predicate: P,
    relaxed: bool,
}

impl<Cx, S, P> Service<Cx, Request<Body>> for Auth<S, P>
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
            return Ok(empty_response(StatusCode::UNAUTHORIZED));
        }
        self.inner
            .call(cx, request)
            .map_err(|error| ServiceError::Inner(error.into()))
    }
}
