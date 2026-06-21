// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{Method, Request, Response, StatusCode};
use kimojio_stack_http::Body;

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

use super::empty_response;

/// CSRF validation layer.
#[derive(Clone, Debug)]
pub struct CsrfLayer {
    token: String,
    disabled: bool,
}

impl CsrfLayer {
    /// Creates CSRF validation requiring `x-csrf-token`.
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            disabled: false,
        }
    }

    /// Explicitly disables CSRF validation.
    pub fn unsafe_disabled() -> Self {
        Self {
            token: String::new(),
            disabled: true,
        }
    }
}

impl<S> Layer<S> for CsrfLayer {
    type Service = Csrf<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Csrf {
            inner,
            token: self.token.clone(),
            disabled: self.disabled,
        }
    }
}

/// CSRF middleware.
pub struct Csrf<S> {
    inner: S,
    token: String,
    disabled: bool,
}

impl<Cx, S> Service<Cx, Request<Body>> for Csrf<S>
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

    fn call(&mut self, cx: &Cx, request: Request<Body>) -> Result<Self::Response, Self::Error> {
        if !self.disabled
            && matches!(
                *request.method(),
                Method::POST | Method::PUT | Method::PATCH | Method::DELETE
            )
            && request
                .headers()
                .get("x-csrf-token")
                .and_then(|value| value.to_str().ok())
                != Some(self.token.as_str())
        {
            return Ok(empty_response(StatusCode::FORBIDDEN));
        }
        self.inner
            .call(cx, request)
            .map_err(|error| ServiceError::Inner(error.into()))
    }
}
