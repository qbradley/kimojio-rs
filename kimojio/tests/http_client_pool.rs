// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(feature = "http")]

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::str;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use kimojio::http::{
    Body, Client, ClientConfig, ClientEvent, Error, HeaderMap, Limits, Method, ProtocolErrorKind,
    Response, Version,
};
use kimojio::operations;
use kimojio_fsm_http::{
    H2ByteStreamEvent, H2ErrorCode, H2Frame, H2FrameType, H2HeaderField, H2OutboundCommit, H2Server,
};
use rustix::net::sockopt::set_socket_linger;

#[cfg(feature = "tls")]
use foreign_types_shared::ForeignTypeRef;
#[cfg(feature = "tls")]
use kimojio::http::{AlpnProtocol, TlsClientConfig};
#[cfg(feature = "tls")]
use openssl::asn1::Asn1Time;
#[cfg(feature = "tls")]
use openssl::bn::{BigNum, MsbOption};
#[cfg(feature = "tls")]
use openssl::hash::MessageDigest;
#[cfg(feature = "tls")]
use openssl::pkey::PKey;
#[cfg(feature = "tls")]
use openssl::rsa::Rsa;
#[cfg(feature = "tls")]
use openssl::ssl::{
    AlpnError, SslAcceptor, SslContextBuilder, SslMethod, SslStream, SslVerifyMode, SslVersion,
};
#[cfg(feature = "tls")]
use openssl::x509::extension::SubjectAlternativeName;
#[cfg(feature = "tls")]
use openssl::x509::{X509, X509NameBuilder};

const WAIT: Duration = Duration::from_secs(10);
static LARGE_RESPONSE_BODY: [u8; 100_000] = [b'x'; 100_000];

#[derive(Clone, Copy)]
struct TestResponse {
    body: &'static [u8],
    headers: &'static [(&'static str, &'static str)],
    close: bool,
}

impl TestResponse {
    const fn new(body: &'static [u8]) -> Self {
        Self {
            body,
            headers: &[],
            close: false,
        }
    }

    const fn with_headers(
        body: &'static [u8],
        headers: &'static [(&'static str, &'static str)],
    ) -> Self {
        Self {
            body,
            headers,
            close: false,
        }
    }

    const fn closing(
        body: &'static [u8],
        headers: &'static [(&'static str, &'static str)],
    ) -> Self {
        Self {
            body,
            headers,
            close: true,
        }
    }

    fn wire(self) -> Vec<u8> {
        let mut wire =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n", self.body.len()).into_bytes();
        for (name, value) in self.headers {
            wire.extend_from_slice(name.as_bytes());
            wire.extend_from_slice(b": ");
            wire.extend_from_slice(value.as_bytes());
            wire.extend_from_slice(b"\r\n");
        }
        wire.extend_from_slice(b"\r\n");
        wire.extend_from_slice(self.body);
        wire
    }
}

#[derive(Debug)]
struct ObservedRequest {
    target: String,
    headers: HeaderMap,
    body: Vec<u8>,
}

struct ConnectionState {
    stream: TcpStream,
    input: Vec<u8>,
}

struct CountingHttp1Peer {
    address: SocketAddr,
    thread: thread::JoinHandle<(usize, Vec<ObservedRequest>)>,
}

impl CountingHttp1Peer {
    fn finish(self) -> (usize, Vec<ObservedRequest>) {
        self.thread.join().expect("HTTP/1 peer thread panicked")
    }
}

fn spawn_counting_http1(responses: Vec<TestResponse>) -> CountingHttp1Peer {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let thread = thread::spawn(move || {
        let deadline = Instant::now() + WAIT;
        let mut accepts = 0;
        let mut connections = Vec::<ConnectionState>::new();
        let mut requests = Vec::new();

        while requests.len() < responses.len() {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(true).unwrap();
                        accepts += 1;
                        connections.push(ConnectionState {
                            stream,
                            input: Vec::new(),
                        });
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => panic!("HTTP/1 accept failed: {error}"),
                }
            }

            let mut position = 0;
            while position < connections.len() && requests.len() < responses.len() {
                let mut remove = false;
                let connection = &mut connections[position];
                let mut buffer = [0; 4096];
                loop {
                    match connection.stream.read(&mut buffer) {
                        Ok(0) => {
                            remove = true;
                            break;
                        }
                        Ok(amount) => connection.input.extend_from_slice(&buffer[..amount]),
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                        Err(error) => panic!("HTTP/1 read failed: {error}"),
                    }
                }

                if let Some((request, consumed)) = parse_request(&connection.input) {
                    connection.input.drain(..consumed);
                    let response = responses[requests.len()];
                    write_nonblocking(&mut connection.stream, &response.wire(), deadline);
                    requests.push(request);
                    remove |= response.close;
                }

                if remove {
                    connections.swap_remove(position);
                } else {
                    position += 1;
                }
            }

            assert!(
                Instant::now() < deadline,
                "timed out after {accepts} accepts and {} requests",
                requests.len()
            );
            thread::sleep(Duration::from_millis(1));
        }
        (accepts, requests)
    });

    CountingHttp1Peer { address, thread }
}

struct IdleInjectionPeer {
    address: SocketAddr,
    inject: Sender<()>,
    injected: Receiver<()>,
    thread: thread::JoinHandle<(usize, Vec<ObservedRequest>)>,
}

impl IdleInjectionPeer {
    fn inject_response(&self) {
        self.inject
            .send(())
            .expect("idle-injection peer stopped before injection");
        self.injected
            .recv_timeout(WAIT)
            .expect("timed out waiting for the unsolicited response");
    }

    fn finish(self) -> (usize, Vec<ObservedRequest>) {
        self.thread
            .join()
            .expect("idle-injection peer thread panicked")
    }
}

fn spawn_idle_injection_peer() -> IdleInjectionPeer {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let (inject, inject_receiver) = mpsc::channel();
    let (injected_sender, injected) = mpsc::channel();
    let thread = thread::spawn(move || {
        let deadline = Instant::now() + WAIT;
        let mut accepts = 1;
        let mut requests = Vec::new();
        let mut first = accept_before(&listener, deadline);
        first.set_read_timeout(Some(WAIT)).unwrap();
        first.set_write_timeout(Some(WAIT)).unwrap();

        requests.push(read_blocking_request(&mut first));
        first
            .write_all(&TestResponse::new(b"prime").wire())
            .unwrap();
        inject_receiver
            .recv_timeout(WAIT)
            .expect("timed out waiting to inject an idle response");
        first
            .write_all(&TestResponse::new(b"unsolicited").wire())
            .unwrap();
        wait_for_tcp_delivery(&first, deadline);
        injected_sender.send(()).unwrap();

        first.set_nonblocking(true).unwrap();
        let mut first_open = true;
        let mut input = Vec::new();
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    accepts += 1;
                    stream.set_read_timeout(Some(WAIT)).unwrap();
                    stream.set_write_timeout(Some(WAIT)).unwrap();
                    requests.push(read_blocking_request(&mut stream));
                    stream
                        .write_all(
                            &TestResponse::closing(b"legitimate", &[("connection", "close")])
                                .wire(),
                        )
                        .unwrap();
                    return (accepts, requests);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("idle-injection accept failed: {error}"),
            }

            if first_open {
                let mut buffer = [0; 4096];
                match first.read(&mut buffer) {
                    Ok(0) => first_open = false,
                    Ok(amount) => input.extend_from_slice(&buffer[..amount]),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset
                        ) =>
                    {
                        first_open = false;
                    }
                    Err(error) => panic!("idle-injection read failed: {error}"),
                }
                if let Some((request, _)) = parse_request(&input) {
                    requests.push(request);
                    write_nonblocking(
                        &mut first,
                        &TestResponse::closing(b"legitimate", &[("connection", "close")]).wire(),
                        deadline,
                    );
                    return (accepts, requests);
                }
            }

            thread::yield_now();
        }
        panic!(
            "timed out waiting for the request after injecting an idle response; \
             observed {accepts} accepts and {} requests",
            requests.len()
        );
    });

    IdleInjectionPeer {
        address,
        inject,
        injected,
        thread,
    }
}

fn parse_request(input: &[u8]) -> Option<(ObservedRequest, usize)> {
    let head_end = input.windows(4).position(|bytes| bytes == b"\r\n\r\n")?;
    let head = str::from_utf8(&input[..head_end]).unwrap();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap().split_whitespace();
    request_line.next().unwrap();
    let target = request_line.next().unwrap().to_owned();
    let mut headers = HeaderMap::new();
    let mut content_length = 0;
    for line in lines {
        let (name, value) = line.split_once(':').unwrap();
        let value = value.trim();
        headers.append(
            name.parse::<kimojio::http::HeaderName>().unwrap(),
            value.parse::<kimojio::http::HeaderValue>().unwrap(),
        );
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().unwrap();
        }
    }
    let body_start = head_end + 4;
    let consumed = body_start + content_length;
    if input.len() < consumed {
        return None;
    }
    Some((
        ObservedRequest {
            target,
            headers,
            body: input[body_start..consumed].to_vec(),
        },
        consumed,
    ))
}

fn write_nonblocking(stream: &mut TcpStream, mut bytes: &[u8], deadline: Instant) {
    while !bytes.is_empty() {
        match stream.write(bytes) {
            Ok(0) => panic!("peer closed while a response was being written"),
            Ok(amount) => bytes = &bytes[amount..],
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "HTTP/1 write timed out");
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("HTTP/1 write failed: {error}"),
        }
    }
}

fn accept_before(listener: &TcpListener, deadline: Instant) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    loop {
        match listener.accept() {
            Ok((stream, _)) => return stream,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "timed out waiting for accept");
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("accept failed: {error}"),
        }
    }
}

fn wait_for_tcp_delivery(stream: &TcpStream, deadline: Instant) {
    loop {
        let mut pending = 0;
        // SAFETY: TIOCOUTQ only writes an integer to the valid pointer supplied
        // here, and `stream` keeps the queried descriptor open for the call.
        let result = unsafe { libc::ioctl(stream.as_raw_fd(), libc::TIOCOUTQ, &mut pending) };
        assert_eq!(
            result,
            0,
            "failed to inspect the TCP send queue: {}",
            io::Error::last_os_error()
        );
        if pending == 0 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for TCP delivery"
        );
        thread::yield_now();
    }
}

fn read_blocking_request(stream: &mut impl Read) -> ObservedRequest {
    let mut input = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let amount = stream.read(&mut buffer).unwrap();
        assert_ne!(amount, 0, "client closed before sending a request");
        input.extend_from_slice(&buffer[..amount]);
        if let Some((request, _)) = parse_request(&input) {
            return request;
        }
    }
}

struct RetryPolicyPeer {
    address: SocketAddr,
    idle_closed: Receiver<()>,
    reset_idle: Sender<()>,
    done: Sender<()>,
    thread: thread::JoinHandle<(usize, Vec<ObservedRequest>)>,
}

impl RetryPolicyPeer {
    fn wait_for_idle_close(&self) {
        self.idle_closed
            .recv_timeout(WAIT)
            .expect("timed out waiting for the peer to close the idle connection");
    }

    fn reset_idle_connection(&self) {
        self.reset_idle
            .send(())
            .expect("retry peer stopped before resetting its idle connection");
        self.wait_for_idle_close();
    }

    fn finish(self) -> (usize, Vec<ObservedRequest>) {
        let _ = self.done.send(());
        self.thread.join().expect("retry peer thread panicked")
    }
}

fn retry_policy_peer(
    serve: impl FnOnce(
        TcpListener,
        Receiver<()>,
        Sender<()>,
        Receiver<()>,
    ) -> (usize, Vec<ObservedRequest>)
    + Send
    + 'static,
) -> RetryPolicyPeer {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let (done, done_receiver) = mpsc::channel();
    let (idle_closed, idle_closed_receiver) = mpsc::channel();
    let (reset_idle, reset_idle_receiver) = mpsc::channel();
    let thread =
        thread::spawn(move || serve(listener, done_receiver, idle_closed, reset_idle_receiver));
    RetryPolicyPeer {
        address,
        idle_closed: idle_closed_receiver,
        reset_idle,
        done,
        thread,
    }
}

fn accept_retry_connection(
    listener: &TcpListener,
    done: &Receiver<()>,
    deadline: Instant,
) -> Option<TcpStream> {
    listener.set_nonblocking(true).unwrap();
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_read_timeout(Some(WAIT)).unwrap();
                stream.set_write_timeout(Some(WAIT)).unwrap();
                return Some(stream);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("retry peer accept failed: {error}"),
        }

        assert!(Instant::now() < deadline, "retry peer accept timed out");
        let wait = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(10));
        match done.recv_timeout(wait) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                return match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_read_timeout(Some(WAIT)).unwrap();
                        stream.set_write_timeout(Some(WAIT)).unwrap();
                        Some(stream)
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => None,
                    Err(error) => panic!("retry peer final accept failed: {error}"),
                };
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

fn observe_extra_retries(
    listener: &TcpListener,
    done: &Receiver<()>,
    deadline: Instant,
    accepts: &mut usize,
    requests: &mut Vec<ObservedRequest>,
) {
    while let Some(mut stream) = accept_retry_connection(listener, done, deadline) {
        *accepts += 1;
        requests.push(read_blocking_request(&mut stream));
        stream
            .write_all(&TestResponse::new(b"unexpected retry").wire())
            .unwrap();
    }
}

fn spawn_reused_idempotent_request_then_close() -> RetryPolicyPeer {
    retry_policy_peer(|listener, done, idle_closed, _reset_idle| {
        let deadline = Instant::now() + WAIT;
        let mut accepts = 0;
        let mut requests = Vec::new();

        if let Some(mut stream) = accept_retry_connection(&listener, &done, deadline) {
            accepts += 1;
            requests.push(read_blocking_request(&mut stream));
            stream
                .write_all(&TestResponse::new(b"prime").wire())
                .unwrap();
            requests.push(read_blocking_request(&mut stream));
            drop(stream);
            idle_closed.send(()).unwrap();
        }

        if let Some(mut stream) = accept_retry_connection(&listener, &done, deadline) {
            accepts += 1;
            requests.push(read_blocking_request(&mut stream));
            stream
                .write_all(&TestResponse::new(b"retried").wire())
                .unwrap();
            drop(stream);
            observe_extra_retries(&listener, &done, deadline, &mut accepts, &mut requests);
        }

        (accepts, requests)
    })
}

fn spawn_fully_written_request_then_close() -> RetryPolicyPeer {
    retry_policy_peer(|listener, done, idle_closed, _reset_idle| {
        let deadline = Instant::now() + WAIT;
        let mut accepts = 0;
        let mut requests = Vec::new();

        if let Some(mut stream) = accept_retry_connection(&listener, &done, deadline) {
            accepts += 1;
            requests.push(read_blocking_request(&mut stream));
            stream
                .write_all(&TestResponse::new(b"prime").wire())
                .unwrap();
            requests.push(read_blocking_request(&mut stream));
            drop(stream);
            idle_closed.send(()).unwrap();
        }

        observe_extra_retries(&listener, &done, deadline, &mut accepts, &mut requests);
        (accepts, requests)
    })
}

fn spawn_reset_idle_then_success() -> RetryPolicyPeer {
    retry_policy_peer(|listener, done, idle_closed, reset_idle| {
        let deadline = Instant::now() + WAIT;
        let mut accepts = 0;
        let mut requests = Vec::new();

        if let Some(mut stream) = accept_retry_connection(&listener, &done, deadline) {
            accepts += 1;
            requests.push(read_blocking_request(&mut stream));
            stream
                .write_all(&TestResponse::new(b"prime").wire())
                .unwrap();
            reset_idle
                .recv_timeout(WAIT)
                .expect("timed out waiting to reset the idle connection");
            set_socket_linger(&stream, Some(Duration::ZERO)).unwrap();
            drop(stream);
            idle_closed.send(()).unwrap();
        }

        if let Some(mut stream) = accept_retry_connection(&listener, &done, deadline) {
            accepts += 1;
            requests.push(read_blocking_request(&mut stream));
            stream
                .write_all(&TestResponse::new(b"retried").wire())
                .unwrap();
            drop(stream);
            observe_extra_retries(&listener, &done, deadline, &mut accepts, &mut requests);
        }

        (accepts, requests)
    })
}

fn spawn_partial_response_on_reused_connection() -> RetryPolicyPeer {
    retry_policy_peer(|listener, done, idle_closed, _reset_idle| {
        let deadline = Instant::now() + WAIT;
        let mut accepts = 0;
        let mut requests = Vec::new();

        if let Some(mut stream) = accept_retry_connection(&listener, &done, deadline) {
            accepts += 1;
            requests.push(read_blocking_request(&mut stream));
            stream
                .write_all(&TestResponse::new(b"prime").wire())
                .unwrap();
            requests.push(read_blocking_request(&mut stream));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nx")
                .unwrap();
            drop(stream);
            idle_closed.send(()).unwrap();
        }

        observe_extra_retries(&listener, &done, deadline, &mut accepts, &mut requests);
        (accepts, requests)
    })
}

fn spawn_fresh_connection_failure() -> RetryPolicyPeer {
    retry_policy_peer(|listener, done, idle_closed, _reset_idle| {
        let deadline = Instant::now() + WAIT;
        let mut accepts = 0;
        let mut requests = Vec::new();

        if let Some(mut stream) = accept_retry_connection(&listener, &done, deadline) {
            accepts += 1;
            requests.push(read_blocking_request(&mut stream));
            drop(stream);
            idle_closed.send(()).unwrap();
        }

        observe_extra_retries(&listener, &done, deadline, &mut accepts, &mut requests);
        (accepts, requests)
    })
}

fn spawn_failed_reused_connection_retry() -> RetryPolicyPeer {
    retry_policy_peer(|listener, done, idle_closed, _reset_idle| {
        let deadline = Instant::now() + WAIT;
        let mut accepts = 0;
        let mut requests = Vec::new();

        if let Some(mut stream) = accept_retry_connection(&listener, &done, deadline) {
            accepts += 1;
            requests.push(read_blocking_request(&mut stream));
            stream
                .write_all(&TestResponse::new(b"prime").wire())
                .unwrap();
            requests.push(read_blocking_request(&mut stream));
            drop(stream);
            idle_closed.send(()).unwrap();
        }

        if let Some(mut stream) = accept_retry_connection(&listener, &done, deadline) {
            accepts += 1;
            requests.push(read_blocking_request(&mut stream));
            drop(stream);
            observe_extra_retries(&listener, &done, deadline, &mut accepts, &mut requests);
        }

        (accepts, requests)
    })
}

async fn get(client: &Client, uri: String) -> Response<Body> {
    operations::timeout_at(Instant::now() + WAIT, client.get(uri).send())
        .await
        .expect("HTTP request timed out")
        .unwrap()
}

async fn get_pair(client: &Client, first: String, second: String) {
    let (first, second) = operations::timeout_at(Instant::now() + WAIT, async {
        futures::join!(client.get(first).send(), client.get(second).send())
    })
    .await
    .expect("concurrent HTTP requests timed out");
    assert_eq!(first.unwrap().body().as_bytes(), b"ok");
    assert_eq!(second.unwrap().body().as_bytes(), b"ok");
}

#[kimojio::test]
async fn sequential_requests_reuse_exactly_one_http1_connection() {
    let peer = spawn_counting_http1(vec![
        TestResponse::new(b"one"),
        TestResponse::new(b"two"),
        TestResponse::new(b"three"),
    ]);
    let client = Client::new();
    let clone = client.clone();

    assert_eq!(
        get(&client, format!("http://{}/one", peer.address))
            .await
            .body()
            .as_bytes(),
        b"one"
    );
    assert_eq!(
        get(&clone, format!("http://{}/two", peer.address))
            .await
            .body()
            .as_bytes(),
        b"two"
    );
    assert_eq!(
        get(&client, format!("http://{}/three", peer.address))
            .await
            .body()
            .as_bytes(),
        b"three"
    );

    let (accepts, requests) = peer.finish();
    assert_eq!(accepts, 1, "three exchanges must use one TCP accept");
    assert_eq!(
        requests
            .iter()
            .map(|request| request.target.as_str())
            .collect::<Vec<_>>(),
        ["/one", "/two", "/three"]
    );
}

#[kimojio::test]
async fn max_requests_per_connection_retires_client_connection() {
    let peer = spawn_counting_http1(vec![
        TestResponse::new(b"one"),
        TestResponse::new(b"two"),
        TestResponse::new(b"three"),
    ]);
    let client = Client::with_config(
        ClientConfig::new().set_limits(Limits::new().set_max_requests_per_connection(2)),
    )
    .unwrap();

    for (target, expected) in [
        ("one", b"one".as_slice()),
        ("two", b"two".as_slice()),
        ("three", b"three".as_slice()),
    ] {
        assert_eq!(
            get(&client, format!("http://{}/{target}", peer.address))
                .await
                .body()
                .as_bytes(),
            expected
        );
    }

    let (accepts, requests) = peer.finish();
    assert_eq!(
        accepts, 2,
        "the client must retire a connection after two completed requests"
    );
    assert_eq!(
        requests
            .iter()
            .map(|request| request.target.as_str())
            .collect::<Vec<_>>(),
        ["/one", "/two", "/three"]
    );
}

#[kimojio::test]
async fn idempotent_request_retries_when_reused_connection_closes_before_response() {
    let peer = spawn_reused_idempotent_request_then_close();
    let client = Client::new();

    assert_eq!(
        get(&client, format!("http://{}/prime", peer.address))
            .await
            .body()
            .as_bytes(),
        b"prime"
    );
    let (event_sender, event_receiver) = mpsc::channel();
    let result = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .get(format!("http://{}/retry", peer.address))
            .send_with_event_handler(move |event| event_sender.send(event).unwrap()),
    )
    .await
    .expect("retried request timed out");
    peer.wait_for_idle_close();
    let (accepts, requests) = peer.finish();

    assert_eq!(result.unwrap().body().as_bytes(), b"retried");
    assert_eq!(
        event_receiver.try_iter().collect::<Vec<_>>(),
        [ClientEvent::RequestRetried],
        "the failed reused exchange must enter the retry branch"
    );
    assert_eq!(accepts, 2, "the retry must establish one fresh connection");
    assert_eq!(
        requests
            .iter()
            .map(|request| request.target.as_str())
            .collect::<Vec<_>>(),
        ["/prime", "/retry", "/retry"],
        "the server must observe the failed attempt and its retry"
    );
}

#[kimojio::test]
async fn fully_written_post_on_reused_connection_is_not_retried() {
    let peer = spawn_fully_written_request_then_close();
    let client = Client::new();

    get(&client, format!("http://{}/prime", peer.address)).await;
    let result = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .post(format!("http://{}/apply", peer.address))
            .body("side effect")
            .send(),
    )
    .await
    .expect("fully-written POST timed out");
    peer.wait_for_idle_close();
    let (accepts, requests) = peer.finish();

    assert!(result.is_err(), "a fully-written POST must not be retried");
    assert_eq!(
        accepts, 1,
        "the failure must not establish a fresh connection"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.target == "/apply")
            .count(),
        1,
        "the side-effecting request must be observed exactly once"
    );
    assert_eq!(requests[1].body, b"side effect");
}

#[kimojio::test]
async fn stale_connection_is_discarded_before_sending_non_idempotent_request() {
    let peer = spawn_reset_idle_then_success();
    let client = Client::new();

    get(&client, format!("http://{}/prime", peer.address)).await;
    peer.reset_idle_connection();
    let (event_sender, event_receiver) = mpsc::channel();
    let result = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .post(format!("http://{}/apply", peer.address))
            .body("side effect")
            .send_with_event_handler(move |event| event_sender.send(event).unwrap()),
    )
    .await
    .expect("POST on a replacement connection timed out");
    let (accepts, requests) = peer.finish();

    assert_eq!(result.unwrap().body().as_bytes(), b"retried");
    assert_eq!(
        event_receiver.try_iter().collect::<Vec<_>>(),
        [ClientEvent::DirtyPooledConnectionDiscarded],
        "checkout must report the stale pooled connection as dirty"
    );
    assert_eq!(
        accepts, 2,
        "checkout must replace the stale connection before sending"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.target == "/apply")
            .count(),
        1,
        "only the fresh connection may observe the POST"
    );
    assert_eq!(requests[1].body, b"side effect");
}

#[kimojio::test]
async fn partial_response_on_reused_connection_is_not_retried() {
    let peer = spawn_partial_response_on_reused_connection();
    let client = Client::new();

    get(&client, format!("http://{}/prime", peer.address)).await;
    let result = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .post(format!("http://{}/apply", peer.address))
            .body("side effect")
            .send(),
    )
    .await
    .expect("partial-response request timed out");
    peer.wait_for_idle_close();
    let (accepts, requests) = peer.finish();

    assert!(result.is_err(), "a partial response must surface its error");
    assert_eq!(
        accepts, 1,
        "response bytes must suppress a fresh connection"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.target == "/apply")
            .count(),
        1,
        "the side-effecting request must not be sent twice"
    );
}

#[kimojio::test]
async fn fresh_connection_failure_is_not_retried() {
    let peer = spawn_fresh_connection_failure();
    let client = Client::new();

    let result = operations::timeout_at(
        Instant::now() + WAIT,
        client.get(format!("http://{}/fresh", peer.address)).send(),
    )
    .await
    .expect("fresh-connection request timed out");
    peer.wait_for_idle_close();
    let (accepts, requests) = peer.finish();

    assert!(result.is_err(), "the fresh connection failure must surface");
    assert_eq!(accepts, 1, "a fresh connection must not be retried");
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.target == "/fresh")
            .count(),
        1
    );
}

#[kimojio::test]
async fn failed_retry_on_fresh_connection_is_not_retried_again() {
    let peer = spawn_failed_reused_connection_retry();
    let client = Client::new();

    get(&client, format!("http://{}/prime", peer.address)).await;
    let result = operations::timeout_at(
        Instant::now() + WAIT,
        client.get(format!("http://{}/retry", peer.address)).send(),
    )
    .await
    .expect("single-retry request timed out");
    peer.wait_for_idle_close();
    let (accepts, requests) = peer.finish();

    assert!(result.is_err(), "the failed retry must surface");
    assert_eq!(
        accepts, 2,
        "one prime connection and one retry connection are expected"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.target == "/retry")
            .count(),
        2,
        "the retry failure must not trigger a third attempt"
    );
}

#[kimojio::test]
async fn separately_constructed_clients_do_not_share_connections() {
    let peer = spawn_counting_http1(vec![
        TestResponse::new(b"first"),
        TestResponse::new(b"second"),
    ]);

    get(&Client::new(), format!("http://{}/first", peer.address)).await;
    get(&Client::new(), format!("http://{}/second", peer.address)).await;

    let (accepts, _) = peer.finish();
    assert_eq!(accepts, 2);
}

#[kimojio::test]
async fn reused_connection_does_not_contaminate_headers_or_bodies() {
    let peer = spawn_counting_http1(vec![
        TestResponse::with_headers(b"response-one", &[("x-first", "one")]),
        TestResponse::with_headers(b"response-two", &[("x-second", "two")]),
    ]);
    let client = Client::new();

    let first = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .post(format!("http://{}/first", peer.address))
            .header("x-request", "first")
            .body("request-one")
            .send(),
    )
    .await
    .expect("first request timed out")
    .unwrap();
    let second = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .post(format!("http://{}/second", peer.address))
            .header("x-request", "second")
            .body("request-two")
            .send(),
    )
    .await
    .expect("second request timed out")
    .unwrap();

    assert_eq!(first.body().as_bytes(), b"response-one");
    assert_eq!(first.headers()["x-first"], "one");
    assert!(!first.headers().contains_key("x-second"));
    assert_eq!(second.body().as_bytes(), b"response-two");
    assert_eq!(second.headers()["x-second"], "two");
    assert!(!second.headers().contains_key("x-first"));

    let (accepts, requests) = peer.finish();
    assert_eq!(accepts, 1);
    assert_eq!(requests[0].headers["x-request"], "first");
    assert_eq!(requests[0].body, b"request-one");
    assert_eq!(requests[1].headers["x-request"], "second");
    assert_eq!(requests[1].body, b"request-two");
}

#[kimojio::test]
async fn response_larger_than_read_buffer_does_not_leak_into_next_exchange() {
    let peer = spawn_counting_http1(vec![
        TestResponse::new(&LARGE_RESPONSE_BODY),
        TestResponse::new(b"second"),
    ]);
    let client = Client::new();

    let first = get(&client, format!("http://{}/large", peer.address)).await;
    let second = get(&client, format!("http://{}/second", peer.address)).await;

    assert_eq!(first.body().as_bytes(), LARGE_RESPONSE_BODY);
    assert_eq!(second.body().as_bytes(), b"second");
    let (accepts, requests) = peer.finish();
    assert_eq!(accepts, 1, "the fully drained connection must be reusable");
    assert_eq!(
        requests
            .iter()
            .map(|request| request.target.as_str())
            .collect::<Vec<_>>(),
        ["/large", "/second"]
    );
}

#[kimojio::test]
async fn connection_close_response_is_not_pooled() {
    let peer = spawn_counting_http1(vec![
        TestResponse::closing(b"first", &[("connection", "keep-alive, upgrade, close")]),
        TestResponse::new(b"second"),
    ]);
    let client = Client::new();

    assert_eq!(
        get(&client, format!("http://{}/first", peer.address))
            .await
            .body()
            .as_bytes(),
        b"first"
    );
    assert_eq!(
        get(&client, format!("http://{}/second", peer.address))
            .await
            .body()
            .as_bytes(),
        b"second"
    );

    let (accepts, _) = peer.finish();
    assert_eq!(accepts, 2, "Connection: close must force a new accept");
}

#[kimojio::test]
async fn unsolicited_response_bytes_on_an_idle_connection_are_never_associated_with_the_next_request()
 {
    let peer = spawn_idle_injection_peer();
    let client = Client::new();

    let first = get(&client, format!("http://{}/prime", peer.address)).await;
    assert_eq!(first.body().as_bytes(), b"prime");
    peer.inject_response();
    let (event_sender, event_receiver) = mpsc::channel();
    let second = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .get(format!("http://{}/next", peer.address))
            .send_with_event_handler(move |event| event_sender.send(event).unwrap()),
    )
    .await
    .expect("request after unsolicited response timed out")
    .unwrap();
    let (accepts, requests) = peer.finish();

    assert_eq!(
        second.body().as_bytes(),
        b"legitimate",
        "bytes received before the request must not be accepted as its response"
    );
    assert_eq!(
        event_receiver.try_iter().collect::<Vec<_>>(),
        [ClientEvent::DirtyPooledConnectionDiscarded],
        "the smuggling defense must report its dirty-connection discard"
    );
    assert_eq!(
        accepts, 2,
        "a readable idle connection must be discarded before checkout"
    );
    assert_eq!(
        requests
            .iter()
            .map(|request| request.target.as_str())
            .collect::<Vec<_>>(),
        ["/prime", "/next"]
    );
}

#[kimojio::test]
async fn idle_timeout_evicts_connection() {
    let peer = spawn_counting_http1(vec![
        TestResponse::new(b"first"),
        TestResponse::new(b"second"),
    ]);
    let client =
        Client::with_config(ClientConfig::new().set_pool_idle_timeout(Duration::from_millis(20)))
            .unwrap();

    get(&client, format!("http://{}/first", peer.address)).await;
    operations::sleep(Duration::from_millis(40)).await.unwrap();
    get(&client, format!("http://{}/second", peer.address)).await;

    let (accepts, _) = peer.finish();
    assert_eq!(accepts, 2, "expired idle connection must be closed");
}

async fn assert_zero_pool_bound_disables_reuse(config: ClientConfig, bound: &str) {
    let peer = spawn_counting_http1(vec![
        TestResponse::new(b"first"),
        TestResponse::new(b"second"),
    ]);
    let client = Client::with_config(config).unwrap();

    get(&client, format!("http://{}/first", peer.address)).await;
    get(&client, format!("http://{}/second", peer.address)).await;

    let (accepts, requests) = peer.finish();
    assert_eq!(
        accepts, 2,
        "a zero {bound} must explicitly disable idle reuse"
    );
    assert_eq!(
        requests
            .iter()
            .map(|request| request.target.as_str())
            .collect::<Vec<_>>(),
        ["/first", "/second"]
    );
}

#[kimojio::test]
async fn zero_pool_bounds_disable_idle_connection_reuse() {
    assert_zero_pool_bound_disables_reuse(
        ClientConfig::new().set_pool_idle_timeout(Duration::ZERO),
        "idle timeout",
    )
    .await;
    assert_zero_pool_bound_disables_reuse(
        ClientConfig::new().set_pool_max_idle_per_key(0),
        "per-key idle cap",
    )
    .await;
    assert_zero_pool_bound_disables_reuse(
        ClientConfig::new().set_pool_max_idle_total(0),
        "total idle cap",
    )
    .await;
}

#[kimojio::test]
async fn total_idle_cap_evicts_oldest_origin() {
    let first_peer = spawn_counting_http1(vec![
        TestResponse::new(b"first-a"),
        TestResponse::new(b"second-a"),
    ]);
    let second_peer = spawn_counting_http1(vec![TestResponse::new(b"only-b")]);
    let client = Client::with_config(ClientConfig::new().set_pool_max_idle_total(1)).unwrap();

    get(&client, format!("http://{}/first", first_peer.address)).await;
    get(&client, format!("http://{}/only", second_peer.address)).await;
    get(&client, format!("http://{}/second", first_peer.address)).await;

    let (first_accepts, _) = first_peer.finish();
    let (second_accepts, _) = second_peer.finish();
    assert_eq!(first_accepts, 2, "the oldest origin should be evicted");
    assert_eq!(second_accepts, 1);
}

#[kimojio::test]
async fn per_key_idle_cap_evicts_extra_connection() {
    let peer = spawn_counting_http1(vec![TestResponse::new(b"ok"); 4]);
    let client = Client::with_config(ClientConfig::new().set_pool_max_idle_per_key(1)).unwrap();
    let base = format!("http://{}", peer.address);

    get_pair(&client, format!("{base}/one"), format!("{base}/two")).await;
    get_pair(&client, format!("{base}/three"), format!("{base}/four")).await;

    let (accepts, _) = peer.finish();
    assert_eq!(
        accepts, 3,
        "only one of the first two connections should remain idle"
    );
}

#[kimojio::test]
async fn different_origins_use_separate_connections() {
    let first_peer = spawn_counting_http1(vec![TestResponse::new(b"a"); 2]);
    let second_peer = spawn_counting_http1(vec![TestResponse::new(b"b"); 2]);
    let client = Client::new();

    assert_eq!(
        get(&client, format!("http://{}/one", first_peer.address))
            .await
            .body()
            .as_bytes(),
        b"a"
    );
    assert_eq!(
        get(&client, format!("http://{}/one", second_peer.address))
            .await
            .body()
            .as_bytes(),
        b"b"
    );
    assert_eq!(
        get(&client, format!("http://{}/two", first_peer.address))
            .await
            .body()
            .as_bytes(),
        b"a"
    );
    assert_eq!(
        get(&client, format!("http://{}/two", second_peer.address))
            .await
            .body()
            .as_bytes(),
        b"b"
    );

    let (first_accepts, _) = first_peer.finish();
    let (second_accepts, _) = second_peer.finish();
    assert_eq!((first_accepts, second_accepts), (1, 1));
}

struct MixedVersionPeer {
    address: SocketAddr,
    thread: thread::JoinHandle<usize>,
}

fn take_server_block(server: &mut H2Server, commit: H2OutboundCommit) -> Vec<u8> {
    let block = server.next_outbound_block().unwrap();
    assert_eq!(block.commit(), commit);
    let bytes = block.bytes().to_vec();
    server.acknowledge_outbound_block(commit).unwrap();
    bytes
}

fn spawn_mixed_version_peer() -> MixedVersionPeer {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let thread = thread::spawn(move || {
        let deadline = Instant::now() + WAIT;
        let mut first = accept_before(&listener, deadline);
        first.set_read_timeout(Some(WAIT)).unwrap();
        first.set_write_timeout(Some(WAIT)).unwrap();
        let request = read_blocking_request(&mut first);
        assert_eq!(request.target, "/http1");
        first
            .write_all(&TestResponse::new(b"http1").wire())
            .unwrap();

        let mut second = accept_before(&listener, deadline);
        second.set_read_timeout(Some(WAIT)).unwrap();
        second.set_write_timeout(Some(WAIT)).unwrap();
        let mut protocol = H2Server::default();
        let mut input = Vec::new();
        let stream_id =
            'request: loop {
                let mut buffer = [0; 4096];
                let amount = second.read(&mut buffer).unwrap();
                assert_ne!(amount, 0, "HTTP/2 client closed before its request");
                input.extend_from_slice(&buffer[..amount]);
                loop {
                    let (event, consumed, output) = protocol.accept_event_bytes(&input).unwrap();
                    if !output.is_empty() {
                        second.write_all(&output).unwrap();
                    }
                    if consumed != 0 {
                        input.drain(..consumed);
                    }
                    if let Some(H2ByteStreamEvent::RequestHeaders {
                        stream_id, headers, ..
                    }) = event
                    {
                        assert!(headers.iter().any(|header| {
                            header.name == b":path" && header.value == b"/http2"
                        }));
                        break 'request stream_id;
                    }
                    if consumed == 0 || input.is_empty() {
                        break;
                    }
                }
            };
        let headers = [H2HeaderField::new(b"x-protocol", b"http2")];
        let commit = protocol
            .response_headers_frame_with_raw_headers(stream_id, 200, &headers, false)
            .unwrap();
        let mut response = take_server_block(&mut protocol, commit);
        response.extend_from_slice(&protocol.data_frame(stream_id, b"http2", true));
        second.write_all(&response).unwrap();
        2
    });
    MixedVersionPeer { address, thread }
}

#[kimojio::test]
async fn different_protocol_versions_do_not_share_connection() {
    let peer = spawn_mixed_version_peer();
    let client = Client::new();

    let first = get(&client, format!("http://{}/http1", peer.address)).await;
    let second = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .get(format!("http://{}/http2", peer.address))
            .version(Version::HTTP_2)
            .send(),
    )
    .await
    .expect("HTTP/2 request timed out")
    .unwrap();

    assert_eq!(first.body().as_bytes(), b"http1");
    assert_eq!(second.version(), Version::HTTP_2);
    assert_eq!(second.body().as_bytes(), b"http2");
    assert_eq!(
        peer.thread.join().expect("mixed peer thread panicked"),
        2,
        "different wire protocols require separate accepts"
    );
}

struct CountingH2Peer {
    address: SocketAddr,
    thread: thread::JoinHandle<usize>,
}

fn wait_for_h2_client_close(stream: &mut TcpStream, protocol: &mut H2Server, input: &mut Vec<u8>) {
    loop {
        let mut buffer = [0; 4096];
        let amount = match stream.read(&mut buffer) {
            Ok(amount) => amount,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset
                ) =>
            {
                return;
            }
            Err(error) => panic!("HTTP/2 client did not retire its connection: {error}"),
        };
        if amount == 0 {
            return;
        }
        input.extend_from_slice(&buffer[..amount]);
        loop {
            let (event, consumed, output) = protocol.accept_event_bytes(input).unwrap();
            if !output.is_empty() {
                stream.write_all(&output).unwrap();
            }
            assert!(
                !matches!(event, Some(H2ByteStreamEvent::RequestHeaders { .. })),
                "HTTP/2 client reused a connection past its request limit"
            );
            if consumed != 0 {
                input.drain(..consumed);
            }
            if consumed == 0 || input.is_empty() {
                break;
            }
        }
    }
}

fn spawn_counting_h2(request_count: usize) -> CountingH2Peer {
    spawn_counting_h2_connections(vec![request_count])
}

fn spawn_counting_h2_connections(requests_per_connection: Vec<usize>) -> CountingH2Peer {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let thread = thread::spawn(move || {
        let mut input = Vec::new();
        let mut requests = 0;
        let mut accepts = 0;
        let connection_count = requests_per_connection.len();

        for (connection_index, request_count) in requests_per_connection.into_iter().enumerate() {
            let mut stream = accept_before(&listener, Instant::now() + WAIT);
            stream.set_read_timeout(Some(WAIT)).unwrap();
            stream.set_write_timeout(Some(WAIT)).unwrap();
            accepts += 1;
            input.clear();
            let mut protocol = H2Server::default();
            let connection_request_target = requests + request_count;

            while requests < connection_request_target {
                let mut buffer = [0; 4096];
                let amount = stream.read(&mut buffer).unwrap();
                assert_ne!(amount, 0, "HTTP/2 client closed before all requests");
                input.extend_from_slice(&buffer[..amount]);
                loop {
                    let (event, consumed, output) = protocol.accept_event_bytes(&input).unwrap();
                    if !output.is_empty() {
                        stream.write_all(&output).unwrap();
                    }
                    if consumed != 0 {
                        input.drain(..consumed);
                    }
                    if let Some(H2ByteStreamEvent::RequestHeaders { stream_id, .. }) = event {
                        requests += 1;
                        let body = format!("response-{requests}");
                        let commit = protocol
                            .response_headers_frame_with_raw_headers(stream_id, 200, &[], false)
                            .unwrap();
                        let mut response = take_server_block(&mut protocol, commit);
                        response.extend_from_slice(&protocol.data_frame(
                            stream_id,
                            body.as_bytes(),
                            true,
                        ));
                        stream.write_all(&response).unwrap();
                    }
                    if consumed == 0 || input.is_empty() {
                        break;
                    }
                }
            }
            while protocol.control_diagnostics().settings_acks == 0 {
                let mut buffer = [0; 4096];
                let amount = stream.read(&mut buffer).unwrap();
                assert_ne!(amount, 0, "HTTP/2 client closed before SETTINGS ack");
                input.extend_from_slice(&buffer[..amount]);
                while !input.is_empty() {
                    let (_, consumed, output) = protocol.accept_event_bytes(&input).unwrap();
                    if !output.is_empty() {
                        stream.write_all(&output).unwrap();
                    }
                    if consumed != 0 {
                        input.drain(..consumed);
                    }
                    if consumed == 0 {
                        break;
                    }
                }
            }
            wait_for_tcp_delivery(&stream, Instant::now() + WAIT);
            if connection_index + 1 < connection_count {
                wait_for_h2_client_close(&mut stream, &mut protocol, &mut input);
            }
        }
        accepts
    });
    CountingH2Peer { address, thread }
}

#[derive(Clone, Copy)]
enum H2CancellationMode {
    PendingSend,
    StreamingBody,
}

struct H2CancellationPeer {
    address: SocketAddr,
    thread: thread::JoinHandle<()>,
}

fn spawn_h2_cancellation_peer(mode: H2CancellationMode) -> H2CancellationPeer {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let thread = thread::spawn(move || {
        let mut stream = accept_before(&listener, Instant::now() + WAIT);
        stream.set_read_timeout(Some(WAIT)).unwrap();
        stream.set_write_timeout(Some(WAIT)).unwrap();
        let mut protocol = H2Server::default();
        let mut input = Vec::new();

        let first_stream = loop {
            if let H2ByteStreamEvent::RequestHeaders {
                stream_id, headers, ..
            } = next_h2_event(&mut stream, &mut protocol, &mut input)
            {
                assert!(
                    headers
                        .iter()
                        .any(|header| { header.name == b":path" && header.value == b"/cancel" })
                );
                break stream_id;
            }
        };
        if matches!(mode, H2CancellationMode::StreamingBody) {
            let commit = protocol
                .response_headers_frame_with_raw_headers(first_stream, 200, &[], false)
                .unwrap();
            let mut response = take_server_block(&mut protocol, commit);
            response.extend_from_slice(&protocol.data_frame(first_stream, b"partial", false));
            stream.write_all(&response).unwrap();
        }

        let mut reset_seen = false;
        let mut sibling_stream = None;
        while !reset_seen || sibling_stream.is_none() {
            match next_h2_event(&mut stream, &mut protocol, &mut input) {
                H2ByteStreamEvent::Reset { stream_id, .. } if stream_id == first_stream => {
                    reset_seen = true;
                }
                H2ByteStreamEvent::RequestHeaders {
                    stream_id, headers, ..
                } if headers
                    .iter()
                    .any(|header| header.name == b":path" && header.value == b"/sibling") =>
                {
                    sibling_stream = Some(stream_id);
                }
                _ => {}
            }
        }
        write_h2_response(
            &mut stream,
            &mut protocol,
            sibling_stream.unwrap(),
            b"sibling",
        );
        wait_for_tcp_delivery(&stream, Instant::now() + WAIT);
    });
    H2CancellationPeer { address, thread }
}

fn h2_request_path(headers: &[H2HeaderField]) -> String {
    let value = headers
        .iter()
        .find(|header| header.name == b":path")
        .expect("HTTP/2 request omitted :path")
        .value
        .as_slice();
    str::from_utf8(value).unwrap().to_owned()
}

fn max_concurrent_streams_settings(limit: usize) -> Vec<u8> {
    let limit = u32::try_from(limit).unwrap();
    let mut payload = vec![0, 3];
    payload.extend_from_slice(&limit.to_be_bytes());
    encode_h2_frame(H2FrameType::Settings, 0, 0, &payload)
}

struct H2CapacityPeer {
    address: SocketAddr,
    release: Arc<AtomicBool>,
    observed: Arc<AtomicUsize>,
    max_before_release: Arc<AtomicUsize>,
    thread: thread::JoinHandle<usize>,
}

impl H2CapacityPeer {
    fn release(&self) {
        self.release.store(true, Ordering::Release);
    }

    fn finish(self) -> usize {
        self.thread.join().expect("HTTP/2 capacity peer panicked")
    }
}

fn spawn_h2_capacity_peer(advertised_limit: usize, request_count: usize) -> H2CapacityPeer {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let release = Arc::new(AtomicBool::new(false));
    let observed = Arc::new(AtomicUsize::new(0));
    let max_before_release = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));
    let worker_release = Arc::clone(&release);
    let worker_observed = Arc::clone(&observed);
    let worker_max = Arc::clone(&max_before_release);
    let worker_completed = Arc::clone(&completed);
    let thread = thread::spawn(move || {
        let deadline = Instant::now() + WAIT;
        let mut accepts = 0;
        let mut workers = Vec::new();

        while worker_completed.load(Ordering::Acquire) < request_count {
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        accepts += 1;
                        stream.set_read_timeout(Some(WAIT)).unwrap();
                        stream.set_write_timeout(Some(WAIT)).unwrap();
                        let release = Arc::clone(&worker_release);
                        let observed = Arc::clone(&worker_observed);
                        let max_before_release = Arc::clone(&worker_max);
                        let completed = Arc::clone(&worker_completed);
                        workers.push(thread::spawn(move || {
                            let settings = max_concurrent_streams_settings(advertised_limit);
                            let mut protocol = H2Server::default();
                            let mut input = Vec::new();
                            while input.len() < 24 {
                                let mut buffer = [0; 4096];
                                let amount = stream.read(&mut buffer).unwrap();
                                assert_ne!(
                                    amount, 0,
                                    "capacity client closed during the HTTP/2 preface"
                                );
                                input.extend_from_slice(&buffer[..amount]);
                            }
                            let (event, consumed, output) =
                                protocol.accept_event_bytes(&input[..24]).unwrap();
                            assert!(event.is_none());
                            assert_eq!(consumed, 24);
                            assert!(output.is_empty());
                            input.drain(..consumed);

                            while input.len() < 9 {
                                let mut buffer = [0; 4096];
                                let amount = stream.read(&mut buffer).unwrap();
                                assert_ne!(
                                    amount, 0,
                                    "capacity client closed during its initial SETTINGS"
                                );
                                input.extend_from_slice(&buffer[..amount]);
                            }
                            let settings_len =
                                u32::from_be_bytes([0, input[0], input[1], input[2]]) as usize;
                            let settings_frame_len = 9 + settings_len;
                            while input.len() < settings_frame_len {
                                let mut buffer = [0; 4096];
                                let amount = stream.read(&mut buffer).unwrap();
                                assert_ne!(
                                    amount, 0,
                                    "capacity client closed during its initial SETTINGS"
                                );
                                input.extend_from_slice(&buffer[..amount]);
                            }
                            let (_, consumed, output) = protocol
                                .accept_event_bytes(&input[..settings_frame_len])
                                .unwrap();
                            assert_eq!(consumed, settings_frame_len);
                            assert!(!output.is_empty());
                            input.drain(..consumed);
                            let mut replacement = settings;
                            replacement.extend_from_slice(&encode_h2_frame(
                                H2FrameType::Settings,
                                0x1,
                                0,
                                &[],
                            ));
                            stream.write_all(&replacement).unwrap();
                            stream
                                .set_read_timeout(Some(Duration::from_millis(20)))
                                .unwrap();
                            let mut prime_stream: Option<u32> = None;
                            let mut held = Vec::<(u32, Vec<u8>)>::new();

                            loop {
                                if release.load(Ordering::Acquire) && !held.is_empty() {
                                    for (stream_id, body) in held.drain(..) {
                                        write_h2_response(
                                            &mut stream,
                                            &mut protocol,
                                            stream_id,
                                            &body,
                                        );
                                        completed.fetch_add(1, Ordering::AcqRel);
                                    }
                                }
                                if completed.load(Ordering::Acquire) >= request_count {
                                    return;
                                }

                                let mut made_progress = false;
                                if !input.is_empty() {
                                    let (event, consumed, output) =
                                        protocol.accept_event_bytes(&input).unwrap();
                                    if !output.is_empty() {
                                        stream.write_all(&output).unwrap();
                                    }
                                    if consumed != 0 {
                                        input.drain(..consumed);
                                        made_progress = true;
                                    }
                                    if let Some(H2ByteStreamEvent::RequestHeaders {
                                        stream_id,
                                        headers,
                                        ..
                                    }) = event
                                    {
                                        let path = h2_request_path(&headers);
                                        if path == "/prime" {
                                            assert!(prime_stream.replace(stream_id).is_none());
                                        } else {
                                            let index = path
                                                .strip_prefix("/work/")
                                                .expect("unexpected capacity request path");
                                            observed.fetch_add(1, Ordering::AcqRel);
                                            let body = format!("work-{index}").into_bytes();
                                            if release.load(Ordering::Acquire) {
                                                write_h2_response(
                                                    &mut stream,
                                                    &mut protocol,
                                                    stream_id,
                                                    &body,
                                                );
                                                completed.fetch_add(1, Ordering::AcqRel);
                                            } else {
                                                held.push((stream_id, body));
                                                max_before_release
                                                    .fetch_max(held.len(), Ordering::AcqRel);
                                            }
                                        }
                                    }
                                }

                                if protocol.control_diagnostics().settings_acks != 0
                                    && let Some(stream_id) = prime_stream.take()
                                {
                                    write_h2_response(
                                        &mut stream,
                                        &mut protocol,
                                        stream_id,
                                        b"prime",
                                    );
                                    made_progress = true;
                                }
                                if made_progress {
                                    continue;
                                }

                                let mut buffer = [0; 4096];
                                match stream.read(&mut buffer) {
                                    Ok(0) => {
                                        assert!(
                                            completed.load(Ordering::Acquire) >= request_count,
                                            "capacity client closed before all requests completed"
                                        );
                                        return;
                                    }
                                    Ok(amount) => input.extend_from_slice(&buffer[..amount]),
                                    Err(error)
                                        if matches!(
                                            error.kind(),
                                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                                        ) => {}
                                    Err(error) => panic!("capacity peer read failed: {error}"),
                                }
                            }
                        }));
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => panic!("capacity peer accept failed: {error}"),
                }
            }
            assert!(
                Instant::now() < deadline,
                "capacity peer timed out after {accepts} accepts, {} requests, and {} completions",
                worker_observed.load(Ordering::Acquire),
                worker_completed.load(Ordering::Acquire)
            );
            thread::sleep(Duration::from_millis(1));
        }

        for worker in workers {
            worker.join().expect("HTTP/2 capacity worker panicked");
        }
        accepts
    });

    H2CapacityPeer {
        address,
        release,
        observed,
        max_before_release,
        thread,
    }
}

struct H2PeerResetPeer {
    address: SocketAddr,
    thread: thread::JoinHandle<()>,
}

struct H2RefusedStreamPeer {
    address: SocketAddr,
    thread: thread::JoinHandle<usize>,
}

fn spawn_h2_refused_stream_peer() -> H2RefusedStreamPeer {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let thread = thread::spawn(move || {
        let deadline = Instant::now() + WAIT;
        let mut first = accept_before(&listener, deadline);
        first.set_read_timeout(Some(WAIT)).unwrap();
        first.set_write_timeout(Some(WAIT)).unwrap();
        let mut first_protocol = H2Server::default();
        let mut first_input = Vec::new();
        let first_stream = loop {
            if let H2ByteStreamEvent::RequestHeaders {
                stream_id, headers, ..
            } = next_h2_event(&mut first, &mut first_protocol, &mut first_input)
            {
                assert_eq!(h2_request_path(&headers), "/refused");
                assert!(
                    headers
                        .iter()
                        .any(|header| header.name == b":method" && header.value == b"POST")
                );
                break stream_id;
            }
        };
        first
            .write_all(
                &first_protocol
                    .rst_stream_frame_with_code(first_stream, H2ErrorCode::RefusedStream)
                    .unwrap(),
            )
            .unwrap();

        let mut second = accept_before(&listener, deadline);
        second.set_read_timeout(Some(WAIT)).unwrap();
        second.set_write_timeout(Some(WAIT)).unwrap();
        let mut second_protocol = H2Server::default();
        let mut second_input = Vec::new();
        let second_stream = loop {
            if let H2ByteStreamEvent::RequestHeaders {
                stream_id, headers, ..
            } = next_h2_event(&mut second, &mut second_protocol, &mut second_input)
            {
                assert_eq!(h2_request_path(&headers), "/refused");
                assert!(
                    headers
                        .iter()
                        .any(|header| header.name == b":method" && header.value == b"POST")
                );
                break stream_id;
            }
        };
        write_h2_response(
            &mut second,
            &mut second_protocol,
            second_stream,
            b"retried-refused",
        );
        wait_for_tcp_delivery(&second, deadline);
        2
    });
    H2RefusedStreamPeer { address, thread }
}

struct H2GracefulGoawayPeer {
    address: SocketAddr,
    thread: thread::JoinHandle<usize>,
}

fn spawn_h2_graceful_goaway_peer() -> H2GracefulGoawayPeer {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let thread = thread::spawn(move || {
        let deadline = Instant::now() + WAIT;
        let mut first = accept_before(&listener, deadline);
        first.set_read_timeout(Some(WAIT)).unwrap();
        first.set_write_timeout(Some(WAIT)).unwrap();
        let mut first_protocol = H2Server::default();
        let mut first_input = Vec::new();
        let mut requests = Vec::new();
        while requests.len() < 2 {
            if let H2ByteStreamEvent::RequestHeaders {
                stream_id, headers, ..
            } = next_h2_event(&mut first, &mut first_protocol, &mut first_input)
            {
                assert!(
                    headers
                        .iter()
                        .any(|header| header.name == b":method" && header.value == b"POST")
                );
                requests.push((stream_id, h2_request_path(&headers)));
            }
        }
        requests.sort_by_key(|(stream_id, _)| *stream_id);
        let (processed_stream, processed_path) = requests.remove(0);
        let (_, unprocessed_path) = requests.remove(0);

        let processed_body = format!("processed:{processed_path}");
        let commit = first_protocol
            .response_headers_frame_with_raw_headers(processed_stream, 200, &[], false)
            .unwrap();
        let mut wire = take_server_block(&mut first_protocol, commit);
        wire.extend_from_slice(&first_protocol.data_frame(
            processed_stream,
            processed_body.as_bytes(),
            true,
        ));
        wire.extend_from_slice(
            &first_protocol
                .goaway_frame(processed_stream, H2ErrorCode::NoError.as_u32())
                .unwrap(),
        );
        first.write_all(&wire).unwrap();

        let mut second = accept_before(&listener, deadline);
        second.set_read_timeout(Some(WAIT)).unwrap();
        second.set_write_timeout(Some(WAIT)).unwrap();
        let mut second_protocol = H2Server::default();
        let mut second_input = Vec::new();
        let retried_stream = loop {
            if let H2ByteStreamEvent::RequestHeaders {
                stream_id, headers, ..
            } = next_h2_event(&mut second, &mut second_protocol, &mut second_input)
            {
                assert_eq!(h2_request_path(&headers), unprocessed_path);
                assert!(
                    headers
                        .iter()
                        .any(|header| header.name == b":method" && header.value == b"POST")
                );
                break stream_id;
            }
        };
        let retried_body = format!("retried:{unprocessed_path}");
        write_h2_response(
            &mut second,
            &mut second_protocol,
            retried_stream,
            retried_body.as_bytes(),
        );
        wait_for_tcp_delivery(&second, deadline);
        2
    });
    H2GracefulGoawayPeer { address, thread }
}

struct H2PartialFramePeer {
    address: SocketAddr,
    done: Sender<()>,
    thread: thread::JoinHandle<usize>,
}

impl H2PartialFramePeer {
    fn finish(self) -> usize {
        let _ = self.done.send(());
        self.thread
            .join()
            .expect("partial-frame HTTP/2 peer panicked")
    }
}

fn spawn_h2_partial_frame_peer() -> H2PartialFramePeer {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let (done, done_receiver) = mpsc::channel();
    let thread = thread::spawn(move || {
        let deadline = Instant::now() + WAIT;
        let mut first = accept_before(&listener, deadline);
        first.set_read_timeout(Some(WAIT)).unwrap();
        first.set_write_timeout(Some(WAIT)).unwrap();
        let mut first_protocol = H2Server::default();
        let mut first_input = Vec::new();

        let prime_stream = loop {
            if let H2ByteStreamEvent::RequestHeaders {
                stream_id, headers, ..
            } = next_h2_event(&mut first, &mut first_protocol, &mut first_input)
            {
                assert_eq!(h2_request_path(&headers), "/prime");
                break stream_id;
            }
        };
        write_h2_response(&mut first, &mut first_protocol, prime_stream, b"prime");

        let partial_stream = loop {
            if let H2ByteStreamEvent::RequestHeaders {
                stream_id, headers, ..
            } = next_h2_event(&mut first, &mut first_protocol, &mut first_input)
            {
                assert_eq!(h2_request_path(&headers), "/partial");
                break stream_id;
            }
        };
        let partial = encode_h2_frame(H2FrameType::Headers, 0x5, partial_stream, &[0x88]);
        first.write_all(&partial[..9]).unwrap();
        drop(first);

        loop {
            match listener.accept() {
                Ok((mut second, _)) => {
                    second.set_read_timeout(Some(WAIT)).unwrap();
                    second.set_write_timeout(Some(WAIT)).unwrap();
                    let mut second_protocol = H2Server::default();
                    let mut second_input = Vec::new();
                    let retried_stream = loop {
                        if let H2ByteStreamEvent::RequestHeaders {
                            stream_id, headers, ..
                        } = next_h2_event(&mut second, &mut second_protocol, &mut second_input)
                        {
                            assert_eq!(h2_request_path(&headers), "/partial");
                            break stream_id;
                        }
                    };
                    write_h2_response(
                        &mut second,
                        &mut second_protocol,
                        retried_stream,
                        b"unexpected-retry",
                    );
                    return 2;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("partial-frame peer accept failed: {error}"),
            }
            if done_receiver.try_recv().is_ok() {
                return 1;
            }
            assert!(
                Instant::now() < deadline,
                "partial-frame peer timed out waiting for completion"
            );
            thread::sleep(Duration::from_millis(1));
        }
    });
    H2PartialFramePeer {
        address,
        done,
        thread,
    }
}

fn spawn_h2_peer_reset_peer() -> H2PeerResetPeer {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let thread = thread::spawn(move || {
        let mut stream = accept_before(&listener, Instant::now() + WAIT);
        stream.set_read_timeout(Some(WAIT)).unwrap();
        stream.set_write_timeout(Some(WAIT)).unwrap();
        let mut protocol = H2Server::default();
        let mut input = Vec::new();
        let mut requests = Vec::new();

        while requests.len() < 3 {
            if let H2ByteStreamEvent::RequestHeaders {
                stream_id, headers, ..
            } = next_h2_event(&mut stream, &mut protocol, &mut input)
            {
                requests.push((h2_request_path(&headers), stream_id));
            }
        }

        let reset_stream = requests
            .iter()
            .find_map(|(path, stream_id)| (path == "/reset").then_some(*stream_id))
            .expect("reset request was not observed");
        stream
            .write_all(&protocol.rst_stream_frame(reset_stream, 0x8).unwrap())
            .unwrap();
        for (path, stream_id) in requests {
            if path != "/reset" {
                let body = format!("{path}-response");
                write_h2_response(&mut stream, &mut protocol, stream_id, body.as_bytes());
            }
        }

        loop {
            if let H2ByteStreamEvent::RequestHeaders {
                stream_id, headers, ..
            } = next_h2_event(&mut stream, &mut protocol, &mut input)
            {
                assert_eq!(h2_request_path(&headers), "/after-reset");
                write_h2_response(&mut stream, &mut protocol, stream_id, b"after-reset");
                wait_for_tcp_delivery(&stream, Instant::now() + WAIT);
                return;
            }
        }
    });
    H2PeerResetPeer { address, thread }
}

struct H2FatalPeer {
    address: SocketAddr,
    fatal_sent: Arc<AtomicBool>,
    thread: thread::JoinHandle<usize>,
}

struct H2UnexpectedEofPeer {
    address: SocketAddr,
    thread: thread::JoinHandle<usize>,
}

fn spawn_h2_unexpected_eof_peer() -> H2UnexpectedEofPeer {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let thread = thread::spawn(move || {
        let deadline = Instant::now() + WAIT;
        let mut first = accept_before(&listener, deadline);
        first.set_read_timeout(Some(WAIT)).unwrap();
        first.set_write_timeout(Some(WAIT)).unwrap();
        let mut first_protocol = H2Server::default();
        let mut first_input = Vec::new();
        let mut requests = Vec::new();

        while requests.len() < 2 {
            if let H2ByteStreamEvent::RequestHeaders {
                headers,
                stream_id: _,
                ..
            } = next_h2_event(&mut first, &mut first_protocol, &mut first_input)
            {
                assert!(
                    headers
                        .iter()
                        .any(|header| header.name == b":method" && header.value == b"POST")
                );
                requests.push(h2_request_path(&headers));
            }
        }
        requests.sort();
        assert_eq!(requests, ["/eof/one", "/eof/two"]);
        drop(first);

        let mut second = accept_before(&listener, deadline);
        second.set_read_timeout(Some(WAIT)).unwrap();
        second.set_write_timeout(Some(WAIT)).unwrap();
        let mut second_protocol = H2Server::default();
        let mut second_input = Vec::new();
        let second_stream = loop {
            if let H2ByteStreamEvent::RequestHeaders {
                stream_id, headers, ..
            } = next_h2_event(&mut second, &mut second_protocol, &mut second_input)
            {
                assert_eq!(h2_request_path(&headers), "/after-eof");
                break stream_id;
            }
        };
        write_h2_response(&mut second, &mut second_protocol, second_stream, b"fresh");
        wait_for_tcp_delivery(&second, deadline);
        2
    });
    H2UnexpectedEofPeer { address, thread }
}

fn spawn_h2_fatal_peer() -> H2FatalPeer {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let fatal_sent = Arc::new(AtomicBool::new(false));
    let observed_fatal = Arc::clone(&fatal_sent);
    let thread = thread::spawn(move || {
        let deadline = Instant::now() + WAIT;
        let mut first = accept_before(&listener, deadline);
        first.set_read_timeout(Some(WAIT)).unwrap();
        first.set_write_timeout(Some(WAIT)).unwrap();
        let mut protocol = H2Server::default();
        let mut input = Vec::new();
        let mut streams = Vec::new();

        while streams.len() < 3 {
            if let H2ByteStreamEvent::RequestHeaders {
                stream_id, headers, ..
            } = next_h2_event(&mut first, &mut protocol, &mut input)
            {
                let path = h2_request_path(&headers);
                assert!(path.starts_with("/fatal/"));
                streams.push(stream_id);
            }
        }

        let mut fatal = Vec::new();
        for stream_id in streams {
            let commit = protocol
                .response_headers_frame_with_raw_headers(stream_id, 200, &[], false)
                .unwrap();
            fatal.extend_from_slice(&take_server_block(&mut protocol, commit));
            fatal.extend_from_slice(&protocol.data_frame(stream_id, b"partial", false));
        }
        fatal.extend_from_slice(&protocol.goaway_frame(0, 0x1).unwrap());
        first.write_all(&fatal).unwrap();
        wait_for_tcp_delivery(&first, deadline);
        observed_fatal.store(true, Ordering::Release);
        wait_for_h2_client_close(&mut first, &mut protocol, &mut input);

        let mut second = accept_before(&listener, deadline);
        second.set_read_timeout(Some(WAIT)).unwrap();
        second.set_write_timeout(Some(WAIT)).unwrap();
        let mut second_protocol = H2Server::default();
        let mut second_input = Vec::new();
        let stream_id = first_h2_request_after_settings_ack(
            &mut second,
            &mut second_protocol,
            &mut second_input,
        );
        write_h2_response(&mut second, &mut second_protocol, stream_id, b"fresh");
        wait_for_tcp_delivery(&second, deadline);
        2
    });

    H2FatalPeer {
        address,
        fatal_sent,
        thread,
    }
}

#[derive(Clone, Copy)]
enum H2ClientDropMode {
    Idle,
    InFlight,
}

struct H2ClientDropPeer {
    address: SocketAddr,
    request_seen: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    thread: thread::JoinHandle<()>,
}

fn spawn_h2_client_drop_peer(mode: H2ClientDropMode) -> H2ClientDropPeer {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let request_seen = Arc::new(AtomicBool::new(false));
    let closed = Arc::new(AtomicBool::new(false));
    let observed_request = Arc::clone(&request_seen);
    let observed_close = Arc::clone(&closed);
    let thread = thread::spawn(move || {
        let deadline = Instant::now() + WAIT;
        let mut stream = accept_before(&listener, deadline);
        stream.set_read_timeout(Some(WAIT)).unwrap();
        stream.set_write_timeout(Some(WAIT)).unwrap();
        let mut protocol = H2Server::default();
        let mut input = Vec::new();
        let stream_id = first_h2_request_after_settings_ack(&mut stream, &mut protocol, &mut input);
        observed_request.store(true, Ordering::Release);

        if matches!(mode, H2ClientDropMode::Idle) {
            write_h2_response(&mut stream, &mut protocol, stream_id, b"complete");
            wait_for_tcp_delivery(&stream, deadline);
        }

        wait_for_h2_client_close(&mut stream, &mut protocol, &mut input);
        observed_close.store(true, Ordering::Release);
    });
    H2ClientDropPeer {
        address,
        request_seen,
        closed,
        thread,
    }
}

async fn wait_for_http2_peer_flag(flag: &AtomicBool, timeout_message: &str) {
    operations::timeout_at(Instant::now() + WAIT, async {
        while !flag.load(Ordering::Acquire) {
            operations::sleep(Duration::from_millis(1)).await.unwrap();
        }
    })
    .await
    .expect(timeout_message);
}

struct H2IdleClosePeer {
    address: SocketAddr,
    closed: Arc<AtomicBool>,
    thread: thread::JoinHandle<usize>,
}

fn spawn_h2_idle_close_peer() -> H2IdleClosePeer {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let closed = Arc::new(AtomicBool::new(false));
    let observed_close = Arc::clone(&closed);
    let thread = thread::spawn(move || {
        let deadline = Instant::now() + WAIT;
        let mut first = accept_before(&listener, deadline);
        first.set_read_timeout(Some(WAIT)).unwrap();
        first.set_write_timeout(Some(WAIT)).unwrap();
        let mut protocol = H2Server::default();
        let mut input = Vec::new();
        let first_stream =
            first_h2_request_after_settings_ack(&mut first, &mut protocol, &mut input);
        write_h2_response(&mut first, &mut protocol, first_stream, b"first");
        wait_for_h2_client_close(&mut first, &mut protocol, &mut input);
        observed_close.store(true, Ordering::Release);

        let mut second = accept_before(&listener, deadline);
        second.set_read_timeout(Some(WAIT)).unwrap();
        second.set_write_timeout(Some(WAIT)).unwrap();
        let mut second_protocol = H2Server::default();
        let mut second_input = Vec::new();
        let second_stream = first_h2_request_after_settings_ack(
            &mut second,
            &mut second_protocol,
            &mut second_input,
        );
        write_h2_response(&mut second, &mut second_protocol, second_stream, b"second");
        wait_for_tcp_delivery(&second, deadline);
        2
    });
    H2IdleClosePeer {
        address,
        closed,
        thread,
    }
}

#[kimojio::test]
async fn sequential_http2_requests_reuse_exactly_one_connection() {
    let peer = spawn_counting_h2(2);
    let client = Client::new();

    for index in 1..=2 {
        let response = operations::timeout_at(
            Instant::now() + WAIT,
            client
                .get(format!("http://{}/request-{index}", peer.address))
                .version(Version::HTTP_2)
                .send(),
        )
        .await
        .expect("HTTP/2 request timed out")
        .unwrap();
        assert_eq!(
            response.body().as_bytes(),
            format!("response-{index}").as_bytes()
        );
    }

    assert_eq!(
        peer.thread.join().expect("HTTP/2 peer thread panicked"),
        1,
        "two sequential streams must use one TCP accept"
    );
}

#[kimojio::test]
async fn concurrent_http2_requests_share_exactly_one_connection() {
    let peer = spawn_counting_h2(2);
    let client = Client::new();
    let base = format!("http://{}", peer.address);

    let (first, second) = operations::timeout_at(Instant::now() + WAIT, async {
        futures::join!(
            client
                .get(format!("{base}/first"))
                .version(Version::HTTP_2)
                .send(),
            client
                .get(format!("{base}/second"))
                .version(Version::HTTP_2)
                .send(),
        )
    })
    .await
    .expect("concurrent HTTP/2 requests timed out");
    let mut bodies = [
        first.unwrap().body().as_bytes().to_vec(),
        second.unwrap().body().as_bytes().to_vec(),
    ];
    bodies.sort();
    assert_eq!(bodies, [b"response-1".to_vec(), b"response-2".to_vec()]);
    assert_eq!(
        peer.thread.join().expect("HTTP/2 peer thread panicked"),
        1,
        "two concurrent streams must use one TCP accept"
    );
}

async fn assert_http2_capacity(advertised_limit: usize, local_limit: usize, expected_limit: usize) {
    let peer = spawn_h2_capacity_peer(advertised_limit, 4);
    let client = Client::with_config(
        ClientConfig::new().set_limits(Limits::new().set_max_active_streams(local_limit)),
    )
    .unwrap();
    let base = format!("http://{}", peer.address);

    let prime = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .get(format!("{base}/prime"))
            .version(Version::HTTP_2)
            .send(),
    )
    .await
    .expect("HTTP/2 capacity prime timed out")
    .unwrap();
    assert_eq!(prime.body().as_bytes(), b"prime");

    let responses = operations::timeout_at(Instant::now() + WAIT, async {
        let requests = async {
            futures::join!(
                client
                    .get(format!("{base}/work/0"))
                    .version(Version::HTTP_2)
                    .send(),
                client
                    .get(format!("{base}/work/1"))
                    .version(Version::HTTP_2)
                    .send(),
                client
                    .get(format!("{base}/work/2"))
                    .version(Version::HTTP_2)
                    .send(),
                client
                    .get(format!("{base}/work/3"))
                    .version(Version::HTTP_2)
                    .send(),
            )
        };
        let release = async {
            let observed = operations::timeout_at(Instant::now() + WAIT, async {
                while peer.observed.load(Ordering::Acquire) < expected_limit {
                    operations::sleep(Duration::from_millis(1)).await.unwrap();
                }
            })
            .await;
            if observed.is_ok() {
                operations::sleep(Duration::from_millis(50)).await.unwrap();
            }
            peer.release();
            observed.expect("capacity peer did not observe the expected concurrent requests");
        };
        let (responses, ()) = futures::join!(requests, release);
        responses
    })
    .await
    .expect("limited concurrent HTTP/2 requests timed out");

    for (index, response) in [responses.0, responses.1, responses.2, responses.3]
        .into_iter()
        .enumerate()
    {
        assert_eq!(
            response.unwrap().body().as_bytes(),
            format!("work-{index}").as_bytes()
        );
    }
    assert_eq!(peer.observed.load(Ordering::Acquire), 4);
    assert_eq!(
        peer.max_before_release.load(Ordering::Acquire),
        expected_limit,
        "one connection exceeded the effective active-stream limit"
    );
    drop(client);
    assert!(peer.finish() >= 1);
}

#[kimojio::test]
async fn peer_http2_settings_limit_active_streams_per_connection() {
    assert_http2_capacity(2, 8, 2).await;
}

#[kimojio::test]
async fn local_http2_stream_limit_is_tighter_than_peer_settings() {
    assert_http2_capacity(5, 2, 2).await;
}

#[kimojio::test]
async fn dropped_http2_send_resets_only_its_stream() {
    let peer = spawn_h2_cancellation_peer(H2CancellationMode::PendingSend);
    let client = Client::new();
    let base = format!("http://{}", peer.address);

    let timed_out = operations::timeout_at(
        Instant::now() + Duration::from_millis(50),
        client
            .get(format!("{base}/cancel"))
            .version(Version::HTTP_2)
            .send(),
    )
    .await;
    assert!(
        timed_out.is_err(),
        "the first request unexpectedly completed"
    );

    let response = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .get(format!("{base}/sibling"))
            .version(Version::HTTP_2)
            .send(),
    )
    .await
    .expect("the sibling request timed out")
    .unwrap();
    assert_eq!(response.body().as_bytes(), b"sibling");
    peer.thread
        .join()
        .expect("HTTP/2 cancellation peer panicked");
}

#[kimojio::test]
async fn dropped_http2_streaming_body_resets_only_its_stream() {
    let peer = spawn_h2_cancellation_peer(H2CancellationMode::StreamingBody);
    let client = Client::new();
    let base = format!("http://{}", peer.address);

    let response = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .get(format!("{base}/cancel"))
            .version(Version::HTTP_2)
            .send_streaming(),
    )
    .await
    .expect("the streaming response head timed out")
    .unwrap();
    drop(response);

    let sibling = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .get(format!("{base}/sibling"))
            .version(Version::HTTP_2)
            .send(),
    )
    .await
    .expect("the sibling request timed out")
    .unwrap();
    assert_eq!(sibling.body().as_bytes(), b"sibling");
    peer.thread
        .join()
        .expect("HTTP/2 cancellation peer panicked");
}

#[kimojio::test]
async fn stalled_http2_streaming_body_times_out_only_its_stream() {
    let peer = spawn_h2_cancellation_peer(H2CancellationMode::StreamingBody);
    let client = Client::with_config(
        ClientConfig::new().set_connection_io_timeout(Duration::from_millis(75)),
    )
    .unwrap();
    let base = format!("http://{}", peer.address);

    let mut response = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .get(format!("{base}/cancel"))
            .version(Version::HTTP_2)
            .send_streaming(),
    )
    .await
    .expect("the streaming response head timed out")
    .unwrap();
    assert_eq!(
        response.body_mut().next_chunk().await.unwrap().unwrap(),
        b"partial"
    );
    let error = response.body_mut().next_chunk().await.unwrap_err();
    assert!(matches!(
        error,
        kimojio::http::Error::Io(kimojio::Errno::TIME | kimojio::Errno::TIMEDOUT)
    ));

    let sibling = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .get(format!("{base}/sibling"))
            .version(Version::HTTP_2)
            .send(),
    )
    .await
    .expect("the sibling request timed out")
    .unwrap();
    assert_eq!(sibling.body().as_bytes(), b"sibling");
    peer.thread
        .join()
        .expect("HTTP/2 cancellation peer panicked");
}

#[kimojio::test]
async fn peer_http2_reset_fails_only_its_stream_and_connection_survives() {
    let peer = spawn_h2_peer_reset_peer();
    let client = Client::new();
    let base = format!("http://{}", peer.address);

    let (reset, first, second) = operations::timeout_at(Instant::now() + WAIT, async {
        futures::join!(
            client
                .get(format!("{base}/reset"))
                .version(Version::HTTP_2)
                .send(),
            client
                .get(format!("{base}/sibling-one"))
                .version(Version::HTTP_2)
                .send(),
            client
                .get(format!("{base}/sibling-two"))
                .version(Version::HTTP_2)
                .send(),
        )
    })
    .await
    .expect("peer-reset HTTP/2 requests timed out");

    assert!(matches!(
        reset.unwrap_err(),
        Error::Protocol(error) if error.kind() == ProtocolErrorKind::PeerReset
    ));
    assert_eq!(first.unwrap().body().as_bytes(), b"/sibling-one-response");
    assert_eq!(second.unwrap().body().as_bytes(), b"/sibling-two-response");

    let after = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .get(format!("{base}/after-reset"))
            .version(Version::HTTP_2)
            .send(),
    )
    .await
    .expect("request after peer reset timed out")
    .unwrap();
    assert_eq!(after.body().as_bytes(), b"after-reset");
    drop(client);
    peer.thread.join().expect("HTTP/2 reset peer panicked");
}

#[kimojio::test]
async fn refused_stream_retries_non_idempotent_request_on_fresh_connection() {
    let peer = spawn_h2_refused_stream_peer();
    let client = Client::new();
    let (event_sender, event_receiver) = mpsc::channel();

    let response = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .post(format!("http://{}/refused", peer.address))
            .version(Version::HTTP_2)
            .send_with_event_handler(move |event| event_sender.send(event).unwrap()),
    )
    .await
    .expect("REFUSED_STREAM retry timed out")
    .unwrap();

    assert_eq!(response.body().as_bytes(), b"retried-refused");
    assert_eq!(
        event_receiver.try_iter().collect::<Vec<_>>(),
        [ClientEvent::RequestRetried],
        "REFUSED_STREAM must enter the fresh-connection retry branch"
    );
    drop(client);
    assert_eq!(peer.thread.join().expect("REFUSED_STREAM peer panicked"), 2);
}

#[kimojio::test]
async fn graceful_http2_goaway_retries_only_unprocessed_stream() {
    let peer = spawn_h2_graceful_goaway_peer();
    let client = Client::new();
    let base = format!("http://{}", peer.address);
    let (event_sender, event_receiver) = mpsc::channel();
    let first_events = event_sender.clone();

    let (first, second) = operations::timeout_at(Instant::now() + WAIT, async {
        futures::join!(
            client
                .post(format!("{base}/goaway/one"))
                .version(Version::HTTP_2)
                .send_with_event_handler(move |event| first_events.send(event).unwrap()),
            client
                .post(format!("{base}/goaway/two"))
                .version(Version::HTTP_2)
                .send_with_event_handler(move |event| event_sender.send(event).unwrap()),
        )
    })
    .await
    .expect("graceful GOAWAY requests timed out");

    let first = first.unwrap();
    let second = second.unwrap();
    let bodies = [
        str::from_utf8(first.body().as_bytes()).unwrap(),
        str::from_utf8(second.body().as_bytes()).unwrap(),
    ];
    assert!(
        bodies
            .iter()
            .any(|body| body.starts_with("processed:/goaway/"))
    );
    assert!(
        bodies
            .iter()
            .any(|body| body.starts_with("retried:/goaway/"))
    );
    assert_eq!(
        event_receiver.try_iter().collect::<Vec<_>>(),
        [ClientEvent::RequestRetried],
        "only the stream above GOAWAY's last-stream ID may replay"
    );
    drop(client);
    assert_eq!(
        peer.thread.join().expect("graceful GOAWAY peer panicked"),
        2
    );
}

#[kimojio::test]
async fn partial_http2_response_frame_prevents_retry() {
    let peer = spawn_h2_partial_frame_peer();
    let client = Client::new();
    let base = format!("http://{}", peer.address);

    let prime = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .get(format!("{base}/prime"))
            .version(Version::HTTP_2)
            .send(),
    )
    .await
    .expect("prime HTTP/2 request timed out")
    .unwrap();
    assert_eq!(prime.body().as_bytes(), b"prime");

    let (event_sender, event_receiver) = mpsc::channel();
    let partial = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .get(format!("{base}/partial"))
            .version(Version::HTTP_2)
            .send_with_event_handler(move |event| event_sender.send(event).unwrap()),
    )
    .await
    .expect("partial-frame HTTP/2 request timed out");

    assert!(
        partial.is_err(),
        "a started response frame must suppress replay"
    );
    assert!(
        event_receiver.try_iter().next().is_none(),
        "response-frame attribution must keep the retry branch closed"
    );
    drop(client);
    assert_eq!(
        peer.finish(),
        1,
        "the partially received response must not open a retry connection"
    );
}

#[kimojio::test]
async fn max_requests_per_connection_retires_http2_client_connection() {
    let peer = spawn_counting_h2_connections(vec![2, 1]);
    let client = Client::with_config(
        ClientConfig::new().set_limits(Limits::new().set_max_requests_per_connection(2)),
    )
    .unwrap();

    for index in 1..=3 {
        let response = operations::timeout_at(
            Instant::now() + WAIT,
            client
                .get(format!("http://{}/request-{index}", peer.address))
                .version(Version::HTTP_2)
                .send(),
        )
        .await
        .expect("HTTP/2 request timed out")
        .unwrap();
        assert_eq!(
            response.body().as_bytes(),
            format!("response-{index}").as_bytes()
        );
    }

    assert_eq!(
        peer.thread.join().expect("HTTP/2 peer thread panicked"),
        2,
        "the client must retire an HTTP/2 connection after two completed requests"
    );
}

#[kimojio::test]
async fn concurrent_http2_requests_reserve_connection_request_limit() {
    let peer = spawn_counting_h2_connections(vec![2, 1]);
    let client = Client::with_config(
        ClientConfig::new().set_limits(Limits::new().set_max_requests_per_connection(2)),
    )
    .unwrap();
    let base = format!("http://{}", peer.address);

    let responses = operations::timeout_at(Instant::now() + WAIT, async {
        futures::join!(
            client
                .get(format!("{base}/one"))
                .version(Version::HTTP_2)
                .send(),
            client
                .get(format!("{base}/two"))
                .version(Version::HTTP_2)
                .send(),
            client
                .get(format!("{base}/three"))
                .version(Version::HTTP_2)
                .send(),
        )
    })
    .await
    .expect("concurrent limited HTTP/2 requests timed out");
    assert!(responses.0.is_ok());
    assert!(responses.1.is_ok());
    assert!(responses.2.is_ok());
    assert_eq!(
        peer.thread.join().expect("HTTP/2 peer thread panicked"),
        2,
        "completed plus active streams must reserve the request limit"
    );
}

#[kimojio::test]
async fn zero_idle_timeout_disables_http2_session_reuse() {
    let peer = spawn_counting_h2_connections(vec![1, 1]);
    let client =
        Client::with_config(ClientConfig::new().set_pool_idle_timeout(Duration::ZERO)).unwrap();

    for path in ["one", "two"] {
        operations::timeout_at(
            Instant::now() + WAIT,
            client
                .get(format!("http://{}/{path}", peer.address))
                .version(Version::HTTP_2)
                .send(),
        )
        .await
        .expect("HTTP/2 request timed out")
        .unwrap();
    }
    assert_eq!(peer.thread.join().expect("HTTP/2 peer thread panicked"), 2);
}

#[kimojio::test]
async fn unexpected_http2_eof_fails_all_in_flight_and_retires_session() {
    let peer = spawn_h2_unexpected_eof_peer();
    let client = Client::new();
    let base = format!("http://{}", peer.address);

    let (first, second) = operations::timeout_at(Instant::now() + WAIT, async {
        futures::join!(
            client
                .post(format!("{base}/eof/one"))
                .version(Version::HTTP_2)
                .send(),
            client
                .post(format!("{base}/eof/two"))
                .version(Version::HTTP_2)
                .send(),
        )
    })
    .await
    .expect("in-flight HTTP/2 requests hung after peer EOF");

    for failure in [first, second] {
        assert!(matches!(
            failure.unwrap_err(),
            Error::UnexpectedEof | Error::Io(_)
        ));
    }

    let fresh = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .get(format!("{base}/after-eof"))
            .version(Version::HTTP_2)
            .send(),
    )
    .await
    .expect("request after HTTP/2 peer EOF timed out")
    .unwrap();
    assert_eq!(fresh.body().as_bytes(), b"fresh");
    drop(client);
    assert_eq!(
        peer.thread
            .join()
            .expect("unexpected-EOF HTTP/2 peer panicked"),
        2
    );
}

#[kimojio::test]
async fn fatal_http2_goaway_fails_all_in_flight_and_racing_request_recovers() {
    let peer = spawn_h2_fatal_peer();
    let client = Client::new();
    let base = format!("http://{}", peer.address);

    let (failed, fresh) = operations::timeout_at(Instant::now() + WAIT, async {
        futures::join!(
            async {
                futures::join!(
                    client
                        .get(format!("{base}/fatal/one"))
                        .version(Version::HTTP_2)
                        .send(),
                    client
                        .get(format!("{base}/fatal/two"))
                        .version(Version::HTTP_2)
                        .send(),
                    client
                        .get(format!("{base}/fatal/three"))
                        .version(Version::HTTP_2)
                        .send(),
                )
            },
            async {
                while !peer.fatal_sent.load(Ordering::Acquire) {
                    operations::sleep(Duration::from_millis(1)).await.unwrap();
                }
                // This submission can reach the old command queue before or
                // after the pump closes it; either outcome must terminate.
                client
                    .get(format!("{base}/after-fatal"))
                    .version(Version::HTTP_2)
                    .send()
                    .await
            },
        )
    })
    .await
    .expect("fatal HTTP/2 requests or recovery timed out");

    for failure in [failed.0, failed.1, failed.2] {
        assert!(matches!(
            failure.unwrap_err(),
            Error::Protocol(error) if error.kind() == ProtocolErrorKind::PeerGoaway
        ));
    }
    assert_eq!(fresh.unwrap().body().as_bytes(), b"fresh");
    drop(client);
    assert_eq!(peer.thread.join().expect("fatal HTTP/2 peer panicked"), 2);
}

#[kimojio::test]
async fn dropping_last_client_shuts_down_idle_http2_pump() {
    let peer = spawn_h2_client_drop_peer(H2ClientDropMode::Idle);
    let client = Client::new();

    let response = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .get(format!("http://{}/idle", peer.address))
            .version(Version::HTTP_2)
            .send(),
    )
    .await
    .expect("idle-shutdown HTTP/2 request timed out")
    .unwrap();
    assert_eq!(response.body().as_bytes(), b"complete");

    drop(client);
    wait_for_http2_peer_flag(
        &peer.closed,
        "peer did not observe idle pump shutdown after Client drop",
    )
    .await;
    peer.thread
        .join()
        .expect("idle Client-drop HTTP/2 peer panicked");
}

#[kimojio::test]
async fn dropping_last_client_with_in_flight_request_closes_http2_transport() {
    let peer = spawn_h2_client_drop_peer(H2ClientDropMode::InFlight);
    let address = peer.address;
    let request = operations::spawn_task(async move {
        let client = Client::new();
        client
            .post(format!("http://{address}/in-flight"))
            .version(Version::HTTP_2)
            .send()
            .await
    });

    wait_for_http2_peer_flag(
        &peer.request_seen,
        "peer did not observe the in-flight HTTP/2 request",
    )
    .await;

    // Aborting drops the task's last Client and its pending send future. The
    // independently spawned pump must still observe pool shutdown and exit.
    request.abort();
    let aborted = operations::timeout_at(Instant::now() + WAIT, request)
        .await
        .expect("aborted HTTP/2 request task did not terminate");
    assert!(matches!(aborted, Err(kimojio::TaskHandleError::Panic(_))));

    wait_for_http2_peer_flag(
        &peer.closed,
        "peer did not observe in-flight pump shutdown after Client drop",
    )
    .await;
    peer.thread
        .join()
        .expect("in-flight Client-drop HTTP/2 peer panicked");
}

#[kimojio::test]
async fn idle_http2_pump_closes_connection_at_pool_timeout() {
    let peer = spawn_h2_idle_close_peer();
    let client =
        Client::with_config(ClientConfig::new().set_pool_idle_timeout(Duration::from_millis(40)))
            .unwrap();
    let base = format!("http://{}", peer.address);

    let first = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .get(format!("{base}/first"))
            .version(Version::HTTP_2)
            .send(),
    )
    .await
    .expect("first idle-timeout request timed out")
    .unwrap();
    assert_eq!(first.body().as_bytes(), b"first");

    operations::timeout_at(Instant::now() + WAIT, async {
        while !peer.closed.load(Ordering::Acquire) {
            operations::sleep(Duration::from_millis(1)).await.unwrap();
        }
    })
    .await
    .expect("peer did not observe the idle HTTP/2 connection close");

    let second = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .get(format!("{base}/second"))
            .version(Version::HTTP_2)
            .send(),
    )
    .await
    .expect("request after idle HTTP/2 pump exit timed out")
    .unwrap();
    assert_eq!(second.body().as_bytes(), b"second");
    drop(client);
    assert_eq!(
        peer.thread.join().expect("idle-close HTTP/2 peer panicked"),
        2
    );
}

#[derive(Clone, Copy)]
enum IdleH2Injection {
    Ping,
    SettingsAndWindowUpdate,
    Goaway,
    UnsolicitedData,
    UnsolicitedHeaders,
}

#[derive(Debug)]
struct IdleH2Observation {
    accepts: usize,
    ping_acks: u64,
    settings_acks: u64,
}

struct IdleH2Peer {
    address: SocketAddr,
    inject: Sender<()>,
    injected: Receiver<()>,
    thread: thread::JoinHandle<IdleH2Observation>,
}

impl IdleH2Peer {
    fn inject(&self) {
        self.inject
            .send(())
            .expect("idle HTTP/2 peer stopped before injection");
        self.injected
            .recv_timeout(WAIT)
            .expect("timed out waiting for idle HTTP/2 injection");
    }

    fn finish(self) -> IdleH2Observation {
        self.thread.join().expect("idle HTTP/2 peer panicked")
    }
}

fn encode_h2_frame(frame_type: H2FrameType, flags: u8, stream_id: u32, payload: &[u8]) -> Vec<u8> {
    let mut wire = Vec::new();
    H2Frame {
        frame_type,
        flags,
        stream_id,
        payload: payload.to_vec(),
    }
    .encode(&mut wire);
    wire
}

fn next_h2_event(
    stream: &mut TcpStream,
    protocol: &mut H2Server,
    input: &mut Vec<u8>,
) -> H2ByteStreamEvent {
    loop {
        if !input.is_empty() {
            let (event, consumed, output) = protocol.accept_event_bytes(input).unwrap();
            if !output.is_empty() {
                stream.write_all(&output).unwrap();
            }
            if consumed != 0 {
                input.drain(..consumed);
            }
            if let Some(event) = event {
                return event;
            }
            if consumed != 0 {
                continue;
            }
        }

        let mut buffer = [0; 4096];
        let amount = stream.read(&mut buffer).unwrap();
        assert_ne!(amount, 0, "HTTP/2 client closed before the next event");
        input.extend_from_slice(&buffer[..amount]);
    }
}

fn first_h2_request_after_settings_ack(
    stream: &mut TcpStream,
    protocol: &mut H2Server,
    input: &mut Vec<u8>,
) -> u32 {
    let mut stream_id = None;
    while stream_id.is_none() || protocol.control_diagnostics().settings_acks == 0 {
        if let H2ByteStreamEvent::RequestHeaders {
            stream_id: observed,
            ..
        } = next_h2_event(stream, protocol, input)
        {
            assert!(stream_id.replace(observed).is_none());
        }
    }
    stream_id.unwrap()
}

fn write_h2_response(stream: &mut TcpStream, protocol: &mut H2Server, stream_id: u32, body: &[u8]) {
    let commit = protocol
        .response_headers_frame_with_raw_headers(stream_id, 200, &[], false)
        .unwrap();
    let mut response = take_server_block(protocol, commit);
    response.extend_from_slice(&protocol.data_frame(stream_id, body, true));
    stream.write_all(&response).unwrap();
}

fn spawn_idle_h2_peer(injection: IdleH2Injection) -> IdleH2Peer {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let (inject, inject_receiver) = mpsc::channel();
    let (injected_sender, injected) = mpsc::channel();
    let thread = thread::spawn(move || {
        let deadline = Instant::now() + WAIT;
        let mut first = accept_before(&listener, deadline);
        first.set_read_timeout(Some(WAIT)).unwrap();
        first.set_write_timeout(Some(WAIT)).unwrap();
        let mut protocol = H2Server::default();
        let mut input = Vec::new();

        let first_stream =
            first_h2_request_after_settings_ack(&mut first, &mut protocol, &mut input);
        write_h2_response(&mut first, &mut protocol, first_stream, b"first");

        inject_receiver
            .recv_timeout(WAIT)
            .expect("timed out waiting to inject idle HTTP/2 traffic");
        let baseline = protocol.control_diagnostics();
        let wire = match injection {
            IdleH2Injection::Ping => encode_h2_frame(H2FrameType::Ping, 0, 0, b"idleping"),
            IdleH2Injection::SettingsAndWindowUpdate => {
                protocol.mark_local_settings_sent();
                let mut wire = encode_h2_frame(H2FrameType::Settings, 0, 0, &[]);
                wire.extend_from_slice(&encode_h2_frame(
                    H2FrameType::WindowUpdate,
                    0,
                    0,
                    &1024u32.to_be_bytes(),
                ));
                wire
            }
            IdleH2Injection::Goaway => protocol.goaway_frame(first_stream, 0).unwrap(),
            IdleH2Injection::UnsolicitedData => {
                encode_h2_frame(H2FrameType::Data, 0x1, first_stream + 2, b"smuggled")
            }
            IdleH2Injection::UnsolicitedHeaders => {
                encode_h2_frame(H2FrameType::Headers, 0x5, first_stream + 2, &[0x88])
            }
        };
        first.write_all(&wire).unwrap();
        wait_for_tcp_delivery(&first, deadline);
        injected_sender.send(()).unwrap();

        let reusable = matches!(
            injection,
            IdleH2Injection::Ping | IdleH2Injection::SettingsAndWindowUpdate
        );
        let (accepts, final_diagnostics) = if reusable {
            listener.set_nonblocking(true).unwrap();
            first.set_nonblocking(true).unwrap();
            'second_request: loop {
                match listener.accept() {
                    Ok((mut replacement, _)) => {
                        replacement.set_read_timeout(Some(WAIT)).unwrap();
                        replacement.set_write_timeout(Some(WAIT)).unwrap();
                        let mut replacement_protocol = H2Server::default();
                        let mut replacement_input = Vec::new();
                        let second_stream = first_h2_request_after_settings_ack(
                            &mut replacement,
                            &mut replacement_protocol,
                            &mut replacement_input,
                        );
                        write_h2_response(
                            &mut replacement,
                            &mut replacement_protocol,
                            second_stream,
                            b"second",
                        );
                        break 'second_request (2, protocol.control_diagnostics());
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) => panic!("idle HTTP/2 replacement accept failed: {error}"),
                }

                let mut buffer = [0; 4096];
                match first.read(&mut buffer) {
                    Ok(0) => {}
                    Ok(amount) => input.extend_from_slice(&buffer[..amount]),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset
                        ) => {}
                    Err(error) => panic!("idle HTTP/2 read failed: {error}"),
                }
                while !input.is_empty() {
                    let (event, consumed, output) = protocol.accept_event_bytes(&input).unwrap();
                    if !output.is_empty() {
                        write_nonblocking(&mut first, &output, deadline);
                    }
                    if consumed != 0 {
                        input.drain(..consumed);
                    }
                    if let Some(H2ByteStreamEvent::RequestHeaders { stream_id, .. }) = event {
                        first.set_nonblocking(false).unwrap();
                        write_h2_response(&mut first, &mut protocol, stream_id, b"second");
                        break 'second_request (1, protocol.control_diagnostics());
                    }
                    if consumed == 0 {
                        break;
                    }
                }

                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for the second HTTP/2 request"
                );
                thread::yield_now();
            }
        } else {
            let mut second = accept_before(&listener, deadline);
            second.set_read_timeout(Some(WAIT)).unwrap();
            second.set_write_timeout(Some(WAIT)).unwrap();
            let mut second_protocol = H2Server::default();
            let mut second_input = Vec::new();
            let second_stream = first_h2_request_after_settings_ack(
                &mut second,
                &mut second_protocol,
                &mut second_input,
            );
            write_h2_response(&mut second, &mut second_protocol, second_stream, b"second");
            (2, protocol.control_diagnostics())
        };
        IdleH2Observation {
            accepts,
            ping_acks: final_diagnostics
                .ping_acks
                .saturating_sub(baseline.ping_acks),
            settings_acks: final_diagnostics
                .settings_acks
                .saturating_sub(baseline.settings_acks),
        }
    });
    IdleH2Peer {
        address,
        inject,
        injected,
        thread,
    }
}

async fn exercise_idle_h2_peer(peer: &IdleH2Peer) {
    let client = Client::new();
    let first = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .get(format!("http://{}/first", peer.address))
            .version(Version::HTTP_2)
            .send(),
    )
    .await
    .expect("first HTTP/2 request timed out")
    .unwrap();
    assert_eq!(first.body().as_bytes(), b"first");

    peer.inject();
    let second = operations::timeout_at(
        Instant::now() + WAIT,
        client
            .get(format!("http://{}/second", peer.address))
            .version(Version::HTTP_2)
            .send(),
    )
    .await
    .expect("second HTTP/2 request timed out")
    .unwrap();
    assert_eq!(second.body().as_bytes(), b"second");
}

#[kimojio::test]
async fn idle_http2_ping_is_acked_and_connection_is_reused() {
    let peer = spawn_idle_h2_peer(IdleH2Injection::Ping);
    exercise_idle_h2_peer(&peer).await;
    let observation = peer.finish();

    assert_eq!(observation.accepts, 1);
    assert_eq!(observation.ping_acks, 1);
}

#[kimojio::test]
async fn idle_http2_settings_and_window_update_keep_connection_reusable() {
    let peer = spawn_idle_h2_peer(IdleH2Injection::SettingsAndWindowUpdate);
    exercise_idle_h2_peer(&peer).await;
    let observation = peer.finish();

    assert_eq!(observation.accepts, 1);
    assert_eq!(observation.settings_acks, 1);
}

#[kimojio::test]
async fn idle_http2_goaway_retires_connection() {
    let peer = spawn_idle_h2_peer(IdleH2Injection::Goaway);
    exercise_idle_h2_peer(&peer).await;
    assert_eq!(peer.finish().accepts, 2);
}

#[kimojio::test]
async fn unsolicited_http2_data_and_headers_on_idle_connections_are_discarded() {
    for injection in [
        IdleH2Injection::UnsolicitedData,
        IdleH2Injection::UnsolicitedHeaders,
    ] {
        let peer = spawn_idle_h2_peer(injection);
        exercise_idle_h2_peer(&peer).await;
        assert_eq!(peer.finish().accepts, 2);
    }
}

#[cfg(feature = "tls")]
fn test_tls_acceptor() -> SslAcceptor {
    let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
    let mut name = X509NameBuilder::new().unwrap();
    name.append_entry_by_text("CN", "127.0.0.1").unwrap();
    let name = name.build();
    let mut serial = BigNum::new().unwrap();
    serial.rand(128, MsbOption::MAYBE_ZERO, false).unwrap();
    let serial = serial.to_asn1_integer().unwrap();
    let mut certificate = X509::builder().unwrap();
    certificate.set_version(2).unwrap();
    certificate.set_serial_number(&serial).unwrap();
    certificate.set_subject_name(&name).unwrap();
    certificate.set_issuer_name(&name).unwrap();
    certificate.set_pubkey(&key).unwrap();
    certificate
        .set_not_before(Asn1Time::days_from_now(0).unwrap().as_ref())
        .unwrap();
    certificate
        .set_not_after(Asn1Time::days_from_now(1).unwrap().as_ref())
        .unwrap();
    let context = certificate.x509v3_context(None, None);
    let subject_alt_name = SubjectAlternativeName::new()
        .ip("127.0.0.1")
        .build(&context)
        .unwrap();
    certificate.append_extension(subject_alt_name).unwrap();
    certificate.sign(&key, MessageDigest::sha256()).unwrap();
    let certificate = certificate.build();

    let mut acceptor = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server()).unwrap();
    acceptor.set_private_key(&key).unwrap();
    acceptor.set_certificate(&certificate).unwrap();
    acceptor.check_private_key().unwrap();
    acceptor
        .set_min_proto_version(Some(SslVersion::TLS1_3))
        .unwrap();
    acceptor
        .set_max_proto_version(Some(SslVersion::TLS1_3))
        .unwrap();
    acceptor.set_num_tickets(0).unwrap();
    acceptor.set_alpn_select_callback(|_, offered| {
        let mut offset = 0;
        while offset < offered.len() {
            let length = usize::from(offered[offset]);
            let start = offset + 1;
            let end = start + length;
            if end > offered.len() {
                return Err(AlpnError::ALERT_FATAL);
            }
            if &offered[start..end] == b"http/1.1" {
                return Ok(&offered[start..end]);
            }
            offset = end;
        }
        Err(AlpnError::ALERT_FATAL)
    });
    acceptor.build()
}

#[cfg(feature = "tls")]
fn test_tls_client_config() -> TlsClientConfig {
    let mut builder = SslContextBuilder::new(SslMethod::tls_client()).unwrap();
    builder.set_verify(SslVerifyMode::NONE);
    builder
        .set_min_proto_version(Some(SslVersion::TLS1_3))
        .unwrap();
    builder
        .set_max_proto_version(Some(SslVersion::TLS1_3))
        .unwrap();
    TlsClientConfig::new(builder, &[AlpnProtocol::Http11]).unwrap()
}

#[cfg(feature = "tls")]
unsafe extern "C" {
    fn SSL_new_session_ticket(ssl: *mut openssl_sys::SSL) -> libc::c_int;
}

#[cfg(feature = "tls")]
fn send_new_session_ticket(stream: &mut SslStream<TcpStream>, deadline: Instant) {
    assert_eq!(stream.ssl().version_str(), "TLSv1.3");
    // SAFETY: `stream` owns a live server-side SSL object. OpenSSL's Rust
    // bindings do not yet expose SSL_new_session_ticket, so this test calls the
    // documented OpenSSL 3 API directly to schedule one post-handshake ticket.
    let scheduled = unsafe { SSL_new_session_ticket(stream.ssl().as_ptr()) };
    assert_eq!(scheduled, 1, "failed to schedule a TLS session ticket");
    stream
        .do_handshake()
        .expect("failed to emit the queued TLS session ticket");
    stream.flush().unwrap();
    wait_for_tcp_delivery(stream.get_ref(), deadline);
}

#[cfg(feature = "tls")]
struct TlsTicketPeer {
    address: SocketAddr,
    inject: Sender<()>,
    injected: Receiver<()>,
    thread: thread::JoinHandle<(usize, Vec<ObservedRequest>)>,
}

#[cfg(feature = "tls")]
impl TlsTicketPeer {
    fn inject_ticket(&self) {
        self.inject
            .send(())
            .expect("TLS ticket peer stopped before injection");
        self.injected
            .recv_timeout(WAIT)
            .expect("timed out waiting for the TLS session ticket");
    }

    fn finish(self) -> (usize, Vec<ObservedRequest>) {
        self.thread.join().expect("TLS ticket peer panicked")
    }
}

#[cfg(feature = "tls")]
fn spawn_tls_ticket_peer() -> TlsTicketPeer {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let (inject, inject_receiver) = mpsc::channel();
    let (injected_sender, injected) = mpsc::channel();
    let thread = thread::spawn(move || {
        let deadline = Instant::now() + WAIT;
        let socket = accept_before(&listener, deadline);
        socket.set_read_timeout(Some(WAIT)).unwrap();
        socket.set_write_timeout(Some(WAIT)).unwrap();
        let acceptor = test_tls_acceptor();
        let mut stream = acceptor.accept(socket).unwrap();
        assert_eq!(stream.ssl().version_str(), "TLSv1.3");

        let mut requests = vec![read_blocking_request(&mut stream)];
        stream
            .write_all(&TestResponse::new(b"first").wire())
            .unwrap();
        stream.flush().unwrap();

        inject_receiver
            .recv_timeout(WAIT)
            .expect("timed out waiting to send the TLS session ticket");
        send_new_session_ticket(&mut stream, deadline);
        injected_sender.send(()).unwrap();

        listener.set_nonblocking(true).unwrap();
        stream.get_ref().set_nonblocking(true).unwrap();
        let mut first_input = Vec::new();
        loop {
            match listener.accept() {
                Ok((socket, _)) => {
                    socket.set_read_timeout(Some(WAIT)).unwrap();
                    socket.set_write_timeout(Some(WAIT)).unwrap();
                    let mut replacement = acceptor.accept(socket).unwrap();
                    requests.push(read_blocking_request(&mut replacement));
                    replacement
                        .write_all(
                            &TestResponse::closing(b"second", &[("connection", "close")]).wire(),
                        )
                        .unwrap();
                    return (2, requests);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("TLS ticket accept failed: {error}"),
            }

            let mut buffer = [0; 4096];
            match stream.read(&mut buffer) {
                Ok(0) => {}
                Ok(amount) => first_input.extend_from_slice(&buffer[..amount]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset
                    ) => {}
                Err(error) => panic!("TLS ticket read failed: {error}"),
            }
            if let Some((request, _)) = parse_request(&first_input) {
                requests.push(request);
                stream.get_ref().set_nonblocking(false).unwrap();
                stream
                    .write_all(&TestResponse::closing(b"second", &[("connection", "close")]).wire())
                    .unwrap();
                return (1, requests);
            }

            assert!(
                Instant::now() < deadline,
                "timed out waiting for the second TLS request"
            );
            thread::yield_now();
        }
    });
    TlsTicketPeer {
        address,
        inject,
        injected,
        thread,
    }
}

#[cfg(feature = "tls")]
struct MixedSchemePeer {
    address: SocketAddr,
    thread: thread::JoinHandle<usize>,
}

#[cfg(feature = "tls")]
fn spawn_mixed_scheme_peer() -> MixedSchemePeer {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let thread = thread::spawn(move || {
        let deadline = Instant::now() + WAIT;
        let mut plain = accept_before(&listener, deadline);
        plain.set_read_timeout(Some(WAIT)).unwrap();
        plain.set_write_timeout(Some(WAIT)).unwrap();
        let request = read_blocking_request(&mut plain);
        assert_eq!(request.target, "/plain");
        plain
            .write_all(&TestResponse::new(b"plain").wire())
            .unwrap();

        let tls_socket = accept_before(&listener, deadline);
        tls_socket.set_read_timeout(Some(WAIT)).unwrap();
        tls_socket.set_write_timeout(Some(WAIT)).unwrap();
        let mut tls = test_tls_acceptor().accept(tls_socket).unwrap();
        let request = read_blocking_request(&mut tls);
        assert_eq!(request.target, "/tls");
        tls.write_all(&TestResponse::closing(b"tls", &[("connection", "close")]).wire())
            .unwrap();
        2
    });
    MixedSchemePeer { address, thread }
}

#[cfg(feature = "tls")]
fn spawn_tls_idle_injection_peer() -> IdleInjectionPeer {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let (inject, inject_receiver) = mpsc::channel();
    let (injected_sender, injected) = mpsc::channel();
    let thread = thread::spawn(move || {
        let deadline = Instant::now() + WAIT;
        let first_socket = accept_before(&listener, deadline);
        first_socket.set_read_timeout(Some(WAIT)).unwrap();
        first_socket.set_write_timeout(Some(WAIT)).unwrap();
        let acceptor = test_tls_acceptor();
        let mut first = acceptor.accept(first_socket).unwrap();
        let mut requests = vec![read_blocking_request(&mut first)];
        first
            .write_all(&TestResponse::new(b"prime").wire())
            .unwrap();

        inject_receiver
            .recv_timeout(WAIT)
            .expect("timed out waiting to inject an idle TLS response");
        first
            .write_all(&TestResponse::new(b"unsolicited").wire())
            .unwrap();
        first.flush().unwrap();
        wait_for_tcp_delivery(first.get_ref(), deadline);
        injected_sender.send(()).unwrap();

        let second_socket = accept_before(&listener, deadline);
        second_socket.set_read_timeout(Some(WAIT)).unwrap();
        second_socket.set_write_timeout(Some(WAIT)).unwrap();
        let mut second = acceptor.accept(second_socket).unwrap();
        requests.push(read_blocking_request(&mut second));
        second
            .write_all(&TestResponse::closing(b"legitimate", &[("connection", "close")]).wire())
            .unwrap();
        (2, requests)
    });

    IdleInjectionPeer {
        address,
        inject,
        injected,
        thread,
    }
}

#[cfg(feature = "tls")]
#[kimojio::test]
async fn different_schemes_do_not_share_connection() {
    let peer = spawn_mixed_scheme_peer();
    let client =
        Client::with_config(ClientConfig::new().set_tls(test_tls_client_config())).unwrap();

    let plain = get(&client, format!("http://{}/plain", peer.address)).await;
    let tls = get(&client, format!("https://{}/tls", peer.address)).await;

    assert_eq!(plain.body().as_bytes(), b"plain");
    assert_eq!(tls.body().as_bytes(), b"tls");
    assert_eq!(
        peer.thread.join().expect("mixed scheme peer panicked"),
        2,
        "HTTP and HTTPS on one authority require separate accepts"
    );
}

#[cfg(feature = "tls")]
#[kimojio::test]
async fn tls13_session_ticket_keeps_pooled_connection_reusable() {
    let peer = spawn_tls_ticket_peer();
    let client =
        Client::with_config(ClientConfig::new().set_tls(test_tls_client_config())).unwrap();

    let first = get(&client, format!("https://{}/first", peer.address)).await;
    assert_eq!(first.body().as_bytes(), b"first");
    peer.inject_ticket();
    let second = get(&client, format!("https://{}/second", peer.address)).await;
    let (accepts, requests) = peer.finish();

    assert_eq!(second.body().as_bytes(), b"second");
    assert_eq!(accepts, 1, "the TLS connection must be reused");
    assert_eq!(
        requests
            .iter()
            .map(|request| request.target.as_str())
            .collect::<Vec<_>>(),
        ["/first", "/second"]
    );
}

#[cfg(feature = "tls")]
#[kimojio::test]
async fn unsolicited_tls_response_bytes_on_an_idle_connection_are_discarded() {
    let peer = spawn_tls_idle_injection_peer();
    let client =
        Client::with_config(ClientConfig::new().set_tls(test_tls_client_config())).unwrap();

    let first = get(&client, format!("https://{}/prime", peer.address)).await;
    assert_eq!(first.body().as_bytes(), b"prime");
    peer.inject_response();
    let second = get(&client, format!("https://{}/next", peer.address)).await;
    let (accepts, requests) = peer.finish();

    assert_eq!(second.body().as_bytes(), b"legitimate");
    assert_eq!(
        accepts, 2,
        "unsolicited TLS application data must force a new connection"
    );
    assert_eq!(
        requests
            .iter()
            .map(|request| request.target.as_str())
            .collect::<Vec<_>>(),
        ["/prime", "/next"]
    );
}

/// A server may send a final response with END_STREAM before the client has
/// finished uploading its request body (RFC 9113 section 8.1). Retiring that
/// exchange in the driver without also marking it abandoned in the pump leaves
/// the pump trying to write more body for a finished exchange, which the driver
/// rejects as a connection-fatal error - destroying every sibling stream.
#[kimojio::test]
async fn early_http2_final_response_spares_sibling_streams() {
    const UPLOAD: usize = 512 * 1024;

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let peer = thread::spawn(move || {
        let mut stream = accept_before(&listener, Instant::now() + WAIT);
        stream.set_read_timeout(Some(WAIT)).unwrap();
        stream.set_write_timeout(Some(WAIT)).unwrap();
        let mut protocol = H2Server::default();
        let mut input = Vec::new();
        let mut answered = Vec::new();

        // Answer each request head immediately and completely, without ever
        // reading the upload body it declared.
        while answered.len() < 2 {
            let mut buffer = [0; 16 * 1024];
            let amount = stream.read(&mut buffer).unwrap();
            assert_ne!(amount, 0, "client closed before both requests arrived");
            input.extend_from_slice(&buffer[..amount]);
            loop {
                let (event, consumed, output) = protocol.accept_event_bytes(&input).unwrap();
                if !output.is_empty() {
                    stream.write_all(&output).unwrap();
                }
                if consumed != 0 {
                    input.drain(..consumed);
                }
                if let Some(H2ByteStreamEvent::RequestHeaders { stream_id, .. }) = event {
                    let body = format!("early-{}", answered.len());
                    let fields = [H2HeaderField::new(
                        b"content-length",
                        body.len().to_string().as_bytes(),
                    )];
                    let commit = protocol
                        .response_headers_frame_with_raw_headers(stream_id, 200, &fields, false)
                        .unwrap();
                    let mut response = take_server_block(&mut protocol, commit);
                    response.extend_from_slice(&protocol.data_frame(
                        stream_id,
                        body.as_bytes(),
                        true,
                    ));
                    stream.write_all(&response).unwrap();
                    answered.push(stream_id);
                }
                if consumed == 0 || input.is_empty() {
                    break;
                }
            }
        }
        // Drain whatever the client still sends so it never blocks on writes.
        let mut scratch = [0; 16 * 1024];
        while stream.read(&mut scratch).is_ok_and(|amount| amount != 0) {}
    });

    let client = Client::new();
    let base = format!("http://{address}");
    let upload = |index: usize| {
        client
            .request(Method::PUT, format!("{base}/early/{index}"))
            .version(Version::HTTP_2)
            .body(vec![b'x'; UPLOAD])
            .send()
    };

    let (first, second) = operations::timeout_at(Instant::now() + WAIT, async {
        futures::join!(upload(0), upload(1))
    })
    .await
    .expect("an early final response stalled its sibling stream");

    let mut bodies = [
        first
            .expect("the first early-answered request failed")
            .body()
            .as_bytes()
            .to_vec(),
        second
            .expect("the second early-answered request failed")
            .body()
            .as_bytes()
            .to_vec(),
    ];
    bodies.sort();
    assert_eq!(bodies, [b"early-0".to_vec(), b"early-1".to_vec()]);
    drop(client);
    peer.join().expect("early final response peer panicked");
}

/// DRIVER-007. Concurrent streaming uploads must interleave on the wire. A
/// streaming body declares no length, so unless the adapter tells the driver
/// what every stream has ready, the fair scheduler cannot see the siblings and
/// serves whichever exchange the pump reaches first until it finishes - letting
/// one upload monopolize the connection while the other waits.
#[kimojio::test]
async fn concurrent_streaming_uploads_interleave_on_one_connection() {
    const CHUNK: usize = 4 * 1024;
    const CHUNKS: usize = 6;

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let client_done = Arc::new(AtomicBool::new(false));
    let released = Arc::clone(&client_done);
    let peer = thread::spawn(move || {
        let mut stream = accept_before(&listener, Instant::now() + WAIT);
        stream.set_read_timeout(Some(WAIT)).unwrap();
        stream.set_write_timeout(Some(WAIT)).unwrap();
        let mut protocol = H2Server::default();
        let mut input = Vec::new();
        let mut heads = Vec::new();
        let mut data_order = Vec::new();
        let mut finished = Vec::new();

        while finished.len() < 2 {
            let mut buffer = [0; 32 * 1024];
            let amount = stream.read(&mut buffer).unwrap();
            assert_ne!(amount, 0, "client closed before both uploads finished");
            input.extend_from_slice(&buffer[..amount]);
            loop {
                let (event, consumed, output) = protocol.accept_event_bytes(&input).unwrap();
                if !output.is_empty() {
                    stream.write_all(&output).unwrap();
                }
                if consumed != 0 {
                    input.drain(..consumed);
                }
                match event {
                    Some(H2ByteStreamEvent::RequestHeaders { stream_id, .. }) => {
                        heads.push(stream_id);
                    }
                    Some(H2ByteStreamEvent::Data {
                        stream_id,
                        end_stream,
                        ..
                    }) => {
                        data_order.push(stream_id);
                        if end_stream {
                            finished.push(stream_id);
                            let body = format!("done-{stream_id}");
                            let fields = [H2HeaderField::new(
                                b"content-length",
                                body.len().to_string().as_bytes(),
                            )];
                            let commit = protocol
                                .response_headers_frame_with_raw_headers(
                                    stream_id, 200, &fields, false,
                                )
                                .unwrap();
                            let mut response = take_server_block(&mut protocol, commit);
                            response.extend_from_slice(&protocol.data_frame(
                                stream_id,
                                body.as_bytes(),
                                true,
                            ));
                            stream.write_all(&response).unwrap();
                        }
                    }
                    _ => {}
                }
                if consumed == 0 || input.is_empty() {
                    break;
                }
            }
        }
        // Hold the connection open until the client has read both responses.
        // Dropping it here races the client's reads and surfaces as a reset that
        // has nothing to do with what this test measures.
        let deadline = Instant::now() + WAIT;
        while !released.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        (heads, data_order)
    });

    let client = Client::new();
    let base = format!("http://{address}");
    let upload = |index: usize| {
        let byte = b'a' + u8::try_from(index).unwrap();
        client
            .request(Method::PUT, format!("{base}/stream/{index}"))
            .version(Version::HTTP_2)
            .body(Body::from_chunks(futures::stream::iter(
                (0..CHUNKS).map(move |_| vec![byte; CHUNK]),
            )))
            .send()
    };

    let (first, second) = operations::timeout_at(Instant::now() + WAIT, async {
        futures::join!(upload(0), upload(1))
    })
    .await
    .expect("concurrent streaming uploads timed out");
    first.expect("the first streaming upload failed");
    second.expect("the second streaming upload failed");
    client_done.store(true, Ordering::Release);

    let (heads, data_order) = peer.join().expect("streaming upload peer panicked");
    assert_eq!(heads.len(), 2, "both uploads must share one connection");
    let [first_stream, second_stream] = heads.as_slice() else {
        unreachable!("exactly two streams were opened");
    };

    assert!(
        data_order.contains(first_stream) && data_order.contains(second_stream),
        "both uploads must have sent data: {data_order:?}"
    );

    // Fairness is about ordering, not totals, and not about a single crossover
    // point either: draining one stream fully and then the other yields equal
    // totals and still contains a crossover. Bound the longest run one stream
    // holds the connection for instead.
    let longest_run = data_order
        .chunk_by(|left, right| left == right)
        .map(<[u32]>::len)
        .max()
        .expect("no request data reached the peer");
    assert!(
        longest_run <= 2,
        "one streaming upload held the connection for {longest_run} consecutive frames: {data_order:?}"
    );
}
