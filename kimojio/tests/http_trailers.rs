// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(feature = "http")]

use std::cell::Cell;
use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener as StdTcpListener, TcpStream};
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use futures::StreamExt;
use http_body_util::StreamBody;
use hyper::body::{Bytes, Frame, Incoming};
use hyper::service::service_fn;
use hyper::{Request as HyperRequest, Response as HyperResponse};
use hyper_util::rt::TokioIo;
use kimojio::http::{
    Body, Client, Error as HttpError, HeaderMap, HeaderValue, ProtocolErrorKind, Response,
    ServeError, Server, StatusCode, Version,
};
use kimojio::{CancellationToken, operations};
use kimojio_fsm_http::{
    H2ByteClientEvent, H2ByteStreamEvent, H2Client, H2HeaderField, H2OutboundCommit, H2Server,
};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

const WAIT: Duration = Duration::from_secs(10);
const BODY: &[u8] = b"payload";
const TRAILER_NAME: &str = "x-checksum";
const TRAILER_VALUE: &str = "complete";

#[derive(Clone, Copy)]
enum WireProtocol {
    Http1,
    Http2,
}

impl WireProtocol {
    const fn version(self) -> Version {
        match self {
            Self::Http1 => Version::HTTP_11,
            Self::Http2 => Version::HTTP_2,
        }
    }
}

fn take_client_block(client: &mut H2Client, commit: H2OutboundCommit) -> Vec<u8> {
    let block = client.next_outbound_block().expect("queued HTTP/2 block");
    assert_eq!(block.commit(), commit);
    let bytes = block.bytes().to_vec();
    client.acknowledge_outbound_block(commit).unwrap();
    bytes
}

fn take_server_block(server: &mut H2Server, commit: H2OutboundCommit) -> Vec<u8> {
    let block = server.next_outbound_block().expect("queued HTTP/2 block");
    assert_eq!(block.commit(), commit);
    let bytes = block.bytes().to_vec();
    server.acknowledge_outbound_block(commit).unwrap();
    bytes
}

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

fn accept_before(listener: &StdTcpListener) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        match listener.accept() {
            Ok((stream, _)) => return stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for a test peer connection"
                );
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("failed to accept a test peer connection: {error}"),
        }
    }
}

fn spawn_request_peer(address: SocketAddr, protocol: WireProtocol) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        stream.set_write_timeout(Some(WAIT)).unwrap();
        match protocol {
            WireProtocol::Http1 => {
                stream
                    .write_all(
                        b"POST /trailers HTTP/1.1\r\n\
                          host: localhost\r\n\
                          transfer-encoding: chunked\r\n\
                          connection: close\r\n\r\n\
                          7\r\npayload\r\n\
                          0\r\nx-checksum: complete\r\n\r\n",
                    )
                    .unwrap();
            }
            WireProtocol::Http2 => {
                let mut client = H2Client::default();
                let mut request = client.connection_preface();
                let (stream_id, commit) = client
                    .open_stream_with_raw_headers(
                        "POST",
                        "http",
                        "localhost",
                        "/trailers",
                        &[],
                        false,
                    )
                    .unwrap();
                request.extend_from_slice(&take_client_block(&mut client, commit));
                request.extend_from_slice(&client.data_frame(stream_id, BODY, false));
                let trailers = [H2HeaderField::new(
                    TRAILER_NAME.as_bytes(),
                    TRAILER_VALUE.as_bytes(),
                )];
                let commit = client
                    .trailers_frame_with_raw_headers(stream_id, &trailers)
                    .unwrap();
                request.extend_from_slice(&take_client_block(&mut client, commit));
                stream.write_all(&request).unwrap();
            }
        }
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        assert!(!response.is_empty(), "server sent no response bytes");
    })
}

async fn run_server_inbound_case(protocol: WireProtocol, streaming: bool) {
    let server = Server::bind((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
    let peer = spawn_request_peer(server.local_addr(), protocol);
    let cancellation = Rc::new(CancellationToken::new());
    let cancel_from_handler = Rc::clone(&cancellation);
    let handler = move |mut request: kimojio::http::Request<Body>| {
        let cancellation = Rc::clone(&cancel_from_handler);
        async move {
            assert_eq!(request.version(), protocol.version());
            if streaming {
                assert!(request.body().is_streaming());
                assert!(
                    request.body().trailers().is_none(),
                    "streaming trailers were exposed before the body was drained"
                );
                let mut body = Vec::new();
                while let Some(chunk) = request.body_mut().next_chunk().await.unwrap() {
                    body.extend_from_slice(&chunk);
                    assert!(
                        request.body().trailers().is_none(),
                        "streaming trailers were exposed before end-of-stream"
                    );
                }
                assert_eq!(body, BODY);
            } else {
                assert!(!request.body().is_streaming());
                assert_eq!(request.body().as_bytes(), BODY);
            }
            assert_eq!(
                request.body().trailers().unwrap()[TRAILER_NAME],
                TRAILER_VALUE
            );
            cancellation.cancel();
            Response::new(Body::from("accepted"))
        }
    };

    let served = if streaming {
        operations::timeout_at(
            Instant::now() + WAIT,
            server.serve_streaming(handler, cancellation),
        )
        .await
    } else {
        operations::timeout_at(Instant::now() + WAIT, server.serve(handler, cancellation)).await
    };
    served
        .expect("server inbound trailer case timed out")
        .unwrap();
    peer.join().expect("request trailer peer panicked");
}

#[kimojio::test]
async fn server_reads_http1_request_trailers_buffered_and_streaming() {
    run_server_inbound_case(WireProtocol::Http1, false).await;
    run_server_inbound_case(WireProtocol::Http1, true).await;
}

#[kimojio::test]
async fn server_reads_http2_request_trailers_buffered_and_streaming() {
    run_server_inbound_case(WireProtocol::Http2, false).await;
    run_server_inbound_case(WireProtocol::Http2, true).await;
}

fn spawn_response_peer(
    protocol: WireProtocol,
    with_trailers: bool,
) -> (SocketAddr, thread::JoinHandle<()>) {
    let listener = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let peer = thread::spawn(move || {
        let mut stream = accept_before(&listener);
        stream.set_read_timeout(Some(WAIT)).unwrap();
        stream.set_write_timeout(Some(WAIT)).unwrap();
        match protocol {
            WireProtocol::Http1 => {
                let (head, remainder) = read_http1_head(&mut stream);
                assert!(head.starts_with(b"GET /trailers HTTP/1.1\r\n"));
                assert!(remainder.is_empty());
                if with_trailers {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\n\
                              transfer-encoding: chunked\r\n\
                              connection: close\r\n\r\n\
                              7\r\npayload\r\n\
                              0\r\nx-checksum: complete\r\n\r\n",
                        )
                        .unwrap();
                } else {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\n\
                              content-length: 7\r\n\
                              connection: close\r\n\r\n\
                              payload",
                        )
                        .unwrap();
                }
            }
            WireProtocol::Http2 => {
                let mut server = H2Server::default();
                let mut input = Vec::new();
                let stream_id = 'request: loop {
                    let mut buffer = [0; 4096];
                    let read = stream.read(&mut buffer).unwrap();
                    assert_ne!(read, 0, "client closed before sending an HTTP/2 request");
                    input.extend_from_slice(&buffer[..read]);
                    loop {
                        let (event, consumed, output) = server.accept_event_bytes(&input).unwrap();
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
                let commit = server
                    .response_headers_frame_with_raw_headers(stream_id, 200, &[], false)
                    .unwrap();
                let mut response = take_server_block(&mut server, commit);
                response.extend_from_slice(&server.data_frame(stream_id, BODY, !with_trailers));
                if with_trailers {
                    let trailers = [H2HeaderField::new(
                        TRAILER_NAME.as_bytes(),
                        TRAILER_VALUE.as_bytes(),
                    )];
                    let commit = server
                        .trailers_frame_with_raw_headers(stream_id, &trailers)
                        .unwrap();
                    response.extend_from_slice(&take_server_block(&mut server, commit));
                }
                stream.write_all(&response).unwrap();
            }
        }
    });
    (address, peer)
}

async fn run_client_inbound_case(protocol: WireProtocol, streaming: bool, with_trailers: bool) {
    let (address, peer) = spawn_response_peer(protocol, with_trailers);
    let request = Client::new()
        .get(format!("http://{address}/trailers"))
        .version(protocol.version());
    let mut response = if streaming {
        operations::timeout_at(Instant::now() + WAIT, request.send_streaming())
            .await
            .expect("streaming client inbound trailer case timed out")
            .unwrap()
    } else {
        operations::timeout_at(Instant::now() + WAIT, request.send())
            .await
            .expect("buffered client inbound trailer case timed out")
            .unwrap()
    };

    if streaming {
        assert!(response.body().is_streaming());
        assert!(response.body().trailers().is_none());
        let mut body = Vec::new();
        while let Some(chunk) = response.body_mut().next_chunk().await.unwrap() {
            body.extend_from_slice(&chunk);
            assert!(
                response.body().trailers().is_none(),
                "streaming response trailers were exposed before end-of-stream"
            );
        }
        assert_eq!(body, BODY);
    } else {
        assert_eq!(response.body().as_bytes(), BODY);
    }

    if with_trailers {
        assert_eq!(
            response.body().trailers().unwrap()[TRAILER_NAME],
            TRAILER_VALUE
        );
    } else {
        assert!(
            response.body().trailers().is_none(),
            "a response without trailers produced an empty trailer map"
        );
    }
    peer.join().expect("response trailer peer panicked");
}

#[kimojio::test]
async fn client_reads_http1_response_trailers_buffered_and_streaming() {
    run_client_inbound_case(WireProtocol::Http1, false, true).await;
    run_client_inbound_case(WireProtocol::Http1, true, true).await;
}

#[kimojio::test]
async fn client_reads_http2_response_trailers_buffered_and_streaming() {
    run_client_inbound_case(WireProtocol::Http2, false, true).await;
    run_client_inbound_case(WireProtocol::Http2, true, true).await;
}

#[kimojio::test]
async fn bodies_without_trailers_report_none() {
    run_client_inbound_case(WireProtocol::Http1, false, false).await;
    run_client_inbound_case(WireProtocol::Http2, true, false).await;
}

#[kimojio::test]
async fn buffered_http1_response_with_trailers_switches_to_chunked_framing() {
    let server = Server::bind((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
    let address = server.local_addr();
    let peer = thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        stream.set_write_timeout(Some(WAIT)).unwrap();
        stream
            .write_all(b"GET /fixed HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n")
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        response
    });

    let cancellation = Rc::new(CancellationToken::new());
    let cancel_from_trailers = Rc::clone(&cancellation);
    operations::timeout_at(
        Instant::now() + WAIT,
        server.serve(
            move |_| {
                let cancellation = Rc::clone(&cancel_from_trailers);
                async move {
                    let mut response =
                        Response::new(Body::from("payload").with_trailers_fn(move || {
                            let mut trailers = HeaderMap::new();
                            trailers.insert("grpc-status", HeaderValue::from_static("0"));
                            cancellation.cancel();
                            trailers
                        }));
                    response
                        .headers_mut()
                        .insert("content-length", HeaderValue::from_static("7"));
                    response
                }
            },
            cancellation,
        ),
    )
    .await
    .expect("HTTP/1 trailer response timed out")
    .unwrap();

    let response = peer.join().expect("HTTP/1 response peer panicked");
    let lower = response.to_ascii_lowercase();
    assert!(
        lower
            .windows(b"transfer-encoding: chunked\r\n".len())
            .any(|window| window == b"transfer-encoding: chunked\r\n")
    );
    assert!(
        !lower
            .windows(b"content-length:".len())
            .any(|window| window == b"content-length:")
    );
    let head_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap()
        + 4;
    assert_eq!(
        &response[head_end..],
        b"7\r\npayload\r\n0\r\ngrpc-status: 0\r\n\r\n"
    );
}

#[kimojio::test]
async fn invalid_or_panicking_http1_trailers_fail_the_response_observably() {
    let server = Server::bind((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
    let address = server.local_addr();
    let peer = thread::spawn(move || {
        ["/forbidden", "/panic"].map(|target| {
            let mut stream = TcpStream::connect(address).unwrap();
            stream.set_read_timeout(Some(WAIT)).unwrap();
            stream.set_write_timeout(Some(WAIT)).unwrap();
            write!(
                stream,
                "GET {target} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n"
            )
            .unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).unwrap();
            response
        })
    });

    let reports = Rc::new((Cell::new(false), Cell::new(false)));
    let observed_reports = Rc::clone(&reports);
    let cancellation = Rc::new(CancellationToken::new());
    let cancel_from_reports = Rc::clone(&cancellation);
    operations::timeout_at(
        Instant::now() + WAIT,
        server.serve_with_error_handler(
            |request| async move {
                match request.uri().path() {
                    "/forbidden" => {
                        let mut trailers = HeaderMap::new();
                        trailers.insert("content-length", HeaderValue::from_static("7"));
                        Response::new(Body::from("payload").with_trailers(trailers))
                    }
                    "/panic" => Response::new(
                        Body::from("payload")
                            .with_trailers_fn(|| panic!("HTTP/1 trailer callback panic")),
                    ),
                    _ => unreachable!("unexpected request target"),
                }
            },
            cancellation,
            move |error| {
                match error {
                    ServeError::Connection(HttpError::Protocol(error))
                        if error.kind() == ProtocolErrorKind::InvalidHeader =>
                    {
                        observed_reports.0.set(true);
                    }
                    ServeError::Connection(HttpError::Protocol(error))
                        if error.kind() == ProtocolErrorKind::InvalidState
                            && error.to_string().contains("trailer callback panicked") =>
                    {
                        observed_reports.1.set(true);
                    }
                    error => panic!("unexpected HTTP/1 trailer report: {error}"),
                }
                if observed_reports.0.get() && observed_reports.1.get() {
                    cancel_from_reports.cancel();
                }
            },
        ),
    )
    .await
    .expect("HTTP/1 trailer failure cases timed out")
    .unwrap();

    let responses = peer.join().expect("HTTP/1 trailer failure peer panicked");
    for response in responses {
        let lower = response.to_ascii_lowercase();
        assert!(lower.starts_with(b"http/1.1 200 ok\r\n"));
        assert!(
            lower
                .windows(b"transfer-encoding: chunked\r\n".len())
                .any(|window| window == b"transfer-encoding: chunked\r\n")
        );
        assert!(
            response.ends_with(b"7\r\npayload\r\n"),
            "a failed trailer response must not look protocol-complete: {response:?}"
        );
    }
    assert!(reports.0.get());
    assert!(reports.1.get());
}

#[derive(Default)]
struct H2TrailerObservation {
    status: Option<u16>,
    body: Vec<u8>,
    data_ended_stream: bool,
    trailer_value: Option<Vec<u8>>,
    trailers_ended_stream: bool,
}

#[kimojio::test]
async fn outbound_http2_trailers_are_a_terminal_headers_frame() {
    let server = Server::bind((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
    let address = server.local_addr();
    let peer = thread::spawn(move || {
        let mut client = H2Client::default();
        let mut request = client.connection_preface();
        let (_, commit) = client
            .open_stream_with_raw_headers("GET", "http", "localhost", "/h2-trailers", &[], true)
            .unwrap();
        request.extend_from_slice(&take_client_block(&mut client, commit));

        let mut stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        stream.set_write_timeout(Some(WAIT)).unwrap();
        stream.write_all(&request).unwrap();

        let mut pending = Vec::new();
        let mut observed = H2TrailerObservation::default();
        while !observed.trailers_ended_stream {
            let mut bytes = [0; 4096];
            let read = stream.read(&mut bytes).unwrap();
            assert_ne!(read, 0, "server closed before sending trailing HEADERS");
            pending.extend_from_slice(&bytes[..read]);
            let mut consumed_total = 0;
            loop {
                let (event, consumed, output) =
                    client.accept_bytes(&pending[consumed_total..]).unwrap();
                if !output.is_empty() {
                    stream.write_all(&output).unwrap();
                }
                if consumed == 0 {
                    break;
                }
                consumed_total += consumed;
                match event {
                    Some(H2ByteClientEvent::ResponseHeaders {
                        headers,
                        end_stream,
                        ..
                    }) => {
                        if let Some(status) =
                            headers.iter().find(|header| header.name == b":status")
                        {
                            observed.status =
                                Some(std::str::from_utf8(&status.value).unwrap().parse().unwrap());
                            assert!(
                                !headers.iter().any(|header| header.name == b"grpc-status"),
                                "grpc-status was sent in the initial response head"
                            );
                        } else {
                            observed.trailer_value = headers
                                .iter()
                                .find(|header| header.name == b"grpc-status")
                                .map(|header| header.value.clone());
                            observed.trailers_ended_stream = end_stream;
                        }
                    }
                    Some(H2ByteClientEvent::Data {
                        payload,
                        end_stream,
                        ..
                    }) => {
                        observed.body.extend_from_slice(&payload);
                        observed.data_ended_stream |= end_stream;
                    }
                    Some(H2ByteClientEvent::Trailers { headers, .. }) => {
                        observed.trailer_value = headers
                            .iter()
                            .find(|header| header.name == b"grpc-status")
                            .map(|header| header.value.clone());
                        observed.trailers_ended_stream = true;
                    }
                    _ => {}
                }
            }
            pending.drain(..consumed_total);
        }
        observed
    });

    let cancellation = Rc::new(CancellationToken::new());
    let cancel_from_trailers = Rc::clone(&cancellation);
    operations::timeout_at(
        Instant::now() + WAIT,
        server.serve(
            move |_| {
                let cancellation = Rc::clone(&cancel_from_trailers);
                async move {
                    Response::new(
                        Body::from_chunks(futures::stream::iter([
                            b"grpc-".to_vec(),
                            b"message".to_vec(),
                        ]))
                        .with_trailers_fn(move || {
                            let mut trailers = HeaderMap::new();
                            trailers.insert("grpc-status", HeaderValue::from_static("0"));
                            cancellation.cancel();
                            trailers
                        }),
                    )
                }
            },
            cancellation,
        ),
    )
    .await
    .expect("HTTP/2 trailer response timed out")
    .unwrap();

    let observed = peer.join().expect("HTTP/2 response peer panicked");
    assert_eq!(observed.status, Some(200));
    assert_eq!(observed.body, b"grpc-message");
    assert!(
        !observed.data_ended_stream,
        "the final DATA frame ended the stream before trailers"
    );
    assert_eq!(observed.trailer_value.as_deref(), Some(b"0".as_slice()));
    assert!(observed.trailers_ended_stream);
}

#[kimojio::test]
async fn grpc_shaped_http2_response_round_trips_through_kimojio_client() {
    let server = Server::bind((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
    let address = server.local_addr();
    let cancellation = Rc::new(CancellationToken::new());
    let serve_task = operations::spawn_task(server.serve(
        |_| async {
            let produced = Rc::new(Cell::new(0));
            let produced_by_stream = Rc::clone(&produced);
            let chunks =
                futures::stream::iter([b"\0\0\0\0\x03one".to_vec(), b"\0\0\0\0\x03two".to_vec()])
                    .map(move |chunk| {
                        produced_by_stream.set(produced_by_stream.get() + 1);
                        chunk
                    });
            let produced_at_end = Rc::clone(&produced);
            let mut response =
                Response::new(Body::from_chunks(chunks).with_trailers_fn(move || {
                    assert_eq!(
                        produced_at_end.get(),
                        2,
                        "the trailer callback ran before the message stream ended"
                    );
                    let mut trailers = HeaderMap::new();
                    trailers.insert("grpc-status", HeaderValue::from_static("0"));
                    trailers.insert("x-message-count", HeaderValue::from_static("2"));
                    trailers
                }));
            response
                .headers_mut()
                .insert("content-type", HeaderValue::from_static("application/grpc"));
            response
        },
        Rc::clone(&cancellation),
    ));

    let mut response = operations::timeout_at(
        Instant::now() + WAIT,
        Client::new()
            .get(format!("http://{address}/grpc.Service/Call"))
            .version(Version::HTTP_2)
            .send_streaming(),
    )
    .await
    .expect("gRPC-shaped response head timed out")
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "application/grpc");
    assert!(response.body().trailers().is_none());

    // Bound the drain. A regression that never terminates the stream should
    // surface here as an assertion rather than hanging the whole test run.
    let messages = operations::timeout_at(Instant::now() + WAIT, async {
        let mut messages = Vec::new();
        while let Some(chunk) = response.body_mut().next_chunk().await.unwrap() {
            messages.extend_from_slice(&chunk);
            assert!(response.body().trailers().is_none());
        }
        messages
    })
    .await
    .expect("the gRPC-shaped response body never completed");
    assert_eq!(messages, b"\0\0\0\0\x03one\0\0\0\0\x03two");
    assert_eq!(response.body().trailers().unwrap()["grpc-status"], "0");
    assert_eq!(response.body().trailers().unwrap()["x-message-count"], "2");

    cancellation.cancel();
    operations::timeout_at(Instant::now() + WAIT, serve_task)
        .await
        .expect("gRPC-shaped server did not stop")
        .unwrap()
        .unwrap();
}

#[kimojio::test]
async fn with_trailers_map_round_trips_over_http1() {
    let server = Server::bind((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
    let address = server.local_addr();
    let cancellation = Rc::new(CancellationToken::new());
    let serve_task = operations::spawn_task(server.serve(
        |_| async {
            let mut trailers = HeaderMap::new();
            trailers.insert("x-static-trailer", HeaderValue::from_static("mapped"));
            Response::new(Body::from("body").with_trailers(trailers))
        },
        Rc::clone(&cancellation),
    ));

    let response = operations::timeout_at(
        Instant::now() + WAIT,
        Client::new()
            .get(format!("http://{address}/mapped-trailers"))
            .send(),
    )
    .await
    .expect("mapped trailer response timed out")
    .unwrap();
    assert_eq!(response.body().as_bytes(), b"body");
    assert_eq!(
        response.body().trailers().unwrap()["x-static-trailer"],
        "mapped"
    );

    cancellation.cancel();
    operations::timeout_at(Instant::now() + WAIT, serve_task)
        .await
        .expect("mapped trailer server did not stop")
        .unwrap()
        .unwrap();
}

fn spawn_hyper_http1_trailer_peer() -> (SocketAddr, oneshot::Sender<()>, thread::JoinHandle<()>) {
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
                    .expect("timed out waiting for Kimojio client")
                    .unwrap();
                let service = service_fn(|request: HyperRequest<Incoming>| async move {
                    assert_eq!(request.uri().path(), "/hyper-trailers");
                    let mut trailers = hyper::HeaderMap::new();
                    trailers.insert(TRAILER_NAME, HeaderValue::from_static(TRAILER_VALUE));
                    let frames = futures::stream::iter([
                        Ok::<_, Infallible>(Frame::data(Bytes::from_static(BODY))),
                        Ok(Frame::trailers(trailers)),
                    ]);
                    let mut response = HyperResponse::new(StreamBody::new(frames));
                    response
                        .headers_mut()
                        .insert("trailer", HeaderValue::from_static(TRAILER_NAME));
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
                            .expect("timed out shutting down Hyper trailer peer")
                            .unwrap();
                    }
                }
            });
    });
    (
        ready_rx
            .recv_timeout(WAIT)
            .expect("Hyper trailer peer did not become ready"),
        shutdown_tx,
        thread,
    )
}

#[kimojio::test]
async fn kimojio_client_reads_trailers_from_hyper_http1() {
    let (address, shutdown, peer) = spawn_hyper_http1_trailer_peer();
    let response = operations::timeout_at(
        Instant::now() + WAIT,
        Client::new()
            .get(format!("http://{address}/hyper-trailers"))
            .header("te", "trailers")
            .send(),
    )
    .await
    .expect("Hyper trailer response timed out")
    .unwrap();
    assert_eq!(response.body().as_bytes(), BODY);
    assert_eq!(
        response.body().trailers().unwrap()[TRAILER_NAME],
        TRAILER_VALUE
    );
    let _ = shutdown.send(());
    peer.join().expect("Hyper trailer peer panicked");
}

#[kimojio::test]
async fn client_request_trailers_fail_explicitly() {
    let mut trailers = HeaderMap::new();
    trailers.insert("x-request-trailer", HeaderValue::from_static("unsupported"));
    let error = Client::new()
        .post("http://127.0.0.1:1/")
        .body(Body::from("body").with_trailers(trailers))
        .send()
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        kimojio::http::Error::Protocol(ref error)
            if error.kind() == ProtocolErrorKind::UnsupportedFeature
    ));
}

/// A handler may legitimately compute an empty trailer map at the end of a
/// body. That must complete the response like any other, not stall it.
#[kimojio::test]
async fn empty_trailer_map_completes_the_response() {
    let server = Server::bind((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
    let address = server.local_addr();
    let cancellation = Rc::new(CancellationToken::new());
    let serve_task = operations::spawn_task(server.serve(
        |_| async {
            Response::new(
                Body::from_chunks(futures::stream::iter([b"payload".to_vec()]))
                    .with_trailers_fn(HeaderMap::new),
            )
        },
        Rc::clone(&cancellation),
    ));

    let mut response = operations::timeout_at(
        Instant::now() + WAIT,
        Client::new()
            .get(format!("http://{address}/empty"))
            .version(Version::HTTP_2)
            .send_streaming(),
    )
    .await
    .expect("empty-trailer response head timed out")
    .unwrap();

    let body = operations::timeout_at(Instant::now() + WAIT, async {
        let mut body = Vec::new();
        while let Some(chunk) = response.body_mut().next_chunk().await.unwrap() {
            body.extend_from_slice(&chunk);
        }
        body
    })
    .await
    .expect("an empty trailer map stalled the response body");
    assert_eq!(body, b"payload");

    cancellation.cancel();
    let _ = operations::timeout_at(Instant::now() + WAIT, serve_task).await;
}
