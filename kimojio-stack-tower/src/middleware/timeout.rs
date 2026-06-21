// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::time::{Duration, Instant};

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

/// Layer that reports timeout when a call exceeds a deadline budget.
///
/// This first stackful timeout wrapper is cooperative: it does not preempt a
/// synchronous inner service while it is running. It reports slow calls after the
/// inner service returns, so inner cleanup and drops still run deterministically.
#[derive(Clone, Copy, Debug)]
pub struct TimeoutLayer {
    timeout: Duration,
}

impl TimeoutLayer {
    /// Creates a timeout layer.
    pub const fn new(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl<S> Layer<S> for TimeoutLayer {
    type Service = Timeout<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Timeout {
            inner,
            timeout: self.timeout,
        }
    }
}

/// Timeout middleware.
pub struct Timeout<S> {
    inner: S,
    timeout: Duration,
}

impl<Cx, Request, S> Service<Cx, Request> for Timeout<S>
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
        let started = Instant::now();
        let result = self
            .inner
            .call(cx, request)
            .map_err(|error| ServiceError::Inner(error.into()));
        if started.elapsed() > self.timeout {
            Err(ServiceError::Timeout)
        } else {
            result
        }
    }
}
