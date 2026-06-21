// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

/// Bounded buffer layer.
#[derive(Clone)]
pub struct BufferLayer {
    capacity: usize,
    queued: Arc<AtomicUsize>,
}

impl BufferLayer {
    /// Creates a bounded buffer layer.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            queued: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl<S> Layer<S> for BufferLayer {
    type Service = Buffer<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Buffer {
            inner,
            capacity: self.capacity,
            queued: Arc::clone(&self.queued),
        }
    }
}

/// Bounded buffer middleware.
pub struct Buffer<S> {
    inner: S,
    capacity: usize,
    queued: Arc<AtomicUsize>,
}

impl<Cx, Request, S> Service<Cx, Request> for Buffer<S>
where
    S: Service<Cx, Request>,
    S::Error: Into<BoxError>,
{
    type Response = S::Response;
    type Error = ServiceError;

    fn ready(&mut self, cx: &Cx) -> Result<Readiness, Self::Error> {
        if self.queued.load(Ordering::Acquire) >= self.capacity {
            return Ok(Readiness::Overloaded);
        }
        self.inner
            .ready(cx)
            .map_err(|error| ServiceError::Inner(error.into()))
    }

    fn call(&mut self, cx: &Cx, request: Request) -> Result<Self::Response, Self::Error> {
        let guard = QueueGuard::try_acquire(&self.queued, self.capacity)?;
        let result = self
            .inner
            .call(cx, request)
            .map_err(|error| ServiceError::Inner(error.into()));
        drop(guard);
        result
    }
}

struct QueueGuard<'a> {
    queued: &'a AtomicUsize,
}

impl<'a> QueueGuard<'a> {
    fn try_acquire(queued: &'a AtomicUsize, capacity: usize) -> Result<Self, ServiceError> {
        let mut current = queued.load(Ordering::Acquire);
        loop {
            if current >= capacity {
                return Err(ServiceError::Overloaded);
            }
            match queued.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(Self { queued }),
                Err(actual) => current = actual,
            }
        }
    }
}

impl Drop for QueueGuard<'_> {
    fn drop(&mut self) {
        self.queued.fetch_sub(1, Ordering::AcqRel);
    }
}
