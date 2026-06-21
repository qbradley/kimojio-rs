// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{HeaderName, Request, Response, header};
use kimojio_stack_http::Body;

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

/// Layer that marks headers as sensitive.
#[derive(Clone, Debug)]
pub struct SensitiveHeadersLayer {
    headers: Vec<HeaderName>,
}

impl SensitiveHeadersLayer {
    /// Creates a layer with conservative default sensitive headers.
    pub fn conservative_defaults() -> Self {
        Self {
            headers: vec![header::AUTHORIZATION, header::COOKIE, header::SET_COOKIE],
        }
    }

    /// Creates a layer with custom relaxed header set.
    pub fn relaxed_custom(headers: Vec<HeaderName>) -> Self {
        Self { headers }
    }
}

impl<S> Layer<S> for SensitiveHeadersLayer {
    type Service = SensitiveHeaders<S>;

    fn layer(&self, inner: S) -> Self::Service {
        SensitiveHeaders {
            inner,
            headers: self.headers.clone(),
        }
    }
}

/// Sensitive headers middleware.
pub struct SensitiveHeaders<S> {
    inner: S,
    headers: Vec<HeaderName>,
}

impl<Cx, S> Service<Cx, Request<Body>> for SensitiveHeaders<S>
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
        mark_sensitive(request.headers_mut(), &self.headers);
        let mut response = self
            .inner
            .call(cx, request)
            .map_err(|error| ServiceError::Inner(error.into()))?;
        mark_sensitive(response.headers_mut(), &self.headers);
        Ok(response)
    }
}

fn mark_sensitive(headers: &mut http::HeaderMap, names: &[HeaderName]) {
    for name in names {
        if let Some(value) = headers.get_mut(name) {
            value.set_sensitive(true);
        }
    }
}
