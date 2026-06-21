// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

/// Layer that sheds work when the inner service is not ready.
#[derive(Clone, Copy, Debug, Default)]
pub struct LoadShedLayer;

impl<S> Layer<S> for LoadShedLayer {
    type Service = LoadShed<S>;

    fn layer(&self, inner: S) -> Self::Service {
        LoadShed { inner }
    }
}

/// Load shedding middleware.
pub struct LoadShed<S> {
    inner: S,
}

impl<Cx, Request, S> Service<Cx, Request> for LoadShed<S>
where
    S: Service<Cx, Request>,
    S::Error: Into<BoxError>,
{
    type Response = S::Response;
    type Error = ServiceError;

    fn ready(&mut self, cx: &Cx) -> Result<Readiness, Self::Error> {
        self.inner
            .ready(cx)
            .map_err(|error| ServiceError::Inner(error.into()))
    }

    fn call(&mut self, cx: &Cx, request: Request) -> Result<Self::Response, Self::Error> {
        match self
            .inner
            .ready(cx)
            .map_err(|error| ServiceError::Inner(error.into()))?
        {
            Readiness::Ready => self
                .inner
                .call(cx, request)
                .map_err(|error| ServiceError::Inner(error.into())),
            Readiness::NotReady | Readiness::Overloaded => Err(ServiceError::Overloaded),
        }
    }
}
