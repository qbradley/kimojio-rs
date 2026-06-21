// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

/// Layer that recreates an inner service after failure.
#[derive(Clone)]
pub struct ReconnectLayer<F> {
    factory: F,
}

impl<F> ReconnectLayer<F> {
    /// Creates a reconnect layer from a service factory.
    pub fn new(factory: F) -> Self {
        Self { factory }
    }
}

impl<S, F> Layer<S> for ReconnectLayer<F>
where
    F: Clone,
{
    type Service = Reconnect<S, F>;

    fn layer(&self, inner: S) -> Self::Service {
        Reconnect {
            inner,
            factory: self.factory.clone(),
        }
    }
}

/// Reconnect middleware.
pub struct Reconnect<S, F> {
    inner: S,
    factory: F,
}

impl<Cx, Request, S, F> Service<Cx, Request> for Reconnect<S, F>
where
    Request: Clone,
    S: Service<Cx, Request>,
    S::Error: Into<BoxError>,
    F: FnMut() -> S,
{
    type Response = S::Response;
    type Error = ServiceError;

    fn ready(&mut self, cx: &Cx) -> Result<Readiness, Self::Error> {
        self.inner
            .ready(cx)
            .map_err(|error| ServiceError::Inner(error.into()))
    }

    fn call(&mut self, cx: &Cx, request: Request) -> Result<Self::Response, Self::Error> {
        match self.inner.call(cx, request.clone()) {
            Ok(response) => Ok(response),
            Err(_) => {
                self.inner = (self.factory)();
                self.inner
                    .call(cx, request)
                    .map_err(|error| ServiceError::Inner(error.into()))
            }
        }
    }
}
