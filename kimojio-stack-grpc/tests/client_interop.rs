// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod interop_proto;

use std::{net::Ipv4Addr, sync::mpsc};

use http::HeaderName;
use interop_proto::{TestRequest, TonicBehavior, TonicTestServer};
use kimojio_stack::Runtime;
use kimojio_stack_grpc::{Metadata, StatusCode, UnaryClient, client::ClientConfig, error::Error};
use kimojio_stack_http::{HttpConfig, h2};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tonic::transport::Server;

#[test]
fn stackful_grpc_client_calls_tonic_server_success_with_metadata() {
    let (addr_tx, addr_rx) = mpsc::channel();
    let (server, shutdown) = spawn_tonic_server(TonicBehavior::Success, addr_tx);
    let addr = addr_rx.recv().unwrap();

    let response = Runtime::new().block_on(|cx| {
        let transport = stack_connect(cx, addr);
        let http = h2::ClientConnection::new(transport, HttpConfig::default());
        let mut client = UnaryClient::new(http, ClientConfig::default());
        let mut metadata = Metadata::new();
        metadata
            .insert(
                HeaderName::from_static("x-request"),
                http::HeaderValue::from_static("stackful"),
            )
            .unwrap();
        metadata
            .insert_bin(HeaderName::from_static("trace-bin"), b"trace")
            .unwrap();
        let response = client
            .call::<_, interop_proto::TestResponse>(
                cx,
                interop_proto::UNARY_PATH,
                metadata,
                &TestRequest {
                    value: "client".to_owned(),
                },
            )
            .unwrap();
        client.close(cx).unwrap();
        response
    });

    shutdown.send(()).unwrap();
    server.join().unwrap();
    assert_eq!(response.message.value, "tonic client");
    assert_eq!(response.status.code(), StatusCode::Ok);
    assert_eq!(
        response.metadata.get(&HeaderName::from_static("x-tonic")),
        Some(&http::HeaderValue::from_static("yes"))
    );
    assert_eq!(
        response
            .metadata
            .get_bin(&HeaderName::from_static("trace-bin"))
            .unwrap()
            .as_deref(),
        Some(b"tonic-response".as_slice())
    );
}

#[test]
fn stackful_grpc_client_receives_tonic_error_status_and_metadata() {
    let (addr_tx, addr_rx) = mpsc::channel();
    let (server, shutdown) = spawn_tonic_server(TonicBehavior::Error, addr_tx);
    let addr = addr_rx.recv().unwrap();

    let error = Runtime::new().block_on(|cx| {
        let transport = stack_connect(cx, addr);
        let http = h2::ClientConnection::new(transport, HttpConfig::default());
        let mut client = UnaryClient::new(http, ClientConfig::default());
        let mut metadata = Metadata::new();
        metadata
            .insert(
                HeaderName::from_static("x-request"),
                http::HeaderValue::from_static("stackful"),
            )
            .unwrap();
        metadata
            .insert_bin(HeaderName::from_static("trace-bin"), b"trace")
            .unwrap();
        let error = client
            .call::<_, interop_proto::TestResponse>(
                cx,
                interop_proto::UNARY_PATH,
                metadata,
                &TestRequest {
                    value: "client".to_owned(),
                },
            )
            .unwrap_err();
        client.close(cx).unwrap();
        error
    });

    shutdown.send(()).unwrap();
    server.join().unwrap();
    match error {
        Error::Status(status) => {
            assert_eq!(status.code(), StatusCode::Unavailable);
            assert_eq!(status.message(), "tonic down: 50% ☃");
            assert_eq!(status.details(), b"opaque");
            assert_eq!(
                status.metadata().get(&HeaderName::from_static("x-error")),
                Some(&http::HeaderValue::from_static("tonic"))
            );
            assert_eq!(
                status
                    .metadata()
                    .get_bin(&HeaderName::from_static("error-bin"))
                    .unwrap()
                    .as_deref(),
                Some(b"details".as_slice())
            );
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

fn spawn_tonic_server(
    behavior: TonicBehavior,
    addr_tx: mpsc::Sender<std::net::SocketAddr>,
) -> (std::thread::JoinHandle<()>, oneshot::Sender<()>) {
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let handle = std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
                addr_tx.send(listener.local_addr().unwrap()).unwrap();
                Server::builder()
                    .add_service(TonicTestServer::new(behavior))
                    .serve_with_incoming_shutdown(
                        tonic::codegen::tokio_stream::wrappers::TcpListenerStream::new(listener),
                        async {
                            let _ = shutdown_rx.await;
                        },
                    )
                    .await
                    .unwrap();
            });
    });
    (handle, shutdown_tx)
}

fn stack_connect(
    cx: &kimojio_stack::RuntimeContext<'_>,
    addr: std::net::SocketAddr,
) -> kimojio_stack_http::StackTransport {
    let socket = cx
        .socket(
            rustix::net::AddressFamily::INET,
            rustix::net::SocketType::STREAM,
            Some(rustix::net::ipproto::TCP),
        )
        .unwrap();
    cx.connect(&socket, &addr).unwrap();
    kimojio_stack_http::StackTransport::plaintext(socket)
}
