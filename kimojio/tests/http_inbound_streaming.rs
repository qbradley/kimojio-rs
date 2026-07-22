// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(feature = "http")]

use std::cell::Cell;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use kimojio::http::{
    Body, Client, ClientConfig, Error as HttpError, ExpectContinueDecision, Limits, Response,
    ServeError, Server, ServerConfig,
};
use kimojio::{CancellationToken, operations};

const WAIT: Duration = Duration::from_secs(10);
const LARGE_BODY_LEN: usize = 4 * 1024 * 1024;
const BACKPRESSURE_BODY_LEN: usize = 32 * 1024 * 1024;

fn read_head(stream: &mut TcpStream) -> (Vec<u8>, Vec<u8>) {
    let mut bytes = Vec::new();
    loop {
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let remainder = bytes.split_off(end + 4);
            return (bytes, remainder);
        }
        let mut chunk = [0; 4096];
        let read = stream.read(&mut chunk).unwrap();
        assert_ne!(read, 0, "peer closed before completing the HTTP head");
        bytes.extend_from_slice(&chunk[..read]);
    }
}

#[kimojio::test]
async fn server_handler_observes_request_chunk_before_peer_finishes_sending() {
    let server = Server::bind((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
    let address = server.local_addr();
    let first_observed = Arc::new(AtomicBool::new(false));
    let sent_complete = Arc::new(AtomicBool::new(false));
    let peer_observed = Arc::clone(&first_observed);
    let peer_complete = Arc::clone(&sent_complete);
    let peer = thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        stream.set_write_timeout(Some(WAIT)).unwrap();
        write!(
            stream,
            "POST /early HTTP/1.1\r\nhost: localhost\r\ncontent-length: {LARGE_BODY_LEN}\r\nconnection: close\r\n\r\n"
        )
        .unwrap();
        stream.write_all(&vec![b'a'; 4096]).unwrap();
        let deadline = Instant::now() + WAIT;
        while !peer_observed.load(Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "handler did not observe the early chunk"
            );
            thread::sleep(Duration::from_millis(1));
        }
        stream
            .write_all(&vec![b'a'; LARGE_BODY_LEN - 4096])
            .unwrap();
        peer_complete.store(true, Ordering::Release);
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        response
    });

    let cancellation = Rc::new(CancellationToken::new());
    let cancel_from_handler = Rc::clone(&cancellation);
    let observed = Arc::clone(&first_observed);
    let complete = Arc::clone(&sent_complete);
    operations::timeout_at(
        Instant::now() + WAIT,
        server.serve_streaming(
            move |mut request| {
                let cancellation = Rc::clone(&cancel_from_handler);
                let observed = Arc::clone(&observed);
                let complete = Arc::clone(&complete);
                async move {
                    assert!(request.body().is_streaming());
                    let mut received = 0;
                    while let Some(chunk) = request.body_mut().next_chunk().await.unwrap() {
                        received += chunk.len();
                        if received != 0 && !observed.swap(true, Ordering::AcqRel) {
                            assert!(
                                !complete.load(Ordering::Acquire),
                                "the complete request was buffered before handler invocation"
                            );
                        }
                    }
                    assert_eq!(received, LARGE_BODY_LEN);
                    cancellation.cancel();
                    Response::new(Body::from("accepted"))
                }
            },
            cancellation,
        ),
    )
    .await
    .expect("streaming request server timed out")
    .unwrap();

    let response = peer.join().expect("request peer panicked");
    assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert!(response.ends_with(b"accepted"));
}

#[kimojio::test]
async fn slow_server_consumer_stops_transport_drain() {
    let limits = Limits::new()
        .set_max_header_bytes(4096)
        .set_read_buffer_bytes(4096);
    let server = Server::bind_with_config(
        (Ipv4Addr::LOCALHOST, 0).into(),
        ServerConfig::new().set_limits(limits),
    )
    .await
    .unwrap();
    let address = server.local_addr();
    let sent = Arc::new(AtomicUsize::new(0));
    let writer_blocked = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let peer_sent = Arc::clone(&sent);
    let peer_blocked = Arc::clone(&writer_blocked);
    let peer_release = Arc::clone(&release);
    let peer = thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        stream.set_write_timeout(Some(WAIT)).unwrap();
        write!(
            stream,
            "POST /backpressure HTTP/1.1\r\nhost: localhost\r\ncontent-length: {BACKPRESSURE_BODY_LEN}\r\nconnection: close\r\n\r\n"
        )
        .unwrap();
        stream.set_nonblocking(true).unwrap();
        let bytes = vec![b'p'; BACKPRESSURE_BODY_LEN];
        let deadline = Instant::now() + WAIT;
        let mut offset = 0;
        while offset < bytes.len() {
            match stream.write(&bytes[offset..]) {
                Ok(0) => panic!("server closed while the request was being written"),
                Ok(amount) => {
                    offset += amount;
                    peer_sent.store(offset, Ordering::Release);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    peer_blocked.store(true, Ordering::Release);
                    assert!(
                        Instant::now() < deadline,
                        "request writer remained blocked after consumer release"
                    );
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("request write failed: {error}"),
            }
            if peer_release.load(Ordering::Acquire) {
                thread::yield_now();
            }
        }
        stream.set_nonblocking(false).unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        response
    });

    let cancellation = Rc::new(CancellationToken::new());
    let cancel_from_handler = Rc::clone(&cancellation);
    let observed_sent = Arc::clone(&sent);
    let observed_blocked = Arc::clone(&writer_blocked);
    let release_writer = Arc::clone(&release);
    operations::timeout_at(
        Instant::now() + WAIT,
        server.serve_streaming(
            move |mut request| {
                let cancellation = Rc::clone(&cancel_from_handler);
                let sent = Arc::clone(&observed_sent);
                let blocked = Arc::clone(&observed_blocked);
                let release = Arc::clone(&release_writer);
                async move {
                    let first = request.body_mut().next_chunk().await.unwrap().unwrap();
                    assert!(!first.is_empty());

                    let deadline = Instant::now() + WAIT;
                    let mut stable = 0;
                    let mut previous = sent.load(Ordering::Acquire);
                    while stable < 5 {
                        assert!(
                            Instant::now() < deadline,
                            "writer never became backpressured"
                        );
                        operations::sleep(Duration::from_millis(10)).await.unwrap();
                        let current = sent.load(Ordering::Acquire);
                        stable = if blocked.load(Ordering::Acquire) && current == previous {
                            stable + 1
                        } else {
                            0
                        };
                        previous = current;
                    }
                    assert!(
                        previous < BACKPRESSURE_BODY_LEN,
                        "transport drained the complete body ahead of the consumer"
                    );
                    operations::sleep(Duration::from_millis(50)).await.unwrap();
                    assert_eq!(
                        sent.load(Ordering::Acquire),
                        previous,
                        "transport resumed draining while the consumer was paused"
                    );

                    release.store(true, Ordering::Release);
                    let mut received = first.len();
                    while let Some(chunk) = request.body_mut().next_chunk().await.unwrap() {
                        received += chunk.len();
                    }
                    assert_eq!(received, BACKPRESSURE_BODY_LEN);
                    cancellation.cancel();
                    Response::new(Body::from("done"))
                }
            },
            cancellation,
        ),
    )
    .await
    .expect("backpressure server timed out")
    .unwrap();

    let response = peer.join().expect("backpressure peer panicked");
    assert!(response.ends_with(b"done"));
}

#[kimojio::test]
async fn client_observes_response_chunk_before_peer_finishes_sending() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let first_observed = Arc::new(AtomicBool::new(false));
    let sent_complete = Arc::new(AtomicBool::new(false));
    let peer_observed = Arc::clone(&first_observed);
    let peer_complete = Arc::clone(&sent_complete);
    let peer = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        stream.set_write_timeout(Some(WAIT)).unwrap();
        let (head, remainder) = read_head(&mut stream);
        assert!(head.starts_with(b"GET /early HTTP/1.1\r\n"));
        assert!(remainder.is_empty());
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-length: {LARGE_BODY_LEN}\r\nconnection: close\r\n\r\n"
        )
        .unwrap();
        stream.write_all(&vec![b'b'; 4096]).unwrap();
        let deadline = Instant::now() + WAIT;
        while !peer_observed.load(Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "client did not observe the early chunk"
            );
            thread::sleep(Duration::from_millis(1));
        }
        stream
            .write_all(&vec![b'b'; LARGE_BODY_LEN - 4096])
            .unwrap();
        peer_complete.store(true, Ordering::Release);
    });

    let client = Client::new();
    let mut response = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .get(format!("http://{address}/early"))
            .send_streaming(),
    )
    .await
    .expect("streaming response head timed out")
    .unwrap();
    assert!(response.body().is_streaming());
    let first = response.body_mut().next_chunk().await.unwrap().unwrap();
    assert!(first.iter().all(|byte| *byte == b'b'));
    assert!(
        !sent_complete.load(Ordering::Acquire),
        "the complete response was buffered before send_streaming returned"
    );
    first_observed.store(true, Ordering::Release);
    let mut received = first.len();
    while let Some(chunk) = response.body_mut().next_chunk().await.unwrap() {
        assert!(chunk.iter().all(|byte| *byte == b'b'));
        received += chunk.len();
    }
    assert_eq!(received, LARGE_BODY_LEN);
    peer.join().expect("response peer panicked");
}

#[kimojio::test]
async fn streaming_response_exceeds_default_buffered_body_limit() {
    const CHUNK_LEN: usize = 32 * 1024;
    const CHUNK_COUNT: usize = 288;
    const BODY_LEN: usize = CHUNK_LEN * CHUNK_COUNT;

    assert!(BODY_LEN > Limits::new().max_body_bytes());
    let server = Server::bind((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
    let address = server.local_addr();
    let cancellation = Rc::new(CancellationToken::new());
    let serve_task = operations::spawn_task(server.serve(
        |_| async {
            Response::new(Body::from_chunks(futures::stream::iter(
                (0..CHUNK_COUNT).map(|_| vec![b's'; CHUNK_LEN]),
            )))
        },
        Rc::clone(&cancellation),
    ));

    let mut response = operations::timeout_at(
        Instant::now() + WAIT,
        Client::new()
            .get(format!("http://{address}/large-stream"))
            .send_streaming(),
    )
    .await
    .expect("large streaming response head timed out")
    .unwrap();
    let received = operations::timeout_at(Instant::now() + WAIT, async {
        let mut received = 0;
        while let Some(chunk) = response.body_mut().next_chunk().await? {
            assert!(chunk.iter().all(|byte| *byte == b's'));
            received += chunk.len();
        }
        Ok::<_, HttpError>(received)
    })
    .await;

    cancellation.cancel();
    operations::timeout_at(Instant::now() + WAIT, serve_task)
        .await
        .expect("large streaming response server did not stop")
        .unwrap()
        .unwrap();
    assert_eq!(
        received
            .expect("large streaming response stalled")
            .expect("large streaming response failed"),
        BODY_LEN
    );
}

#[kimojio::test]
async fn stalled_streaming_client_read_honors_connection_io_timeout() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let peer = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        stream.set_write_timeout(Some(WAIT)).unwrap();
        let (head, remainder) = read_head(&mut stream);
        assert!(head.starts_with(b"GET /stalled HTTP/1.1\r\n"));
        assert!(remainder.is_empty());
        stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nprefix")
            .unwrap();
        thread::sleep(Duration::from_millis(500));
    });

    let client = Client::with_config(
        ClientConfig::new().set_connection_io_timeout(Duration::from_millis(75)),
    )
    .unwrap();
    let mut response = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .get(format!("http://{address}/stalled"))
            .send_streaming(),
    )
    .await
    .expect("stalled response head timed out")
    .unwrap();
    assert_eq!(
        response.body_mut().next_chunk().await.unwrap().unwrap(),
        b"prefix"
    );
    let result = operations::timeout_at(
        Instant::now() + Duration::from_millis(250),
        response.body_mut().next_chunk(),
    )
    .await;
    drop(response);
    peer.join().expect("stalled response peer panicked");
    let error = result
        .expect("streaming response read ignored its configured deadline")
        .unwrap_err();
    assert!(
        matches!(
            error,
            HttpError::Io(kimojio::Errno::TIME) | HttpError::Io(kimojio::Errno::TIMEDOUT)
        ),
        "unexpected streaming response timeout: {error}"
    );
}

#[kimojio::test]
async fn streaming_handler_composes_with_expect_continue_hook() {
    let server = Server::bind((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
    let address = server.local_addr();
    let peer = thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        stream.set_write_timeout(Some(WAIT)).unwrap();
        stream
            .write_all(
                b"POST /expect-stream HTTP/1.1\r\n\
                  host: localhost\r\n\
                  content-length: 12\r\n\
                  expect: 100-continue\r\n\
                  connection: close\r\n\r\n",
            )
            .unwrap();
        let (interim, remainder) = read_head(&mut stream);
        assert_eq!(interim, b"HTTP/1.1 100 Continue\r\n\r\n");
        assert!(remainder.is_empty());
        stream.write_all(b"request-body").unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        response
    });

    let hook_calls = Rc::new(Cell::new(0));
    let handler_calls = Rc::new(Cell::new(0));
    let observed_hooks = Rc::clone(&hook_calls);
    let observed_handlers = Rc::clone(&handler_calls);
    let hooks_seen_by_handler = Rc::clone(&hook_calls);
    let cancellation = Rc::new(CancellationToken::new());
    let cancel_from_handler = Rc::clone(&cancellation);
    operations::timeout_at(
        Instant::now() + WAIT,
        server.serve_streaming_with_expect_continue(
            move |mut request| {
                observed_handlers.set(observed_handlers.get() + 1);
                assert_eq!(hooks_seen_by_handler.get(), 1);
                let cancellation = Rc::clone(&cancel_from_handler);
                async move {
                    let mut body = Vec::new();
                    while let Some(chunk) = request.body_mut().next_chunk().await.unwrap() {
                        body.extend_from_slice(&chunk);
                    }
                    assert_eq!(body, b"request-body");
                    cancellation.cancel();
                    Response::new(Body::from("accepted"))
                }
            },
            move |head| {
                assert_eq!(head.uri(), "/expect-stream");
                observed_hooks.set(observed_hooks.get() + 1);
                ExpectContinueDecision::Continue
            },
            cancellation,
        ),
    )
    .await
    .expect("streaming Expect server timed out")
    .unwrap();

    let response = peer.join().expect("streaming Expect peer panicked");
    assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert!(response.ends_with(b"accepted"));
    assert_eq!(hook_calls.get(), 1);
    assert_eq!(handler_calls.get(), 1);
}

#[kimojio::test]
async fn buffered_server_and_client_remain_the_default() {
    let server = Server::bind((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
    let address = server.local_addr();
    let cancellation = Rc::new(CancellationToken::new());
    let cancel_from_handler = Rc::clone(&cancellation);
    let handler_called = Rc::new(Cell::new(false));
    let observed_handler = Rc::clone(&handler_called);
    let serve_task = operations::spawn_task(server.serve(
        move |request| {
            assert!(!request.body().is_streaming());
            assert_eq!(request.body().as_bytes(), b"buffered request");
            observed_handler.set(true);
            let cancellation = Rc::clone(&cancel_from_handler);
            async move {
                cancellation.cancel();
                Response::new(Body::from("buffered response"))
            }
        },
        cancellation,
    ));

    let response = operations::timeout_at(
        Instant::now() + WAIT,
        Client::new()
            .post(format!("http://{address}/buffered"))
            .body("buffered request")
            .send(),
    )
    .await
    .expect("buffered client timed out")
    .unwrap();
    assert!(!response.body().is_streaming());
    assert_eq!(response.body().as_bytes(), b"buffered response");
    operations::timeout_at(Instant::now() + WAIT, serve_task)
        .await
        .expect("buffered server did not stop")
        .unwrap()
        .unwrap();
    assert!(handler_called.get());
}

#[kimojio::test]
async fn partially_read_streaming_response_is_not_reused() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let accepts = Arc::new(AtomicUsize::new(0));
    let observed_accepts = Arc::clone(&accepts);
    let peer = thread::spawn(move || {
        let (mut first, _) = listener.accept().unwrap();
        observed_accepts.fetch_add(1, Ordering::AcqRel);
        first.set_read_timeout(Some(WAIT)).unwrap();
        first.set_write_timeout(Some(WAIT)).unwrap();
        let (head, _) = read_head(&mut first);
        assert!(head.starts_with(b"GET /partial HTTP/1.1\r\n"));
        first
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100000\r\n\r\nfirst-response-prefix")
            .unwrap();
        let mut probe = [0; 1];
        match first.read(&mut probe) {
            Ok(0) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::ConnectionReset
                ) => {}
            result => panic!("dropped streaming response did not close its connection: {result:?}"),
        }

        let (mut second, _) = listener.accept().unwrap();
        observed_accepts.fetch_add(1, Ordering::AcqRel);
        second.set_read_timeout(Some(WAIT)).unwrap();
        second.set_write_timeout(Some(WAIT)).unwrap();
        let (head, remainder) = read_head(&mut second);
        assert!(head.starts_with(b"GET /after-drop HTTP/1.1\r\n"));
        assert!(remainder.is_empty());
        second
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\nconnection: close\r\n\r\nfresh")
            .unwrap();
    });

    let client = Client::new();
    let mut partial = client
        .get(format!("http://{address}/partial"))
        .send_streaming()
        .await
        .unwrap();
    assert!(partial.body_mut().next_chunk().await.unwrap().is_some());
    drop(partial);

    let fresh = operations::timeout_at(
        Instant::now() + WAIT,
        client.get(format!("http://{address}/after-drop")).send(),
    )
    .await
    .expect("request after partial response timed out")
    .unwrap();
    assert_eq!(fresh.body().as_bytes(), b"fresh");
    peer.join().expect("retirement peer panicked");
    assert_eq!(accepts.load(Ordering::Acquire), 2);
}

#[kimojio::test]
async fn partially_read_streaming_request_retires_http1_connection() {
    let server = Server::bind((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
    let address = server.local_addr();
    let peer = thread::spawn(move || {
        let mut first = TcpStream::connect(address).unwrap();
        first.set_read_timeout(Some(WAIT)).unwrap();
        first.set_write_timeout(Some(WAIT)).unwrap();
        first
            .write_all(
                b"POST /partial-request HTTP/1.1\r\n\
                  host: localhost\r\n\
                  content-length: 100000\r\n\r\n\
                  first-request-prefix",
            )
            .unwrap();
        let mut first_response = Vec::new();
        first.read_to_end(&mut first_response).unwrap();
        assert!(first_response.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert!(first_response.ends_with(b"partial"));

        let mut second = TcpStream::connect(address).unwrap();
        second.set_read_timeout(Some(WAIT)).unwrap();
        second.set_write_timeout(Some(WAIT)).unwrap();
        second
            .write_all(
                b"GET /fresh-request HTTP/1.1\r\n\
                  host: localhost\r\n\
                  connection: close\r\n\r\n",
            )
            .unwrap();
        let mut second_response = Vec::new();
        second.read_to_end(&mut second_response).unwrap();
        second_response
    });

    let calls = Rc::new(Cell::new(0));
    let observed_calls = Rc::clone(&calls);
    let cancellation = Rc::new(CancellationToken::new());
    let cancel_from_handler = Rc::clone(&cancellation);
    operations::timeout_at(
        Instant::now() + WAIT,
        server.serve_streaming(
            move |mut request| {
                let call = observed_calls.get() + 1;
                observed_calls.set(call);
                let cancellation = Rc::clone(&cancel_from_handler);
                async move {
                    match call {
                        1 => {
                            assert_eq!(request.uri(), "/partial-request");
                            assert!(
                                request.body_mut().next_chunk().await.unwrap().is_some(),
                                "partial request did not yield its prefix"
                            );
                            Response::new(Body::from("partial"))
                        }
                        2 => {
                            assert_eq!(request.uri(), "/fresh-request");
                            assert!(request.body_mut().next_chunk().await.unwrap().is_none());
                            cancellation.cancel();
                            Response::new(Body::from("fresh"))
                        }
                        _ => panic!("unexpected handler call"),
                    }
                }
            },
            cancellation,
        ),
    )
    .await
    .expect("partial-request server timed out")
    .unwrap();

    let second_response = peer.join().expect("partial-request peer panicked");
    assert!(second_response.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert!(second_response.ends_with(b"fresh"));
    assert_eq!(calls.get(), 2);
}

#[kimojio::test]
async fn streaming_request_pull_honors_connection_io_timeout() {
    let server = Server::bind_with_config(
        (Ipv4Addr::LOCALHOST, 0).into(),
        ServerConfig::new().set_connection_io_timeout(Duration::from_millis(50)),
    )
    .await
    .unwrap();
    let address = server.local_addr();
    let peer = thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .write_all(
                b"POST /timeout HTTP/1.1\r\n\
                  host: localhost\r\n\
                  content-length: 100\r\n\r\n",
            )
            .unwrap();
        thread::sleep(Duration::from_millis(250));
    });

    let handler_called = Rc::new(Cell::new(false));
    let observed_handler = Rc::clone(&handler_called);
    let timeout_reported = Rc::new(Cell::new(false));
    let observed_timeout = Rc::clone(&timeout_reported);
    let cancellation = Rc::new(CancellationToken::new());
    let cancel_from_error = Rc::clone(&cancellation);
    operations::timeout_at(
        Instant::now() + WAIT,
        server.serve_streaming_with_error_handler(
            move |mut request| {
                observed_handler.set(true);
                async move {
                    let _ = request.body_mut().next_chunk().await;
                    Response::new(Body::empty())
                }
            },
            cancellation,
            move |error| {
                if matches!(
                    error,
                    ServeError::Connection(HttpError::Io(kimojio::Errno::TIME))
                        | ServeError::Connection(HttpError::Io(kimojio::Errno::TIMEDOUT))
                ) {
                    observed_timeout.set(true);
                    cancel_from_error.cancel();
                } else {
                    panic!("unexpected streaming timeout report: {error}");
                }
            },
        ),
    )
    .await
    .expect("streaming timeout server did not stop")
    .unwrap();
    peer.join().expect("timeout peer panicked");
    assert!(handler_called.get());
    assert!(timeout_reported.get());
}
