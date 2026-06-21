// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use http::{HeaderName, HeaderValue, Request, Response};
use kimojio_stack_http::Body;

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

/// Layer that ensures requests and responses carry a request ID.
#[derive(Clone, Debug)]
pub struct RequestIdLayer {
    header: HeaderName,
    next: Arc<AtomicU64>,
}

impl RequestIdLayer {
    /// Creates a request ID layer.
    pub fn new(header: HeaderName) -> Self {
        Self {
            header,
            next: Arc::new(AtomicU64::new(1)),
        }
    }
}

impl<S> Layer<S> for RequestIdLayer {
    type Service = RequestId<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequestId {
            inner,
            header: self.header.clone(),
            next: Arc::clone(&self.next),
        }
    }
}

/// Request ID middleware.
pub struct RequestId<S> {
    inner: S,
    header: HeaderName,
    next: Arc<AtomicU64>,
}

impl<Cx, S> Service<Cx, Request<Body>> for RequestId<S>
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

    fn call(&mut self, cx: &Cx, mut request: Request<Body>) -> Result<Self::Response, Self::Error> {
        let value = request
            .headers()
            .get(&self.header)
            .cloned()
            .unwrap_or_else(|| {
                let id = self.next.fetch_add(1, Ordering::AcqRel);
                HeaderValue::from_str(&format!("req-{id}")).expect("valid generated request id")
            });
        request
            .headers_mut()
            .insert(self.header.clone(), value.clone());
        let mut response = self
            .inner
            .call(cx, request)
            .map_err(|error| ServiceError::Inner(error.into()))?;
        response.headers_mut().insert(self.header.clone(), value);
        Ok(response)
    }
}
