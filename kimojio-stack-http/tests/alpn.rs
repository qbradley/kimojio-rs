// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod support;

use kimojio_stack::Runtime;
use kimojio_stack_http::tls::{self, HttpProtocol};

#[test]
fn stack_tls_negotiates_h2_alpn_for_http2() {
    let contexts = support::tls_contexts(Some(support::h2_alpn()), false);
    let (client_fd, server_fd) = support::tls_socket_pair();
    let mut runtime = Runtime::new();

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let server = scope.spawn(move |cx| {
                let stream = contexts
                    .server
                    .server(cx, support::TLS_BUFFER_SIZE, server_fd)
                    .unwrap();
                let selected = tls::selected_protocol(&stream);
                stream.close(cx).unwrap();
                selected
            });

            let client = scope.spawn(move |cx| {
                let stream = contexts
                    .client
                    .client(cx, support::TLS_BUFFER_SIZE, client_fd, "localhost")
                    .unwrap();
                let selected = tls::selected_protocol(&stream);
                stream.close(cx).unwrap();
                selected
            });

            assert_eq!(client.join(cx).unwrap(), Some(HttpProtocol::Http2));
            assert_eq!(server.join(cx).unwrap(), Some(HttpProtocol::Http2));
        });
    });
}
