// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::Arc;

use http::{HeaderMap, Method, Request, Response, StatusCode};
use kimojio_stack_http::Body;
use kimojio_stack_router::{
    BodyBytes, Extension, MethodRouter, PathParams, QueryParams, Rejection, Router, State,
    extractor_fn, handler_fn,
};
use kimojio_stack_tower::Service;

fn request(method: Method, uri: &str, body: &'static [u8]) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::copy_from_slice(body, Default::default()).unwrap())
        .unwrap()
}

fn body_text(response: Response<Body>) -> String {
    String::from_utf8_lossy(response.body().as_bytes()).into_owned()
}

#[test]
fn router_core_method_and_path_routes_dispatch_to_handlers() {
    let mut router = Router::new()
        .route(
            "/health",
            Method::GET,
            handler_fn(|_: &(), _| Ok::<_, Rejection>("ok")),
        )
        .unwrap()
        .route(
            "/health",
            Method::POST,
            handler_fn(|_: &(), _| Ok::<_, Rejection>((StatusCode::CREATED, "created"))),
        )
        .unwrap();

    let get = router
        .call(&(), request(Method::GET, "/health", b""))
        .unwrap();
    assert_eq!(get.status(), StatusCode::OK);
    assert_eq!(body_text(get), "ok");

    let post = router
        .call(&(), request(Method::POST, "/health", b""))
        .unwrap();
    assert_eq!(post.status(), StatusCode::CREATED);
    assert_eq!(body_text(post), "created");
}

#[test]
fn router_core_path_query_header_body_state_and_extension_extractors_work() {
    #[derive(Debug)]
    struct AppState {
        prefix: &'static str,
    }

    let mut router = Router::new()
        .route(
            "/users/:id",
            Method::POST,
            extractor_fn::<PathParams, _>(|_: &(), path: PathParams| {
                Ok::<_, Rejection>(format!("id={}", path.get("id").unwrap()))
            }),
        )
        .unwrap()
        .route(
            "/query",
            Method::GET,
            extractor_fn::<QueryParams, _>(|_: &(), query: QueryParams| {
                Ok::<_, Rejection>(format!("q={}", query.get("q").unwrap()))
            }),
        )
        .unwrap()
        .route(
            "/body",
            Method::POST,
            extractor_fn::<BodyBytes, _>(|_: &(), body: BodyBytes| {
                Ok::<_, Rejection>(String::from_utf8_lossy(&body.0).into_owned())
            }),
        )
        .unwrap()
        .route(
            "/headers",
            Method::GET,
            extractor_fn::<HeaderMap, _>(|_: &(), headers: HeaderMap| {
                Ok::<_, Rejection>(
                    headers
                        .get("x-test")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("missing")
                        .to_owned(),
                )
            }),
        )
        .unwrap()
        .route(
            "/state",
            Method::GET,
            extractor_fn::<State<AppState>, _>(|_: &(), state: State<AppState>| {
                Ok::<_, Rejection>(format!("{}state", state.0.prefix))
            }),
        )
        .unwrap()
        .route(
            "/extension",
            Method::GET,
            extractor_fn::<Extension<Arc<String>>, _>(
                |_: &(), extension: Extension<Arc<String>>| {
                    Ok::<_, Rejection>(extension.0.as_str().to_owned())
                },
            ),
        )
        .unwrap()
        .with_state(AppState { prefix: "app-" });

    assert_eq!(
        body_text(
            router
                .call(&(), request(Method::POST, "/users/42", b""))
                .unwrap()
        ),
        "id=42"
    );
    assert_eq!(
        body_text(
            router
                .call(&(), request(Method::GET, "/query?q=hello+world", b""))
                .unwrap()
        ),
        "q=hello world"
    );
    assert_eq!(
        body_text(
            router
                .call(&(), request(Method::POST, "/body", b"payload"))
                .unwrap()
        ),
        "payload"
    );
    let mut header_request = request(Method::GET, "/headers", b"");
    header_request
        .headers_mut()
        .insert("x-test", "ok".parse().unwrap());
    assert_eq!(body_text(router.call(&(), header_request).unwrap()), "ok");
    assert_eq!(
        body_text(
            router
                .call(&(), request(Method::GET, "/state", b""))
                .unwrap()
        ),
        "app-state"
    );

    let mut extension_request = request(Method::GET, "/extension", b"");
    extension_request
        .extensions_mut()
        .insert(Arc::new("extension".to_owned()));
    assert_eq!(
        body_text(router.call(&(), extension_request).unwrap()),
        "extension"
    );
}

#[test]
fn router_core_nested_routers_merge_and_fallbacks_work() {
    let nested = Router::new()
        .route(
            "/items/:id",
            Method::GET,
            extractor_fn::<PathParams, _>(|_: &(), path: PathParams| {
                Ok::<_, Rejection>(format!("item={}", path.get("id").unwrap()))
            }),
        )
        .unwrap();
    let sibling = Router::new()
        .route(
            "/sibling",
            Method::GET,
            handler_fn(|_: &(), _| Ok::<_, Rejection>("sibling")),
        )
        .unwrap();
    let mut router = Router::new()
        .route(
            "/root",
            Method::GET,
            handler_fn(|_: &(), _| Ok::<_, Rejection>("root")),
        )
        .unwrap()
        .nest("/api", nested)
        .unwrap()
        .merge(sibling)
        .unwrap()
        .fallback(handler_fn(|_: &(), _| {
            Ok::<_, Rejection>((StatusCode::NOT_FOUND, "custom missing"))
        }));

    assert_eq!(
        body_text(
            router
                .call(&(), request(Method::GET, "/root", b""))
                .unwrap()
        ),
        "root"
    );
    assert_eq!(
        body_text(
            router
                .call(&(), request(Method::GET, "/api/items/7", b""))
                .unwrap()
        ),
        "item=7"
    );
    assert_eq!(
        body_text(
            router
                .call(&(), request(Method::GET, "/sibling", b""))
                .unwrap()
        ),
        "sibling"
    );
    let missing = router
        .call(&(), request(Method::GET, "/missing", b""))
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_text(missing), "custom missing");
}

#[test]
fn router_core_method_router_adds_multiple_methods_for_one_path() {
    let methods = MethodRouter::new()
        .get(handler_fn(|_: &(), _| Ok::<_, Rejection>("get")))
        .put(handler_fn(|_: &(), _| Ok::<_, Rejection>("put")));
    let mut router = Router::new().route_methods("/resource", methods).unwrap();

    assert_eq!(
        body_text(
            router
                .call(&(), request(Method::GET, "/resource", b""))
                .unwrap()
        ),
        "get"
    );
    assert_eq!(
        body_text(
            router
                .call(&(), request(Method::PUT, "/resource", b""))
                .unwrap()
        ),
        "put"
    );
}

#[test]
fn router_core_literal_route_wins_over_parameter_route() {
    let mut router = Router::new()
        .route(
            "/users/:id",
            Method::GET,
            extractor_fn::<PathParams, _>(|_: &(), path: PathParams| {
                Ok::<_, Rejection>(format!("param={}", path.get("id").unwrap()))
            }),
        )
        .unwrap()
        .route(
            "/users/new",
            Method::GET,
            handler_fn(|_: &(), _| Ok::<_, Rejection>("literal")),
        )
        .unwrap();

    assert_eq!(
        body_text(
            router
                .call(&(), request(Method::GET, "/users/new", b""))
                .unwrap()
        ),
        "literal"
    );
    assert_eq!(
        body_text(
            router
                .call(&(), request(Method::GET, "/users/42", b""))
                .unwrap()
        ),
        "param=42"
    );
}

#[test]
fn router_core_segment_precedence_is_independent_of_insertion_order() {
    let mut router = Router::new()
        .route(
            "/:x/foo",
            Method::GET,
            extractor_fn::<PathParams, _>(|_: &(), path: PathParams| {
                Ok::<_, Rejection>(format!("first={}", path.get("x").unwrap()))
            }),
        )
        .unwrap()
        .route(
            "/bar/:y",
            Method::GET,
            extractor_fn::<PathParams, _>(|_: &(), path: PathParams| {
                Ok::<_, Rejection>(format!("second={}", path.get("y").unwrap()))
            }),
        )
        .unwrap();

    assert_eq!(
        body_text(
            router
                .call(&(), request(Method::GET, "/bar/foo", b""))
                .unwrap()
        ),
        "second=foo"
    );

    let mut reversed = Router::new()
        .route(
            "/bar/:y",
            Method::GET,
            extractor_fn::<PathParams, _>(|_: &(), path: PathParams| {
                Ok::<_, Rejection>(format!("second={}", path.get("y").unwrap()))
            }),
        )
        .unwrap()
        .route(
            "/:x/foo",
            Method::GET,
            extractor_fn::<PathParams, _>(|_: &(), path: PathParams| {
                Ok::<_, Rejection>(format!("first={}", path.get("x").unwrap()))
            }),
        )
        .unwrap();

    assert_eq!(
        body_text(
            reversed
                .call(&(), request(Method::GET, "/bar/foo", b""))
                .unwrap()
        ),
        "second=foo"
    );
}

#[test]
fn router_core_duplicate_and_ambiguous_routes_are_rejected() {
    let duplicate = Router::new()
        .route(
            "/users/:id",
            Method::GET,
            handler_fn(|_: &(), _| Ok::<_, Rejection>("one")),
        )
        .unwrap()
        .route(
            "/users/:name",
            Method::GET,
            handler_fn(|_: &(), _| Ok::<_, Rejection>("two")),
        );
    assert!(duplicate.is_err());

    let invalid = Router::<()>::new().route(
        "missing-leading-slash",
        Method::GET,
        handler_fn(|_: &(), _| Ok::<_, Rejection>("bad")),
    );
    assert!(invalid.is_err());
}

#[test]
fn router_core_method_not_allowed_and_extractor_failures_are_rejections() {
    let mut router = Router::new()
        .route(
            "/get",
            Method::GET,
            handler_fn(|_: &(), _| Ok::<_, Rejection>("get")),
        )
        .unwrap()
        .route(
            "/state",
            Method::GET,
            extractor_fn::<State<String>, _>(|_: &(), _| Ok::<_, Rejection>("state")),
        )
        .unwrap();

    let method = router
        .call(&(), request(Method::POST, "/get", b""))
        .unwrap_err();
    assert_eq!(method.status(), StatusCode::METHOD_NOT_ALLOWED);

    let state = router
        .call(&(), request(Method::GET, "/state", b""))
        .unwrap_err();
    assert_eq!(state.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[test]
fn router_core_streaming_body_can_buffer_for_protocol_neutral_response_paths() {
    let response = kimojio_stack_router::StreamingBody::from_chunks(
        StatusCode::ACCEPTED,
        vec![
            kimojio_stack_router::StreamingChunk::data("a"),
            kimojio_stack_router::StreamingChunk::data("b"),
            kimojio_stack_router::StreamingChunk::finish(),
        ],
    )
    .into_buffered_response();

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(body_text(response), "ab");
}
