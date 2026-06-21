// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

/// Layer that forwards only requests accepted by a predicate.
#[derive(Clone)]
pub struct FilterLayer<P> {
    predicate: P,
}

impl<P> FilterLayer<P> {
    /// Creates a filter layer.
    pub fn new(predicate: P) -> Self {
        Self { predicate }
    }
}

impl<S, P> Layer<S> for FilterLayer<P>
where
    P: Clone,
{
    type Service = Filter<S, P>;

    fn layer(&self, inner: S) -> Self::Service {
        Filter {
            inner,
            predicate: self.predicate.clone(),
        }
    }
}

/// Predicate filter middleware.
pub struct Filter<S, P> {
    inner: S,
    predicate: P,
}

impl<Cx, Request, S, P> Service<Cx, Request> for Filter<S, P>
where
    S: Service<Cx, Request>,
    S::Error: Into<BoxError>,
    P: Fn(&Request) -> bool,
{
    type Response = S::Response;
    type Error = ServiceError;

    fn ready(&mut self, cx: &Cx) -> Result<Readiness, Self::Error> {
        self.inner
            .ready(cx)
            .map_err(|error| ServiceError::Inner(error.into()))
    }

    fn call(&mut self, cx: &Cx, request: Request) -> Result<Self::Response, Self::Error> {
        if !(self.predicate)(&request) {
            return Err(ServiceError::InvalidRequest("request rejected by filter"));
        }
        self.inner
            .call(cx, request)
            .map_err(|error| ServiceError::Inner(error.into()))
    }
}
