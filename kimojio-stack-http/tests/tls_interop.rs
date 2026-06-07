// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod support;

use http::{Request, Response, StatusCode};
use kimojio_stack::Runtime;
use kimojio_stack_http::{
    HttpConfig, http1,
    tls::{self, HttpProtocol},
};

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

            server.join(cx).unwrap();
            let response = client.join(cx).unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.body().as_bytes(), b"pong");
        });
    });
}
