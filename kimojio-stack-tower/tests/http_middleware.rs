// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::panic;

use http::{HeaderName, HeaderValue, Method, Request, Response, StatusCode, header};
use kimojio_stack_http::Body;
use kimojio_stack_tower::http::{
    AddExtensionLayer, AuthLayer, CatchPanicLayer, CorsLayer, CsrfLayer, HttpTimeoutLayer,
    NormalizePathLayer, PropagateHeaderLayer, RequestIdLayer, SensitiveHeadersLayer,
    SetHeaderLayer, SetStatusLayer, TraceEvent, TraceLayer, ValidateRequestLayer,
};
use kimojio_stack_tower::{Service, ServiceError, ServiceExt, service_fn};

fn request(method: Method, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

fn ok_service() -> impl Service<(), Request<Body>, Response = Response<Body>, Error = ServiceError>
{
    service_fn(|_: &(), _request: Request<Body>| {
        Ok::<_, ServiceError>(Response::new(
            Body::copy_from_slice(b"ok", Default::default()).unwrap(),
        ))
    })
}

#[test]
fn http_middleware_add_extension_and_validate_request() {
    let mut service = service_fn(|_: &(), request: Request<Body>| {
        let extension = request
            .extensions()
            .get::<u64>()
            .copied()
            .unwrap_or_default();
        Ok::<_, ServiceError>(Response::new(
            Body::copy_from_slice(extension.to_string().as_bytes(), Default::default()).unwrap(),
        ))
    })
    .layer(AddExtensionLayer::new(42_u64))
    .layer(ValidateRequestLayer::new(|request: &Request<Body>| {
        request.uri().path() == "/allowed"
    }));

    assert_eq!(
        service
            .call(&(), request(Method::GET, "/allowed"))
            .unwrap()
            .body()
            .as_bytes(),
        b"42"
    );
    assert_eq!(
        service
            .call(&(), request(Method::GET, "/denied"))
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );

    let mut bypass =
        ok_service().layer(ValidateRequestLayer::unsafe_bypass(|_: &Request<Body>| {
            false
        }));
    assert_eq!(
        bypass
            .call(&(), request(Method::GET, "/denied"))
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

#[test]
fn http_middleware_auth_cors_and_csrf_cover_default_and_relaxed_paths() {
    let mut auth = ok_service().layer(AuthLayer::new(|request: &Request<Body>| {
        request.headers().contains_key("authorization")
    }));
    assert_eq!(
        auth.call(&(), request(Method::GET, "/")).unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let mut authed = request(Method::GET, "/");
    authed
        .headers_mut()
        .insert("authorization", HeaderValue::from_static("token"));
    assert_eq!(auth.call(&(), authed).unwrap().status(), StatusCode::OK);

    let mut relaxed_auth =
        ok_service().layer(AuthLayer::relaxed_allow_all(|_: &Request<Body>| false));
    assert_eq!(
        relaxed_auth
            .call(&(), request(Method::GET, "/"))
            .unwrap()
            .status(),
        StatusCode::OK
    );

    let mut cors = ok_service().layer(
        CorsLayer::allow_origin("https://allowed.example")
            .allow_methods(vec![Method::GET, Method::POST])
            .allow_headers(vec![HeaderName::from_static("x-token")]),
    );
    let mut denied = request(Method::GET, "/");
    denied.headers_mut().insert(
        header::ORIGIN,
        HeaderValue::from_static("https://denied.example"),
    );
    assert_eq!(
        cors.call(&(), denied).unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let mut allowed = request(Method::OPTIONS, "/");
    allowed.headers_mut().insert(
        header::ORIGIN,
        HeaderValue::from_static("https://allowed.example"),
    );
    allowed.headers_mut().insert(
        header::ACCESS_CONTROL_REQUEST_METHOD,
        HeaderValue::from_static("POST"),
    );
    allowed
        .headers_mut()
        .insert("x-token", HeaderValue::from_static("ok"));
    let allowed = cors.call(&(), allowed).unwrap();
    assert_eq!(allowed.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        allowed
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_METHODS)
            .unwrap(),
        "GET, POST"
    );
    assert_eq!(
        allowed
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_HEADERS)
            .unwrap(),
        "x-token"
    );
    let non_cors_options = request(Method::OPTIONS, "/");
    assert_eq!(
        cors.call(&(), non_cors_options).unwrap().status(),
        StatusCode::OK
    );

    let mut relaxed_cors = ok_service().layer(CorsLayer::relaxed_allow_any());
    let mut any = request(Method::GET, "/");
    any.headers_mut().insert(
        header::ORIGIN,
        HeaderValue::from_static("https://any.example"),
    );
    assert_eq!(
        relaxed_cors
            .call(&(), any)
            .unwrap()
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .unwrap(),
        "*"
    );

    let mut csrf = ok_service().layer(CsrfLayer::new("token"));
    assert_eq!(
        csrf.call(&(), request(Method::POST, "/")).unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let mut csrf_ok = request(Method::POST, "/");
    csrf_ok
        .headers_mut()
        .insert("x-csrf-token", HeaderValue::from_static("token"));
    assert_eq!(csrf.call(&(), csrf_ok).unwrap().status(), StatusCode::OK);
    let mut csrf_disabled = ok_service().layer(CsrfLayer::unsafe_disabled());
    assert_eq!(
        csrf_disabled
            .call(&(), request(Method::POST, "/"))
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

#[test]
fn http_middleware_headers_status_request_id_trace_and_normalize_path() {
    let request_id = HeaderName::from_static("x-request-id");
    let trace = TraceLayer::new();
    let mut service = service_fn(|_: &(), request: Request<Body>| {
        assert_eq!(request.uri().path(), "/path");
        assert_eq!(request.headers().get("x-injected").unwrap(), "yes");
        Ok::<_, ServiceError>(Response::new(Body::empty()))
    })
    .layer(NormalizePathLayer)
    .layer(SetHeaderLayer::request(
        HeaderName::from_static("x-injected"),
        HeaderValue::from_static("yes"),
    ))
    .layer(SetHeaderLayer::response(
        HeaderName::from_static("x-response"),
        HeaderValue::from_static("set"),
    ))
    .layer(SetStatusLayer::new(StatusCode::ACCEPTED))
    .layer(RequestIdLayer::new(request_id.clone()))
    .layer(PropagateHeaderLayer::new(HeaderName::from_static("x-copy")))
    .layer(trace.clone());

    let mut copy_request = request(Method::GET, "/path/");
    copy_request
        .headers_mut()
        .insert("x-copy", HeaderValue::from_static("copy"));
    let response = service.call(&(), copy_request).unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(response.headers().get("x-response").unwrap(), "set");
    assert_eq!(response.headers().get("x-copy").unwrap(), "copy");
    assert!(response.headers().contains_key(&request_id));
    assert_eq!(trace.events(), 2);
    let records = trace.records();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].event, TraceEvent::Request);
    assert_eq!(records[1].event, TraceEvent::Response);
    assert_eq!(records[1].status, Some(StatusCode::ACCEPTED));

    let incoming_id = HeaderValue::from_static("incoming");
    let mut with_id = request(Method::GET, "/path/");
    with_id
        .headers_mut()
        .insert(request_id.clone(), incoming_id.clone());
    let response = service.call(&(), with_id).unwrap();
    assert_eq!(response.headers().get(request_id).unwrap(), &incoming_id);
}

#[test]
fn http_middleware_sensitive_headers_and_catch_panic() {
    let mut sensitive = service_fn(|_: &(), request: Request<Body>| {
        assert!(
            request
                .headers()
                .get(header::AUTHORIZATION)
                .unwrap()
                .is_sensitive()
        );
        let mut response = Response::new(Body::empty());
        response
            .headers_mut()
            .insert(header::SET_COOKIE, HeaderValue::from_static("secret"));
        Ok::<_, ServiceError>(response)
    })
    .layer(SensitiveHeadersLayer::conservative_defaults());
    let mut sensitive_request = request(Method::GET, "/");
    sensitive_request
        .headers_mut()
        .insert(header::AUTHORIZATION, HeaderValue::from_static("secret"));
    let response = sensitive.call(&(), sensitive_request).unwrap();
    assert!(
        response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .is_sensitive()
    );

    let mut relaxed_sensitive = service_fn(|_: &(), request: Request<Body>| {
        assert!(
            !request
                .headers()
                .get(header::AUTHORIZATION)
                .unwrap()
                .is_sensitive()
        );
        Ok::<_, ServiceError>(Response::new(Body::empty()))
    })
    .layer(SensitiveHeadersLayer::relaxed_custom(vec![]));
    let mut relaxed_request = request(Method::GET, "/");
    relaxed_request
        .headers_mut()
        .insert(header::AUTHORIZATION, HeaderValue::from_static("visible"));
    let relaxed_response = relaxed_sensitive.call(&(), relaxed_request).unwrap();
    assert_eq!(relaxed_response.status(), StatusCode::OK);

    let previous_hook = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));
    let mut panic_service = service_fn(
        |_: &(), _request: Request<Body>| -> Result<Response<Body>, ServiceError> {
            panic!("caught by middleware");
        },
    )
    .layer(CatchPanicLayer);
    let response = panic_service.call(&(), request(Method::GET, "/")).unwrap();
    panic::set_hook(previous_hook);
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    let mut panic_success = ok_service().layer(CatchPanicLayer);
    assert_eq!(
        panic_success
            .call(&(), request(Method::GET, "/"))
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

#[test]
fn http_middleware_timeout_alias_reports_slow_call() {
    let mut service = service_fn(|_: &(), request: Request<Body>| {
        std::thread::sleep(std::time::Duration::from_millis(3));
        Ok::<_, ServiceError>(Response::new(request.into_body()))
    })
    .layer(HttpTimeoutLayer::new(std::time::Duration::from_millis(1)));

    assert!(matches!(
        service.call(&(), request(Method::GET, "/")),
        Err(ServiceError::Timeout)
    ));

    let mut fast = ok_service().layer(HttpTimeoutLayer::new(std::time::Duration::from_secs(1)));
    assert_eq!(
        fast.call(&(), request(Method::GET, "/")).unwrap().status(),
        StatusCode::OK
    );
}
