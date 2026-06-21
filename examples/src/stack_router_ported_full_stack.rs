// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{HeaderName, HeaderValue, Method, Request, Response, StatusCode};
use kimojio_stack_http::Body;
use kimojio_stack_tower::http::{
    AuthLayer, CacheLayer, GovernorLayer, MemoryCacheStore, MemorySessionStore, RequestIdLayer,
    SessionLayer, SetHeaderLayer, TraceLayer,
};
use kimojio_stack_tower::{
    LoadShedLayer, Readiness, RetryLayer, Service, ServiceError, ServiceExt, TimeoutLayer,
    service_fn,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FullStackReport {
    pub unauthorized_status: StatusCode,
    pub first_status: StatusCode,
    pub cached_status: StatusCode,
    pub rate_limited_status: StatusCode,
    pub has_request_id: bool,
    pub has_stack_header: bool,
    pub has_session_cookie: bool,
    pub trace_events: usize,
}

pub fn run(authorized: bool) -> StatusCode {
    if authorized {
        run_report().first_status
    } else {
        run_report().unauthorized_status
    }
}

pub fn run_report() -> FullStackReport {
    let trace = TraceLayer::new();
    let mut service = service_fn(|_: &(), _request: Request<Body>| {
        Ok::<_, ServiceError>(Response::new(Body::empty()))
    })
    .layer(SetHeaderLayer::response(
        HeaderName::from_static("x-stack"),
        HeaderValue::from_static("router"),
    ))
    .layer(RequestIdLayer::new(HeaderName::from_static("x-request-id")))
    .layer(SessionLayer::new(MemorySessionStore::new()))
    .layer(CacheLayer::new(MemoryCacheStore::new(4)))
    .layer(GovernorLayer::new(2, |request: &Request<Body>| {
        request.uri().path().to_owned()
    }))
    .layer(AuthLayer::new(|request: &Request<Body>| {
        request.headers().contains_key("authorization")
    }))
    .layer(trace.clone())
    .layer(RetryLayer::new(1))
    .layer(TimeoutLayer::new(std::time::Duration::from_secs(1)))
    .layer(LoadShedLayer);

    assert_eq!(service.ready(&()).unwrap(), Readiness::Ready);
    let unauthorized = Request::builder()
        .method(Method::GET)
        .uri("/full")
        .body(Body::empty())
        .unwrap();
    let unauthorized_status = service.call(&(), unauthorized).unwrap().status();

    let mut first_request = Request::builder()
        .method(Method::GET)
        .uri("/full")
        .body(Body::empty())
        .unwrap();
    first_request
        .headers_mut()
        .insert("authorization", HeaderValue::from_static("token"));
    let first = service.call(&(), first_request).unwrap();
    let has_request_id = first.headers().contains_key("x-request-id");
    let has_stack_header = first.headers().contains_key("x-stack");
    let has_session_cookie = first.headers().contains_key(http::header::SET_COOKIE);
    let first_status = first.status();

    let mut cached_request = Request::builder()
        .method(Method::GET)
        .uri("/full")
        .body(Body::empty())
        .unwrap();
    cached_request
        .headers_mut()
        .insert("authorization", HeaderValue::from_static("token"));
    let cached_status = service.call(&(), cached_request).unwrap().status();

    let mut limited_request = Request::builder()
        .method(Method::GET)
        .uri("/full")
        .body(Body::empty())
        .unwrap();
    limited_request
        .headers_mut()
        .insert("authorization", HeaderValue::from_static("token"));
    let rate_limited_status = service.call(&(), limited_request).unwrap().status();

    FullStackReport {
        unauthorized_status,
        first_status,
        cached_status,
        rate_limited_status,
        has_request_id,
        has_stack_header,
        has_session_cookie,
        trace_events: trace.events(),
    }
}
