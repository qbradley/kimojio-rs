// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod support;

use kimojio_stack::Runtime;
use kimojio_stack_http::{ErrorKind, StackTransport, tls};

#[test]
fn tls_handshake_failure_maps_to_tls_error_kind() {
    let contexts = support::tls_contexts(None, false);
    let server_ctx = contexts.server;
    let (client_fd, server_fd) = support::tls_socket_pair();
    let mut runtime = Runtime::new();

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let server = scope.spawn(move |cx| {
                match tls::server_transport(
                    cx,
                    &server_ctx,
                    support::TLS_BUFFER_SIZE,
                    server_fd,
                    None,
                ) {
                    Ok(_) => panic!("plaintext peer unexpectedly completed TLS handshake"),
                    Err(error) => error.kind(),
                }
            });

            let client = scope.spawn(move |cx| {
                let mut transport = StackTransport::plaintext(client_fd);
                transport.write_all(cx, b"not tls").unwrap();
                transport.close(cx).unwrap();
            });

            client.join(cx).unwrap();
            assert_eq!(server.join(cx).unwrap(), ErrorKind::Tls);
        });
    });
}

#[test]
fn tls_certificate_verification_failure_maps_to_tls_error_kind() {
    let contexts = support::tls_contexts(None, true);
    let server_ctx = contexts.server;
    let client_ctx = contexts.client;
    let (client_fd, server_fd) = support::tls_socket_pair();
    let mut runtime = Runtime::new();

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let server = scope.spawn(move |cx| {
                let result = tls::server_transport(
                    cx,
                    &server_ctx,
                    support::TLS_BUFFER_SIZE,
                    server_fd,
                    None,
                );
                result.err().map(|error| error.kind())
            });

            let client = scope.spawn(move |cx| {
                match tls::client_transport(
                    cx,
                    &client_ctx,
                    support::TLS_BUFFER_SIZE,
                    client_fd,
                    None,
                ) {
                    Ok(_) => panic!("untrusted self-signed certificate was accepted"),
                    Err(error) => error.kind(),
                }
            });

            assert_eq!(client.join(cx).unwrap(), ErrorKind::Tls);
            let _ = server.join(cx).unwrap();
        });
    });
}

#[test]
fn tls_close_notify_maps_to_eof_read() {
    let contexts = support::tls_contexts(None, false);
    let server_ctx = contexts.server;
    let client_ctx = contexts.client;
    let (client_fd, server_fd) = support::tls_socket_pair();
    let mut runtime = Runtime::new();

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let server = scope.spawn(move |cx| {
                let mut stream = server_ctx
                    .server(cx, support::TLS_BUFFER_SIZE, server_fd)
                    .unwrap();
                stream.shutdown(cx).unwrap();
                stream.close(cx).unwrap();
            });

            let client = scope.spawn(move |cx| {
                let mut stream = client_ctx
                    .client(cx, support::TLS_BUFFER_SIZE, client_fd)
                    .unwrap();
                let mut byte = [0_u8; 1];
                let amount = stream.read_exact_or_eof(cx, &mut byte).unwrap();
                stream.close(cx).unwrap();
                amount
            });

            server.join(cx).unwrap();
            assert_eq!(client.join(cx).unwrap(), 0);
        });
    });
}
