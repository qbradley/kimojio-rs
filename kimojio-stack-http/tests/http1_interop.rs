// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod support;

use std::{net::Ipv4Addr, sync::mpsc, thread};

use http::{Request, Response, StatusCode};
use kimojio_stack::Runtime;
use kimojio_stack_http::{HttpConfig, StackTransport, http1};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

#[test]
fn stackful_http1_client_talks_to_tokio_server_with_mixed_case_headers() {
    let (addr_tx, addr_rx) = mpsc::channel();
    let server = support::spawn_tokio(async move {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        addr_tx.send(listener.local_addr().unwrap()).unwrap();
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_http_message(&mut stream).await;
        assert!(request.starts_with(b"POST /interop HTTP/1.1\r\n"));
        assert!(request.windows(4).any(|window| window == b"ping"));
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nCoNtEnT-LeNgTh: 4\r\nX-ToKiO: yes\r\n\r\npong")
            .await
            .unwrap();
    });

    let addr = addr_rx.recv().unwrap();
    let response = support::run_stackful(|cx| {
        let transport = support::stack_connect(cx, addr);
        let mut client = http1::ClientConnection::new(transport, HttpConfig::default());
        let request = Request::builder()
            .method("POST")
            .uri("/interop")
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
fn tokio_http1_client_talks_to_stackful_server_with_mixed_case_headers() {
    let (addr_tx, addr_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        Runtime::new().block_on(|cx| {
            let (listener, addr) = support::stack_listener(cx);
            addr_tx.send(addr).unwrap();
            let connection = cx.accept(&listener).unwrap();
            let transport = StackTransport::plaintext(connection);
            let mut server = http1::ServerConnection::new(transport, HttpConfig::default());
            server
                .serve_one(cx, |request| {
                    assert_eq!(request.uri(), "/stackful");
                    assert_eq!(request.body().as_bytes(), b"ping");
                    Ok(Response::builder()
                        .status(StatusCode::CREATED)
                        .header("x-stackful", "yes")
                        .body(support::body(b"pong"))
                        .unwrap())
                })
                .unwrap();
            server.close(cx).unwrap();
            cx.close(listener).unwrap();
        });
    });

    let addr = addr_rx.recv().unwrap();
    let response = support::spawn_tokio(async move {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(
                b"POST /stackful HTTP/1.1\r\nhOsT: localhost\r\nContent-Length: 4\r\n\r\nping",
            )
            .await
            .unwrap();
        read_http_message(&mut stream).await
    })
    .join()
    .unwrap();

    server.join().unwrap();
    assert!(response.starts_with(b"HTTP/1.1 201 Created\r\n"));
    assert!(response.windows(4).any(|window| window == b"pong"));
}

async fn read_http_message(stream: &mut TcpStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut buf = [0_u8; 1024];
    loop {
        let read = stream.read(&mut buf).await.unwrap();
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..read]);
        if let Some(body_start) = find_header_end(&bytes) {
            let content_length = content_length(&bytes[..body_start]);
            if bytes.len() >= body_start + content_length {
                break;
            }
        }
    }
    bytes
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|offset| offset + 4)
}

fn content_length(head: &[u8]) -> usize {
    std::str::from_utf8(head)
        .unwrap()
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .unwrap_or(0)
}
