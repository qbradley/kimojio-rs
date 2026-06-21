// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::panic::{self, AssertUnwindSafe};

use http::{Request, Response, StatusCode};
use kimojio_stack_http::Body;

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

use super::empty_response;

/// Layer that converts handler panics into 500 responses.
#[derive(Clone, Copy, Debug, Default)]
pub struct CatchPanicLayer;

impl<S> Layer<S> for CatchPanicLayer {
    type Service = CatchPanic<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CatchPanic { inner }
    }
}

/// Panic-catching middleware.
pub struct CatchPanic<S> {
    inner: S,
}

impl<Cx, S> Service<Cx, Request<Body>> for CatchPanic<S>
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
        match panic::catch_unwind(AssertUnwindSafe(|| self.inner.call(cx, request))) {
            Ok(result) => result.map_err(|error| ServiceError::Inner(error.into())),
            Err(_) => Ok(empty_response(StatusCode::INTERNAL_SERVER_ERROR)),
        }
    }
}
