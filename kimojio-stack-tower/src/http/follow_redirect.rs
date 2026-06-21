// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{Request, Response, StatusCode, Uri, header};
use kimojio_stack_http::Body;

use crate::{BoxError, Layer, Readiness, Service, ServiceError};

/// Follows redirect responses for client-style services.
#[derive(Clone, Copy, Debug)]
pub struct FollowRedirectLayer {
    max_redirects: usize,
}

impl FollowRedirectLayer {
    /// Creates a redirect-following layer.
    pub fn new(max_redirects: usize) -> Self {
        Self { max_redirects }
    }
}

impl<S> Layer<S> for FollowRedirectLayer {
    type Service = FollowRedirect<S>;

    fn layer(&self, inner: S) -> Self::Service {
        FollowRedirect {
            inner,
            max_redirects: self.max_redirects,
        }
    }
}

/// Follow-redirect middleware.
pub struct FollowRedirect<S> {
    inner: S,
    max_redirects: usize,
}

impl<Cx, S> Service<Cx, Request<Body>> for FollowRedirect<S>
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
        for _ in 0..=self.max_redirects {
            let response = self
                .inner
                .call(cx, clone_request(&request)?)
                .map_err(|error| ServiceError::Inner(error.into()))?;
            if !is_redirect(response.status()) {
                return Ok(response);
            }
            let Some(location) = response.headers().get(header::LOCATION) else {
                return Ok(response);
            };
            *request.uri_mut() = location
                .to_str()
                .ok()
                .and_then(|value| value.parse::<Uri>().ok())
                .ok_or(ServiceError::InvalidRequest("invalid redirect location"))?;
        }
        Err(ServiceError::Overloaded)
    }
}

fn is_redirect(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    )
}

fn clone_request(request: &Request<Body>) -> Result<Request<Body>, ServiceError> {
    let mut builder = Request::builder()
        .method(request.method().clone())
        .uri(request.uri().clone())
        .version(request.version());
    *builder
        .headers_mut()
        .expect("new request builder has headers") = request.headers().clone();
    Ok(builder
        .body(request.body().clone())
        .expect("valid request clone"))
}
