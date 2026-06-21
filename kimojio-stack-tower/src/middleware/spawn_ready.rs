// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

/// Layer that proactively drives inner readiness before calls.
#[derive(Clone, Default)]
pub struct SpawnReadyLayer {
    drives: Arc<AtomicUsize>,
}

impl SpawnReadyLayer {
    /// Creates a spawn-ready layer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns how many readiness drives this layer performed.
    pub fn drives(&self) -> usize {
        self.drives.load(Ordering::Acquire)
    }
}

impl<S> Layer<S> for SpawnReadyLayer {
    type Service = SpawnReady<S>;

    fn layer(&self, inner: S) -> Self::Service {
        SpawnReady {
            inner,
            drives: Arc::clone(&self.drives),
        }
    }
}

/// Readiness-driving middleware.
pub struct SpawnReady<S> {
    inner: S,
    drives: Arc<AtomicUsize>,
}

impl<Cx, Request, S> Service<Cx, Request> for SpawnReady<S>
where
    S: Service<Cx, Request>,
    S::Error: Into<BoxError>,
{
    type Response = S::Response;
    type Error = ServiceError;

    fn ready(&mut self, cx: &Cx) -> Result<Readiness, Self::Error> {
        self.drives.fetch_add(1, Ordering::AcqRel);
        self.inner
            .ready(cx)
            .map_err(|error| ServiceError::Inner(error.into()))
    }

    fn call(&mut self, cx: &Cx, request: Request) -> Result<Self::Response, Self::Error> {
        match self.ready(cx)? {
            Readiness::Ready => self
                .inner
                .call(cx, request)
                .map_err(|error| ServiceError::Inner(error.into())),
            Readiness::NotReady | Readiness::Overloaded => Err(ServiceError::Overloaded),
        }
    }
}
