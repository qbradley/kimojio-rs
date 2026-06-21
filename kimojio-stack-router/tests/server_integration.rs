// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use bytes::Bytes;
use http::{Method, Request, StatusCode};
use kimojio_stack_http::{
    Body, BodyLimits, HttpConfig, RuntimeStackTransport, StackTransport, h2, http1,
};
use kimojio_stack_router::{
    BodyBytes, Rejection, Router, StreamingBody, StreamingChunk, extractor_fn, handler_fn,
    serve_h2_buffered_requests, serve_h2_streaming_once, serve_http1_one,
};
use kimojio_stack_steal::RingFd;
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

fn stack_transport_pair() -> (StackTransport, StackTransport) {
    let (client_fd, server_fd) = socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::empty(),
        None,
    )
    .unwrap();
    (
        StackTransport::plaintext(client_fd),
        StackTransport::plaintext(server_fd),
    )
}

fn steal_transport_pair() -> (RuntimeStackTransport<RingFd>, RuntimeStackTransport<RingFd>) {
    let (client_fd, server_fd) = socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::empty(),
        None,
    )
    .unwrap();
    (
        RuntimeStackTransport::plaintext_socket(RingFd::from_owned(client_fd)),
        RuntimeStackTransport::plaintext_socket(RingFd::from_owned(server_fd)),
    )
}

fn body_text(response: http::Response<Body>) -> String {
    String::from_utf8_lossy(response.body().as_bytes()).into_owned()
}

#[test]
fn server_integration_http1_stack_runtime_serves_router() {
    let (client_transport, server_transport) = stack_transport_pair();
    let mut runtime = kimojio_stack::Runtime::new();

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let server = scope.spawn(move |cx| {
                let mut router = Router::new()
                    .route(
                        "/hello/:name",
                        Method::GET,
                        extractor_fn::<kimojio_stack_router::PathParams, _>(
                            |_: &_, path: kimojio_stack_router::PathParams| {
                                Ok::<_, Rejection>(format!("hello {}", path.get("name").unwrap()))
                            },
                        ),
                    )
                    .unwrap();
                serve_http1_one(cx, server_transport, &mut router, HttpConfig::default()).unwrap();
            });

            let client = scope.spawn(move |cx| {
                let mut client =
                    http1::ClientConnection::new(client_transport, HttpConfig::default());
                let request = Request::builder()
                    .method("GET")
                    .uri("/hello/stack")
                    .body(Body::empty())
                    .unwrap();
                let response = client.send(cx, &request).unwrap();
                client.close(cx).unwrap();
                response
            });

            server.join(cx);
            let response = client.join(cx);
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(body_text(response), "hello stack");
        });
    });
}

#[test]
fn server_integration_h2_stack_runtime_serves_router() {
    let (client_transport, server_transport) = stack_transport_pair();
    let mut runtime = kimojio_stack::Runtime::new();

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let server = scope.spawn(move |cx| {
                let mut router = Router::new()
                    .route(
                        "/echo",
                        Method::POST,
                        extractor_fn::<BodyBytes, _>(|_: &_, body: BodyBytes| {
                            Ok::<_, Rejection>(String::from_utf8_lossy(&body.0).into_owned())
                        }),
                    )
                    .unwrap();
                serve_h2_buffered_requests(
                    cx,
                    server_transport,
                    &mut router,
                    HttpConfig::default(),
                    1,
                )
                .unwrap();
            });

            let client = scope.spawn(move |cx| {
                let mut client = h2::ClientConnection::new(client_transport, HttpConfig::default());
                let request = Request::builder()
                    .method("POST")
                    .uri("/echo")
                    .body(Body::copy_from_slice(b"stack-h2", BodyLimits::new(1024)).unwrap())
                    .unwrap();
                let response = client.send(cx, &request).unwrap();
                client.close(cx).unwrap();
                response
            });

            server.join(cx);
            let response = client.join(cx);
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(body_text(response), "stack-h2");
        });
    });
}

#[test]
fn server_integration_h2_streaming_response_uses_protocol_specific_path() {
    let (client_transport, server_transport) = stack_transport_pair();
    let mut runtime = kimojio_stack::Runtime::new();

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let server = scope.spawn(move |cx| {
                let mut server = h2::ServerConnection::new(server_transport, HttpConfig::default());
                serve_h2_streaming_once(cx, &mut server, BodyLimits::new(1024), |request| {
                    assert_eq!(request.uri().path(), "/stream");
                    Ok(StreamingBody::from_chunks(
                        StatusCode::ACCEPTED,
                        vec![
                            StreamingChunk::data("stream-"),
                            StreamingChunk::data("body"),
                            StreamingChunk::finish(),
                        ],
                    ))
                })
                .unwrap();
                server.shutdown_write_and_close_after_peer(cx).unwrap();
            });

            let client = scope.spawn(move |cx| {
                let mut client = h2::ClientConnection::new(client_transport, HttpConfig::default());
                let request = Request::builder()
                    .method("POST")
                    .uri("/stream")
                    .body(())
                    .unwrap();
                let stream_id = client.send_request_headers(cx, &request).unwrap();
                client
                    .send_request_data(cx, stream_id, Bytes::from_static(b"ignored"))
                    .unwrap();
                client.finish_request_stream(cx, stream_id).unwrap();
                let response = client.read_response(cx, stream_id).unwrap();
                client.close(cx).unwrap();
                response
            });

            server.join(cx);
            let response = client.join(cx);
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            assert_eq!(body_text(response), "stream-body");
        });
    });
}

#[test]
fn server_integration_h2_steal_runtime_serves_router() {
    let (client_transport, server_transport) = steal_transport_pair();
    let mut runtime = kimojio_stack_steal::Runtime::new();

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let server = scope.spawn_stealable(move |cx| {
                let mut router = Router::new()
                    .route(
                        "/steal",
                        Method::GET,
                        handler_fn(|_: &_, _| Ok::<_, Rejection>("steal")),
                    )
                    .unwrap();
                serve_h2_buffered_requests(
                    cx,
                    server_transport,
                    &mut router,
                    HttpConfig::default(),
                    1,
                )
                .unwrap();
            });

            let client = scope.spawn_local(move |cx| {
                let mut client =
                    h2::RuntimeClientConnection::new(client_transport, HttpConfig::default());
                let request = Request::builder()
                    .method("GET")
                    .uri("/steal")
                    .body(Body::empty())
                    .unwrap();
                let response = client.send(cx, &request).unwrap();
                client.close(cx).unwrap();
                response
            });

            server.join(cx);
            let response = client.join(cx);
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(body_text(response), "steal");
        });
    });
}
