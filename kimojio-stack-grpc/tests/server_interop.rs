// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod interop_proto;

use std::{net::SocketAddr, sync::mpsc, thread};

use http::HeaderName;
use interop_proto::{TestRequest, TestResponse};
use kimojio_stack::Runtime;
use kimojio_stack_grpc::{Status, StatusCode, UnaryReply, UnaryServer, server::ServerConfig};
use kimojio_stack_http::{HttpConfig, StackTransport, h2};
use tonic::{Code, Request, metadata::MetadataValue, transport::Endpoint};

#[test]
fn tonic_client_calls_stackful_grpc_server_success_with_metadata() {
    let (addr_tx, addr_rx) = mpsc::channel();
    let server = spawn_stackful_server(StackfulBehavior::Success, addr_tx);
    let addr = addr_rx.recv().unwrap();

    let response = run_tonic_client(addr, "client").unwrap();

    server.join().unwrap();
    assert_eq!(response.get_ref().value, "stackful client");
    assert_eq!(
        response.metadata().get("x-stackful").unwrap(),
        MetadataValue::from_static("yes")
    );
    assert_eq!(
        response
            .metadata()
            .get_bin("trace-bin")
            .unwrap()
            .to_bytes()
            .unwrap()
            .as_ref(),
        b"stackful-response"
    );
    assert_eq!(
        response.metadata().get("x-trailer").unwrap(),
        MetadataValue::from_static("done")
    );
}

#[test]
fn tonic_client_receives_stackful_error_status_and_trailers() {
    let (addr_tx, addr_rx) = mpsc::channel();
    let server = spawn_stackful_server(StackfulBehavior::Error, addr_tx);
    let addr = addr_rx.recv().unwrap();

    let status = run_tonic_client(addr, "client").unwrap_err();

    server.join().unwrap();
    assert_eq!(status.code(), Code::Unavailable);
    assert_eq!(status.message(), "stackful down: 50% ☃");
    assert_eq!(
        status.metadata().get("x-error").unwrap(),
        MetadataValue::from_static("stackful")
    );
    assert_eq!(
        status
            .metadata()
            .get_bin("error-bin")
            .unwrap()
            .to_bytes()
            .unwrap()
            .as_ref(),
        b"details"
    );
}

#[derive(Clone, Copy)]
enum StackfulBehavior {
    Success,
    Error,
}

fn spawn_stackful_server(
    behavior: StackfulBehavior,
    addr_tx: mpsc::Sender<SocketAddr>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        Runtime::new().block_on(|cx| {
            let listener = cx
                .socket(
                    rustix::net::AddressFamily::INET,
                    rustix::net::SocketType::STREAM,
                    Some(rustix::net::ipproto::TCP),
                )
                .unwrap();
            let loopback = std::net::SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, 0);
            cx.bind(&listener, &loopback).unwrap();
            cx.listen(&listener, 8).unwrap();
            let addr: SocketAddr = rustix::net::getsockname(&listener)
                .unwrap()
                .try_into()
                .unwrap();
            addr_tx.send(addr).unwrap();

            let connection = cx.accept(&listener).unwrap();
            let transport = StackTransport::plaintext(connection);
            let mut http = h2::ServerConnection::new(transport, HttpConfig::default());
            let mut grpc = UnaryServer::new(ServerConfig::default());
            grpc.add_unary::<TestRequest, TestResponse, _>(
                interop_proto::UNARY_PATH,
                move |_cx, metadata, request| {
                    assert_eq!(
                        metadata.get(&HeaderName::from_static("x-request")),
                        Some(&http::HeaderValue::from_static("tonic"))
                    );
                    assert_eq!(
                        metadata
                            .get_bin(&HeaderName::from_static("trace-bin"))
                            .unwrap()
                            .as_deref(),
                        Some(b"trace".as_slice())
                    );

                    match behavior {
                        StackfulBehavior::Success => {
                            let mut reply = UnaryReply::new(TestResponse {
                                value: format!("stackful {}", request.value),
                            });
                            reply
                                .metadata
                                .insert(
                                    HeaderName::from_static("x-stackful"),
                                    http::HeaderValue::from_static("yes"),
                                )
                                .unwrap();
                            reply
                                .metadata
                                .insert_bin(
                                    HeaderName::from_static("trace-bin"),
                                    b"stackful-response",
                                )
                                .unwrap();
                            reply
                                .trailers
                                .insert(
                                    HeaderName::from_static("x-trailer"),
                                    http::HeaderValue::from_static("done"),
                                )
                                .unwrap();
                            Ok(reply)
                        }
                        StackfulBehavior::Error => {
                            let mut status =
                                Status::new(StatusCode::Unavailable, "stackful down: 50% ☃");
                            status
                                .metadata_mut()
                                .insert(
                                    HeaderName::from_static("x-error"),
                                    http::HeaderValue::from_static("stackful"),
                                )
                                .unwrap();
                            status
                                .metadata_mut()
                                .insert_bin(HeaderName::from_static("error-bin"), b"details")
                                .unwrap();
                            Err(status)
                        }
                    }
                },
            );
            grpc.serve_one(cx, &mut http).unwrap();
            http.goaway(cx, 1, 0).unwrap();
            http.shutdown_write_and_close_after_peer(cx).unwrap();
            cx.close(listener).unwrap();
        });
    })
}

fn run_tonic_client(
    addr: SocketAddr,
    value: &str,
) -> Result<tonic::Response<TestResponse>, tonic::Status> {
    std::thread::spawn({
        let value = value.to_owned();
        move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    let channel = Endpoint::from_shared(format!("http://{addr}"))
                        .unwrap()
                        .connect()
                        .await
                        .unwrap();
                    let mut request = Request::new(TestRequest { value });
                    request
                        .metadata_mut()
                        .insert("x-request", MetadataValue::from_static("tonic"));
                    request
                        .metadata_mut()
                        .insert_bin("trace-bin", MetadataValue::from_bytes(b"trace"));
                    interop_proto::tonic_unary_call(channel, request).await
                })
        }
    })
    .join()
    .unwrap()
}
