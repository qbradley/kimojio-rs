// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{HeaderName, HeaderValue, Method, Request, Response, StatusCode, header};
use kimojio_stack_http::Body;

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

use super::empty_response;

/// CORS layer with conservative default origin matching.
#[derive(Clone, Debug)]
pub struct CorsLayer {
    allowed_origin: Option<String>,
    allow_methods: Vec<Method>,
    allow_headers: Vec<HeaderName>,
    allow_any: bool,
}

impl CorsLayer {
    /// Allows one origin.
    pub fn allow_origin(origin: impl Into<String>) -> Self {
        Self {
            allowed_origin: Some(origin.into()),
            allow_methods: vec![Method::GET, Method::POST, Method::PUT, Method::DELETE],
            allow_headers: Vec::new(),
            allow_any: false,
        }
    }

    /// Explicitly allows any origin.
    pub fn relaxed_allow_any() -> Self {
        Self {
            allowed_origin: None,
            allow_methods: vec![Method::GET, Method::POST, Method::PUT, Method::DELETE],
            allow_headers: Vec::new(),
            allow_any: true,
        }
    }

    /// Sets methods allowed for CORS preflight responses.
    pub fn allow_methods(mut self, methods: Vec<Method>) -> Self {
        self.allow_methods = methods;
        self
    }

    /// Sets headers allowed for CORS preflight responses.
    pub fn allow_headers(mut self, headers: Vec<HeaderName>) -> Self {
        self.allow_headers = headers;
        self
    }
}

impl<S> Layer<S> for CorsLayer {
    type Service = Cors<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Cors {
            inner,
            allowed_origin: self.allowed_origin.clone(),
            allow_methods: self.allow_methods.clone(),
            allow_headers: self.allow_headers.clone(),
            allow_any: self.allow_any,
        }
    }
}

/// CORS middleware.
pub struct Cors<S> {
    inner: S,
    allowed_origin: Option<String>,
    allow_methods: Vec<Method>,
    allow_headers: Vec<HeaderName>,
    allow_any: bool,
}

impl<Cx, S> Service<Cx, Request<Body>> for Cors<S>
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
        let origin = request
            .headers()
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        if let Some(origin) = &origin
            && !self.allow_any
            && self.allowed_origin.as_deref() != Some(origin.as_str())
        {
            return Ok(empty_response(StatusCode::FORBIDDEN));
        }
        let is_preflight = request.method() == Method::OPTIONS
            && request.headers().contains_key(header::ORIGIN)
            && request
                .headers()
                .contains_key(header::ACCESS_CONTROL_REQUEST_METHOD);
        if is_preflight {
            let mut response = empty_response(StatusCode::NO_CONTENT);
            self.apply_origin(origin.as_deref(), &mut response);
            self.apply_preflight(&mut response);
            return Ok(response);
        }
        let mut response = self
            .inner
            .call(cx, request)
            .map_err(|error| ServiceError::Inner(error.into()))?;
        self.apply_origin(origin.as_deref(), &mut response);
        Ok(response)
    }
}

impl<S> Cors<S> {
    fn apply_origin(&self, origin: Option<&str>, response: &mut Response<Body>) {
        let value = if self.allow_any {
            "*"
        } else {
            origin.unwrap_or_default()
        };
        if !value.is_empty() {
            response.headers_mut().insert(
                header::ACCESS_CONTROL_ALLOW_ORIGIN,
                value.parse().expect("valid CORS origin header"),
            );
            append_vary(response.headers_mut(), "Origin");
        }
    }

    fn apply_preflight(&self, response: &mut Response<Body>) {
        let methods = self
            .allow_methods
            .iter()
            .map(Method::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        if !methods.is_empty() {
            response.headers_mut().insert(
                header::ACCESS_CONTROL_ALLOW_METHODS,
                methods.parse().expect("valid CORS methods header"),
            );
            append_vary(response.headers_mut(), "Access-Control-Request-Method");
        }
        let headers = self
            .allow_headers
            .iter()
            .map(HeaderName::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        if !headers.is_empty() {
            response.headers_mut().insert(
                header::ACCESS_CONTROL_ALLOW_HEADERS,
                headers.parse().expect("valid CORS headers header"),
            );
            append_vary(response.headers_mut(), "Access-Control-Request-Headers");
        }
    }
}

fn append_vary(headers: &mut http::HeaderMap, name: &str) {
    let Some(existing) = headers
        .get(header::VARY)
        .and_then(|value| value.to_str().ok())
    else {
        if let Ok(value) = HeaderValue::from_str(name) {
            headers.insert(header::VARY, value);
        }
        return;
    };
    if existing
        .split(',')
        .any(|value| value.trim().eq_ignore_ascii_case(name))
    {
        return;
    }
    let value = format!("{existing}, {name}");
    if let Ok(value) = HeaderValue::from_str(&value) {
        headers.insert(header::VARY, value);
    }
}
