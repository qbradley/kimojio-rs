use std::{cell::RefCell, rc::Rc, time::Duration};

use futures::{FutureExt, StreamExt};
use kimojio::{AsyncStreamRead, AsyncStreamWrite, OwnedFdStream, operations};
use kimojio_http1::{
    Config, ConnectionId, Error, IncomingBody, IncomingFrame, OutgoingBody, OutgoingFrame, connect,
    http::{HeaderMap, Request, Response},
    serve_connection,
};

fn config(slot: u64) -> Config {
    let mut config = Config::new(ConnectionId {
        slot,
        generation: 1,
    });
    config.protocol.max_buffer_bytes = 16 * 1024;
    config.protocol.max_head_bytes = 8 * 1024;
    config
}

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

fn request(path: &str, body: OutgoingBody) -> Request<OutgoingBody> {
    Request::builder()
        .uri(path)
        .header("host", "test")
        .body(body)
        .unwrap()
}

#[kimojio::test]
async fn revoked_upload_source_cannot_fail_an_early_response_body() {
    for outcome in 0..4 {
        let (stream, mut peer) = pair();
        let mut config = config(90 + outcome);
        config.turn_budget = 1;
        let (mut client, driver) = connect(stream, config);
        let (begin_upload, permitted) = kimojio::oneshot();
        let source = futures::stream::once(async move {
            permitted.recv().await.unwrap();
            match outcome {
                0 => Some(Ok(OutgoingFrame::Data(b"too late".to_vec()))),
                1 => None,
                2 => Some(Ok(OutgoingFrame::Trailers(HeaderMap::new()))),
                _ => Some(Err(Error::Application("revoked source".into()))),
            }
        })
        .filter_map(futures::future::ready);
        let app = async {
            let mut request = request("/", OutgoingBody::from_stream(None, source));
            *request.method_mut() = kimojio_http1::http::Method::POST;
            let mut response = client.send(request).await.unwrap();
            assert_eq!(response.status(), 413);
            let _ = begin_upload.send(());
            assert_eq!(
                response.body_mut().collect(64).await.unwrap(),
                b"request rejected"
            );
            let _ = client.shutdown().await;
        };
        let raw = async {
            let mut head = Vec::new();
            let mut buffer = [0; 1024];
            while !head.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let n = peer.try_read(&mut buffer, None).await.unwrap();
                assert_ne!(n, 0);
                head.extend_from_slice(&buffer[..n]);
            }
            peer.write(
                b"HTTP/1.1 413 Content Too Large\r\nContent-Length: 16\r\nConnection: close\r\n\r\nrequest rejected",
                None,
            )
            .await
            .unwrap();
            assert_eq!(peer.try_read(&mut buffer, None).await.unwrap(), 0);
            peer.close().await.unwrap();
        };
        operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
            let ((), driver, ()) = futures::join!(app, driver.run(), raw);
            driver.unwrap();
        })
        .await
        .unwrap();
    }
}

#[test]
fn driver_futures_do_not_embed_transport_buffers() {
    let (a, b) = pair();
    let (_, driver) = connect(a, config(80));
    let client = driver.run();
    let server = serve_connection(b, config(81), |_| async {
        Ok(Response::new(OutgoingBody::empty()))
    });
    assert!(std::mem::size_of_val(&client) < 1024);
    assert!(std::mem::size_of_val(&server) < 1024);
}

#[kimojio::test]
async fn streamed_request_response_trailers_and_keepalive() {
    let (a, b) = pair();
    let (mut client, driver) = connect(a, config(1));
    let handler = |mut request: Request<IncomingBody>| async move {
        let mut bytes = Vec::new();
        let mut trailers = None;
        while let Some(frame) = request.body_mut().frame().await? {
            match frame {
                IncomingFrame::Data(chunk) => bytes.extend_from_slice(&chunk),
                IncomingFrame::Trailers(headers) => trailers = Some(headers),
            }
        }
        assert_eq!(bytes, b"one-two");
        assert_eq!(trailers.unwrap()["x-request"], "complete");
        let mut trailers = HeaderMap::new();
        trailers.insert("x-response", "complete".parse().unwrap());
        let body = OutgoingBody::from_stream(
            None,
            futures::stream::iter([
                Ok(OutgoingFrame::Data(bytes)),
                Ok(OutgoingFrame::Trailers(trailers)),
            ]),
        );
        Ok(Response::new(body))
    };
    let application = async {
        for _ in 0..2 {
            let mut trailers = HeaderMap::new();
            trailers.insert("x-request", "complete".parse().unwrap());
            let body = OutgoingBody::from_stream(
                None,
                futures::stream::iter([
                    Ok(OutgoingFrame::Data(b"one-".to_vec())),
                    Ok(OutgoingFrame::Data(b"two".to_vec())),
                    Ok(OutgoingFrame::Trailers(trailers)),
                ]),
            );
            let mut response = client.send(request("/", body)).await.unwrap();
            let mut received = Vec::new();
            let mut trailers = None;
            while let Some(frame) = response.body_mut().frame().await.unwrap() {
                match frame {
                    IncomingFrame::Data(chunk) => received.extend_from_slice(&chunk),
                    IncomingFrame::Trailers(headers) => trailers = Some(headers),
                }
            }
            assert_eq!(received, b"one-two");
            assert_eq!(trailers.unwrap()["x-response"], "complete");
        }
        client.shutdown().await.unwrap();
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(5), async {
        let ((), driver, server) = futures::join!(
            application,
            driver.run(),
            serve_connection(b, config(2), handler)
        );
        driver.unwrap();
        server.unwrap();
    })
    .await
    .unwrap();
}

#[kimojio::test]
async fn abandoned_queued_request_is_not_sent() {
    let (a, b) = pair();
    let (mut client, driver) = connect(a, config(3));
    let seen = Rc::new(RefCell::new(Vec::new()));
    let handler_seen = seen.clone();
    let server = serve_connection(b, config(4), move |request: Request<IncomingBody>| {
        handler_seen
            .borrow_mut()
            .push(request.uri().path().to_owned());
        async { Ok(Response::new(OutgoingBody::full(b"reply"))) }
    });
    let application = async {
        let mut first = client
            .send(request("/first", OutgoingBody::empty()))
            .await
            .unwrap();
        assert!(
            client
                .send(request("/abandoned", OutgoingBody::empty()))
                .now_or_never()
                .is_none()
        );
        assert_eq!(first.body_mut().collect(100).await.unwrap(), b"reply");
        let mut third = client
            .send(request("/third", OutgoingBody::empty()))
            .await
            .unwrap();
        assert_eq!(third.body_mut().collect(100).await.unwrap(), b"reply");
        client.shutdown().await.unwrap();
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(5), async {
        let ((), driver, server) = futures::join!(application, driver.run(), server);
        driver.unwrap();
        server.unwrap();
    })
    .await
    .unwrap();
    assert_eq!(&*seen.borrow(), &["/first", "/third"]);
}

#[kimojio::test]
async fn graceful_shutdown_rejects_a_queued_request() {
    let (a, b) = pair();
    let (mut client, driver) = connect(a, config(30));
    let control = client.control();
    let server = serve_connection(b, config(31), |_| async {
        Ok(Response::new(OutgoingBody::full(b"first")))
    });
    let app = async {
        let mut first = client
            .send(request("/first", OutgoingBody::empty()))
            .await
            .unwrap();
        let queued = client.send(request("/queued", OutgoingBody::empty()));
        let consume = async {
            control.graceful();
            assert_eq!(first.body_mut().collect(100).await.unwrap(), b"first");
        };
        let (queued, ()) = futures::join!(queued, consume);
        assert!(matches!(queued, Err(Error::Closed)));
        client.shutdown().await.unwrap();
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
        let ((), client, server) = futures::join!(app, driver.run(), server);
        client.unwrap();
        server.unwrap();
    })
    .await
    .unwrap();
}

#[kimojio::test]
async fn invalid_configuration_rejects_an_already_queued_request() {
    let (stream, peer) = pair();
    let mut invalid = config(32);
    invalid.protocol.max_buffer_bytes = 0;
    let (mut client, driver) = connect(stream, invalid);
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
        let (send, driver) = futures::join!(
            client.send(request("/", OutgoingBody::empty())),
            driver.run(),
        );
        assert!(matches!(send, Err(Error::Closed)));
        assert!(driver.is_err());
    })
    .await
    .unwrap();
    drop(peer);
}

#[kimojio::test]
async fn expect_continue_unblocks_a_streaming_upload() {
    let (a, b) = pair();
    let (mut client, driver) = connect(a, config(33));
    let server = serve_connection(
        b,
        config(34),
        |mut request: Request<IncomingBody>| async move {
            assert_eq!(request.body_mut().collect(100).await?, b"upload");
            Ok(Response::new(OutgoingBody::empty()))
        },
    );
    let app = async {
        let body = OutgoingBody::from_stream(
            None,
            futures::stream::iter([Ok(OutgoingFrame::Data(b"upload".to_vec()))]),
        );
        let mut request = request("/", body);
        request
            .headers_mut()
            .insert("expect", "100-continue".parse().unwrap());
        let mut response = client.send(request).await.unwrap();
        assert_eq!(response.status(), 200);
        assert!(response.body_mut().frame().await.unwrap().is_none());
        client.shutdown().await.unwrap();
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
        let ((), client, server) = futures::join!(app, driver.run(), server);
        client.unwrap();
        server.unwrap();
    })
    .await
    .unwrap();
}

#[kimojio::test]
async fn early_rejection_does_not_send_an_unsolicited_continue() {
    let (stream, mut peer) = pair();
    let server = serve_connection(stream, config(35), |_| async {
        let mut response = Response::new(OutgoingBody::empty());
        *response.status_mut() = http::StatusCode::PAYLOAD_TOO_LARGE;
        Ok(response)
    });
    let raw = async {
        peer.write(b"POST /early HTTP/1.1\r\nHost: test\r\nExpect: 100-continue\r\nContent-Length: 100\r\n\r\n", None).await.unwrap();
        let mut received = Vec::new();
        let mut buffer = [0; 1024];
        loop {
            let n = peer.try_read(&mut buffer, None).await.unwrap();
            if n == 0 {
                break;
            }
            received.extend_from_slice(&buffer[..n]);
        }
        assert!(received.starts_with(b"HTTP/1.1 413"), "{received:?}");
        assert!(!received.windows(12).any(|bytes| bytes == b"100 Continue"));
        peer.close().await.unwrap();
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
        let (server, ()) = futures::join!(server, raw);
        server.unwrap();
    })
    .await
    .unwrap();
}

#[kimojio::test]
async fn zero_payload_capacity_still_polls_for_end_and_trailers() {
    let (a, b) = pair();
    let mut client_config = config(36);
    client_config.protocol.max_body_bytes = 6;
    let mut server_config = config(37);
    server_config.protocol.max_body_bytes = 6;
    let (mut client, driver) = connect(a, client_config);
    let server = serve_connection(
        b,
        server_config,
        |mut request: Request<IncomingBody>| async move {
            let bytes = request.body_mut().collect(6).await?;
            let mut trailers = HeaderMap::new();
            trailers.insert("x-end", "yes".parse().unwrap());
            Ok(Response::new(OutgoingBody::from_stream(
                None,
                futures::stream::iter([
                    Ok(OutgoingFrame::Data(bytes)),
                    Ok(OutgoingFrame::Trailers(trailers)),
                ]),
            )))
        },
    );
    let app = async {
        let body = OutgoingBody::from_stream(
            None,
            futures::stream::iter([
                Ok(OutgoingFrame::Data(b"one".to_vec())),
                Ok(OutgoingFrame::Data(b"two".to_vec())),
            ]),
        );
        let mut response = client.send(request("/", body)).await.unwrap();
        let mut bytes = Vec::new();
        let mut trailer = false;
        while let Some(frame) = response.body_mut().frame().await.unwrap() {
            match frame {
                IncomingFrame::Data(chunk) => bytes.extend_from_slice(&chunk),
                IncomingFrame::Trailers(headers) => trailer = headers["x-end"] == "yes",
            }
        }
        assert_eq!(bytes, b"onetwo");
        assert!(trailer);
        client.shutdown().await.unwrap();
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
        let ((), client, server) = futures::join!(app, driver.run(), server);
        client.unwrap();
        server.unwrap();
    })
    .await
    .unwrap();
}

#[kimojio::test]
async fn early_exchange_completion_is_not_a_successful_input_end() {
    let (stream, mut peer) = pair();
    let retained = Rc::new(RefCell::new(None));
    let handler_retained = retained.clone();
    let server = serve_connection(stream, config(38), move |request: Request<IncomingBody>| {
        *handler_retained.borrow_mut() = Some(request.into_body());
        async { Ok(Response::new(OutgoingBody::empty())) }
    });
    let raw = async {
        peer.write(
            b"POST / HTTP/1.1\r\nHost: test\r\nContent-Length: 100\r\n\r\n",
            None,
        )
        .await
        .unwrap();
        let mut received = Vec::new();
        let mut buffer = [0; 1024];
        loop {
            let n = peer.try_read(&mut buffer, None).await.unwrap();
            if n == 0 {
                break;
            }
            received.extend_from_slice(&buffer[..n]);
        }
        assert!(received.starts_with(b"HTTP/1.1 200"));
        peer.close().await.unwrap();
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
        let (server, ()) = futures::join!(server, raw);
        server.unwrap();
    })
    .await
    .unwrap();
    let mut incoming = retained.borrow_mut().take().unwrap();
    assert!(matches!(incoming.frame().await, Err(Error::Cancelled)));
}

#[kimojio::test]
async fn unsolicited_response_is_not_reused_for_a_later_request() {
    let (stream, mut peer) = pair();
    let mut config = config(39);
    config.turn_budget = 1;
    let (mut client, driver) = connect(stream, config);
    let control = client.control();
    let app = async {
        if let Ok(mut first) = client.send(request("/first", OutgoingBody::empty())).await {
            let _ = first.body_mut().collect(100).await;
            let second = client.send(request("/second", OutgoingBody::empty())).await;
            assert!(
                second.is_err(),
                "unsolicited response became a later reply: {second:?}"
            );
        }
        control.abort();
    };
    let raw = async {
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let n = peer.try_read(&mut buffer, None).await.unwrap();
            assert_ne!(n, 0);
            request.extend_from_slice(&buffer[..n]);
        }
        peer.write(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\nHTTP/1.1 299 Unsolicited\r\nContent-Length: 0\r\n\r\n", None).await.unwrap();
        while let Ok(n) = peer.try_read(&mut buffer, None).await {
            if n == 0 {
                break;
            }
        }
        peer.close().await.unwrap();
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
        let ((), _, ()) = futures::join!(app, driver.run(), raw);
    })
    .await
    .unwrap();
}

#[kimojio::test]
async fn echo_streams_when_upload_waits_for_final_response_headers() {
    for expect in [false, true] {
        let (a, b) = pair();
        let (mut client, driver) = connect(a, config(if expect { 42 } else { 40 }));
        let (begin_upload, permitted) = kimojio::oneshot();
        let body = OutgoingBody::from_stream(
            None,
            futures::stream::once(async move {
                permitted.recv().await.map_err(|_| Error::Closed)?;
                Ok(OutgoingFrame::Data(b"gated upload".to_vec()))
            }),
        );
        let mut request = request("/echo", body);
        if expect {
            request
                .headers_mut()
                .insert("expect", "100-continue".parse().unwrap());
        }
        let server = serve_connection(
            b,
            config(if expect { 43 } else { 41 }),
            |request: Request<IncomingBody>| async move {
                let mut incoming = request.into_body();
                incoming.accept().await?;
                let source = futures::stream::try_unfold(incoming, |mut incoming| async move {
                    Ok(match incoming.frame().await? {
                        Some(IncomingFrame::Data(chunk)) => {
                            Some((OutgoingFrame::Data(chunk.to_vec()), incoming))
                        }
                        Some(IncomingFrame::Trailers(headers)) => {
                            Some((OutgoingFrame::Trailers(headers), incoming))
                        }
                        None => None,
                    })
                });
                Ok(Response::new(OutgoingBody::from_stream(None, source)))
            },
        );
        let app = async {
            let mut response = client.send(request).await.unwrap();
            assert_eq!(response.status(), 200);
            begin_upload.send(()).unwrap();
            assert_eq!(
                response.body_mut().collect(100).await.unwrap(),
                b"gated upload"
            );
            client.shutdown().await.unwrap();
        };
        operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
            let ((), client, server) = futures::join!(app, driver.run(), server);
            client.unwrap();
            server.unwrap();
        })
        .await
        .unwrap();
    }
}

#[kimojio::test]
async fn dropping_driver_rejects_pending_body_acceptance() {
    let (stream, mut peer) = pair();
    let (mut client, connection) = connect(stream, config(44));
    let raw = operations::spawn_task(async move {
        let mut buffer = [0; 1024];
        let mut head = Vec::new();
        while !head.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let n = peer.try_read(&mut buffer, None).await.unwrap();
            assert_ne!(n, 0);
            head.extend_from_slice(&buffer[..n]);
        }
        peer.write(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n", None)
            .await
            .unwrap();
        while let Ok(n) = peer.try_read(&mut buffer, None).await {
            if n == 0 {
                break;
            }
        }
        peer.close().await.unwrap();
    });
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
        let mut driver = Box::pin(connection.run());
        let response = {
            let send = client.send(request("/", OutgoingBody::empty())).fuse();
            futures::pin_mut!(send);
            futures::select_biased! {
                response = send => response.unwrap(),
                result = driver.as_mut().fuse() => panic!("driver stopped before response: {result:?}"),
            }
        };
        let mut incoming = response.into_body();
        let mut accepted = Box::pin(incoming.accept());
        assert!(accepted.as_mut().now_or_never().is_none());
        drop(driver);
        assert!(accepted.await.is_err());
        raw.await.unwrap();
    }).await.unwrap();
}

#[kimojio::test]
async fn early_final_response_progresses_during_upload() {
    let (a, mut peer) = pair();
    let (mut client, driver) = connect(a, config(5));
    let polls = Rc::new(std::cell::Cell::new(0));
    let source_polls = polls.clone();
    let body = OutgoingBody::from_stream(
        None,
        futures::stream::repeat_with(move || {
            source_polls.set(source_polls.get() + 1);
            Ok(OutgoingFrame::Data(vec![b'x'; 16 * 1024]))
        }),
    );
    let application = async {
        let response = client.send(request("/early", body)).await.unwrap();
        assert_eq!(response.status(), 413);
        drop(response);
        let _ = client.shutdown().await;
    };
    let raw_peer = async {
        let mut buffer = [0; 1024];
        let mut head = Vec::new();
        while !head.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let read = peer.try_read(&mut buffer, None).await.unwrap();
            assert_ne!(read, 0);
            head.extend_from_slice(&buffer[..read]);
        }
        peer.write(
            b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            None,
        )
        .await
        .unwrap();
        operations::sleep(Duration::from_millis(20)).await.unwrap();
        peer.close().await.unwrap();
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(5), async {
        let ((), _, ()) = futures::join!(application, driver.run(), raw_peer);
    })
    .await
    .unwrap();
    assert!(
        polls.get() < 100,
        "unbounded upload polling: {}",
        polls.get()
    );
}

#[kimojio::test]
async fn fallible_source_terminates_without_hanging() {
    let (a, mut peer) = pair();
    let (mut client, driver) = connect(a, config(6));
    let body = OutgoingBody::from_stream(
        None,
        futures::stream::iter([
            Ok(OutgoingFrame::Data(b"prefix".to_vec())),
            Err(Error::Application("source failed".into())),
        ]),
    );
    let application = async {
        assert!(client.send(request("/", body)).await.is_err());
    };
    let raw_peer = async {
        let mut buffer = [0; 1024];
        while let Ok(n) = peer.try_read(&mut buffer, None).await {
            if n == 0 {
                break;
            }
        }
        peer.close().await.unwrap();
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(5), async {
        let ((), result, ()) = futures::join!(application, driver.run(), raw_peer);
        assert!(result.is_err());
    })
    .await
    .unwrap();
}

#[kimojio::test]
async fn core_selects_wire_version_without_handler_reconstruction() {
    let (a, b) = pair();
    let (mut client, driver) = connect(a, config(45));
    let server = serve_connection(b, config(46), |_| async {
        let mut response = Response::new(OutgoingBody::full(b"reply"));
        *response.version_mut() = http::Version::HTTP_2;
        Ok(response)
    });
    let app = async {
        let mut request = request("/", OutgoingBody::empty());
        *request.version_mut() = http::Version::HTTP_10;
        let mut response = client.send(request).await.unwrap();
        assert_eq!(response.version(), http::Version::HTTP_10);
        assert_eq!(response.body_mut().collect(100).await.unwrap(), b"reply");
        client.shutdown().await.unwrap();
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
        let ((), client, server) = futures::join!(app, driver.run(), server);
        client.unwrap();
        server.unwrap();
    })
    .await
    .unwrap();
}

#[kimojio::test]
async fn suppressed_response_source_releases_its_request_body() {
    let (server, mut peer) = pair();
    let handler = |request: Request<IncomingBody>| async move {
        if request.method() == "HEAD" {
            let mut incoming = request.into_body();
            let first = incoming.frame().await?;
            assert!(matches!(first, Some(IncomingFrame::Data(_))));
            let source =
                futures::stream::try_unfold((first, incoming), |(first, mut body)| async move {
                    let frame = match first {
                        Some(frame) => Some(frame),
                        None => body.frame().await?,
                    };
                    Ok(match frame {
                        Some(IncomingFrame::Data(chunk)) => {
                            Some((OutgoingFrame::Data(chunk.to_vec()), (None, body)))
                        }
                        Some(IncomingFrame::Trailers(headers)) => {
                            Some((OutgoingFrame::Trailers(headers), (None, body)))
                        }
                        None => None,
                    })
                });
            Ok(Response::new(OutgoingBody::from_stream(None, source)))
        } else {
            Ok(Response::new(OutgoingBody::full(b"done")))
        }
    };
    let raw = async {
        peer.write(
            b"HEAD / HTTP/1.1\r\nHost: test\r\nContent-Length: 7\r\n\r\npayload",
            None,
        )
        .await
        .unwrap();
        let mut head = Vec::new();
        let mut byte = [0];
        while !head.ends_with(b"\r\n\r\n") {
            assert_eq!(peer.try_read(&mut byte, None).await.unwrap(), 1);
            head.push(byte[0]);
        }
        assert!(head.starts_with(b"HTTP/1.1 200"));
        assert_eq!(peer.try_read(&mut byte, None).await.unwrap(), 0);
        peer.close().await.unwrap();
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
        let (result, ()) = futures::join!(serve_connection(server, config(7), handler), raw);
        result.unwrap();
    })
    .await
    .unwrap();
}
