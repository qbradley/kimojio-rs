// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{Method, Request, StatusCode};
use kimojio_stack_http::Body;
use kimojio_stack_router::{PathParams, Rejection, Router, State, extractor_fn, handler_fn};
use kimojio_stack_tower::Service;

#[derive(Debug)]
struct ConformanceState {
    service_name: &'static str,
}

fn router<Cx>() -> Router<Cx> {
    let api = Router::new()
        .route(
            "/items/:id",
            Method::GET,
            extractor_fn::<PathParams, _>(|_: &Cx, path: PathParams| {
                Ok::<_, Rejection>(format!("item {}", path.get("id").unwrap()))
            }),
        )
        .unwrap()
        .route(
            "/state",
            Method::GET,
            extractor_fn::<State<ConformanceState>, _>(|_: &Cx, state: State<ConformanceState>| {
                Ok::<_, Rejection>(format!("state {}", state.0.service_name))
            }),
        )
        .unwrap();
    Router::new()
        .route(
            "/health",
            Method::GET,
            handler_fn(|_: &Cx, _| Ok::<_, Rejection>("ok")),
        )
        .unwrap()
        .nest("/api", api)
        .unwrap()
        .with_state(ConformanceState {
            service_name: "stack-router",
        })
}

fn request(path: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(path)
        .body(Body::empty())
        .unwrap()
}

fn body(response: http::Response<Body>) -> String {
    String::from_utf8_lossy(response.body().as_bytes()).into_owned()
}

fn assert_router_scenarios<Cx>(runtime_family: &str, router: &mut Router<Cx>, cx: &Cx) {
    for (scenario, path, expected_status, expected_body) in [
        ("health", "/health", StatusCode::OK, "ok"),
        ("nested path", "/api/items/7", StatusCode::OK, "item 7"),
        ("state", "/api/state", StatusCode::OK, "state stack-router"),
        (
            "fallback",
            "/missing",
            StatusCode::NOT_FOUND,
            "route not found",
        ),
    ] {
        println!("runtime={runtime_family} scenario={scenario}");
        let response = router.call(cx, request(path)).unwrap();
        assert_eq!(response.status(), expected_status, "{scenario}");
        assert_eq!(body(response), expected_body, "{scenario}");
    }
}

#[test]
fn stack_router_conformance_stack_runtime() {
    let mut runtime = kimojio_stack::Runtime::new();
    runtime.block_on(|cx| {
        let mut router = router();
        assert_router_scenarios("stack", &mut router, cx);
    });
}

#[test]
fn stack_router_conformance_steal_runtime() {
    let mut runtime = kimojio_stack_steal::Runtime::new();
    runtime.block_on(|cx| {
        let mut router = router();
        assert_router_scenarios("stack-steal", &mut router, cx);
    });
}
