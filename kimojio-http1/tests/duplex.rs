use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};

use futures::StreamExt;
use kimojio::{
    AsyncStreamRead, AsyncStreamWrite, Errno, OwnedFdStream, OwnedFdStreamRead, OwnedFdStreamWrite,
    ReceiverOneshot, SenderOneshot, SplittableStream, operations,
};
use kimojio_http1::{
    Config, ConnectionId, Error, IncomingBody, IncomingFrame, OutgoingBody, OutgoingFrame, connect,
    http::{Request, Response},
    serve_connection,
};

fn pair() -> (OwnedFdStream, OwnedFdStream) {
    let (a, b) = rustix::net::socketpair(
        rustix::net::AddressFamily::UNIX,
        rustix::net::SocketType::STREAM,
        rustix::net::SocketFlags::CLOEXEC,
        None,
    )
    .unwrap();
    (OwnedFdStream::new(a), OwnedFdStream::new(b))
}

#[kimojio::test]
async fn explicit_forwarding_reuses_one_socket_for_fixed_and_chunked_gated_uploads() {
    let (stream, peer) = pair();
    let (mut client, driver) = connect(stream, config(209));
    let calls = Rc::new(Cell::new(0));
    let handler_calls = calls.clone();
    let server = serve_connection(peer, config(210), move |request: Request<IncomingBody>| {
        handler_calls.set(handler_calls.get() + 1);
        async move {
            let mut incoming = request.into_body();
            incoming.accept().await?;
            Ok(Response::new(
                OutgoingBody::from_incoming(incoming).continue_request_body(),
            ))
        }
    });
    let app = async {
        for fixed in [false, true] {
            for expect in [false, true] {
                let (permit, permitted) = kimojio::oneshot();
                let mut frames = vec![
                    Ok(OutgoingFrame::Data(Vec::new())),
                    Ok(OutgoingFrame::Data(vec![0xfe; 16 * 1024])),
                    Ok(OutgoingFrame::Data(vec![0; 13])),
                ];
                if !fixed {
                    let mut trailers = kimojio_http1::http::HeaderMap::new();
                    trailers.insert("x-duplex", "complete".parse().unwrap());
                    frames.push(Ok(OutgoingFrame::Trailers(trailers)));
                }
                let source = futures::stream::once(async move {
                    permitted.recv().await.unwrap();
                    Ok(OutgoingFrame::Data(b"abc".to_vec()))
                })
                .chain(futures::stream::iter(frames));
                let body = OutgoingBody::from_stream(fixed.then_some(3 + 16 * 1024 + 13), source);
                let mut request = Request::builder()
                    .method("POST")
                    .uri("/duplex")
                    .header("host", "test");
                if expect {
                    request = request.header("expect", "100-continue");
                }
                let mut response = client.send(request.body(body).unwrap()).await.unwrap();
                assert_eq!(response.status(), 200);
                assert_ne!(
                    response
                        .headers()
                        .get("connection")
                        .map(|value| value.as_bytes()),
                    Some(&b"close"[..])
                );
                permit.send(()).unwrap();
                let mut bytes = Vec::new();
                let mut trailers = false;
                while let Some(frame) = response.body_mut().frame().await.unwrap() {
                    match frame {
                        IncomingFrame::Data(chunk) => bytes.extend_from_slice(&chunk),
                        IncomingFrame::Trailers(headers) => {
                            assert_eq!(headers["x-duplex"], "complete");
                            trailers = true;
                        }
                    }
                }
                let mut expected = b"abc".to_vec();
                expected.extend_from_slice(&[0xfe; 16 * 1024]);
                expected.extend_from_slice(&[0; 13]);
                assert_eq!(bytes, expected);
                assert_eq!(trailers, !fixed);
            }
        }
        client.shutdown().await.unwrap();
        assert_eq!(calls.get(), 4);
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(5), async {
        let ((), driver, server) = futures::join!(app, driver.run(), server);
        driver.unwrap();
        server.unwrap();
    })
    .await
    .unwrap();
}

fn config(slot: u64) -> Config {
    let mut config = Config::new(ConnectionId {
        slot,
        generation: 1,
    });
    config.protocol.max_buffer_bytes = 16 * 1024;
    config.protocol.max_head_bytes = 8 * 1024;
    config.turn_budget = 1;
    config
}

async fn head(peer: &mut OwnedFdStream) -> String {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        assert_eq!(peer.try_read(&mut byte, None).await.unwrap(), 1);
        head.push(byte[0]);
        assert!(head.len() < 4096);
    }
    String::from_utf8(head).unwrap()
}

async fn tail(peer: &mut OwnedFdStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 512];
    loop {
        match peer.try_read(&mut buffer, None).await {
            Ok(0) | Err(Errno::CONNRESET) => return bytes,
            Ok(n) => bytes.extend_from_slice(&buffer[..n]),
            Err(error) => panic!("unexpected read error: {error}"),
        }
        assert!(bytes.len() < 8192);
    }
}

struct SourceDropped(Rc<Cell<bool>>);

impl Drop for SourceDropped {
    fn drop(&mut self) {
        assert!(!self.0.replace(true));
    }
}

struct GatedStream {
    stream: OwnedFdStream,
    release: ReceiverOneshot<()>,
    source_dropped: Rc<Cell<bool>>,
    partial: SenderOneshot<()>,
}

struct GatedWriter {
    stream: OwnedFdStreamWrite,
    release: Option<ReceiverOneshot<()>>,
    source_dropped: Rc<Cell<bool>>,
    partial: Option<SenderOneshot<()>>,
}

impl SplittableStream for GatedStream {
    type ReadStream = OwnedFdStreamRead;
    type WriteStream = GatedWriter;

    async fn split(self) -> Result<(OwnedFdStreamRead, GatedWriter), Errno> {
        let (read, write) = self.stream.split().await?;
        Ok((
            read,
            GatedWriter {
                stream: write,
                release: Some(self.release),
                source_dropped: self.source_dropped,
                partial: Some(self.partial),
            },
        ))
    }
}

impl AsyncStreamWrite for GatedWriter {
    async fn write(&mut self, bytes: &[u8], deadline: Option<Instant>) -> Result<(), Errno> {
        if bytes == b"final-lease" {
            assert!(self.source_dropped.get());
            self.stream.write(&bytes[..1], deadline).await?;
            self.partial.take().unwrap().send(()).unwrap();
            self.release.take().unwrap().recv().await.unwrap();
            self.stream.write(&bytes[1..], deadline).await
        } else {
            self.stream.write(bytes, deadline).await
        }
    }

    async fn shutdown(&mut self) -> Result<(), Errno> {
        self.stream.shutdown().await
    }
    async fn close(&mut self) -> Result<(), Errno> {
        self.stream.close().await
    }
}

#[kimojio::test]
async fn known_length_source_drop_preserves_held_last_lease_and_partial_write_then_reuses() {
    for chunked in [false, true] {
        held_last_lease_case(chunked).await;
    }
}

async fn held_last_lease_case(chunked: bool) {
    let (mut peer, stream) = pair();
    let (release, released) = kimojio::oneshot();
    let (partial, partial_done) = kimojio::oneshot();
    let dropped = Rc::new(Cell::new(false));
    let calls = Rc::new(Cell::new(0));
    let handler_calls = calls.clone();
    let handler_dropped = dropped.clone();
    let server = serve_connection(
        GatedStream {
            stream,
            release: released,
            source_dropped: dropped.clone(),
            partial,
        },
        config(201),
        move |request: Request<IncomingBody>| {
            handler_calls.set(handler_calls.get() + 1);
            let first = handler_calls.get() == 1;
            let dropped = handler_dropped.clone();
            async move {
                if !first {
                    return Ok(Response::new(OutgoingBody::full(b"ok")));
                }
                let mut incoming = request.into_body();
                incoming.accept().await?;
                let source = futures::stream::try_unfold(
                    (incoming, SourceDropped(dropped)),
                    |(mut incoming, guard)| async move {
                        Ok(match incoming.frame().await? {
                            Some(IncomingFrame::Data(chunk)) => {
                                Some((OutgoingFrame::Forward(chunk), (incoming, guard)))
                            }
                            _ => None,
                        })
                    },
                );
                Ok(Response::new(
                    OutgoingBody::from_stream(Some(11), source).continue_request_body(),
                ))
            }
        },
    );
    let raw = async {
        let request: &[u8] = if chunked {
            b"POST /duplex HTTP/1.1\r\nHost: test\r\nTransfer-Encoding: chunked\r\n\r\nb\r\nfinal-lease\r\n0\r\nx-finished: yes\r\n\r\nGET /next HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n"
        } else {
            b"POST /duplex HTTP/1.1\r\nHost: test\r\nContent-Length: 11\r\n\r\nfinal-leaseGET /next HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n"
        };
        peer.write(request, None).await.unwrap();
        let response = head(&mut peer).await;
        assert!(response.starts_with("HTTP/1.1 200 "));
        assert!(!response.to_ascii_lowercase().contains("connection: close"));
        let mut first = [0];
        peer.read(&mut first, None).await.unwrap();
        assert_eq!(&first, b"f");
        partial_done.recv().await.unwrap();
        operations::sleep(Duration::from_millis(10)).await.unwrap();
        assert!(dropped.get());
        assert_eq!(
            calls.get(),
            1,
            "reuse preceded original write and receive lease settlement"
        );
        release.send(()).unwrap();
        let mut rest = [0; 10];
        peer.read(&mut rest, None).await.unwrap();
        assert_eq!(&rest, b"inal-lease");
        let response = head(&mut peer).await;
        assert!(response.starts_with("HTTP/1.1 200 "));
        assert_eq!(tail(&mut peer).await, b"ok");
        assert_eq!(calls.get(), 2);
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
        let ((), server) = futures::join!(raw, server);
        server.unwrap();
    })
    .await
    .unwrap();
}

#[kimojio::test]
async fn response_first_and_input_first_exchange_completion_reuse_with_expect_continue() {
    for expect in [false, true] {
        let (mut peer, stream) = pair();
        let (incoming_send, incoming_recv) = kimojio::async_channel();
        let calls = Rc::new(Cell::new(0));
        let handler_calls = calls.clone();
        let server = serve_connection(
            stream,
            config(202),
            move |mut request: Request<IncomingBody>| {
                handler_calls.set(handler_calls.get() + 1);
                let send = incoming_send.clone();
                async move {
                    if request.uri().path() == "/response-first" {
                        send.try_send(request.into_body()).ok().unwrap();
                        Ok(Response::new(
                            OutgoingBody::full(b"early").continue_request_body(),
                        ))
                    } else if request.uri().path() == "/input-first" {
                        assert_eq!(request.body_mut().collect(16).await?, b"input");
                        Ok(Response::new(
                            OutgoingBody::full(b"late").continue_request_body(),
                        ))
                    } else {
                        Ok(Response::new(OutgoingBody::full(b"done")))
                    }
                }
            },
        );
        let consumer = async {
            let mut incoming = incoming_recv.recv().await.unwrap();
            assert_eq!(incoming.collect(16).await.unwrap(), b"input");
        };
        let raw = async {
            let expect = if expect {
                "Expect: 100-continue\r\n"
            } else {
                ""
            };
            peer.write(format!("POST /response-first HTTP/1.1\r\nHost: test\r\nContent-Length: 5\r\n{expect}\r\n").as_bytes(), None).await.unwrap();
            let mut response = head(&mut peer).await;
            if !expect.is_empty() {
                assert_eq!(response, "HTTP/1.1 100 Continue\r\n\r\n");
                response = head(&mut peer).await;
            }
            assert!(response.starts_with("HTTP/1.1 200 "));
            assert!(!response.to_ascii_lowercase().contains("connection: close"));
            let mut early = [0; 5];
            peer.read(&mut early, None).await.unwrap();
            assert_eq!(&early, b"early");
            peer.write(
                b"inputPOST /input-first HTTP/1.1\r\nHost: test\r\nContent-Length: 5\r\n\r\ninput",
                None,
            )
            .await
            .unwrap();
            let response = head(&mut peer).await;
            assert!(response.starts_with("HTTP/1.1 200 "));
            assert!(!response.to_ascii_lowercase().contains("connection: close"));
            let mut late = [0; 4];
            peer.read(&mut late, None).await.unwrap();
            assert_eq!(&late, b"late");
            peer.write(
                b"GET /done HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n",
                None,
            )
            .await
            .unwrap();
            assert!(head(&mut peer).await.starts_with("HTTP/1.1 200 "));
            assert_eq!(tail(&mut peer).await, b"done");
            assert_eq!(calls.get(), 3);
        };
        operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
            let ((), (), server) = futures::join!(raw, consumer, server);
            server.unwrap();
        })
        .await
        .unwrap();
    }
}

#[kimojio::test]
async fn abandoned_prefix_never_turns_into_successful_discard_or_reuse() {
    for buffered in [false, true] {
        let (mut peer, stream) = pair();
        let calls = Rc::new(Cell::new(0));
        let handler_calls = calls.clone();
        let server = serve_connection(
            stream,
            config(203),
            move |request: Request<IncomingBody>| {
                handler_calls.set(handler_calls.get() + 1);
                async move {
                    let mut incoming = request.into_body();
                    incoming.accept().await?;
                    let source = futures::stream::once(async move {
                        let Some(IncomingFrame::Data(chunk)) = incoming.frame().await? else {
                            panic!("missing prefix");
                        };
                        assert_eq!(&*chunk, b"payload");
                        Ok(OutgoingFrame::Forward(chunk))
                    });
                    Ok(Response::new(
                        OutgoingBody::from_stream(Some(7), source).continue_request_body(),
                    ))
                }
            },
        );
        let raw = async {
            let mut request = b"POST /prefix HTTP/1.1\r\nHost: test\r\nTransfer-Encoding: chunked\r\n\r\n7\r\npayload\r\n".to_vec();
            if buffered {
                request.extend_from_slice(
                    b"7\r\nignored\r\n0\r\n\r\nGET /forbidden HTTP/1.1\r\nHost: test\r\n\r\n",
                );
            }
            peer.write(&request, None).await.unwrap();
            let response = head(&mut peer).await;
            assert!(response.starts_with("HTTP/1.1 200 "));
            assert_eq!(tail(&mut peer).await, b"payload");
            assert_eq!(calls.get(), 1);
        };
        operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
            let ((), server) = futures::join!(raw, server);
            assert!(matches!(
                server,
                Err(Error::Protocol(kimojio_fsm_http1::Failure::Cancelled))
            ));
        })
        .await
        .unwrap();
    }
}

#[kimojio::test]
async fn dropped_duplex_consumer_after_final_output_cancels_without_a_lease() {
    let (mut peer, stream) = pair();
    let (incoming_send, incoming_recv) = kimojio::oneshot();
    let mut incoming_send = Some(incoming_send);
    let server = serve_connection(stream, config(204), move |request| {
        incoming_send
            .take()
            .unwrap()
            .send(request.into_body())
            .ok()
            .unwrap();
        async { Ok(Response::new(OutgoingBody::empty().continue_request_body())) }
    });
    let raw = async {
        peer.write(
            b"POST /drop HTTP/1.1\r\nHost: test\r\nContent-Length: 5\r\n\r\n",
            None,
        )
        .await
        .unwrap();
        assert!(head(&mut peer).await.starts_with("HTTP/1.1 200 "));
        drop(incoming_recv.recv().await.unwrap());
        assert!(tail(&mut peer).await.is_empty());
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
        let ((), server) = futures::join!(raw, server);
        assert!(matches!(
            server,
            Err(Error::Protocol(kimojio_fsm_http1::Failure::Cancelled))
        ));
    })
    .await
    .unwrap();
}

#[kimojio::test]
async fn body_deadline_survives_final_output_and_a_held_last_lease() {
    let (mut peer, stream) = pair();
    let (incoming_send, incoming_recv) = kimojio::oneshot();
    let mut incoming_send = Some(incoming_send);
    let mut config = config(205);
    config.protocol.body_timeout_ns = Some(20_000_000);
    let server = serve_connection(stream, config, move |request| {
        incoming_send
            .take()
            .unwrap()
            .send(request.into_body())
            .ok()
            .unwrap();
        async { Ok(Response::new(OutgoingBody::empty().continue_request_body())) }
    });
    let consumer = async {
        let mut incoming = incoming_recv.recv().await.unwrap();
        let Some(IncomingFrame::Data(chunk)) = incoming.frame().await.unwrap() else {
            panic!("missing lease");
        };
        assert_eq!(&*chunk, b"input");
        operations::sleep(Duration::from_millis(60)).await.unwrap();
        drop(chunk);
        assert!(incoming.frame().await.is_err());
    };
    let raw = async {
        peer.write(
            b"POST /timeout HTTP/1.1\r\nHost: test\r\nContent-Length: 5\r\n\r\ninput",
            None,
        )
        .await
        .unwrap();
        assert!(head(&mut peer).await.starts_with("HTTP/1.1 200 "));
        assert!(tail(&mut peer).await.is_empty());
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
        let ((), (), server) = futures::join!(raw, consumer, server);
        assert!(matches!(
            server,
            Err(Error::Protocol(kimojio_fsm_http1::Failure::Timeout))
        ));
    })
    .await
    .unwrap();
}

#[kimojio::test]
async fn failed_duplex_source_does_not_finish_chunked_output_or_reuse() {
    let (mut peer, stream) = pair();
    let calls = Rc::new(Cell::new(0));
    let handler_calls = calls.clone();
    let server = serve_connection(
        stream,
        config(206),
        move |request: Request<IncomingBody>| {
            handler_calls.set(handler_calls.get() + 1);
            async move {
                let mut incoming = request.into_body();
                incoming.accept().await?;
                let source = futures::stream::try_unfold(
                    (incoming, false),
                    |(mut incoming, sent)| async move {
                        if sent {
                            return Err(Error::Application("source failed".into()));
                        }
                        let Some(IncomingFrame::Data(chunk)) = incoming.frame().await? else {
                            panic!("missing prefix");
                        };
                        Ok(Some((OutgoingFrame::Forward(chunk), (incoming, true))))
                    },
                );
                Ok(Response::new(
                    OutgoingBody::from_stream(None, source).continue_request_body(),
                ))
            }
        },
    );
    let raw = async {
        peer.write(
            b"POST /failure HTTP/1.1\r\nHost: test\r\nContent-Length: 9\r\n\r\nabc",
            None,
        )
        .await
        .unwrap();
        let response = head(&mut peer).await;
        assert!(response.starts_with("HTTP/1.1 200 "));
        assert!(
            response
                .to_ascii_lowercase()
                .contains("transfer-encoding: chunked")
        );
        assert_eq!(tail(&mut peer).await, b"3\r\nabc\r\n");
        assert_eq!(calls.get(), 1);
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
        let ((), server) = futures::join!(raw, server);
        assert!(matches!(
            server,
            Err(Error::Protocol(kimojio_fsm_http1::Failure::Application))
        ));
    })
    .await
    .unwrap();
}

#[kimojio::test]
async fn client_rejects_response_only_policy_without_sending_a_request() {
    let (stream, peer) = pair();
    let (mut client, driver) = connect(stream, config(207));
    let calls = Rc::new(Cell::new(0));
    let handler_calls = calls.clone();
    let server = serve_connection(peer, config(208), move |_| {
        handler_calls.set(handler_calls.get() + 1);
        async { Ok(Response::new(OutgoingBody::full(b"ok"))) }
    });
    let request = |body| {
        Request::builder()
            .uri("/")
            .header("host", "test")
            .body(body)
            .unwrap()
    };
    let app = async {
        assert!(matches!(
            client
                .send(request(OutgoingBody::empty().continue_request_body()))
                .await,
            Err(Error::InvalidMetadata)
        ));
        let mut response = client.send(request(OutgoingBody::empty())).await.unwrap();
        assert_eq!(response.body_mut().collect(16).await.unwrap(), b"ok");
        client.shutdown().await.unwrap();
        assert_eq!(calls.get(), 1);
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
        let ((), driver, server) = futures::join!(app, driver.run(), server);
        driver.unwrap();
        server.unwrap();
    })
    .await
    .unwrap();
}
