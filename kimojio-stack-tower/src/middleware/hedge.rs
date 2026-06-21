// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::time::{Duration, Instant};

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

/// Layer that issues a bounded follow-up attempt when the first attempt is slow.
#[derive(Clone, Copy, Debug)]
pub struct HedgeLayer {
    threshold: Duration,
}

impl HedgeLayer {
    /// Creates a hedge layer with a latency threshold.
    pub const fn new(threshold: Duration) -> Self {
        Self { threshold }
    }
}

impl<S> Layer<S> for HedgeLayer {
    type Service = Hedge<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Hedge {
            inner,
            threshold: self.threshold,
        }
    }
}

/// Hedging middleware.
pub struct Hedge<S> {
    inner: S,
    threshold: Duration,
}

impl<Cx, Request, S> Service<Cx, Request> for Hedge<S>
where
    Request: Clone,
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
        let backup = request.clone();
        let started = Instant::now();
        match self.inner.call(cx, request) {
            Ok(response) if started.elapsed() <= self.threshold => Ok(response),
            Ok(_) | Err(_) => self
                .inner
                .call(cx, backup)
                .map_err(|error| ServiceError::Inner(error.into())),
        }
    }
}
