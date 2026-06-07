// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use http::{HeaderName, HeaderValue, Response, StatusCode, header::CONTENT_TYPE};
use kimojio_stack::Runtime;
use kimojio_stack_grpc::{
    ErrorKind, Metadata, ServerConfig, Status, StatusCode as GrpcStatusCode, UnaryClient,
    UnaryReply, UnaryServer, client::ClientConfig, codec,
};
use kimojio_stack_http::{Body, BodyLimits, HttpConfig, StackTransport, h2};
use prost::Message;
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

#[derive(Clone, PartialEq, Message)]
struct TestMessage {
    #[prost(string, tag = "1")]
    value: String,
}

#[test]
fn protocol_limits_rejects_unsupported_compression_flag() {
    let error = codec::decode_message::<TestMessage>(&[1, 0, 0, 0, 0], 1024).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::UnsupportedCompression);
}

#[test]
fn protocol_limits_rejects_malformed_message_length() {
    let error = codec::decode_message::<TestMessage>(&[0, 0, 0, 0, 4, 1, 2], 1024).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Protocol);
}

#[test]
fn protocol_limits_rejects_oversized_messages() {
    let message = TestMessage {
        value: "too large".to_owned(),
    };
    let encoded = codec::encode_message(&message, 1024).unwrap();

    let encode_error = codec::encode_message(&message, 1).unwrap_err();
    let decode_error = codec::decode_message::<TestMessage>(&encoded, 1).unwrap_err();

    assert_eq!(encode_error.kind(), ErrorKind::SizeLimit);
    assert_eq!(decode_error.kind(), ErrorKind::SizeLimit);
}

#[test]
fn protocol_limits_client_requires_status_trailers() {
    let (client_transport, server_transport) = socket_transport_pair();
    let mut runtime = Runtime::new();

    let error = runtime.block_on(|cx| {
        cx.scope(|scope| {
            let server = scope.spawn(move |cx| {
                let mut http = h2::ServerConnection::new(server_transport, HttpConfig::default());
                let incoming = http.accept(cx).unwrap().unwrap();
                let response = Response::builder()
                    .status(StatusCode::OK)
                    .header(CONTENT_TYPE, "application/grpc")
                    .body(
                        Body::from_bytes(
                            codec::encode_message(
                                &TestMessage {
                                    value: "response".to_owned(),
                                },
                                1024,
                            )
                            .unwrap(),
                            BodyLimits::new(1024),
                        )
                        .unwrap(),
                    )
                    .unwrap();
                http.send_response(cx, incoming.stream_id, &response)
                    .unwrap();
                http.shutdown_write_and_close_after_peer(cx).unwrap();
            });

            let http = h2::ClientConnection::new(client_transport, HttpConfig::default());
            let mut client = UnaryClient::new(http, ClientConfig::default());
            let error = client
                .call::<_, TestMessage>(
                    cx,
                    "/test.Protocol/Unary",
                    Metadata::new(),
                    &TestMessage {
                        value: "request".to_owned(),
                    },
                )
                .unwrap_err();
            client.close(cx).unwrap();
            server.join(cx).unwrap();
            error
        })
    });

    assert_eq!(error.kind(), ErrorKind::Protocol);
}

#[test]
fn protocol_limits_server_rejects_non_post_before_handler() {
    let (client_transport, server_transport) = socket_transport_pair();
    let mut runtime = Runtime::new();

    let status = runtime.block_on(|cx| {
        cx.scope(|scope| {
            let server = scope.spawn(move |cx| {
                let mut http = h2::ServerConnection::new(server_transport, HttpConfig::default());
                let mut grpc = UnaryServer::new(ServerConfig::default());
                grpc.add_unary::<TestMessage, TestMessage, _>(
                    "/test.Protocol/Unary",
                    |_cx, _metadata, _request| -> Result<UnaryReply<TestMessage>, Status> {
                        panic!("non-POST request reached handler");
                    },
                );
                grpc.serve_one(cx, &mut http).unwrap();
                http.shutdown_write_and_close_after_peer(cx).unwrap();
            });

            let mut client = h2::ClientConnection::new(client_transport, HttpConfig::default());
            let request = http::Request::builder()
                .method("GET")
                .uri("/test.Protocol/Unary")
                .header(CONTENT_TYPE, "application/grpc")
                .body(
                    Body::from_bytes(
                        codec::encode_message(
                            &TestMessage {
                                value: "request".to_owned(),
                            },
                            1024,
                        )
                        .unwrap(),
                        BodyLimits::new(1024),
                    )
                    .unwrap(),
                )
                .unwrap();
            let stream_id = client.send_request(cx, &request).unwrap();
            let response = client.read_response_with_trailers(cx, stream_id).unwrap();
            client.close(cx).unwrap();
            server.join(cx).unwrap();
            Status::from_trailers(&response.trailers).unwrap()
        })
    });

    assert_eq!(status.code(), GrpcStatusCode::InvalidArgument);
}

#[test]
fn protocol_limits_server_rejects_unsupported_grpc_content_type() {
    let (client_transport, server_transport) = socket_transport_pair();
    let mut runtime = Runtime::new();

    let status = runtime.block_on(|cx| {
        cx.scope(|scope| {
            let server = scope.spawn(move |cx| {
                let mut http = h2::ServerConnection::new(server_transport, HttpConfig::default());
                let mut grpc = UnaryServer::new(ServerConfig::default());
                grpc.add_unary::<TestMessage, TestMessage, _>(
                    "/test.Protocol/Unary",
                    |_cx, _metadata, _request| -> Result<UnaryReply<TestMessage>, Status> {
                        panic!("invalid content-type request reached handler");
                    },
                );
                grpc.serve_one(cx, &mut http).unwrap();
                http.shutdown_write_and_close_after_peer(cx).unwrap();
            });

            let mut client = h2::ClientConnection::new(client_transport, HttpConfig::default());
            let request = http::Request::builder()
                .method("POST")
                .uri("/test.Protocol/Unary")
                .header(CONTENT_TYPE, "application/grpc-web")
                .body(Body::empty())
                .unwrap();
            let stream_id = client.send_request(cx, &request).unwrap();
            let response = client.read_response_with_trailers(cx, stream_id).unwrap();
            client.close(cx).unwrap();
            server.join(cx).unwrap();
            Status::from_trailers(&response.trailers).unwrap()
        })
    });

    assert_eq!(status.code(), GrpcStatusCode::InvalidArgument);
}

#[test]
fn protocol_limits_rejects_invalid_metadata_names() {
    let mut metadata = Metadata::new();
    let content_type = metadata
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/grpc"))
        .unwrap_err();
    let binary_name = metadata
        .insert(
            HeaderName::from_static("trace-bin"),
            HeaderValue::from_static("not-binary-api"),
        )
        .unwrap_err();
    let ascii_name = metadata
        .insert_bin(HeaderName::from_static("trace"), b"abc")
        .unwrap_err();

    assert_eq!(content_type.kind(), ErrorKind::Protocol);
    assert_eq!(binary_name.kind(), ErrorKind::Protocol);
    assert_eq!(ascii_name.kind(), ErrorKind::Protocol);
}

#[test]
fn protocol_limits_status_rejects_invalid_binary_details() {
    let mut trailers = kimojio_stack_http::Trailers::new();
    trailers.insert(
        kimojio_stack_grpc::status::GRPC_STATUS,
        HeaderValue::from_static("13"),
    );
    trailers.insert(
        kimojio_stack_grpc::status::GRPC_STATUS_DETAILS_BIN,
        HeaderValue::from_static("%%%"),
    );

    let error = Status::from_trailers(&trailers).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Protocol);
}

fn socket_transport_pair() -> (StackTransport, StackTransport) {
    let (client, server) = socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::empty(),
        None,
    )
    .unwrap();
    (
        StackTransport::plaintext(client),
        StackTransport::plaintext(server),
    )
}
