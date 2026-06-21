// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{Request, Uri};
use kimojio_stack_http::Body;

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

/// Layer that normalizes trailing slashes.
#[derive(Clone, Copy, Debug, Default)]
pub struct NormalizePathLayer;

impl<S> Layer<S> for NormalizePathLayer {
    type Service = NormalizePath<S>;

    fn layer(&self, inner: S) -> Self::Service {
        NormalizePath { inner }
    }
}

/// Path normalization middleware.
pub struct NormalizePath<S> {
    inner: S,
}

impl<Cx, S> Service<Cx, Request<Body>> for NormalizePath<S>
where
    S: Service<Cx, Request<Body>>,
    S::Error: Into<BoxError>,
{
    type Response = S::Response;
    type Error = ServiceError;

    fn ready(&mut self, cx: &Cx) -> Result<Readiness, Self::Error> {
        self.inner
            .ready(cx)
            .map_err(|error| ServiceError::Inner(error.into()))
    }

    fn call(&mut self, cx: &Cx, mut request: Request<Body>) -> Result<Self::Response, Self::Error> {
        let uri = request.uri().clone();
        let path = uri.path();
        if path.len() > 1 && path.ends_with('/') {
            let normalized = path.trim_end_matches('/');
            let suffix = uri
                .query()
                .map(|query| format!("{normalized}?{query}"))
                .unwrap_or_else(|| normalized.to_owned());
            *request.uri_mut() = suffix.parse::<Uri>().map_err(|_| {
                ServiceError::InvalidRequest("normalized path produced invalid URI")
            })?;
        }
        self.inner
            .call(cx, request)
            .map_err(|error| ServiceError::Inner(error.into()))
    }
}
