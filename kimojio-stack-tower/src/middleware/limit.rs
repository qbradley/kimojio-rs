// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

/// Concurrency limit layer.
#[derive(Clone)]
pub struct ConcurrencyLimitLayer {
    max: usize,
    in_flight: Arc<AtomicUsize>,
}

impl ConcurrencyLimitLayer {
    /// Creates a concurrency limit layer.
    pub fn new(max: usize) -> Self {
        Self {
            max: max.max(1),
            in_flight: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl<S> Layer<S> for ConcurrencyLimitLayer {
    type Service = ConcurrencyLimit<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ConcurrencyLimit {
            inner,
            max: self.max,
            in_flight: Arc::clone(&self.in_flight),
        }
    }
}

/// Concurrency limit middleware.
pub struct ConcurrencyLimit<S> {
    inner: S,
    max: usize,
    in_flight: Arc<AtomicUsize>,
}

impl<Cx, Request, S> Service<Cx, Request> for ConcurrencyLimit<S>
where
    S: Service<Cx, Request>,
    S::Error: Into<BoxError>,
{
    type Response = S::Response;
    type Error = ServiceError;

    fn ready(&mut self, cx: &Cx) -> Result<Readiness, Self::Error> {
        if self.in_flight.load(Ordering::Acquire) >= self.max {
            return Ok(Readiness::Overloaded);
        }
        self.inner
            .ready(cx)
            .map_err(|error| ServiceError::Inner(error.into()))
    }

    fn call(&mut self, cx: &Cx, request: Request) -> Result<Self::Response, Self::Error> {
        let guard = InFlightGuard::try_acquire(&self.in_flight, self.max)?;
        let result = self
            .inner
            .call(cx, request)
            .map_err(|error| ServiceError::Inner(error.into()));
        drop(guard);
        result
    }
}

struct InFlightGuard<'a> {
    counter: &'a AtomicUsize,
}

impl<'a> InFlightGuard<'a> {
    fn try_acquire(counter: &'a AtomicUsize, max: usize) -> Result<Self, ServiceError> {
        let mut current = counter.load(Ordering::Acquire);
        loop {
            if current >= max {
                return Err(ServiceError::Overloaded);
            }
            match counter.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(Self { counter }),
                Err(actual) => current = actual,
            }
        }
    }
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Simple bounded-call rate limit layer.
#[derive(Clone)]
pub struct RateLimitLayer {
    remaining: Arc<AtomicUsize>,
}

impl RateLimitLayer {
    /// Creates a rate limit that allows `limit` calls over the layer lifetime.
    pub fn new(limit: usize) -> Self {
        Self {
            remaining: Arc::new(AtomicUsize::new(limit)),
        }
    }
}

impl<S> Layer<S> for RateLimitLayer {
    type Service = RateLimit<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RateLimit {
            inner,
            remaining: Arc::clone(&self.remaining),
        }
    }
}

/// Simple bounded-call rate limit middleware.
pub struct RateLimit<S> {
    inner: S,
    remaining: Arc<AtomicUsize>,
}

impl<Cx, Request, S> Service<Cx, Request> for RateLimit<S>
where
    S: Service<Cx, Request>,
    S::Error: Into<BoxError>,
{
    type Response = S::Response;
    type Error = ServiceError;

    fn ready(&mut self, cx: &Cx) -> Result<Readiness, Self::Error> {
        if self.remaining.load(Ordering::Acquire) == 0 {
            return Ok(Readiness::Overloaded);
        }
        self.inner
            .ready(cx)
            .map_err(|error| ServiceError::Inner(error.into()))
    }

    fn call(&mut self, cx: &Cx, request: Request) -> Result<Self::Response, Self::Error> {
        let mut remaining = self.remaining.load(Ordering::Acquire);
        loop {
            if remaining == 0 {
                return Err(ServiceError::Overloaded);
            }
            match self.remaining.compare_exchange_weak(
                remaining,
                remaining - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => remaining = actual,
            }
        }
        self.inner
            .call(cx, request)
            .map_err(|error| ServiceError::Inner(error.into()))
    }
}
