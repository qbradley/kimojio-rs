// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod support;

use std::{num::NonZeroUsize, time::Duration};

use http::{Request, Response, StatusCode};
use kimojio_stack::{Errno, IoRuntime, Runtime};
use kimojio_stack_http::{
    HttpConfig, http1,
    tls::{self, HttpProtocol},
};
use kimojio_stack_steal::{Runtime as StealRuntime, RuntimeConfig as StealRuntimeConfig};

#[test]
fn stackful_http1_client_and_server_exchange_over_tls() {
    let contexts = support::tls_contexts(Some(support::http1_alpn()), false);
    let server_ctx = contexts.server;
    let client_ctx = contexts.client;
    let (client_fd, server_fd) = support::tls_socket_pair();
    let mut runtime = Runtime::new();

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let server = scope.spawn(move |cx| {
                let transport = tls::server_transport(
                    cx,
                    &server_ctx,
                    support::TLS_BUFFER_SIZE,
                    server_fd,
                    Some(HttpProtocol::Http1),
                )
                .unwrap();
                let mut server = http1::ServerConnection::new(transport, HttpConfig::default());
                server
                    .serve_one(cx, |request| {
                        assert_eq!(request.uri(), "/tls");
                        assert_eq!(request.body().as_bytes(), b"ping");
                        Ok(Response::builder()
                            .status(StatusCode::OK)
                            .body(support::body(b"pong"))
                            .unwrap())
                    })
                    .unwrap();
                server.close(cx).unwrap();
            });

            let client = scope.spawn(move |cx| {
                let transport = tls::client_transport(
                    cx,
                    &client_ctx,
                    support::TLS_BUFFER_SIZE,
                    client_fd,
                    "localhost",
                    Some(HttpProtocol::Http1),
                )
                .unwrap();
                let mut client = http1::ClientConnection::new(transport, HttpConfig::default());
                let request = Request::builder()
                    .method("POST")
                    .uri("/tls")
                    .body(support::body(b"ping"))
                    .unwrap();
                let response = client.send(cx, &request).unwrap();
                client.close(cx).unwrap();
                response
            });

            server.join(cx);
            let response = client.join(cx);
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.body().as_bytes(), b"pong");
        });
    });
}

#[test]
fn runtime_agnostic_http1_tls_exchange_runs_on_stealing_runtime() {
    let contexts = support::tls_contexts(Some(support::http1_alpn()), false);
    let server_ctx = contexts.server;
    let client_ctx = contexts.client;
    let (client_fd, server_fd) = support::tls_socket_pair();
    let mut runtime = StealRuntime::with_config(StealRuntimeConfig {
        workers: NonZeroUsize::new(2).unwrap(),
        ..StealRuntimeConfig::default()
    });

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let server = scope.spawn(move |cx| {
                let transport = tls::server_transport_with_runtime(
                    cx,
                    &server_ctx,
                    support::TLS_BUFFER_SIZE,
                    server_fd,
                    Some(HttpProtocol::Http1),
                )
                .unwrap();
                let mut server =
                    http1::RuntimeServerConnection::new(transport, HttpConfig::default());
                server
                    .serve_one(cx, |request| {
                        assert_eq!(request.uri(), "/runtime-agnostic-tls");
                        assert_eq!(request.body().as_bytes(), b"ping");
                        Ok(Response::builder()
                            .status(StatusCode::OK)
                            .body(support::body(b"pong"))
                            .unwrap())
                    })
                    .unwrap();
                server.close(cx).unwrap();
            });

            let client = scope.spawn(move |cx| {
                let transport = tls::client_transport_with_runtime(
                    cx,
                    &client_ctx,
                    support::TLS_BUFFER_SIZE,
                    client_fd,
                    "localhost",
                    Some(HttpProtocol::Http1),
                )
                .unwrap();
                let mut client =
                    http1::RuntimeClientConnection::new(transport, HttpConfig::default());
                let request = Request::builder()
                    .method("POST")
                    .uri("/runtime-agnostic-tls")
                    .body(support::body(b"ping"))
                    .unwrap();
                let response = client.send(cx, &request).unwrap();
                client.close(cx).unwrap();
                response
            });

            server.join(cx);
            let response = client.join(cx);
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.body().as_bytes(), b"pong");
        });
    });
}

#[test]
fn stackful_http_tls_read_timeout_cancels_and_closes_transport() {
    let contexts = support::tls_contexts(Some(support::http1_alpn()), false);
    let server_ctx = contexts.server;
    let client_ctx = contexts.client;
    let (client_fd, server_fd) = support::tls_socket_pair();
    let mut runtime = Runtime::new();

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let server = scope.spawn(move |cx| {
                let transport = tls::server_transport(
                    cx,
                    &server_ctx,
                    support::TLS_BUFFER_SIZE,
                    server_fd,
                    Some(HttpProtocol::Http1),
                )
                .unwrap();
                cx.sleep(Duration::from_millis(25)).unwrap();
                transport.close(cx).unwrap();
            });

            let client = scope.spawn(move |cx| {
                let mut transport = tls::client_transport(
                    cx,
                    &client_ctx,
                    support::TLS_BUFFER_SIZE,
                    client_fd,
                    "localhost",
                    Some(HttpProtocol::Http1),
                )
                .unwrap();
                transport.set_io_timeout(Some(Duration::from_millis(1)));
                let mut byte = [0_u8; 1];
                assert_eq!(
                    transport.read(cx, &mut byte),
                    Err(kimojio_stack_http::Error::Io(Errno::TIME))
                );
                transport.close(cx).unwrap();
            });

            client.join(cx);
            server.join(cx);
        });
    });
}

#[test]
fn runtime_agnostic_http_tls_read_timeout_cancels_and_closes_on_stealing_runtime() {
    let contexts = support::tls_contexts(Some(support::http1_alpn()), false);
    let server_ctx = contexts.server;
    let client_ctx = contexts.client;
    let (client_fd, server_fd) = support::tls_socket_pair();
    let mut runtime = StealRuntime::with_config(StealRuntimeConfig {
        workers: NonZeroUsize::new(2).unwrap(),
        ..StealRuntimeConfig::default()
    });

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let server = scope.spawn(move |cx| {
                let transport = match tls::server_transport_with_runtime(
                    cx,
                    &server_ctx,
                    support::TLS_BUFFER_SIZE,
                    server_fd,
                    Some(HttpProtocol::Http1),
                ) {
                    Ok(transport) => transport,
                    Err(kimojio_stack_http::Error::Tls(Errno::PIPE)) => return,
                    Err(error) => panic!("server TLS transport failed: {error:?}"),
                };
                cx.sleep_for(Duration::from_millis(25)).unwrap();
                transport.close(cx).unwrap();
            });

            let client = scope.spawn(move |cx| {
                let mut transport = tls::client_transport_with_runtime(
                    cx,
                    &client_ctx,
                    support::TLS_BUFFER_SIZE,
                    client_fd,
                    "localhost",
                    Some(HttpProtocol::Http1),
                )
                .unwrap();
                transport.set_io_timeout(Some(Duration::from_millis(1)));
                let mut byte = [0_u8; 1];
                assert_eq!(
                    transport.read(cx, &mut byte),
                    Err(kimojio_stack_http::Error::Io(Errno::TIME))
                );
                transport.close(cx).unwrap();
            });

            client.join(cx);
            server.join(cx);
        });
    });
}
