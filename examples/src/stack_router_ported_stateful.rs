// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{Method, Request, Response, StatusCode};
use kimojio_stack_http::Body;
use kimojio_stack_tower::http::{
    CacheLayer, GovernorLayer, MemoryCacheStore, MemorySessionStore, Session, SessionLayer,
};
use kimojio_stack_tower::{Service, ServiceError, ServiceExt, service_fn};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatefulReport {
    pub first_body: String,
    pub cached_body: String,
    pub rate_limited_status: StatusCode,
}

pub fn run() -> (String, String) {
    let report = run_report();
    (report.first_body, report.cached_body)
}

pub fn run_report() -> StatefulReport {
    let session_store = MemorySessionStore::new();
    let cache_store = MemoryCacheStore::new(8);
    let mut service = service_fn(|_: &(), request: Request<Body>| {
        let session = request.extensions().get::<Session>().unwrap();
        Ok::<_, ServiceError>(Response::new(
            Body::copy_from_slice(session.id.as_bytes(), Default::default()).unwrap(),
        ))
    })
    .layer(SessionLayer::new(session_store))
    .layer(CacheLayer::new(cache_store))
    .layer(GovernorLayer::new(2, |request: &Request<Body>| {
        request.uri().path().to_owned()
    }));

    let first = service
        .call(
            &(),
            Request::builder()
                .method(Method::GET)
                .uri("/stateful")
                .body(Body::empty())
                .unwrap(),
        )
        .unwrap();
    let second = service
        .call(
            &(),
            Request::builder()
                .method(Method::GET)
                .uri("/stateful")
                .body(Body::empty())
                .unwrap(),
        )
        .unwrap();
    let third = service
        .call(
            &(),
            Request::builder()
                .method(Method::GET)
                .uri("/stateful")
                .body(Body::empty())
                .unwrap(),
        )
        .unwrap();
    StatefulReport {
        first_body: String::from_utf8_lossy(first.body().as_bytes()).into_owned(),
        cached_body: String::from_utf8_lossy(second.body().as_bytes()).into_owned(),
        rate_limited_status: third.status(),
    }
}
