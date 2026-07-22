// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//! Public-boundary interoperability tests with Tokio ecosystem HTTP peers.
//!
//! Each peer runs on a dedicated thread so Kimojio's thread-local executor is
//! never mixed with Tokio's `Send` futures.

#![cfg(feature = "http")]

use std::cell::{Cell, RefCell};
use std::convert::Infallible;
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener as StdTcpListener, TcpStream};
use std::process::{Command, Output, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::service::service_fn;
use hyper::{Request as HyperRequest, Response as HyperResponse};
use hyper_util::rt::{TokioExecutor, TokioIo};
use kimojio::http::{
    Body, Client, ClientConfig, Error as HttpError, ExpectContinueDecision, HeaderValue, Limits,
    Method, ProtocolErrorKind, Response, ServeError, Server, ServerConfig, StatusCode, Version,
};
use kimojio::{CancellationToken, operations};
use kimojio_fsm_http::{H2ByteStreamEvent, H2Client, H2HeaderField, H2Server};
use tokio::net::TcpListener;
use tokio::sync::{Notify, oneshot};

const WAIT: Duration = Duration::from_secs(10);
/// Sentinel meaning no `RST_STREAM` was observed.
const NO_RESET: u32 = u32::MAX;
/// RFC 9113 `PROTOCOL_ERROR` code.
const H2_PROTOCOL_ERROR: u32 = 0x1;
const LARGE_BODY_LEN: usize = 100_000;

fn read_http1_head(stream: &mut TcpStream) -> (Vec<u8>, Vec<u8>) {
    let mut bytes = Vec::new();
    loop {
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let remainder = bytes.split_off(end + 4);
            return (bytes, remainder);
        }
        let mut chunk = [0; 4096];
        let read = stream.read(&mut chunk).unwrap();
        assert_ne!(read, 0, "peer closed before completing an HTTP/1 head");
        bytes.extend_from_slice(&chunk[..read]);
    }
}

fn spawn_curl_expect_http1(address: SocketAddr, body: Vec<u8>) -> thread::JoinHandle<Output> {
    thread::spawn(move || {
        let mut child = Command::new("curl")
            .args([
                "--http1.1",
                "--silent",
                "--show-error",
                "--max-time",
                "10",
                "--expect100-timeout",
                "5",
                "--header",
                "Expect: 100-continue",
                "--data-binary",
                "@-",
                "--dump-header",
                "-",
            ])
            .arg(format!("http://{address}/expect"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("curl is required for HTTP interoperability tests");
        child
            .stdin
            .take()
            .expect("curl stdin was piped")
            .write_all(&body)
            .unwrap();
        child.wait_with_output().unwrap()
    })
}

struct ExpectProxy {
    address: SocketAddr,
    thread: thread::JoinHandle<Vec<u8>>,
}

fn spawn_expect_proxy(upstream: SocketAddr) -> ExpectProxy {
    let listener = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let thread = thread::spawn(move || {
        let (mut client, _) = listener.accept().unwrap();
        client.set_read_timeout(Some(WAIT)).unwrap();
        client.set_write_timeout(Some(WAIT)).unwrap();
        let mut server = TcpStream::connect(upstream).unwrap();
        server.set_read_timeout(Some(WAIT)).unwrap();
        server.set_write_timeout(Some(WAIT)).unwrap();

        let (head, initial_body) = read_http1_head(&mut client);
        assert!(
            head.windows(b"expect: 100-continue\r\n".len())
                .any(|window| window.eq_ignore_ascii_case(b"expect: 100-continue\r\n"))
        );
        server.write_all(&head).unwrap();
        server.write_all(&initial_body).unwrap();

        let mut response = Vec::new();
        server.read_to_end(&mut response).unwrap();
        client.write_all(&response).unwrap();

        let mut transmitted_body = initial_body;
        client.read_to_end(&mut transmitted_body).unwrap();
        transmitted_body
    });
    ExpectProxy { address, thread }
}

struct NoContinuePeer {
    address: SocketAddr,
    thread: thread::JoinHandle<Vec<u8>>,
}

fn spawn_no_continue_peer(expected_body_len: usize) -> NoContinuePeer {
    let listener = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let thread = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        stream.set_write_timeout(Some(WAIT)).unwrap();
        let (head, mut body) = read_http1_head(&mut stream);
        assert!(
            head.windows(b"expect: 100-continue\r\n".len())
                .any(|window| window.eq_ignore_ascii_case(b"expect: 100-continue\r\n"))
        );
        assert!(
            body.is_empty(),
            "the request body arrived before the continue wait expired"
        );
        while body.len() < expected_body_len {
            let mut chunk = [0; 4096];
            let remaining = expected_body_len - body.len();
            let capacity = remaining.min(chunk.len());
            let read = stream.read(&mut chunk[..capacity]).unwrap();
            assert_ne!(read, 0, "client closed before sending the timed-out body");
            body.extend_from_slice(&chunk[..read]);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 8\r\nconnection: close\r\n\r\naccepted")
            .unwrap();
        body
    });
    NoContinuePeer { address, thread }
}

struct FinalBeforeBodyPeer {
    address: SocketAddr,
    /// Reports whether the follow-up request arrived on the retired connection.
    thread: thread::JoinHandle<bool>,
}

fn spawn_final_before_body_peer() -> FinalBeforeBodyPeer {
    let listener = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let thread = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        stream.set_write_timeout(Some(WAIT)).unwrap();

        let (first, first_remainder) = read_http1_head(&mut stream);
        assert!(first.starts_with(b"POST /reject-and-reuse HTTP/1.1\r\n"));
        assert!(
            first_remainder.is_empty(),
            "the rejected request body was transmitted"
        );
        stream
            .write_all(
                b"HTTP/1.1 417 Expectation Failed\r\n\
                  content-length: 0\r\n\r\n",
            )
            .unwrap();

        // The declared body never arrived, so this connection's framing state
        // is ambiguous and the client must retire it despite the keep-alive
        // signal. Keep it open and perfectly reusable so that retirement is the
        // only thing that can send the follow-up request elsewhere - a peer
        // that hangs up here would force a second connection on its own and
        // prove nothing.
        stream.set_nonblocking(true).unwrap();
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + WAIT;
        let (mut serving, reused_retired_connection) = loop {
            match listener.accept() {
                Ok((fresh, _)) => break (fresh, false),
                Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                Err(error) => panic!("accepting the follow-up connection failed: {error}"),
            }
            let mut probe = [0u8; 1];
            match stream.peek(&mut probe) {
                // The client retired the connection by closing it.
                Ok(0) => {}
                Ok(_) => break (stream, true),
                Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                Err(error) => panic!("probing the retired connection failed: {error}"),
            }
            assert!(Instant::now() < deadline, "no follow-up request arrived");
            thread::sleep(Duration::from_millis(1));
        };

        serving.set_nonblocking(false).unwrap();
        serving.set_read_timeout(Some(WAIT)).unwrap();
        serving.set_write_timeout(Some(WAIT)).unwrap();
        let (second, second_remainder) = read_http1_head(&mut serving);
        assert!(second.starts_with(b"GET /after-rejection HTTP/1.1\r\n"));
        assert!(second_remainder.is_empty());
        serving
            .write_all(
                b"HTTP/1.1 200 OK\r\n\
                  content-length: 6\r\n\
                  connection: close\r\n\r\n\
                  reused",
            )
            .unwrap();
        reused_retired_connection
    });
    FinalBeforeBodyPeer { address, thread }
}

struct AbandonedStreamingPeer {
    address: SocketAddr,
    thread: thread::JoinHandle<()>,
}

fn accept_std_before(listener: &StdTcpListener, deadline: Instant) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    loop {
        match listener.accept() {
            Ok((stream, _)) => return stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for a connection"
                );
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("failed to accept a connection: {error}"),
        }
    }
}

fn spawn_abandoned_streaming_peer() -> AbandonedStreamingPeer {
    let listener = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let thread = thread::spawn(move || {
        let deadline = Instant::now() + WAIT;
        let mut first = accept_std_before(&listener, deadline);
        first.set_read_timeout(Some(WAIT)).unwrap();
        first.set_write_timeout(Some(WAIT)).unwrap();
        let (head, remainder) = read_http1_head(&mut first);
        assert!(head.starts_with(b"POST /abandoned-stream HTTP/1.1\r\n"));
        assert!(
            head.windows(b"transfer-encoding: chunked\r\n".len())
                .any(|window| window.eq_ignore_ascii_case(b"transfer-encoding: chunked\r\n"))
        );
        assert!(
            !head
                .windows(b"content-length:".len())
                .any(|window| window.eq_ignore_ascii_case(b"content-length:"))
        );

        let mut body_wire = remainder;
        match first.read_to_end(&mut body_wire) {
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::ConnectionReset
                ) => {}
            Err(error) => panic!("failed to read the abandoned request: {error}"),
        }
        assert_eq!(body_wire, b"7\r\npartial\r\n");

        let mut second = accept_std_before(&listener, deadline);
        second.set_read_timeout(Some(WAIT)).unwrap();
        second.set_write_timeout(Some(WAIT)).unwrap();
        let (head, remainder) = read_http1_head(&mut second);
        assert!(head.starts_with(b"GET /after-abandoned-stream HTTP/1.1\r\n"));
        assert!(remainder.is_empty());
        second
            .write_all(
                b"HTTP/1.1 200 OK\r\n\
                  content-length: 5\r\n\
                  connection: close\r\n\r\n\
                  fresh",
            )
            .unwrap();
    });
    AbandonedStreamingPeer { address, thread }
}

fn header_values<'a>(headers: &'a hyper::HeaderMap, name: &'static str) -> Vec<&'a str> {
    headers
        .get_all(name)
        .iter()
        .map(|value| value.to_str().unwrap())
        .collect()
}

fn large_body(byte: u8) -> Vec<u8> {
    vec![byte; LARGE_BODY_LEN]
}

fn take_client_block(client: &mut H2Client, commit: kimojio_fsm_http::H2OutboundCommit) -> Vec<u8> {
    let block = client.next_outbound_block().unwrap();
    assert_eq!(block.commit(), commit);
    let bytes = block.bytes().to_vec();
    client.acknowledge_outbound_block(commit).unwrap();
    bytes
}

fn take_server_block(server: &mut H2Server, commit: kimojio_fsm_http::H2OutboundCommit) -> Vec<u8> {
    let block = server.next_outbound_block().unwrap();
    assert_eq!(block.commit(), commit);
    let bytes = block.bytes().to_vec();
    server.acknowledge_outbound_block(commit).unwrap();
    bytes
}

fn h2_request_bytes(headers: &[(&[u8], &[u8])], body: &[u8]) -> Vec<u8> {
    let mut client = H2Client::default();
    let mut output = client.connection_preface();
    let headers = headers
        .iter()
        .map(|(name, value)| H2HeaderField::new(*name, *value))
        .collect::<Vec<_>>();
    let (stream_id, commit) = client
        .open_stream_with_raw_headers("POST", "http", "localhost", "/length", &headers, false)
        .unwrap();
    output.extend_from_slice(&take_client_block(&mut client, commit));
    output.extend_from_slice(&client.data_frame(stream_id, body, true));
    output
}

/// Reads frames until the peer observes `RST_STREAM` for `stream_id` and
/// returns its error code.
///
/// A malformed request is a stream error rather than a connection error
/// (RFC 9113 section 8.1.1), so the server resets the stream and keeps the
/// connection open. Reading to end of file would block until the read timeout.
fn read_until_rst_stream(stream: &mut TcpStream, stream_id: u32) -> Option<u32> {
    const RST_STREAM: u8 = 0x3;
    let mut buffered = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let mut offset = 0;
        while buffered.len() >= offset + 9 {
            let length = u32::from_be_bytes([
                0,
                buffered[offset],
                buffered[offset + 1],
                buffered[offset + 2],
            ]) as usize;
            if buffered.len() < offset + 9 + length {
                break;
            }
            let kind = buffered[offset + 3];
            let frame_stream = u32::from_be_bytes([
                buffered[offset + 5] & 0x7f,
                buffered[offset + 6],
                buffered[offset + 7],
                buffered[offset + 8],
            ]);
            if kind == RST_STREAM && frame_stream == stream_id {
                let payload: [u8; 4] = buffered[offset + 9..offset + 9 + length].try_into().ok()?;
                return Some(u32::from_be_bytes(payload));
            }
            offset += 9 + length;
        }
        buffered.drain(..offset);
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffered.extend_from_slice(&chunk[..read]);
    }
}

fn spawn_h2_response(
    status: u16,
    headers: Vec<H2HeaderField>,
    body: &'static [u8],
) -> (SocketAddr, thread::JoinHandle<()>) {
    let listener = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let peer = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        stream.set_write_timeout(Some(WAIT)).unwrap();
        let mut protocol = H2Server::default();
        let mut input = Vec::new();
        let stream_id = 'request: loop {
            let mut buffer = [0; 4096];
            let read = stream.read(&mut buffer).unwrap();
            assert_ne!(read, 0, "Kimojio client closed before sending a request");
            input.extend_from_slice(&buffer[..read]);
            loop {
                let (event, consumed, output) = protocol.accept_event_bytes(&input).unwrap();
                if !output.is_empty() {
                    stream.write_all(&output).unwrap();
                }
                if consumed != 0 {
                    input.drain(..consumed);
                }
                if let Some(H2ByteStreamEvent::RequestHeaders { stream_id, .. }) = event {
                    break 'request stream_id;
                }
                if consumed == 0 || input.is_empty() {
                    break;
                }
            }
        };
        let commit = protocol
            .response_headers_frame_with_raw_headers(stream_id, status, &headers, body.is_empty())
            .unwrap();
        let mut response = take_server_block(&mut protocol, commit);
        if !body.is_empty() {
            response.extend_from_slice(&protocol.data_frame(stream_id, body, true));
        }
        stream.write_all(&response).unwrap();
    });
    (address, peer)
}

struct TokioPeer {
    address: SocketAddr,
    shutdown: oneshot::Sender<()>,
    thread: thread::JoinHandle<()>,
}

impl TokioPeer {
    fn stop(self) {
        let _ = self.shutdown.send(());
        self.thread.join().expect("Tokio peer thread panicked");
    }
}

struct CountingTokioPeer {
    address: SocketAddr,
    shutdown: oneshot::Sender<()>,
    connections: Arc<AtomicU32>,
    thread: thread::JoinHandle<()>,
}

impl CountingTokioPeer {
    fn stop(self) -> u32 {
        let _ = self.shutdown.send(());
        self.thread.join().expect("Tokio peer thread panicked");
        self.connections.load(Ordering::Acquire)
    }
}

fn spawn_hyper_http1() -> TokioPeer {
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let thread = thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
                ready_tx.send(listener.local_addr().unwrap()).unwrap();
                let (stream, _) = tokio::time::timeout(WAIT, listener.accept())
                    .await
                    .expect("timed out waiting for Kimojio HTTP/1 client")
                    .unwrap();

                let service = service_fn(|request: HyperRequest<Incoming>| async move {
                    assert_eq!(request.method(), hyper::Method::POST);
                    assert_eq!(request.headers()["x-client"], "kimojio");
                    assert_eq!(
                        header_values(request.headers(), "x-duplicate"),
                        ["one", "two"]
                    );
                    let target = request.uri().path_and_query().unwrap().as_str().to_owned();
                    if target == "/streaming-upload" {
                        assert!(!request.headers().contains_key("content-length"));
                        assert_eq!(request.headers()["transfer-encoding"], "chunked");
                    }
                    let body = request.into_body().collect().await.unwrap().to_bytes();
                    match target.as_str() {
                        "/interop?h1=1" => assert_eq!(body, "request-http1"),
                        "/streaming-upload" => assert_eq!(body, "streamed-request-http1"),
                        _ => panic!("unexpected Hyper HTTP/1 request target: {target}"),
                    }

                    let mut response =
                        HyperResponse::new(Full::new(Bytes::from_static(b"response-http1")));
                    *response.status_mut() = hyper::StatusCode::CREATED;
                    response
                        .headers_mut()
                        .insert("x-peer", HeaderValue::from_static("hyper-http1"));
                    response
                        .headers_mut()
                        .append("set-cookie", HeaderValue::from_static("first=1"));
                    response
                        .headers_mut()
                        .append("set-cookie", HeaderValue::from_static("second=2"));
                    Ok::<_, Infallible>(response)
                });

                let connection = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service);
                tokio::pin!(connection);
                tokio::select! {
                    result = &mut connection => result.unwrap(),
                    _ = shutdown_rx => {
                        connection.as_mut().graceful_shutdown();
                        tokio::time::timeout(WAIT, connection)
                            .await
                            .expect("timed out shutting down hyper HTTP/1 connection")
                            .unwrap();
                    }
                }
            });
    });

    TokioPeer {
        address: ready_rx
            .recv_timeout(WAIT)
            .expect("hyper HTTP/1 server did not become ready"),
        shutdown: shutdown_tx,
        thread,
    }
}

fn spawn_hyper_large_streaming_upload(
    expected_len: usize,
    received: Arc<AtomicUsize>,
) -> TokioPeer {
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let thread = thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
                ready_tx.send(listener.local_addr().unwrap()).unwrap();
                let (stream, _) = tokio::time::timeout(WAIT, listener.accept())
                    .await
                    .expect("timed out waiting for the large streaming upload")
                    .unwrap();

                let service = service_fn(move |request: HyperRequest<Incoming>| {
                    let received = Arc::clone(&received);
                    async move {
                        assert_eq!(request.method(), hyper::Method::POST);
                        assert_eq!(request.uri().path(), "/large-streaming-upload");
                        assert!(!request.headers().contains_key("content-length"));
                        assert_eq!(request.headers()["transfer-encoding"], "chunked");

                        let mut body = request.into_body();
                        let mut total = 0;
                        while let Some(frame) = body.frame().await {
                            let frame = frame.unwrap();
                            if let Ok(data) = frame.into_data() {
                                assert!(data.iter().all(|byte| *byte == b'l'));
                                total += data.len();
                                received.store(total, Ordering::Release);
                            }
                        }
                        assert_eq!(total, expected_len);
                        Ok::<_, Infallible>(HyperResponse::new(Full::new(Bytes::from_static(
                            b"accepted",
                        ))))
                    }
                });

                let connection = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service);
                tokio::pin!(connection);
                tokio::select! {
                    result = &mut connection => result.unwrap(),
                    _ = shutdown_rx => {
                        connection.as_mut().graceful_shutdown();
                        tokio::time::timeout(WAIT, connection)
                            .await
                            .expect("timed out shutting down the large-upload peer")
                            .unwrap();
                    }
                }
            });
    });

    TokioPeer {
        address: ready_rx
            .recv_timeout(WAIT)
            .expect("large-upload Hyper server did not become ready"),
        shutdown: shutdown_tx,
        thread,
    }
}

fn spawn_hyper_large_streaming_response(
    chunk_count: usize,
    chunk_len: usize,
    produced: Arc<AtomicUsize>,
) -> TokioPeer {
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let thread = thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
                ready_tx.send(listener.local_addr().unwrap()).unwrap();
                let (stream, _) = tokio::time::timeout(WAIT, listener.accept())
                    .await
                    .expect("timed out waiting for the streaming response client")
                    .unwrap();

                let service = service_fn(move |request: HyperRequest<Incoming>| {
                    let produced = Arc::clone(&produced);
                    async move {
                        assert_eq!(request.method(), hyper::Method::GET);
                        assert_eq!(request.uri().path(), "/large-streaming-response");
                        let chunks = futures::stream::unfold(0, move |index| {
                            let produced = Arc::clone(&produced);
                            async move {
                                if index == chunk_count {
                                    return None;
                                }
                                produced.fetch_add(1, Ordering::AcqRel);
                                let frame = Frame::data(Bytes::from(vec![b'r'; chunk_len]));
                                Some((Ok::<_, Infallible>(frame), index + 1))
                            }
                        });
                        Ok::<_, Infallible>(HyperResponse::new(StreamBody::new(chunks)))
                    }
                });

                let connection = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service);
                tokio::pin!(connection);
                tokio::select! {
                    result = &mut connection => result.unwrap(),
                    _ = shutdown_rx => {
                        connection.as_mut().graceful_shutdown();
                        tokio::time::timeout(WAIT, connection)
                            .await
                            .expect("timed out shutting down the streaming-response peer")
                            .unwrap();
                    }
                }
            });
    });

    TokioPeer {
        address: ready_rx
            .recv_timeout(WAIT)
            .expect("streaming-response Hyper server did not become ready"),
        shutdown: shutdown_tx,
        thread,
    }
}

fn spawn_hyper_http1_reuse() -> CountingTokioPeer {
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let connections = Arc::new(AtomicU32::new(0));
    let observed_connections = Arc::clone(&connections);
    let thread = thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
                ready_tx.send(listener.local_addr().unwrap()).unwrap();
                let mut tasks = tokio::task::JoinSet::new();

                loop {
                    tokio::select! {
                        _ = &mut shutdown_rx => break,
                        accepted = listener.accept() => {
                            let (stream, _) = accepted.unwrap();
                            observed_connections.fetch_add(1, Ordering::Release);
                            tasks.spawn(async move {
                                let service =
                                    service_fn(|request: HyperRequest<Incoming>| async move {
                                        let body = match request.uri().path() {
                                            "/pool/one" => Bytes::from_static(b"one"),
                                            "/pool/two" => Bytes::from_static(b"two"),
                                            path => panic!("unexpected pooling request path: {path}"),
                                        };
                                        assert_eq!(request.method(), hyper::Method::GET);
                                        Ok::<_, Infallible>(HyperResponse::new(Full::new(body)))
                                    });
                                let _ = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(TokioIo::new(stream), service)
                                    .await;
                            });
                        }
                    }
                }

                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
            });
    });

    CountingTokioPeer {
        address: ready_rx
            .recv_timeout(WAIT)
            .expect("hyper HTTP/1 reuse server did not become ready"),
        shutdown: shutdown_tx,
        connections,
        thread,
    }
}

fn spawn_hyper_http2() -> TokioPeer {
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let thread = thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
                ready_tx.send(listener.local_addr().unwrap()).unwrap();
                let (stream, _) = tokio::time::timeout(WAIT, listener.accept())
                    .await
                    .expect("timed out waiting for Kimojio HTTP/2 client")
                    .unwrap();

                let service = service_fn(|request: HyperRequest<Incoming>| async move {
                    assert_eq!(request.version(), hyper::Version::HTTP_2);
                    assert_eq!(request.method(), hyper::Method::PUT);
                    assert_eq!(request.headers()["x-client"], "kimojio");
                    assert_eq!(
                        header_values(request.headers(), "x-duplicate"),
                        ["one", "two"]
                    );
                    let path = request.uri().path().to_owned();
                    if path == "/streaming-upload-h2" {
                        assert!(!request.headers().contains_key("content-length"));
                        assert!(!request.headers().contains_key("transfer-encoding"));
                    }
                    let body = request.into_body().collect().await.unwrap().to_bytes();
                    let response_body = match path.as_str() {
                        "/interop-h2" => {
                            assert_eq!(body.as_ref(), large_body(b'q'));
                            Bytes::from(large_body(b's'))
                        }
                        "/streaming-upload-h2" => {
                            assert_eq!(body, "streamed-request-http2");
                            Bytes::from_static(b"response-http2")
                        }
                        _ => panic!("unexpected Hyper HTTP/2 request path: {path}"),
                    };

                    let mut response = HyperResponse::new(Full::new(response_body));
                    *response.status_mut() = hyper::StatusCode::ACCEPTED;
                    response
                        .headers_mut()
                        .insert("x-peer", HeaderValue::from_static("hyper-http2"));
                    response
                        .headers_mut()
                        .append("set-cookie", HeaderValue::from_static("first=1"));
                    response
                        .headers_mut()
                        .append("set-cookie", HeaderValue::from_static("second=2"));
                    Ok::<_, Infallible>(response)
                });

                let mut builder = hyper::server::conn::http2::Builder::new(TokioExecutor::new());
                builder.initial_stream_window_size(1_024);
                let connection = builder.serve_connection(TokioIo::new(stream), service);
                tokio::pin!(connection);
                tokio::select! {
                    result = &mut connection => result.unwrap(),
                    _ = shutdown_rx => {
                        connection.as_mut().graceful_shutdown();
                        tokio::time::timeout(WAIT, connection)
                            .await
                            .expect("timed out shutting down hyper HTTP/2 connection")
                            .unwrap();
                    }
                }
            });
    });

    TokioPeer {
        address: ready_rx
            .recv_timeout(WAIT)
            .expect("hyper HTTP/2 server did not become ready"),
        shutdown: shutdown_tx,
        thread,
    }
}

struct OrderedHyperH2Peer {
    address: SocketAddr,
    shutdown: oneshot::Sender<()>,
    response_order: Arc<Mutex<Vec<usize>>>,
    thread: thread::JoinHandle<()>,
}

impl OrderedHyperH2Peer {
    fn stop(self) -> Vec<usize> {
        let _ = self.shutdown.send(());
        self.thread.join().expect("Tokio peer thread panicked");
        Arc::try_unwrap(self.response_order)
            .expect("response-order state remained shared")
            .into_inner()
            .unwrap()
    }
}

fn spawn_ordered_hyper_http2() -> OrderedHyperH2Peer {
    const REQUESTS: usize = 4;

    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let response_order = Arc::new(Mutex::new(Vec::new()));
    let observed_order = Arc::clone(&response_order);
    let thread = thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
                ready_tx.send(listener.local_addr().unwrap()).unwrap();
                let (stream, _) = tokio::time::timeout(WAIT, listener.accept())
                    .await
                    .expect("timed out waiting for concurrent Kimojio HTTP/2 requests")
                    .unwrap();
                let started = Arc::new(AtomicUsize::new(0));
                let all_started = Arc::new(Notify::new());
                let next_rank = Arc::new(AtomicUsize::new(0));
                let rank_changed = Arc::new(Notify::new());

                let service = service_fn(move |request: HyperRequest<Incoming>| {
                    let started = Arc::clone(&started);
                    let all_started = Arc::clone(&all_started);
                    let next_rank = Arc::clone(&next_rank);
                    let rank_changed = Arc::clone(&rank_changed);
                    let response_order = Arc::clone(&observed_order);
                    async move {
                        assert_eq!(request.version(), hyper::Version::HTTP_2);
                        assert_eq!(request.method(), hyper::Method::GET);
                        let index = request
                            .uri()
                            .path()
                            .strip_prefix("/ordered/")
                            .expect("unexpected ordered request path")
                            .parse::<usize>()
                            .unwrap();
                        assert!(index < REQUESTS);

                        if started.fetch_add(1, Ordering::AcqRel) + 1 == REQUESTS {
                            all_started.notify_waiters();
                        }
                        loop {
                            let notified = all_started.notified();
                            if started.load(Ordering::Acquire) == REQUESTS {
                                break;
                            }
                            notified.await;
                        }

                        let rank = match index {
                            3 => 0,
                            1 => 1,
                            2 => 2,
                            0 => 3,
                            _ => unreachable!(),
                        };
                        loop {
                            let notified = rank_changed.notified();
                            if next_rank.load(Ordering::Acquire) == rank {
                                break;
                            }
                            notified.await;
                        }
                        response_order.lock().unwrap().push(index);
                        next_rank.fetch_add(1, Ordering::Release);
                        rank_changed.notify_waiters();

                        Ok::<_, Infallible>(HyperResponse::new(Full::new(Bytes::from(format!(
                            "response-{index}"
                        )))))
                    }
                });

                let connection = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service);
                tokio::pin!(connection);
                tokio::select! {
                    result = &mut connection => result.unwrap(),
                    _ = shutdown_rx => {
                        connection.as_mut().graceful_shutdown();
                        tokio::time::timeout(WAIT, connection)
                            .await
                            .expect("timed out shutting down ordered hyper HTTP/2 connection")
                            .unwrap();
                    }
                }
            });
    });

    OrderedHyperH2Peer {
        address: ready_rx
            .recv_timeout(WAIT)
            .expect("ordered hyper HTTP/2 server did not become ready"),
        shutdown: shutdown_tx,
        response_order,
        thread,
    }
}

fn spawn_interleaved_hyper_http2() -> TokioPeer {
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let thread = thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
                ready_tx.send(listener.local_addr().unwrap()).unwrap();
                let (stream, _) = tokio::time::timeout(WAIT, listener.accept())
                    .await
                    .expect("timed out waiting for interleaved Kimojio HTTP/2 requests")
                    .unwrap();
                let first_stream_chunk = Arc::new(AtomicBool::new(false));
                let first_stream_chunk_ready = Arc::new(Notify::new());
                let buffered_chunk = Arc::new(AtomicBool::new(false));
                let buffered_chunk_ready = Arc::new(Notify::new());

                let service = service_fn(move |request: HyperRequest<Incoming>| {
                    let first_stream_chunk = Arc::clone(&first_stream_chunk);
                    let first_stream_chunk_ready = Arc::clone(&first_stream_chunk_ready);
                    let buffered_chunk = Arc::clone(&buffered_chunk);
                    let buffered_chunk_ready = Arc::clone(&buffered_chunk_ready);
                    async move {
                        assert_eq!(request.version(), hyper::Version::HTTP_2);
                        assert_eq!(request.method(), hyper::Method::GET);
                        let body = match request.uri().path() {
                            "/stream" => {
                                let chunks = futures::stream::unfold(0, move |index| {
                                    let first_stream_chunk = Arc::clone(&first_stream_chunk);
                                    let first_stream_chunk_ready =
                                        Arc::clone(&first_stream_chunk_ready);
                                    let buffered_chunk = Arc::clone(&buffered_chunk);
                                    let buffered_chunk_ready = Arc::clone(&buffered_chunk_ready);
                                    async move {
                                        match index {
                                            0 => {
                                                first_stream_chunk.store(true, Ordering::Release);
                                                first_stream_chunk_ready.notify_waiters();
                                                Some((
                                                    Ok::<_, Infallible>(Frame::data(
                                                        Bytes::from_static(b"stream-first-"),
                                                    )),
                                                    1,
                                                ))
                                            }
                                            1 => {
                                                loop {
                                                    let notified = buffered_chunk_ready.notified();
                                                    if buffered_chunk.load(Ordering::Acquire) {
                                                        break;
                                                    }
                                                    notified.await;
                                                }
                                                Some((
                                                    Ok::<_, Infallible>(Frame::data(
                                                        Bytes::from_static(b"stream-last"),
                                                    )),
                                                    2,
                                                ))
                                            }
                                            _ => None,
                                        }
                                    }
                                });
                                StreamBody::new(chunks).boxed_unsync()
                            }
                            "/buffered" => {
                                loop {
                                    let notified = first_stream_chunk_ready.notified();
                                    if first_stream_chunk.load(Ordering::Acquire) {
                                        break;
                                    }
                                    notified.await;
                                }
                                let chunks = futures::stream::once(async move {
                                    buffered_chunk.store(true, Ordering::Release);
                                    buffered_chunk_ready.notify_waiters();
                                    Ok::<_, Infallible>(Frame::data(Bytes::from_static(
                                        b"buffered",
                                    )))
                                });
                                StreamBody::new(chunks).boxed_unsync()
                            }
                            path => panic!("unexpected interleaved request path: {path}"),
                        };
                        Ok::<_, Infallible>(HyperResponse::new(body))
                    }
                });

                let connection = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service);
                tokio::pin!(connection);
                tokio::select! {
                    result = &mut connection => result.unwrap(),
                    _ = shutdown_rx => {
                        connection.as_mut().graceful_shutdown();
                        tokio::time::timeout(WAIT, connection)
                            .await
                            .expect("timed out shutting down interleaved hyper HTTP/2 connection")
                            .unwrap();
                    }
                }
            });
    });

    TokioPeer {
        address: ready_rx
            .recv_timeout(WAIT)
            .expect("interleaved hyper HTTP/2 server did not become ready"),
        shutdown: shutdown_tx,
        thread,
    }
}

fn spawn_concurrent_upload_hyper_http2(expected_len: usize) -> TokioPeer {
    const REQUESTS: usize = 4;

    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let thread = thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
                ready_tx.send(listener.local_addr().unwrap()).unwrap();
                let (stream, _) = tokio::time::timeout(WAIT, listener.accept())
                    .await
                    .expect("timed out waiting for concurrent Kimojio HTTP/2 uploads")
                    .unwrap();
                let requests = Arc::new(AtomicUsize::new(0));
                let all_requests = Arc::new(Notify::new());
                let completed_bodies = Arc::new(AtomicUsize::new(0));
                let all_bodies_complete = Arc::new(Notify::new());

                let service = service_fn(move |request: HyperRequest<Incoming>| {
                    let requests = Arc::clone(&requests);
                    let all_requests = Arc::clone(&all_requests);
                    let completed_bodies = Arc::clone(&completed_bodies);
                    let all_bodies_complete = Arc::clone(&all_bodies_complete);
                    async move {
                        assert_eq!(request.version(), hyper::Version::HTTP_2);
                        assert_eq!(request.method(), hyper::Method::PUT);
                        if let Some(declared) = request.headers().get("content-length") {
                            assert_eq!(declared, expected_len.to_string().as_str());
                        }
                        let index = request
                            .uri()
                            .path()
                            .strip_prefix("/upload/")
                            .expect("unexpected upload request path")
                            .parse::<usize>()
                            .unwrap();
                        assert!(index < REQUESTS);
                        let expected = b'a' + u8::try_from(index).unwrap();
                        if requests.fetch_add(1, Ordering::AcqRel) + 1 == REQUESTS {
                            all_requests.notify_waiters();
                        }
                        loop {
                            let notified = all_requests.notified();
                            if requests.load(Ordering::Acquire) == REQUESTS {
                                break;
                            }
                            notified.await;
                        }

                        let mut body = request.into_body();
                        let mut received = 0;
                        while let Some(frame) = body.frame().await {
                            let frame = frame.unwrap();
                            if let Ok(data) = frame.into_data() {
                                assert!(data.iter().all(|byte| *byte == expected));
                                received += data.len();
                            }
                        }
                        assert_eq!(received, expected_len);
                        if completed_bodies.fetch_add(1, Ordering::AcqRel) + 1 == REQUESTS {
                            all_bodies_complete.notify_waiters();
                        }
                        loop {
                            let notified = all_bodies_complete.notified();
                            if completed_bodies.load(Ordering::Acquire) == REQUESTS {
                                break;
                            }
                            notified.await;
                        }
                        Ok::<_, Infallible>(HyperResponse::new(Full::new(Bytes::from(format!(
                            "uploaded-{index}"
                        )))))
                    }
                });

                let mut builder = hyper::server::conn::http2::Builder::new(TokioExecutor::new());
                builder.initial_stream_window_size(1_024);
                // Keep the aggregate window out of the barrier so the test isolates
                // per-stream request-body scheduling.
                builder.initial_connection_window_size(
                    u32::try_from(expected_len * REQUESTS * 2).unwrap(),
                );
                let connection = builder.serve_connection(TokioIo::new(stream), service);
                tokio::pin!(connection);
                tokio::select! {
                    result = &mut connection => result.unwrap(),
                    _ = shutdown_rx => {
                        connection.as_mut().graceful_shutdown();
                        tokio::time::timeout(WAIT, connection)
                            .await
                            .expect("timed out shutting down upload hyper HTTP/2 connection")
                            .unwrap();
                    }
                }
            });
    });

    TokioPeer {
        address: ready_rx
            .recv_timeout(WAIT)
            .expect("upload hyper HTTP/2 server did not become ready"),
        shutdown: shutdown_tx,
        thread,
    }
}

#[kimojio::test]
async fn curl_expect_continue_to_kimojio_server_accepts_body() {
    let server = Server::bind((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
    let request_body = vec![b'x'; 32 * 1024];
    let peer = spawn_curl_expect_http1(server.local_addr(), request_body.clone());
    let cancellation = Rc::new(CancellationToken::new());
    let cancel_from_handler = Rc::clone(&cancellation);
    let hook_called = Rc::new(Cell::new(false));
    let observed_hook = Rc::clone(&hook_called);

    let serve_result = operations::timeout_at(
        Instant::now() + WAIT,
        server.serve_with_expect_continue(
            move |request| {
                assert_eq!(request.method(), Method::POST);
                assert_eq!(request.uri(), "/expect");
                assert_eq!(request.body().as_bytes(), request_body);
                let cancellation = Rc::clone(&cancel_from_handler);
                async move {
                    cancellation.cancel();
                    Response::builder()
                        .status(StatusCode::CREATED)
                        .body(Body::from("accepted"))
                        .unwrap()
                }
            },
            move |head| {
                assert_eq!(head.method(), Method::POST);
                assert_eq!(head.uri(), "/expect");
                observed_hook.set(true);
                ExpectContinueDecision::Continue
            },
            cancellation,
        ),
    )
    .await;

    let output = peer.join().expect("curl peer thread panicked");
    assert!(
        output.status.success(),
        "curl failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serve_result
        .expect("Kimojio Expect server timed out")
        .unwrap();
    assert!(hook_called.get());
    let interim = output
        .stdout
        .windows(b"HTTP/1.1 100 Continue\r\n".len())
        .position(|window| window == b"HTTP/1.1 100 Continue\r\n")
        .expect("curl did not receive 100 Continue");
    let final_response = output
        .stdout
        .windows(b"HTTP/1.1 201 Created\r\n".len())
        .position(|window| window == b"HTTP/1.1 201 Created\r\n")
        .expect("curl did not receive the final response");
    assert!(interim < final_response);
    assert!(output.stdout.ends_with(b"accepted"));
}

#[kimojio::test]
async fn ignored_expect_continue_degrades_to_automatic_continue() {
    let server = Server::bind((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
    let address = server.local_addr();
    let peer = thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        stream.set_write_timeout(Some(WAIT)).unwrap();
        stream
            .write_all(
                b"POST /ignored HTTP/1.1\r\n\
                  host: localhost\r\n\
                  content-length: 12\r\n\
                  expect: 100-continue\r\n\
                  connection: close\r\n\r\n",
            )
            .unwrap();
        let (interim, remainder) = read_http1_head(&mut stream);
        assert_eq!(interim, b"HTTP/1.1 100 Continue\r\n\r\n");
        assert!(remainder.is_empty());
        stream.write_all(b"request-body").unwrap();
        let mut final_response = Vec::new();
        stream.read_to_end(&mut final_response).unwrap();
        final_response
    });
    let cancellation = Rc::new(CancellationToken::new());
    let cancel_from_handler = Rc::clone(&cancellation);

    let serve_result = operations::timeout_at(
        Instant::now() + WAIT,
        server.serve(
            move |request| {
                assert_eq!(request.uri(), "/ignored");
                assert_eq!(request.body().as_bytes(), b"request-body");
                let cancellation = Rc::clone(&cancel_from_handler);
                async move {
                    cancellation.cancel();
                    Response::new(Body::from("accepted"))
                }
            },
            cancellation,
        ),
    )
    .await;

    let response = peer.join().expect("raw Expect peer thread panicked");
    serve_result
        .expect("automatic Continue server timed out")
        .unwrap();
    assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert!(response.ends_with(b"accepted"));
}

#[kimojio::test]
async fn rejected_expect_continue_never_transmits_request_body() {
    let server = Server::bind((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
    let proxy = spawn_expect_proxy(server.local_addr());
    let cancellation = Rc::new(CancellationToken::new());
    let cancel_from_test = Rc::clone(&cancellation);
    let hook_called = Rc::new(Cell::new(false));
    let observed_hook = Rc::clone(&hook_called);
    let handler_called = Rc::new(Cell::new(false));
    let observed_handler = Rc::clone(&handler_called);
    let serve_task = operations::spawn_task(server.serve_with_expect_continue(
        move |_request| {
            observed_handler.set(true);
            async { Response::new(Body::empty()) }
        },
        move |head| {
            assert_eq!(head.uri(), "/reject");
            observed_hook.set(true);
            ExpectContinueDecision::Reject(
                Response::builder()
                    .status(StatusCode::PAYLOAD_TOO_LARGE)
                    .body(Body::empty())
                    .unwrap(),
            )
        },
        cancellation,
    ));
    let request_body = vec![b'z'; 128 * 1024];

    let response = operations::timeout_at(
        Instant::now() + WAIT,
        Client::new()
            .post(format!("http://{}/reject", proxy.address))
            .header("expect", "100-continue")
            .body(request_body)
            .send(),
    )
    .await
    .expect("rejected Expect request timed out")
    .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let transmitted_body = proxy.thread.join().expect("Expect proxy thread panicked");
    cancel_from_test.cancel();
    operations::timeout_at(Instant::now() + WAIT, serve_task)
        .await
        .expect("rejection server did not shut down")
        .unwrap()
        .unwrap();

    assert!(hook_called.get());
    assert!(!handler_called.get());
    assert!(
        transmitted_body.is_empty(),
        "client transmitted {} rejected body bytes",
        transmitted_body.len()
    );
}

#[kimojio::test]
async fn client_sends_expect_body_after_bounded_wait() {
    let request_body = vec![b't'; 8 * 1024];
    let peer = spawn_no_continue_peer(request_body.len());
    let client = Client::with_config(
        ClientConfig::new().set_expect_continue_timeout(Duration::from_millis(200)),
    )
    .unwrap();

    let response = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .post(format!("http://{}/timeout", peer.address))
            .header("expect", "100-continue")
            .body(request_body.clone())
            .send(),
    )
    .await
    .expect("bounded Expect wait hung")
    .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.body().as_bytes(), b"accepted");
    assert_eq!(
        peer.thread
            .join()
            .expect("no-continue peer thread panicked"),
        request_body
    );
}

#[kimojio::test]
async fn final_expect_response_suppresses_body_and_retires_connection() {
    let peer = spawn_final_before_body_peer();
    let client = Client::new();

    let rejected = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .post(format!("http://{}/reject-and-reuse", peer.address))
            .header("expect", "100-continue")
            .body("must-not-be-sent")
            .send(),
    )
    .await
    .expect("early final response timed out")
    .unwrap();
    assert_eq!(rejected.status(), StatusCode::EXPECTATION_FAILED);

    let reused = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .get(format!("http://{}/after-rejection", peer.address))
            .send(),
    )
    .await
    .expect("request after early final response timed out")
    .unwrap();
    // Served on a fresh connection, because the rejected exchange left a
    // declared-but-unsent body behind.
    assert_eq!(reused.body().as_bytes(), b"reused");
    let reused_retired_connection = peer
        .thread
        .join()
        .expect("early-final pooling peer thread panicked");
    assert!(
        !reused_retired_connection,
        "the client pooled a connection whose declared body it never sent"
    );
}

#[kimojio::test]
async fn kimojio_client_to_hyper_http1() {
    let peer = spawn_hyper_http1();
    let response = operations::timeout_at(
        Instant::now() + WAIT,
        Client::new()
            .post(format!("http://{}/interop?h1=1", peer.address))
            .header("x-client", "kimojio")
            .header("x-duplicate", "one")
            .header("x-duplicate", "two")
            .body("request-http1")
            .send(),
    )
    .await
    .expect("Kimojio HTTP/1 request timed out")
    .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers()["x-peer"], "hyper-http1");
    assert_eq!(
        header_values(response.headers(), "set-cookie"),
        ["first=1", "second=2"]
    );
    assert_eq!(response.body().as_bytes(), b"response-http1");
    peer.stop();
}

#[kimojio::test]
async fn kimojio_client_streams_chunked_upload_to_hyper_http1() {
    let peer = spawn_hyper_http1();
    let body = Body::from_chunks(futures::stream::iter([
        b"streamed-".to_vec(),
        b"request-".to_vec(),
        b"http1".to_vec(),
    ]));
    let response = operations::timeout_at(
        Instant::now() + WAIT,
        Client::new()
            .post(format!("http://{}/streaming-upload", peer.address))
            .header("x-client", "kimojio")
            .header("x-duplicate", "one")
            .header("x-duplicate", "two")
            .body(body)
            .send(),
    )
    .await
    .expect("streaming upload timed out")
    .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.body().as_bytes(), b"response-http1");
    peer.stop();
}

#[kimojio::test]
async fn large_streaming_upload_generation_is_interleaved_with_hyper_reads() {
    const CHUNK_BYTES: usize = 256 * 1024;
    const BODY_BYTES: usize = 32 * 1024 * 1024;

    let received = Arc::new(AtomicUsize::new(0));
    let peer = spawn_hyper_large_streaming_upload(BODY_BYTES, Arc::clone(&received));
    let generated = Rc::new(Cell::new(0usize));
    let max_ahead = Rc::new(Cell::new(0usize));
    let source_received = Arc::clone(&received);
    let source_generated = Rc::clone(&generated);
    let source_max_ahead = Rc::clone(&max_ahead);
    let chunks = futures::stream::unfold(0usize, move |offset| {
        let received = Arc::clone(&source_received);
        let generated = Rc::clone(&source_generated);
        let max_ahead = Rc::clone(&source_max_ahead);
        async move {
            if offset == BODY_BYTES {
                return None;
            }
            let deadline = Instant::now() + WAIT;
            while received.load(Ordering::Acquire) < offset {
                assert!(
                    Instant::now() < deadline,
                    "body generation ran ahead of transport progress"
                );
                operations::sleep(Duration::from_millis(1)).await.unwrap();
            }

            let length = CHUNK_BYTES.min(BODY_BYTES - offset);
            let next = offset + length;
            let observed = received.load(Ordering::Acquire);
            generated.set(next);
            max_ahead.set(max_ahead.get().max(next.saturating_sub(observed)));
            Some((vec![b'l'; length], next))
        }
    });
    let response = operations::timeout_at(
        Instant::now() + WAIT,
        Client::new()
            .post(format!("http://{}/large-streaming-upload", peer.address))
            .body(Body::from_chunks(chunks))
            .send(),
    )
    .await
    .expect("large streaming upload timed out")
    .unwrap();

    assert_eq!(response.body().as_bytes(), b"accepted");
    assert_eq!(generated.get(), BODY_BYTES);
    assert_eq!(received.load(Ordering::Acquire), BODY_BYTES);
    assert!(
        max_ahead.get() <= CHUNK_BYTES,
        "the producer got {} bytes ahead of the peer",
        max_ahead.get()
    );
    peer.stop();
}

#[kimojio::test]
async fn streaming_client_backpressures_real_hyper_peer() {
    const CHUNK_COUNT: usize = 512;
    const CHUNK_LEN: usize = 64 * 1024;
    const BODY_LEN: usize = CHUNK_COUNT * CHUNK_LEN;

    let produced = Arc::new(AtomicUsize::new(0));
    let peer = spawn_hyper_large_streaming_response(CHUNK_COUNT, CHUNK_LEN, Arc::clone(&produced));
    let client = Client::with_config(
        ClientConfig::new().set_limits(Limits::new().set_read_buffer_bytes(128 * 1024)),
    )
    .unwrap();
    let mut response = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .get(format!("http://{}/large-streaming-response", peer.address))
            .send_streaming(),
    )
    .await
    .expect("streaming response head timed out")
    .unwrap();
    let deadline = Instant::now() + WAIT;
    let mut stable = 0;
    let mut previous = produced.load(Ordering::Acquire);
    while stable < 5 {
        assert!(
            Instant::now() < deadline,
            "Hyper response producer never became backpressured"
        );
        operations::sleep(Duration::from_millis(10)).await.unwrap();
        let current = produced.load(Ordering::Acquire);
        stable = if current != 0 && current == previous {
            stable + 1
        } else {
            0
        };
        previous = current;
    }
    assert!(
        previous < CHUNK_COUNT,
        "client eagerly drained the entire Hyper response"
    );
    operations::sleep(Duration::from_millis(50)).await.unwrap();
    assert_eq!(
        produced.load(Ordering::Acquire),
        previous,
        "Hyper resumed producing while the client body was idle"
    );

    let mut received = 0;
    while let Some(chunk) = response.body_mut().next_chunk().await.unwrap() {
        assert!(chunk.iter().all(|byte| *byte == b'r'));
        received += chunk.len();
    }
    assert_eq!(received, BODY_LEN);
    peer.stop();
}

#[kimojio::test]
async fn errored_streaming_upload_retires_connection_before_pooling() {
    let peer = spawn_abandoned_streaming_peer();
    let client = Client::new();
    let chunks: Vec<std::io::Result<Vec<u8>>> = vec![
        Ok(b"partial".to_vec()),
        Err(std::io::Error::other("source abandoned")),
    ];

    let error = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .post(format!("http://{}/abandoned-stream", peer.address))
            .body(Body::from_stream(futures::stream::iter(chunks)))
            .send(),
    )
    .await
    .expect("abandoned streaming upload timed out")
    .unwrap_err();
    assert!(
        matches!(error, HttpError::BodyStream { ref source } if source.to_string() == "source abandoned")
    );

    let response = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .get(format!("http://{}/after-abandoned-stream", peer.address))
            .send(),
    )
    .await
    .expect("request after abandoned stream timed out")
    .unwrap();
    assert_eq!(response.body().as_bytes(), b"fresh");
    peer.thread.join().expect("stream retirement peer panicked");
}

#[kimojio::test]
async fn kimojio_client_resolves_localhost() {
    let peer = spawn_hyper_http1();
    let response = operations::timeout_at(
        Instant::now() + WAIT,
        Client::new()
            .post(format!(
                "http://localhost:{}/interop?h1=1",
                peer.address.port()
            ))
            .header("x-client", "kimojio")
            .header("x-duplicate", "one")
            .header("x-duplicate", "two")
            .body("request-http1")
            .send(),
    )
    .await
    .expect("Kimojio HTTP/1 host-name request timed out")
    .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers()["x-peer"], "hyper-http1");
    assert_eq!(response.body().as_bytes(), b"response-http1");
    peer.stop();
}

#[kimojio::test]
async fn kimojio_client_reuses_hyper_http1_connection_for_sequential_requests() {
    let peer = spawn_hyper_http1_reuse();
    let client = Client::new();

    for (path, body) in [("one", b"one".as_slice()), ("two", b"two".as_slice())] {
        let response = operations::timeout_at(
            Instant::now() + WAIT,
            client
                .get(format!("http://{}/pool/{path}", peer.address))
                .send(),
        )
        .await
        .expect("Kimojio HTTP/1 pooling request timed out")
        .unwrap();
        assert_eq!(response.body().as_bytes(), body);
    }

    assert_eq!(
        peer.stop(),
        1,
        "two sequential requests must reach hyper on one TCP connection"
    );
}

#[kimojio::test]
async fn kimojio_client_to_hyper_http2_prior_knowledge() {
    let peer = spawn_hyper_http2();
    let response = operations::timeout_at(
        Instant::now() + WAIT,
        Client::new()
            .request(Method::PUT, format!("http://{}/interop-h2", peer.address))
            .version(Version::HTTP_2)
            .header("x-client", "kimojio")
            .header("x-duplicate", "one")
            .header("x-duplicate", "two")
            .body(large_body(b'q'))
            .send(),
    )
    .await
    .expect("Kimojio HTTP/2 request timed out")
    .unwrap();

    assert_eq!(response.version(), Version::HTTP_2);
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(response.headers()["x-peer"], "hyper-http2");
    assert_eq!(
        header_values(response.headers(), "set-cookie"),
        ["first=1", "second=2"]
    );
    assert_eq!(response.body().as_bytes(), large_body(b's'));
    peer.stop();
}

#[kimojio::test]
async fn kimojio_client_streams_response_from_hyper_http2() {
    let peer = spawn_hyper_http2();
    let mut response = operations::timeout_at(
        Instant::now() + WAIT,
        Client::new()
            .request(Method::PUT, format!("http://{}/interop-h2", peer.address))
            .version(Version::HTTP_2)
            .header("x-client", "kimojio")
            .header("x-duplicate", "one")
            .header("x-duplicate", "two")
            .body(large_body(b'q'))
            .send_streaming(),
    )
    .await
    .expect("streaming Kimojio HTTP/2 client timed out")
    .unwrap();
    assert_eq!(response.version(), Version::HTTP_2);
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert!(response.body().is_streaming());

    let mut body = Vec::new();
    while let Some(chunk) = response.body_mut().next_chunk().await.unwrap() {
        body.extend_from_slice(&chunk);
    }
    assert_eq!(body, large_body(b's'));
    peer.stop();
}

#[kimojio::test]
async fn kimojio_client_streams_upload_to_hyper_http2_without_content_length() {
    let peer = spawn_hyper_http2();
    let response = operations::timeout_at(
        Instant::now() + WAIT,
        Client::new()
            .request(
                Method::PUT,
                format!("http://{}/streaming-upload-h2", peer.address),
            )
            .version(Version::HTTP_2)
            .header("x-client", "kimojio")
            .header("x-duplicate", "one")
            .header("x-duplicate", "two")
            .body(Body::from_chunks(futures::stream::iter([
                b"streamed-".to_vec(),
                b"request-".to_vec(),
                b"http2".to_vec(),
            ])))
            .send(),
    )
    .await
    .expect("streaming HTTP/2 upload timed out")
    .unwrap();

    assert_eq!(response.version(), Version::HTTP_2);
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(response.body().as_bytes(), b"response-http2");
    peer.stop();
}

#[kimojio::test]
async fn kimojio_client_matches_out_of_order_hyper_http2_responses() {
    let peer = spawn_ordered_hyper_http2();
    let client = Client::new();
    let base = format!("http://{}", peer.address);

    let responses = operations::timeout_at(Instant::now() + WAIT, async {
        futures::join!(
            client
                .get(format!("{base}/ordered/0"))
                .version(Version::HTTP_2)
                .send(),
            client
                .get(format!("{base}/ordered/1"))
                .version(Version::HTTP_2)
                .send(),
            client
                .get(format!("{base}/ordered/2"))
                .version(Version::HTTP_2)
                .send(),
            client
                .get(format!("{base}/ordered/3"))
                .version(Version::HTTP_2)
                .send(),
        )
    })
    .await
    .expect("concurrent hyper HTTP/2 requests timed out");

    for (index, response) in [responses.0, responses.1, responses.2, responses.3]
        .into_iter()
        .enumerate()
    {
        assert_eq!(
            response.unwrap().body().as_bytes(),
            format!("response-{index}").as_bytes()
        );
    }
    drop(client);
    assert_eq!(peer.stop(), [3, 1, 2, 0]);
}

#[kimojio::test]
async fn kimojio_client_interleaves_streaming_and_buffered_hyper_http2_responses() {
    let peer = spawn_interleaved_hyper_http2();
    let client = Client::new();
    let base = format!("http://{}", peer.address);

    let (streamed, buffered) = operations::timeout_at(Instant::now() + WAIT, async {
        futures::join!(
            async {
                let mut response = client
                    .get(format!("{base}/stream"))
                    .version(Version::HTTP_2)
                    .send_streaming()
                    .await
                    .unwrap();
                let mut body = Vec::new();
                while let Some(chunk) = response.body_mut().next_chunk().await.unwrap() {
                    body.extend_from_slice(&chunk);
                }
                body
            },
            client
                .get(format!("{base}/buffered"))
                .version(Version::HTTP_2)
                .send(),
        )
    })
    .await
    .expect("interleaved hyper HTTP/2 responses timed out");

    assert_eq!(streamed, b"stream-first-stream-last");
    assert_eq!(buffered.unwrap().body().as_bytes(), b"buffered");
    drop(client);
    peer.stop();
}

#[kimojio::test]
async fn kimojio_client_fairly_schedules_concurrent_hyper_http2_uploads() {
    const UPLOAD_LEN: usize = 8 * 1024;

    let peer = spawn_concurrent_upload_hyper_http2(UPLOAD_LEN);
    let client = Client::new();
    let base = format!("http://{}", peer.address);
    let upload = |index: usize| {
        let byte = b'a' + u8::try_from(index).unwrap();
        client
            .request(Method::PUT, format!("{base}/upload/{index}"))
            .version(Version::HTTP_2)
            .body(vec![byte; UPLOAD_LEN])
            .send()
    };

    let responses = operations::timeout_at(Instant::now() + WAIT, async {
        futures::join!(upload(0), upload(1), upload(2), upload(3))
    })
    .await
    .expect("concurrent hyper HTTP/2 uploads timed out");

    for (index, response) in [responses.0, responses.1, responses.2, responses.3]
        .into_iter()
        .enumerate()
    {
        assert_eq!(
            response.unwrap().body().as_bytes(),
            format!("uploaded-{index}").as_bytes()
        );
    }
    drop(client);
    peer.stop();
}

fn spawn_reqwest_http1(address: SocketAddr) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let client = reqwest::Client::builder()
                    .http1_only()
                    .timeout(WAIT)
                    .build()
                    .unwrap();
                let response = tokio::time::timeout(
                    WAIT,
                    client
                        .post(format!("http://{address}/from-reqwest?h1=1"))
                        .header("x-client", "reqwest")
                        .header("x-duplicate", "one")
                        .header("x-duplicate", "two")
                        .body("reqwest-http1")
                        .send(),
                )
                .await
                .expect("reqwest HTTP/1 request timed out")
                .unwrap();

                assert_eq!(response.status(), reqwest::StatusCode::CREATED);
                assert_eq!(response.headers()["x-peer"], "kimojio-http1");
                assert_eq!(
                    header_values(response.headers(), "set-cookie"),
                    ["first=1", "second=2"]
                );
                assert_eq!(response.bytes().await.unwrap(), "kimojio-response-http1");
            });
    })
}

fn spawn_reqwest_streaming_http1(
    address: SocketAddr,
    response_done: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let client = reqwest::Client::builder()
                    .http1_only()
                    .timeout(WAIT)
                    .build()
                    .unwrap();

                let streamed = client
                    .get(format!("http://{address}/streamed-response"))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(streamed.status(), reqwest::StatusCode::OK);
                assert!(!streamed.headers().contains_key("content-length"));
                assert_eq!(streamed.headers()["transfer-encoding"], "chunked");
                assert_eq!(streamed.bytes().await.unwrap(), "streamed-response-http1");

                let buffered = client
                    .get(format!("http://{address}/buffered-response"))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(buffered.status(), reqwest::StatusCode::OK);
                assert_eq!(
                    buffered.headers()["content-length"],
                    "buffered-response-http1".len().to_string()
                );
                assert!(!buffered.headers().contains_key("transfer-encoding"));
                assert_eq!(buffered.bytes().await.unwrap(), "buffered-response-http1");
                response_done.store(true, Ordering::Release);
            });
    })
}

fn spawn_reqwest_streaming_http2(
    address: SocketAddr,
    response_done: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let client = reqwest_http2_client();
                let response = client
                    .get(format!("http://{address}/streamed-response-h2"))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.version(), reqwest::Version::HTTP_2);
                assert_eq!(response.status(), reqwest::StatusCode::OK);
                assert!(!response.headers().contains_key("content-length"));
                assert_eq!(response.bytes().await.unwrap(), "streamed-response-http2");
                response_done.store(true, Ordering::Release);
            });
    })
}

fn spawn_reqwest_http1_reuse(address: SocketAddr) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let client = reqwest::Client::builder()
                    .http1_only()
                    .timeout(Duration::from_secs(2))
                    .build()
                    .unwrap();
                let first = client
                    .get(format!("http://{address}/reuse/one"))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(first.status(), reqwest::StatusCode::OK);
                assert_eq!(first.bytes().await.unwrap(), "one");

                // With the server capped at one active connection, this
                // partial connection occupies the next accept slot only if the
                // first request's connection was closed. The second request
                // can succeed promptly only by checking out that pooled
                // connection again.
                let mut blocker = TcpStream::connect(address).unwrap();
                blocker.write_all(b"P").unwrap();
                thread::sleep(Duration::from_millis(100));

                let second = client
                    .get(format!("http://{address}/reuse/two"))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(second.status(), reqwest::StatusCode::OK);
                assert_eq!(second.bytes().await.unwrap(), "two");
            });
    })
}

fn reqwest_http2_client() -> reqwest::Client {
    reqwest::Client::builder()
        .http2_prior_knowledge()
        .http2_initial_stream_window_size(1_024)
        .timeout(WAIT)
        .build()
        .unwrap()
}

fn spawn_reqwest_http2(
    address: SocketAddr,
    response_done: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let client = reqwest_http2_client();
                let response = tokio::time::timeout(
                    WAIT,
                    client
                        .put(format!("http://{address}/from-reqwest-h2"))
                        .header("x-client", "reqwest")
                        .header("x-duplicate", "one")
                        .header("x-duplicate", "two")
                        .body(large_body(b'r'))
                        .send(),
                )
                .await
                .expect("reqwest HTTP/2 request timed out")
                .unwrap();

                assert_eq!(response.version(), reqwest::Version::HTTP_2);
                assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
                assert_eq!(response.headers()["x-peer"], "kimojio-http2");
                assert_eq!(
                    header_values(response.headers(), "set-cookie"),
                    ["first=1", "second=2"]
                );
                assert_eq!(response.bytes().await.unwrap().as_ref(), large_body(b'k'));
                response_done.store(true, Ordering::Release);
            });
    })
}

async fn assert_multiplexed_reqwest_response(
    client: &reqwest::Client,
    address: SocketAddr,
    path: &'static str,
    request_body: &'static str,
    response_byte: u8,
) {
    let response = client
        .put(format!("http://{address}{path}"))
        .body(request_body)
        .send()
        .await
        .unwrap();

    assert_eq!(response.version(), reqwest::Version::HTTP_2);
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.headers()["x-peer"], "kimojio-http2-multiplexed");
    assert_eq!(
        response.bytes().await.unwrap().as_ref(),
        large_body(response_byte)
    );
}

fn spawn_reqwest_http2_concurrent(
    address: SocketAddr,
    response_done: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let client = reqwest_http2_client();
                // Establish the pooled connection before concurrent checkout
                // so every request multiplexes over this HTTP/2 connection.
                let warmup = client
                    .get(format!("http://{address}/multiplex/warmup"))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(warmup.version(), reqwest::Version::HTTP_2);
                assert_eq!(warmup.bytes().await.unwrap(), "warmup");

                tokio::time::timeout(WAIT, async {
                    tokio::join!(
                        assert_multiplexed_reqwest_response(
                            &client,
                            address,
                            "/multiplex/alpha",
                            "request-alpha",
                            b'a',
                        ),
                        assert_multiplexed_reqwest_response(
                            &client,
                            address,
                            "/multiplex/bravo",
                            "request-bravo",
                            b'b',
                        ),
                        assert_multiplexed_reqwest_response(
                            &client,
                            address,
                            "/multiplex/charlie",
                            "request-charlie",
                            b'c',
                        ),
                        assert_multiplexed_reqwest_response(
                            &client,
                            address,
                            "/multiplex/delta",
                            "request-delta",
                            b'd',
                        ),
                    );
                })
                .await
                .expect("concurrent reqwest HTTP/2 requests timed out");
                response_done.store(true, Ordering::Release);
            });
    })
}

fn spawn_reqwest_http2_head_of_line(
    address: SocketAddr,
    slow_started: Arc<AtomicBool>,
    release_slow: Arc<AtomicBool>,
    ready_response_done: Arc<AtomicBool>,
    response_done: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let client = reqwest_http2_client();
                // Establish the pooled connection before the slow and ready
                // streams are started.
                let warmup = client
                    .get(format!("http://{address}/barrier/warmup"))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(warmup.version(), reqwest::Version::HTTP_2);
                assert_eq!(warmup.bytes().await.unwrap(), "warmup");

                let slow_client = client.clone();
                let slow = tokio::spawn(async move {
                    let response = slow_client
                        .get(format!("http://{address}/barrier/slow"))
                        .send()
                        .await
                        .unwrap();
                    assert_eq!(response.version(), reqwest::Version::HTTP_2);
                    assert_eq!(response.status(), reqwest::StatusCode::OK);
                    response.bytes().await.unwrap()
                });

                tokio::time::timeout(WAIT, async {
                    while !slow_started.load(Ordering::Acquire) {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("slow HTTP/2 handler did not start");

                let ready = client
                    .get(format!("http://{address}/barrier/ready"))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(ready.version(), reqwest::Version::HTTP_2);
                assert_eq!(ready.status(), reqwest::StatusCode::OK);
                assert_eq!(ready.bytes().await.unwrap(), "ready");
                ready_response_done.store(true, Ordering::Release);

                let slow_body = slow.await.expect("slow reqwest task panicked");
                assert_eq!(slow_body, "slow");
                assert!(release_slow.load(Ordering::Acquire));
                response_done.store(true, Ordering::Release);
            });
    })
}

async fn wait_for_atomic_signal(signal: &AtomicBool) {
    while !signal.load(Ordering::Acquire) {
        operations::sleep(Duration::from_millis(1))
            .await
            .expect("signal wait failed");
    }
}

fn kimojio_response(
    status: StatusCode,
    peer: &'static str,
    body: impl Into<Body>,
) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("x-peer", peer)
        .header("set-cookie", "first=1")
        .header("set-cookie", "second=2")
        .body(body.into())
        .unwrap()
}

#[kimojio::test]
async fn reqwest_on_tokio_to_kimojio_server_http1() {
    let server = Server::bind((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
    let peer = spawn_reqwest_http1(server.local_addr());
    let cancellation = Rc::new(CancellationToken::new());
    let cancel_from_handler = Rc::clone(&cancellation);

    let serve_result = operations::timeout_at(
        Instant::now() + WAIT,
        server.serve(
            move |request| {
                let cancellation = Rc::clone(&cancel_from_handler);
                async move {
                    assert_eq!(request.version(), Version::HTTP_11);
                    assert_eq!(request.method(), Method::POST);
                    assert_eq!(request.uri(), "/from-reqwest?h1=1");
                    assert_eq!(request.headers()["x-client"], "reqwest");
                    assert_eq!(
                        header_values(request.headers(), "x-duplicate"),
                        ["one", "two"]
                    );
                    assert_eq!(request.body().as_bytes(), b"reqwest-http1");
                    cancellation.cancel();
                    kimojio_response(
                        StatusCode::CREATED,
                        "kimojio-http1",
                        "kimojio-response-http1",
                    )
                }
            },
            cancellation,
        ),
    )
    .await;

    peer.join().expect("reqwest HTTP/1 peer thread panicked");
    serve_result
        .expect("Kimojio HTTP/1 server timed out")
        .unwrap();
}

#[kimojio::test]
async fn reqwest_reassembles_chunked_streaming_response_and_buffered_stays_length_framed() {
    let server = Server::bind((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
    let response_done = Arc::new(AtomicBool::new(false));
    let peer = spawn_reqwest_streaming_http1(server.local_addr(), Arc::clone(&response_done));
    let cancellation = Rc::new(CancellationToken::new());
    let cancel_after_response = Rc::clone(&cancellation);
    operations::spawn_task(async move {
        wait_for_atomic_signal(&response_done).await;
        cancel_after_response.cancel();
    });

    let serve_result = operations::timeout_at(
        Instant::now() + WAIT,
        server.serve(
            move |request| async move {
                match request.uri().path() {
                    "/streamed-response" => {
                        Response::new(Body::from_chunks(futures::stream::iter([
                            b"streamed-".to_vec(),
                            b"response-".to_vec(),
                            b"http1".to_vec(),
                        ])))
                    }
                    "/buffered-response" => {
                        Response::new(Body::new(b"buffered-response-http1".to_vec()))
                    }
                    path => panic!("unexpected streaming response path: {path}"),
                }
            },
            cancellation,
        ),
    )
    .await;

    peer.join().expect("streaming HTTP/1 reqwest peer panicked");
    serve_result
        .expect("streaming HTTP/1 server timed out")
        .unwrap();
}

#[kimojio::test]
async fn buffered_http1_response_wire_remains_byte_exact() {
    let server = Server::bind((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
    let address = server.local_addr();
    let response_done = Arc::new(AtomicBool::new(false));
    let peer_done = Arc::clone(&response_done);
    let peer = thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        stream
            .write_all(
                b"GET /buffered-wire HTTP/1.1\r\n\
                  host: localhost\r\n\
                  connection: close\r\n\r\n",
            )
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        peer_done.store(true, Ordering::Release);
        response
    });
    let cancellation = Rc::new(CancellationToken::new());
    let cancel_after_response = Rc::clone(&cancellation);
    operations::spawn_task(async move {
        wait_for_atomic_signal(&response_done).await;
        cancel_after_response.cancel();
    });

    operations::timeout_at(
        Instant::now() + WAIT,
        server.serve(
            move |request| async move {
                assert_eq!(request.uri().path(), "/buffered-wire");
                Response::new(Body::new(b"buffered".to_vec()))
            },
            cancellation,
        ),
    )
    .await
    .expect("buffered wire server timed out")
    .unwrap();

    assert_eq!(
        peer.join().expect("buffered wire peer panicked"),
        b"HTTP/1.1 200 OK\r\n\
          content-length: 8\r\n\
          connection: close\r\n\r\n\
          buffered"
    );
}

#[kimojio::test]
async fn reqwest_http1_reuses_one_connection_for_sequential_requests() {
    let config = ServerConfig::new()
        .set_limits(Limits::new().set_max_requests_per_connection(4))
        .set_max_connections(1)
        .set_connection_io_timeout(Duration::from_secs(5));
    let server = Server::bind_with_config((Ipv4Addr::LOCALHOST, 0).into(), config)
        .await
        .unwrap();
    let peer = spawn_reqwest_http1_reuse(server.local_addr());
    let cancellation = Rc::new(CancellationToken::new());
    let seen = Rc::new(RefCell::new(Vec::new()));
    let observed = Rc::clone(&seen);
    let cancel_from_handler = Rc::clone(&cancellation);

    let serve_result = operations::timeout_at(
        Instant::now() + WAIT,
        server.serve(
            move |request| {
                let path = request.uri().path().to_owned();
                observed.borrow_mut().push(path.clone());
                let done = observed.borrow().len() == 2;
                let cancellation = Rc::clone(&cancel_from_handler);
                async move {
                    if done {
                        cancellation.cancel();
                    }
                    Response::new(Body::from(path.rsplit('/').next().unwrap()))
                }
            },
            cancellation,
        ),
    )
    .await;

    peer.join()
        .expect("sequential reqwest HTTP/1 peer thread panicked");
    serve_result
        .expect("Kimojio HTTP/1 reuse server timed out")
        .unwrap();
    assert_eq!(*seen.borrow(), ["/reuse/one", "/reuse/two"]);
}

#[kimojio::test]
async fn reqwest_on_tokio_to_kimojio_server_http2_prior_knowledge() {
    let server = Server::bind((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
    let response_done = Arc::new(AtomicBool::new(false));
    let peer = spawn_reqwest_http2(server.local_addr(), Arc::clone(&response_done));
    let cancellation = Rc::new(CancellationToken::new());
    let cancel_after_response = Rc::clone(&cancellation);
    operations::spawn_task(async move {
        while !response_done.load(Ordering::Acquire) {
            let _ = operations::sleep(Duration::from_millis(1)).await;
        }
        cancel_after_response.cancel();
    });

    let serve_result = operations::timeout_at(
        Instant::now() + WAIT,
        server.serve(
            move |request| async move {
                assert_eq!(request.version(), Version::HTTP_2);
                assert_eq!(request.method(), Method::PUT);
                assert_eq!(request.uri(), "/from-reqwest-h2");
                assert_eq!(request.headers()["x-client"], "reqwest");
                assert_eq!(
                    header_values(request.headers(), "x-duplicate"),
                    ["one", "two"]
                );
                assert_eq!(request.body().as_bytes(), large_body(b'r'));
                kimojio_response(StatusCode::ACCEPTED, "kimojio-http2", large_body(b'k'))
            },
            cancellation,
        ),
    )
    .await;

    peer.join().expect("reqwest HTTP/2 peer thread panicked");
    serve_result
        .expect("Kimojio HTTP/2 server timed out")
        .unwrap();
}

#[kimojio::test]
async fn reqwest_reassembles_http2_streaming_response_without_content_length() {
    let server = Server::bind((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
    let response_done = Arc::new(AtomicBool::new(false));
    let peer = spawn_reqwest_streaming_http2(server.local_addr(), Arc::clone(&response_done));
    let cancellation = Rc::new(CancellationToken::new());
    let cancel_after_response = Rc::clone(&cancellation);
    operations::spawn_task(async move {
        wait_for_atomic_signal(&response_done).await;
        cancel_after_response.cancel();
    });

    let serve_result = operations::timeout_at(
        Instant::now() + WAIT,
        server.serve(
            move |request| async move {
                assert_eq!(request.version(), Version::HTTP_2);
                assert_eq!(request.uri().path(), "/streamed-response-h2");
                Response::new(Body::from_chunks(futures::stream::iter([
                    b"streamed-".to_vec(),
                    b"response-".to_vec(),
                    b"http2".to_vec(),
                ])))
            },
            cancellation,
        ),
    )
    .await;

    peer.join().expect("streaming HTTP/2 reqwest peer panicked");
    serve_result
        .expect("streaming HTTP/2 server timed out")
        .unwrap();
}

#[kimojio::test]
async fn reqwest_http2_multiplexes_concurrent_requests_on_one_connection() {
    let server = Server::bind((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
    let response_done = Arc::new(AtomicBool::new(false));
    let peer = spawn_reqwest_http2_concurrent(server.local_addr(), Arc::clone(&response_done));
    let cancellation = Rc::new(CancellationToken::new());
    let cancel_after_response = Rc::clone(&cancellation);
    operations::spawn_task(async move {
        wait_for_atomic_signal(&response_done).await;
        cancel_after_response.cancel();
    });

    let serve_result = operations::timeout_at(
        Instant::now() + WAIT,
        server.serve_streaming(
            move |mut request| async move {
                assert_eq!(request.version(), Version::HTTP_2);
                let path = request.uri().path().to_owned();
                let mut body = Vec::new();
                while let Some(chunk) = request.body_mut().next_chunk().await.unwrap() {
                    body.extend_from_slice(&chunk);
                }
                match path.as_str() {
                    "/multiplex/warmup" => {
                        assert_eq!(request.method(), Method::GET);
                        assert!(body.is_empty());
                        kimojio_response(StatusCode::OK, "kimojio-http2-multiplexed", "warmup")
                    }
                    path => {
                        assert_eq!(request.method(), Method::PUT);
                        let (expected_request, response_byte) = match path {
                            "/multiplex/alpha" => (b"request-alpha".as_slice(), b'a'),
                            "/multiplex/bravo" => (b"request-bravo".as_slice(), b'b'),
                            "/multiplex/charlie" => (b"request-charlie".as_slice(), b'c'),
                            "/multiplex/delta" => (b"request-delta".as_slice(), b'd'),
                            _ => panic!("unexpected multiplexed request path: {path}"),
                        };
                        assert_eq!(body, expected_request);
                        kimojio_response(
                            StatusCode::OK,
                            "kimojio-http2-multiplexed",
                            large_body(response_byte),
                        )
                    }
                }
            },
            cancellation,
        ),
    )
    .await;

    peer.join()
        .expect("concurrent reqwest HTTP/2 peer thread panicked");
    serve_result
        .expect("Kimojio HTTP/2 multiplexing server timed out")
        .unwrap();
}

#[kimojio::test]
async fn reqwest_http2_ready_response_is_not_blocked_by_slow_sibling() {
    let server = Server::bind((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
    let slow_started = Arc::new(AtomicBool::new(false));
    let release_slow = Arc::new(AtomicBool::new(false));
    let ready_response_done = Arc::new(AtomicBool::new(false));
    let response_done = Arc::new(AtomicBool::new(false));
    let peer = spawn_reqwest_http2_head_of_line(
        server.local_addr(),
        Arc::clone(&slow_started),
        Arc::clone(&release_slow),
        Arc::clone(&ready_response_done),
        Arc::clone(&response_done),
    );
    let cancellation = Rc::new(CancellationToken::new());
    let serve_cancellation = Rc::clone(&cancellation);
    let handler_slow_started = Arc::clone(&slow_started);
    let handler_release_slow = Arc::clone(&release_slow);
    let serve_task = operations::spawn_task(server.serve(
        move |request| {
            let slow_started = Arc::clone(&handler_slow_started);
            let release_slow = Arc::clone(&handler_release_slow);
            async move {
                assert_eq!(request.version(), Version::HTTP_2);
                assert_eq!(request.method(), Method::GET);
                match request.uri().path() {
                    "/barrier/warmup" => {
                        kimojio_response(StatusCode::OK, "kimojio-http2-barrier", "warmup")
                    }
                    "/barrier/slow" => {
                        slow_started.store(true, Ordering::Release);
                        wait_for_atomic_signal(&release_slow).await;
                        kimojio_response(StatusCode::OK, "kimojio-http2-barrier", "slow")
                    }
                    "/barrier/ready" => {
                        kimojio_response(StatusCode::OK, "kimojio-http2-barrier", "ready")
                    }
                    path => panic!("unexpected barrier request path: {path}"),
                }
            }
        },
        serve_cancellation,
    ));

    operations::timeout_at(Instant::now() + WAIT, wait_for_atomic_signal(&slow_started))
        .await
        .expect("slow HTTP/2 handler did not start");
    let ready_while_slow_was_blocked = operations::timeout_at(
        Instant::now() + WAIT / 2,
        wait_for_atomic_signal(&ready_response_done),
    )
    .await;
    release_slow.store(true, Ordering::Release);
    operations::timeout_at(
        Instant::now() + WAIT,
        wait_for_atomic_signal(&response_done),
    )
    .await
    .expect("reqwest did not receive both HTTP/2 responses");
    cancellation.cancel();
    operations::timeout_at(Instant::now() + WAIT, serve_task)
        .await
        .expect("multiplexed server did not shut down")
        .unwrap()
        .unwrap();
    peer.join()
        .expect("head-of-line reqwest HTTP/2 peer thread panicked");
    ready_while_slow_was_blocked.expect("ready HTTP/2 response was blocked by a slow sibling");
}

#[kimojio::test]
async fn kimojio_server_enforces_http2_request_content_length() {
    type Case = (
        &'static str,
        Vec<(&'static [u8], &'static [u8])>,
        &'static [u8],
        bool,
    );
    let cases: Vec<Case> = vec![
        ("exact", vec![(b"content-length", b"3")], b"abc", true),
        ("exact empty", vec![(b"content-length", b"0")], b"", true),
        (
            "duplicate equivalent",
            vec![(b"content-length", b"3"), (b"content-length", b"3")],
            b"abc",
            true,
        ),
        (
            "comma-list equivalent",
            vec![(b"content-length", b"3, 3")],
            b"abc",
            true,
        ),
        ("too short", vec![(b"content-length", b"4")], b"abc", false),
        ("too long", vec![(b"content-length", b"2")], b"abc", false),
        (
            "malformed",
            vec![(b"content-length", b"three")],
            b"abc",
            false,
        ),
        (
            "conflicting",
            vec![(b"content-length", b"2"), (b"content-length", b"3")],
            b"abc",
            false,
        ),
    ];

    for (name, headers, body, valid) in cases {
        let server = Server::bind((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
        let address = server.local_addr();
        let request = h2_request_bytes(&headers, body);
        let reset_code = Arc::new(AtomicU32::new(NO_RESET));
        let peer_reset_code = Arc::clone(&reset_code);
        let peer = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream.set_read_timeout(Some(WAIT)).unwrap();
            stream.write_all(&request).unwrap();
            if valid {
                let mut response = Vec::new();
                stream.read_to_end(&mut response).unwrap();
            } else if let Some(code) = read_until_rst_stream(&mut stream, 1) {
                peer_reset_code.store(code, Ordering::Release);
            }
        });

        let cancellation = Rc::new(CancellationToken::new());
        let cancel_from_handler = Rc::clone(&cancellation);
        let handler_called = Rc::new(RefCell::new(false));
        let called = Rc::clone(&handler_called);
        let protocol_error = Rc::new(RefCell::new(false));
        let reported = Rc::clone(&protocol_error);
        let cancel_from_error = Rc::clone(&cancellation);
        let cancel_from_test = Rc::clone(&cancellation);
        let expected_body = body;
        let serve_task = operations::spawn_task(server.serve_with_error_handler(
            move |request| {
                *called.borrow_mut() = true;
                let cancellation = Rc::clone(&cancel_from_handler);
                async move {
                    assert_eq!(request.body().as_bytes(), expected_body);
                    cancellation.cancel();
                    Response::new(Body::empty())
                }
            },
            cancellation,
            move |error| {
                if matches!(
                    error,
                    ServeError::Connection(HttpError::Protocol(ref error))
                        if error.kind() == ProtocolErrorKind::InvalidContentLength
                ) {
                    *reported.borrow_mut() = true;
                }
                cancel_from_error.cancel();
            },
        ));

        if !valid {
            // Resetting the stream leaves the connection open, so nothing else
            // will stop the server once the peer has seen the reset.
            operations::timeout_at(Instant::now() + WAIT, async {
                while reset_code.load(Ordering::Acquire) == NO_RESET {
                    operations::sleep(Duration::from_millis(1)).await.unwrap();
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{name}: stream reset was not observed"));
            cancel_from_test.cancel();
        }

        operations::timeout_at(Instant::now() + WAIT, serve_task)
            .await
            .unwrap_or_else(|_| panic!("{name}: server timed out"))
            .unwrap()
            .unwrap();
        peer.join()
            .unwrap_or_else(|_| panic!("{name}: peer thread panicked"));

        assert_eq!(*handler_called.borrow(), valid, "{name}");
        // A malformed request is a stream error (RFC 9113 section 8.1.1), so it
        // resets its own stream instead of failing the whole connection.
        assert!(!*protocol_error.borrow(), "{name}");
        let observed = reset_code.load(Ordering::Acquire);
        if valid {
            assert_eq!(observed, NO_RESET, "{name}");
        } else {
            assert_eq!(observed, H2_PROTOCOL_ERROR, "{name}");
        }
    }
}

#[kimojio::test]
async fn kimojio_client_enforces_http2_response_content_length() {
    type Case = (
        &'static str,
        Method,
        u16,
        Vec<H2HeaderField>,
        &'static [u8],
        bool,
    );
    let cases: Vec<Case> = vec![
        (
            "exact",
            Method::GET,
            200,
            vec![H2HeaderField::new(b"content-length", b"3")],
            b"abc",
            true,
        ),
        (
            "exact empty",
            Method::GET,
            200,
            vec![H2HeaderField::new(b"content-length", b"0")],
            b"",
            true,
        ),
        (
            "duplicate equivalent",
            Method::GET,
            200,
            vec![
                H2HeaderField::new(b"content-length", b"3"),
                H2HeaderField::new(b"content-length", b"3"),
            ],
            b"abc",
            true,
        ),
        (
            "comma-list equivalent",
            Method::GET,
            200,
            vec![H2HeaderField::new(b"content-length", b"3, 3")],
            b"abc",
            true,
        ),
        (
            "too short",
            Method::GET,
            200,
            vec![H2HeaderField::new(b"content-length", b"4")],
            b"abc",
            false,
        ),
        (
            "too long",
            Method::GET,
            200,
            vec![H2HeaderField::new(b"content-length", b"2")],
            b"abc",
            false,
        ),
        (
            "malformed",
            Method::GET,
            200,
            vec![H2HeaderField::new(b"content-length", b"three")],
            b"",
            false,
        ),
        (
            "conflicting",
            Method::GET,
            200,
            vec![
                H2HeaderField::new(b"content-length", b"2"),
                H2HeaderField::new(b"content-length", b"3"),
            ],
            b"",
            false,
        ),
        (
            "HEAD metadata length",
            Method::HEAD,
            200,
            vec![H2HeaderField::new(b"content-length", b"99")],
            b"",
            true,
        ),
        (
            "304 metadata length",
            Method::GET,
            304,
            vec![H2HeaderField::new(b"content-length", b"99")],
            b"",
            true,
        ),
        (
            "204 forbids content-length",
            Method::GET,
            204,
            vec![H2HeaderField::new(b"content-length", b"0")],
            b"",
            false,
        ),
    ];

    for (name, method, status, headers, body, valid) in cases {
        let (address, peer) = spawn_h2_response(status, headers, body);
        let result = operations::timeout_at(
            Instant::now() + WAIT,
            Client::new()
                .request(method, format!("http://{address}/length"))
                .version(Version::HTTP_2)
                .send(),
        )
        .await
        .unwrap_or_else(|_| panic!("{name}: client timed out"));
        peer.join()
            .unwrap_or_else(|_| panic!("{name}: peer thread panicked"));

        if valid {
            assert_eq!(
                result
                    .unwrap_or_else(|error| panic!("{name}: {error}"))
                    .body()
                    .as_bytes(),
                body,
                "{name}"
            );
        } else {
            assert!(
                matches!(
                    result,
                    Err(HttpError::Protocol(ref error))
                        if error.kind() == ProtocolErrorKind::InvalidContentLength
                ),
                "{name}: {result:?}"
            );
        }
    }
}

/// A server that closes while unread request bytes are still queued makes the
/// kernel send RST, and RST discards whatever the peer has not read yet. That
/// silently throws away the GOAWAY frame explaining the failure, so the peer
/// sees only a connection reset.
#[kimojio::test]
async fn http2_reports_goaway_before_closing_on_a_protocol_error() {
    let server = Server::bind((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
    let address = server.local_addr();

    let mut request = H2Client::default().connection_preface();
    // A frame larger than the advertised maximum is a connection error.
    request.extend_from_slice(&[0, 0x40, 1, 0, 0, 0, 0, 0, 0]);

    let peer = thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        stream.write_all(&request).unwrap();
        // Far more than one read can drain, so the server is guaranteed to
        // still hold unread bytes when it closes. That is what makes the
        // kernel send a reset instead of an orderly shutdown.
        let _ = stream.write_all(&[0; 512 * 1024]);
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .expect("the peer must reach a clean end of stream, not a connection reset");
        response
    });

    let cancellation = Rc::new(CancellationToken::new());
    let cancel_from_error = Rc::clone(&cancellation);
    operations::timeout_at(
        Instant::now() + WAIT,
        server.serve_with_error_handler(
            move |_request| async move { Response::new(Body::empty()) },
            cancellation,
            move |_error| cancel_from_error.cancel(),
        ),
    )
    .await
    .expect("server timed out")
    .unwrap();

    let response = peer.join().expect("peer thread panicked");
    let goaway = response
        .windows(4)
        .any(|window| window[3] == 0x07 && window[0] == 0 && window[1] == 0);
    assert!(
        goaway,
        "the peer must receive a GOAWAY frame explaining the failure, got {response:?}"
    );
}

/// An HTTP/2 peer may reuse a connection for sequential requests after each
/// response completes.
#[kimojio::test]
async fn http2_serves_two_requests_on_one_connection() {
    let server = Server::bind((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
    let address = server.local_addr();

    let mut client = H2Client::default();
    let mut first = client.connection_preface();
    let (_, commit) = client
        .open_stream_with_raw_headers("GET", "http", "localhost", "/first", &[], true)
        .unwrap();
    first.extend_from_slice(&take_client_block(&mut client, commit));
    let (_, commit) = client
        .open_stream_with_raw_headers("GET", "http", "localhost", "/second", &[], true)
        .unwrap();
    let second = take_client_block(&mut client, commit);

    let peer = thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        stream.write_all(&first).unwrap();
        let mut buffer = [0; 1024];
        // Complete one exchange before sending the next to pin sequential
        // reuse of the same connection.
        assert!(stream.read(&mut buffer).unwrap() > 0);
        stream.write_all(&second).unwrap();
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest).unwrap();
    });

    let cancellation = Rc::new(CancellationToken::new());
    let targets = Rc::new(RefCell::new(Vec::new()));
    let seen = Rc::clone(&targets);
    let cancel_from_handler = Rc::clone(&cancellation);
    let cancel_from_error = Rc::clone(&cancellation);
    operations::timeout_at(
        Instant::now() + WAIT,
        server.serve_with_error_handler(
            move |request| {
                seen.borrow_mut().push(request.uri().path().to_string());
                let cancellation = Rc::clone(&cancel_from_handler);
                let done = seen.borrow().len() == 2;
                async move {
                    if done {
                        cancellation.cancel();
                    }
                    Response::new(Body::empty())
                }
            },
            cancellation,
            move |error| {
                cancel_from_error.cancel();
                panic!("the connection must survive between requests: {error}");
            },
        ),
    )
    .await
    .expect("server timed out")
    .unwrap();

    peer.join().expect("peer thread panicked");
    assert_eq!(*targets.borrow(), vec!["/first", "/second"]);
}

/// Concurrent streaming uploads must all make progress. Alternating protocol
/// progress with one body chunk clears the pump's body preference, and the
/// pump's idle wait only wakes for exchanges that still need a chunk - never
/// for a sibling already holding bytes to write. Without a final write attempt
/// before parking, every sibling that had a chunk ready stalls forever.
#[kimojio::test]
async fn concurrent_hyper_http2_streaming_uploads_all_make_progress() {
    const CHUNK: usize = 1024;
    const CHUNKS: usize = 8;

    let peer = spawn_concurrent_upload_hyper_http2(CHUNK * CHUNKS);
    let client = Client::new();
    let base = format!("http://{}", peer.address);
    let upload = |index: usize| {
        let byte = b'a' + u8::try_from(index).unwrap();
        client
            .request(Method::PUT, format!("{base}/upload/{index}"))
            .version(Version::HTTP_2)
            .body(Body::from_chunks(futures::stream::iter(
                (0..CHUNKS).map(move |_| vec![byte; CHUNK]),
            )))
            .send()
    };

    let responses = operations::timeout_at(Instant::now() + Duration::from_secs(8), async {
        futures::join!(upload(0), upload(1), upload(2), upload(3))
    })
    .await
    .expect("a concurrent streaming upload stalled holding a ready chunk");

    for (index, response) in [responses.0, responses.1, responses.2, responses.3]
        .into_iter()
        .enumerate()
    {
        assert_eq!(
            response.unwrap().body().as_bytes(),
            format!("uploaded-{index}").as_bytes()
        );
    }
    drop(client);
    peer.stop();
}
