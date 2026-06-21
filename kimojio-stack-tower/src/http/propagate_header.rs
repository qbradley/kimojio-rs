// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{HeaderName, Request, Response};
use kimojio_stack_http::Body;

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

/// Layer that copies a request header to the response.
#[derive(Clone, Debug)]
pub struct PropagateHeaderLayer {
    name: HeaderName,
}

impl PropagateHeaderLayer {
    /// Creates a propagation layer.
    pub fn new(name: HeaderName) -> Self {
        Self { name }
    }
}

impl<S> Layer<S> for PropagateHeaderLayer {
    type Service = PropagateHeader<S>;

    fn layer(&self, inner: S) -> Self::Service {
        PropagateHeader {
            inner,
            name: self.name.clone(),
        }
    }
}

/// Header propagation middleware.
pub struct PropagateHeader<S> {
    inner: S,
    name: HeaderName,
}

impl<Cx, S> Service<Cx, Request<Body>> for PropagateHeader<S>
where
    S: Service<Cx, Request<Body>, Response = Response<Body>>,
    S::Error: Into<BoxError>,
{
    type Response = Response<Body>;
    type Error = ServiceError;

    fn ready(&mut self, cx: &Cx) -> Result<Readiness, Self::Error> {
        self.inner
            .ready(cx)
            .map_err(|error| ServiceError::Inner(error.into()))
    }

    fn call(&mut self, cx: &Cx, request: Request<Body>) -> Result<Self::Response, Self::Error> {
        let value = request.headers().get(&self.name).cloned();
        let mut response = self
            .inner
            .call(cx, request)
            .map_err(|error| ServiceError::Inner(error.into()))?;
        if let Some(value) = value {
            response.headers_mut().insert(self.name.clone(), value);
        }
        Ok(response)
    }
}
