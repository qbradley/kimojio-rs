// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod support;

use std::{net::Ipv4Addr, sync::mpsc, thread};

use bytes::{Bytes, BytesMut};
use http::{Request, Response, StatusCode};
use kimojio_stack::Runtime;
use kimojio_stack_http::{
    HttpConfig, StackTransport,
    h2::{
        self, ClientConnection, ConnectionState, Frame, FrameFlags, FramePayload, FrameType,
        ServerConnection, Settings, StreamId,
    },
};
use tokio::net::{TcpListener, TcpStream};

#[test]
fn stackful_h2_client_talks_to_tokio_h2_server_with_settings_variations() {
    let (addr_tx, addr_rx) = mpsc::channel();
    let server = support::spawn_tokio(async move {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        addr_tx.send(listener.local_addr().unwrap()).unwrap();
        let (mut stream, _) = listener.accept().await.unwrap();
        let settings = Settings {
            initial_window_size: 4096,
            max_frame_size: 32 * 1024,
            max_header_list_size: 8192,
            ..Settings::default()
        };
        let mut state = ConnectionState::new(settings).unwrap();
        support::h2_server_handshake(&mut stream, &mut state).await;

        let (stream_id, headers, body) = read_tokio_request(&mut stream, &mut state).await;
        assert!(
            headers
                .iter()
                .any(|header| header.name.as_ref() == b":method")
        );
        assert_eq!(body, b"ping");

        let block = state.encode_header_block(&[
            h2::Header::new(":status", "200"),
            h2::Header::new("content-type", "text/plain"),
        ]);
        support::write_h2_frame(
            &mut stream,
            &Frame::headers(stream_id, Bytes::from(block), false),
        )
        .await;
        support::write_h2_frame(
            &mut stream,
            &Frame::data(stream_id, Bytes::from_static(b"pong"), true),
        )
        .await;
    });

    let addr = addr_rx.recv().unwrap();
    let response = support::run_stackful(|cx| {
        let transport = support::stack_connect(cx, addr);
        let mut client = ClientConnection::new(transport, HttpConfig::default());
        let request = Request::builder()
            .method("POST")
            .uri("/h2")
            .body(support::body(b"ping"))
            .unwrap();
        let response = client.send(cx, &request).unwrap();
        client.close(cx).unwrap();
        response
    });

    server.join().unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.body().as_bytes(), b"pong");
}

#[test]
fn tokio_h2_client_talks_to_stackful_h2_server() {
    let (addr_tx, addr_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        Runtime::new().block_on(|cx| {
            let (listener, addr) = support::stack_listener(cx);
            addr_tx.send(addr).unwrap();
            let connection = cx.accept(&listener).unwrap();
            let transport = StackTransport::plaintext(connection);
            let mut server = ServerConnection::new(transport, HttpConfig::default());
            let incoming = server.accept(cx).unwrap().unwrap();
            assert_eq!(incoming.request.uri(), "/tokio");
            assert_eq!(incoming.request.body().as_bytes(), b"ping");
            let response = Response::builder()
                .status(StatusCode::ACCEPTED)
                .body(support::body(b"pong"))
                .unwrap();
            server
                .send_response(cx, incoming.stream_id, &response)
                .unwrap();
            server.close(cx).unwrap();
            cx.close(listener).unwrap();
        });
    });

    let addr = addr_rx.recv().unwrap();
    let (status, body) = support::spawn_tokio(async move {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let mut state = ConnectionState::new(Settings::default()).unwrap();
        support::h2_client_handshake(&mut stream, &mut state).await;
        let stream_id = StreamId::new(1).unwrap();
        state.open_stream(stream_id).unwrap();
        let block = support::encode_headers(
            &mut state,
            &[
                (":method", "POST"),
                (":scheme", "http"),
                (":path", "/tokio"),
            ],
        );
        support::write_h2_frame(
            &mut stream,
            &Frame::headers(stream_id, Bytes::from(block), false),
        )
        .await;
        support::write_h2_frame(
            &mut stream,
            &Frame::data(stream_id, Bytes::from_static(b"ping"), true),
        )
        .await;
        read_tokio_response(&mut stream, &mut state, stream_id).await
    })
    .join()
    .unwrap();

    server.join().unwrap();
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body, b"pong");
}

async fn read_tokio_request(
    stream: &mut TcpStream,
    state: &mut ConnectionState,
) -> (StreamId, Vec<h2::Header>, Vec<u8>) {
    let mut headers = None;
    let mut body = Vec::new();
    loop {
        let frame = support::read_h2_frame(stream, state.local_settings().max_frame_size).await;
        match frame.frame_type {
            FrameType::Settings => {
                state.receive_settings(&frame).unwrap();
                while let Some(frame) = state.pop_outbound() {
                    support::write_h2_frame(stream, &frame).await;
                }
            }
            FrameType::Headers => {
                let id = StreamId::new(frame.stream_id).unwrap();
                let FramePayload::Headers(block) = frame.payload else {
                    unreachable!();
                };
                headers = Some(support::decode_headers(state, &block));
                if frame.flags.contains(FrameFlags::END_STREAM) {
                    return (id, headers.unwrap(), body);
                }
            }
            FrameType::Data => {
                let id = StreamId::new(frame.stream_id).unwrap();
                let FramePayload::Data(data) = frame.payload else {
                    unreachable!();
                };
                body.extend_from_slice(&data);
                if frame.flags.contains(FrameFlags::END_STREAM) {
                    return (id, headers.unwrap(), body);
                }
            }
            _ => {}
        }
    }
}

async fn read_tokio_response(
    stream: &mut TcpStream,
    state: &mut ConnectionState,
    expected_stream_id: StreamId,
) -> (StatusCode, Vec<u8>) {
    let mut status = None;
    let mut body = BytesMut::new();
    loop {
        let frame = support::read_h2_frame(stream, state.local_settings().max_frame_size).await;
        match frame.frame_type {
            FrameType::Settings => {
                state.receive_settings(&frame).unwrap();
                while let Some(frame) = state.pop_outbound() {
                    support::write_h2_frame(stream, &frame).await;
                }
            }
            FrameType::Headers => {
                assert_eq!(frame.stream_id, expected_stream_id.get());
                let FramePayload::Headers(block) = frame.payload else {
                    unreachable!();
                };
                for header in support::decode_headers(state, &block) {
                    if header.name.as_ref() == b":status" {
                        status = Some(
                            std::str::from_utf8(&header.value)
                                .unwrap()
                                .parse::<u16>()
                                .unwrap(),
                        );
                    }
                }
                if frame.flags.contains(FrameFlags::END_STREAM) {
                    return (
                        StatusCode::from_u16(status.unwrap()).unwrap(),
                        body.to_vec(),
                    );
                }
            }
            FrameType::Data => {
                assert_eq!(frame.stream_id, expected_stream_id.get());
                let FramePayload::Data(data) = frame.payload else {
                    unreachable!();
                };
                body.extend_from_slice(&data);
                if frame.flags.contains(FrameFlags::END_STREAM) {
                    return (
                        StatusCode::from_u16(status.unwrap()).unwrap(),
                        body.to_vec(),
                    );
                }
            }
            _ => {}
        }
    }
}
