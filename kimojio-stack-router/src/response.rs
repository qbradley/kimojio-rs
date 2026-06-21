// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Response conversion helpers.

use bytes::Bytes;
use http::{Response, StatusCode};
use kimojio_stack_http::Body;
use kimojio_stack_tower::ServiceError;

use crate::{Rejection, StreamingBody};

/// Result alias returned by router handlers.
pub type ResponseResult = Result<Response<Body>, Rejection>;

/// Converts values returned by handlers into HTTP responses.
pub trait IntoResponse {
    /// Converts this value into a response.
    fn into_response(self) -> Response<Body>;
}

impl IntoResponse for Response<Body> {
    fn into_response(self) -> Response<Body> {
        self
    }
}

impl IntoResponse for Body {
    fn into_response(self) -> Response<Body> {
        Response::new(self)
    }
}

impl IntoResponse for Bytes {
    fn into_response(self) -> Response<Body> {
        Body::from_bytes(self, Default::default()).map_or_else(
            |_| StatusCode::PAYLOAD_TOO_LARGE.into_response(),
            IntoResponse::into_response,
        )
    }
}

impl IntoResponse for &'static str {
    fn into_response(self) -> Response<Body> {
        Body::copy_from_slice(self.as_bytes(), Default::default()).map_or_else(
            |_| StatusCode::PAYLOAD_TOO_LARGE.into_response(),
            IntoResponse::into_response,
        )
    }
}

impl IntoResponse for String {
    fn into_response(self) -> Response<Body> {
        Body::copy_from_slice(self.as_bytes(), Default::default()).map_or_else(
            |_| StatusCode::PAYLOAD_TOO_LARGE.into_response(),
            IntoResponse::into_response,
        )
    }
}

impl IntoResponse for () {
    fn into_response(self) -> Response<Body> {
        Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Body::empty())
            .expect("valid empty response")
    }
}

impl IntoResponse for StatusCode {
    fn into_response(self) -> Response<Body> {
        Response::builder()
            .status(self)
            .body(Body::empty())
            .expect("valid status response")
    }
}

impl<T> IntoResponse for (StatusCode, T)
where
    T: IntoResponse,
{
    fn into_response(self) -> Response<Body> {
        let (status, response) = self;
        let mut response = response.into_response();
        *response.status_mut() = status;
        response
    }
}

impl IntoResponse for Rejection {
    fn into_response(self) -> Response<Body> {
        self.into_response()
    }
}

impl IntoResponse for ServiceError {
    fn into_response(self) -> Response<Body> {
        let status = match self {
            ServiceError::InvalidRequest(_) => StatusCode::BAD_REQUEST,
            ServiceError::Timeout => StatusCode::REQUEST_TIMEOUT,
            ServiceError::NotReady | ServiceError::Overloaded => StatusCode::SERVICE_UNAVAILABLE,
            ServiceError::Canceled | ServiceError::Inner(_) | ServiceError::Layer { .. } => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        status.into_response()
    }
}

impl IntoResponse for StreamingBody {
    fn into_response(self) -> Response<Body> {
        self.into_buffered_response()
    }
}
