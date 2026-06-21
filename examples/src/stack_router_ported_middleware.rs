// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{HeaderName, HeaderValue, Method, Request, Response, StatusCode};
use kimojio_stack_http::Body;
use kimojio_stack_tower::http::{
    AuthLayer, CorsLayer, RequestIdLayer, SetHeaderLayer, TraceLayer, ValidateRequestLayer,
};
use kimojio_stack_tower::{Service, ServiceError, ServiceExt, service_fn};

pub fn run(authorized: bool) -> (StatusCode, bool, usize) {
    let trace = TraceLayer::new();
    let mut service = service_fn(|_: &(), _request: Request<Body>| {
        Ok::<_, ServiceError>(Response::new(Body::empty()))
    })
    .layer(SetHeaderLayer::response(
        HeaderName::from_static("x-powered-by"),
        HeaderValue::from_static("kimojio"),
    ))
    .layer(RequestIdLayer::new(HeaderName::from_static("x-request-id")))
    .layer(CorsLayer::relaxed_allow_any())
    .layer(ValidateRequestLayer::new(|request: &Request<Body>| {
        request.uri().path() == "/secure"
    }))
    .layer(AuthLayer::new(|request: &Request<Body>| {
        request.headers().contains_key("authorization")
    }))
    .layer(trace.clone());

    let mut request = Request::builder()
        .method(Method::GET)
        .uri("/secure")
        .body(Body::empty())
        .unwrap();
    if authorized {
        request
            .headers_mut()
            .insert("authorization", HeaderValue::from_static("token"));
    }
    let response = service.call(&(), request).unwrap();
    (
        response.status(),
        response.headers().contains_key("x-request-id"),
        trace.events(),
    )
}
