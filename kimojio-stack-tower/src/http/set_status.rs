// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{Request, Response, StatusCode};
use kimojio_stack_http::Body;

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

/// Layer that overrides response status.
#[derive(Clone, Copy, Debug)]
pub struct SetStatusLayer {
    status: StatusCode,
}

impl SetStatusLayer {
    /// Creates a status override layer.
    pub const fn new(status: StatusCode) -> Self {
        Self { status }
    }
}

impl<S> Layer<S> for SetStatusLayer {
    type Service = SetStatus<S>;

    fn layer(&self, inner: S) -> Self::Service {
        SetStatus {
            inner,
            status: self.status,
        }
    }
}

/// Set-status middleware.
pub struct SetStatus<S> {
    inner: S,
    status: StatusCode,
}

impl<Cx, S> Service<Cx, Request<Body>> for SetStatus<S>
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
        let mut response = self
            .inner
            .call(cx, request)
            .map_err(|error| ServiceError::Inner(error.into()))?;
        *response.status_mut() = self.status;
        Ok(response)
    }
}
