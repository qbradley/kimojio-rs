// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{Method, Request, Response, StatusCode};
use kimojio_stack_http::Body;
use kimojio_stack_router::{
    PathParams, QueryParams, Rejection, Router, State, extractor_fn, handler_fn,
};
use kimojio_stack_tower::Service;

#[derive(Debug)]
pub struct RoutesState {
    pub prefix: &'static str,
}

pub fn router<Cx>() -> Router<Cx> {
    let api = Router::new()
        .route(
            "/users/:id",
            Method::GET,
            extractor_fn::<PathParams, _>(|_: &Cx, path: PathParams| {
                Ok::<_, Rejection>(format!("user {}", path.get("id").unwrap()))
            }),
        )
        .unwrap()
        .route(
            "/query",
            Method::GET,
            extractor_fn::<QueryParams, _>(|_: &Cx, query: QueryParams| {
                Ok::<_, Rejection>(query.get("q").unwrap_or("missing").to_owned())
            }),
        )
        .unwrap()
        .route(
            "/state",
            Method::GET,
            extractor_fn::<State<RoutesState>, _>(|_: &Cx, state: State<RoutesState>| {
                Ok::<_, Rejection>(format!("{}-state", state.0.prefix))
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
        .fallback(handler_fn(|_: &Cx, _| {
            Ok::<_, Rejection>((StatusCode::NOT_FOUND, "missing"))
        }))
        .with_state(RoutesState { prefix: "routes" })
}

pub fn run(path: &str) -> Response<Body> {
    let mut router = router::<()>();
    router
        .call(
            &(),
            Request::builder()
                .method(Method::GET)
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .unwrap_or_else(Rejection::into_response)
}
