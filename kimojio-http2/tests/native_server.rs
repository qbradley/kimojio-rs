use std::{cell::Cell, future::Future, rc::Rc, time::Duration};

use futures::FutureExt;
use kimojio::{oneshot, operations};
use kimojio_http2::{
    Config, Error, IncomingBody, IncomingFrame, OutgoingBody, OutgoingFrame, Shutdown,
    StreamOutcome, connect_native,
    http::{HeaderMap, HeaderValue, Method, Request, Response, Version},
    serve_connection_native, serve_connection_native_with_shutdown,
};

fn request(path: &str, body: OutgoingBody) -> Request<OutgoingBody> {
    Request::builder()
        .method("POST")
        .uri(format!("http://native.test{path}"))
        .body(body)
        .unwrap()
}

fn streaming(chunks: usize, trailers: bool) -> OutgoingBody {
    let mut headers = HeaderMap::new();
    headers.insert("x-finished", "yes".parse().unwrap());
    OutgoingBody::from_stream(futures::stream::iter(
        (0..chunks)
            .map(|_| Ok(OutgoingFrame::Data(vec![0xa5; 16 * 1024])))
            .chain(trailers.then_some(Ok(OutgoingFrame::Trailers(headers)))),
    ))
}

async fn consume(body: &mut IncomingBody, bytes: usize, trailers: bool) {
    let mut size = 0;
    let mut sections = 0;
    while let Some(frame) = body.frame().await.unwrap() {
        match frame {
            IncomingFrame::Data(chunk) => {
                assert!(chunk.iter().all(|byte| *byte == 0xa5));
                size += chunk.len();
            }
            IncomingFrame::Trailers(headers) => {
                assert_eq!(headers["x-finished"], "yes");
                sections += 1;
            }
        }
    }
    assert_eq!(size, bytes);
    assert_eq!(sections, usize::from(trailers));
    assert_eq!(body.completion().await.unwrap(), StreamOutcome::Complete);
}

async fn bounded(future: impl Future<Output = ()>) {
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(10), future)
        .await
        .unwrap();
}

#[kimojio::test]
async fn repeated_concurrent_streaming_echo_exceeds_both_actual_windows() {
    let (fd, peer) = kimojio::pipe::bipipe();
    rustix::net::sockopt::set_socket_send_buffer_size(&fd, 4096).unwrap();
    rustix::net::sockopt::set_socket_send_buffer_size(&peer, 4096).unwrap();
    let (client, connection) = connect_native(fd, Config::default());
    let count = Rc::new(Cell::new(0));
    let seen = count.clone();
    let server = serve_connection_native(peer, Config::default(), move |request| {
        seen.set(seen.get() + 1);
        async move {
            assert_eq!(request.version(), Version::HTTP_2);
            assert_eq!(request.uri().authority().unwrap().as_str(), "native.test");
            assert_eq!(request.headers().get_all("x-duplicate").iter().count(), 2);
            assert!(request.headers()["authorization"].is_sensitive());
            Ok(Response::new(OutgoingBody::from_incoming(
                request.into_body(),
            )))
        }
    });
    let app = async {
        for _ in 0..3 {
            let send = || async {
                let mut request = request("/echo", streaming(96, true));
                request
                    .headers_mut()
                    .append("x-duplicate", "one".parse().unwrap());
                request
                    .headers_mut()
                    .append("x-duplicate", "two".parse().unwrap());
                let mut secret = HeaderValue::from_static("private");
                secret.set_sensitive(true);
                request.headers_mut().insert("authorization", secret);
                let mut response = client.send(request).await.unwrap();
                consume(response.body_mut(), 96 * 16 * 1024, true).await;
            };
            futures::join!(send(), send());
        }
        client.control().graceful();
    };
    bounded(async {
        let ((), client, server) = futures::join!(app, connection.run(), server);
        client.unwrap();
        server.unwrap();
    })
    .await;
    assert_eq!(count.get(), 6);
}

#[kimojio::test]
async fn unread_request_drop_preserves_each_response_end_shape() {
    for request_shape in 0..3 {
        for response_shape in 0..3 {
            let (fd, peer) = kimojio::pipe::bipipe();
            let config = Config {
                turn_budget: 1,
                ..Config::default()
            };
            let (client, connection) = connect_native(fd, config.clone());
            let server = serve_connection_native(peer, config, move |request| async move {
                drop(request);
                let body = match response_shape {
                    0 => OutgoingBody::empty(),
                    1 => OutgoingBody::full(vec![0xa5; 64 * 1024]),
                    _ => streaming(12, true),
                };
                Ok(Response::new(body))
            });
            let app = async {
                let body = match request_shape {
                    0 => OutgoingBody::empty(),
                    1 => OutgoingBody::full(vec![0xa5; 64 * 1024]),
                    _ => streaming(96, true),
                };
                let mut response = client.send(request("/unread", body)).await.unwrap();
                let size = match response_shape {
                    0 => 0,
                    1 => 64 * 1024,
                    _ => 12 * 16 * 1024,
                };
                consume(response.body_mut(), size, response_shape == 2).await;
                client.control().graceful();
            };
            bounded(async {
                let ((), client, server) = futures::join!(app, connection.run(), server);
                client.unwrap();
                server.unwrap();
            })
            .await;
        }
    }
}

#[kimojio::test]
async fn early_final_response_keeps_a_delayed_upload_alive() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let (start, receive) = oneshot();
    let source = futures::stream::once(async move {
        receive.recv().await.unwrap();
        Ok(OutgoingFrame::Data(vec![0xa5; 64 * 1024]))
    });
    let server = serve_connection_native(peer, Config::default(), |request| async move {
        drop(request);
        Ok(Response::builder()
            .status(413)
            .body(OutgoingBody::empty())
            .unwrap())
    });
    let app = async {
        let mut response = client
            .send(request("/early", OutgoingBody::from_stream(source)))
            .await
            .unwrap();
        assert_eq!(response.status(), 413);
        assert!(response.body_mut().frame().await.unwrap().is_none());
        assert!(response.body_mut().completion().now_or_never().is_none());
        start.send(()).unwrap();
        assert_eq!(
            response.body_mut().completion().await.unwrap(),
            StreamOutcome::Complete
        );
        client.control().graceful();
    };
    bounded(async {
        let ((), client, server) = futures::join!(app, connection.run(), server);
        client.unwrap();
        server.unwrap();
    })
    .await;
}

#[kimojio::test]
async fn paused_consumer_and_pending_handler_do_not_stop_a_sibling() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let (release, receive) = oneshot();
    let mut receive = Some(receive);
    let entered = Rc::new(Cell::new(false));
    let marker = entered.clone();
    let server = serve_connection_native(peer, Config::default(), move |request| {
        let wait = if request.uri().path() == "/pending" {
            receive.take()
        } else {
            None
        };
        let marker = marker.clone();
        async move {
            let path = request.uri().path().to_owned();
            drop(request);
            if let Some(wait) = wait {
                marker.set(true);
                wait.recv().await.unwrap();
            }
            Ok(Response::new(if path == "/large" {
                streaming(96, false)
            } else {
                OutgoingBody::empty()
            }))
        }
    });
    let app = async {
        let mut pending = Box::pin(client.send(request("/pending", OutgoingBody::empty())));
        assert!(pending.as_mut().now_or_never().is_none());
        while !entered.get() {
            operations::yield_io().await;
        }
        let mut large = client
            .send(request("/large", OutgoingBody::empty()))
            .await
            .unwrap();
        let Some(IncomingFrame::Data(held)) = large.body_mut().frame().await.unwrap() else {
            panic!("data");
        };
        let first = held.len();
        let mut sibling = client
            .send(request("/small", OutgoingBody::empty()))
            .await
            .unwrap();
        consume(sibling.body_mut(), 0, false).await;
        assert!(!held.is_empty());
        drop(held);
        let rest = large.body_mut().collect(2 * 1024 * 1024).await.unwrap();
        assert_eq!(first + rest.len(), 96 * 16 * 1024);
        large.body_mut().completion().await.unwrap();
        release.send(()).unwrap();
        consume(pending.await.unwrap().body_mut(), 0, false).await;
        client.control().graceful();
    };
    bounded(async {
        let ((), client, server) = futures::join!(app, connection.run(), server);
        client.unwrap();
        server.unwrap();
    })
    .await;
}

#[kimojio::test]
async fn classic_connect_streams_both_tunnel_halves() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let server = serve_connection_native(peer, Config::default(), |request| async move {
        assert_eq!(request.method(), Method::CONNECT);
        assert_eq!(
            request.uri().authority().unwrap().as_str(),
            "tunnel.test:443"
        );
        assert!(request.uri().scheme().is_none());
        Ok(Response::new(OutgoingBody::from_incoming(
            request.into_body(),
        )))
    });
    let app = async {
        let request = Request::builder()
            .method("CONNECT")
            .uri("tunnel.test:443")
            .body(streaming(96, false))
            .unwrap();
        let mut response = client.send(request).await.unwrap();
        consume(response.body_mut(), 96 * 16 * 1024, false).await;
        client.control().graceful();
    };
    bounded(async {
        let ((), client, server) = futures::join!(app, connection.run(), server);
        client.unwrap();
        server.unwrap();
    })
    .await;
}

#[kimojio::test]
async fn failed_handlers_and_invalid_final_responses_are_stream_local() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let server = serve_connection_native(peer, Config::default(), |request| {
        assert_ne!(
            request.uri().path(),
            "/construction-panic",
            "handler construction failure"
        );
        async move {
            match request.uri().path() {
                "/error" => Err(Error::Application("handler failure".into())),
                "/panic" => panic!("handler future failure"),
                "/informational" => Ok(Response::builder()
                    .status(103)
                    .body(OutgoingBody::empty())
                    .unwrap()),
                "/oversize" => Ok(Response::new(OutgoingBody::full(vec![0; 128 * 1024]))),
                _ => Ok(Response::new(OutgoingBody::empty())),
            }
        }
    });
    let app = async {
        for path in [
            "/error",
            "/informational",
            "/oversize",
            "/panic",
            "/construction-panic",
        ] {
            assert!(matches!(
                client.send(request(path, streaming(12, false))).await,
                Err(Error::Stream(StreamOutcome::Reset(_)))
            ));
        }
        let mut response = client
            .send(request("/healthy", OutgoingBody::empty()))
            .await
            .unwrap();
        consume(response.body_mut(), 0, false).await;
        client.control().graceful();
    };
    bounded(async {
        let ((), client, server) = futures::join!(app, connection.run(), server);
        client.unwrap();
        server.unwrap();
    })
    .await;
}

struct Dropped(Rc<Cell<bool>>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.set(true);
    }
}

#[kimojio::test]
async fn handler_and_response_source_native_reads_cancel_without_affecting_siblings() {
    for in_source in [false, true] {
        let (fd, peer) = kimojio::pipe::bipipe();
        let (read, write) = kimojio::pipe::bipipe();
        let read = Rc::new(read);
        let entered = Rc::new(Cell::new(false));
        let dropped = Rc::new(Cell::new(false));
        let handler_read = read.clone();
        let handler_entered = entered.clone();
        let handler_dropped = dropped.clone();
        let (client, connection) = connect_native(fd, Config::default());
        let server = serve_connection_native(peer, Config::default(), move |request| {
            let read = handler_read.clone();
            let entered = handler_entered.clone();
            let dropped = handler_dropped.clone();
            async move {
                if request.uri().path() != "/native" {
                    return Ok(Response::new(streaming(12, false)));
                }
                let source = async move {
                    let _guard = Dropped(dropped);
                    let mut bytes = vec![0; 32];
                    entered.set(true);
                    operations::timeout_at(
                        kimojio::clock_now() + Duration::from_secs(5),
                        operations::read(read.as_ref(), &mut bytes),
                    )
                    .await
                    .unwrap()
                    .map_err(Error::Transport)?;
                    Ok(OutgoingFrame::Data(bytes))
                };
                if in_source {
                    Ok(Response::new(OutgoingBody::from_stream(
                        futures::stream::once(source),
                    )))
                } else {
                    source.await?;
                    Ok(Response::new(OutgoingBody::empty()))
                }
            }
        });
        let app = async {
            let mut pending = Box::pin(client.send(request("/native", OutgoingBody::empty())));
            if in_source {
                let mut response = pending.await.unwrap();
                while !entered.get() {
                    operations::yield_io().await;
                }
                response.body().cancel();
                assert!(response.body_mut().completion().await.is_err());
            } else {
                assert!(pending.as_mut().now_or_never().is_none());
                while !entered.get() {
                    operations::yield_io().await;
                }
                drop(pending);
            }
            while !dropped.get() {
                operations::yield_io().await;
            }
            operations::write_with_timeout(&write, b"x", None)
                .await
                .unwrap();
            let mut byte = [0];
            assert_eq!(operations::read(read.as_ref(), &mut byte).await.unwrap(), 1);
            assert_eq!(&byte, b"x");
            let mut sibling = client
                .send(request("/sibling", OutgoingBody::empty()))
                .await
                .unwrap();
            consume(sibling.body_mut(), 12 * 16 * 1024, false).await;
            client.control().graceful();
        };
        bounded(async {
            let ((), client, server) = futures::join!(app, connection.run(), server);
            client.unwrap();
            server.unwrap();
        })
        .await;
        operations::close(write).await.unwrap();
        operations::close(Rc::try_unwrap(read).unwrap())
            .await
            .unwrap();
    }
}

#[kimojio::test]
async fn server_graceful_shutdown_preserves_an_admitted_handler() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let shutdown = Shutdown::default();
    let (release, receive) = oneshot();
    let mut receive = Some(receive);
    let entered = Rc::new(Cell::new(false));
    let marker = entered.clone();
    let server = serve_connection_native_with_shutdown(
        peer,
        Config::default(),
        shutdown.clone(),
        move |request| {
            let receive = receive.take().unwrap();
            let marker = marker.clone();
            async move {
                drop(request);
                marker.set(true);
                receive.recv().await.unwrap();
                Ok(Response::new(streaming(12, true)))
            }
        },
    );
    let app = async {
        let mut response = Box::pin(client.send(request("/finish", OutgoingBody::empty())));
        assert!(response.as_mut().now_or_never().is_none());
        while !entered.get() {
            operations::yield_io().await;
        }
        shutdown.graceful();
        release.send(()).unwrap();
        consume(response.await.unwrap().body_mut(), 12 * 16 * 1024, true).await;
    };
    bounded(async {
        let ((), client, server) = futures::join!(app, connection.run(), server);
        client.unwrap();
        server.unwrap();
    })
    .await;
}

#[kimojio::test]
async fn server_request_chunk_survives_actual_close_until_release() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let shutdown = Shutdown::default();
    let (chunk, receive) = oneshot();
    let mut chunk = Some(chunk);
    let server_done = Cell::new(false);
    let client_done = Cell::new(false);
    let server = serve_connection_native_with_shutdown(
        peer,
        Config::default(),
        shutdown.clone(),
        move |mut request| {
            let send = chunk.take().unwrap();
            async move {
                let Some(IncomingFrame::Data(chunk)) = request.body_mut().frame().await? else {
                    panic!("data");
                };
                send.send(chunk).unwrap();
                std::future::pending::<Result<Response<OutgoingBody>, Error>>().await
            }
        },
    );
    let app = async {
        let mut sending =
            Box::pin(client.send(request("/retain", OutgoingBody::full(vec![0x42; 4096]))));
        assert!(sending.as_mut().now_or_never().is_none());
        let chunk = receive.recv().await.unwrap();
        shutdown.abort();
        shutdown.graceful();
        while !client_done.get() {
            operations::yield_io().await;
        }
        assert!(!server_done.get());
        assert!(chunk.iter().all(|byte| *byte == 0x42));
        drop(chunk);
        assert!(sending.await.is_err());
    };
    bounded(async {
        futures::join!(
            app,
            async {
                assert!(connection.run().await.is_err());
                client_done.set(true);
            },
            async {
                assert_eq!(
                    server.await,
                    Err(Error::Connection(kimojio_http2::ConnectionResult::Aborted))
                );
                server_done.set(true);
            }
        );
    })
    .await;
    assert!(server_done.get());
}

#[kimojio::test]
async fn dropping_request_after_terminal_delivery_does_not_reset_response() {
    for shape in 0..3 {
        let (fd, peer) = kimojio::pipe::bipipe();
        let config = Config {
            turn_budget: 1,
            ..Config::default()
        };
        let (client, connection) = connect_native(fd, config.clone());
        let server = serve_connection_native(peer, config, move |mut request| async move {
            if shape == 1 {
                assert!(matches!(
                    request.body_mut().frame().await?,
                    Some(IncomingFrame::Data(_))
                ));
            } else if shape == 2 {
                while let Some(frame) = request.body_mut().frame().await? {
                    if matches!(frame, IncomingFrame::Trailers(_)) {
                        break;
                    }
                }
            }
            // The handler intentionally does not await the pending ReceiveEnd.
            drop(request);
            Ok(Response::new(streaming(12, true)))
        });
        let app = async {
            let body = match shape {
                0 => OutgoingBody::empty(),
                1 => OutgoingBody::full(vec![0xa5; 1024]),
                _ => streaming(1, true),
            };
            let mut response = client.send(request("/drop-end", body)).await.unwrap();
            consume(response.body_mut(), 12 * 16 * 1024, true).await;
            client.control().graceful();
        };
        bounded(async {
            let ((), client, server) = futures::join!(app, connection.run(), server);
            client.unwrap();
            server.unwrap();
        })
        .await;
    }
}

#[kimojio::test]
async fn response_and_trailer_admission_pressure_never_replays_headers() {
    let (fd, peer) = kimojio::pipe::bipipe();
    rustix::net::sockopt::set_socket_send_buffer_size(&peer, 1024).unwrap();
    let (client, connection) = connect_native(fd, Config::default());
    let mut config = Config {
        turn_budget: 1,
        ..Config::default()
    };
    config.protocol.max_outbound_items = 4;
    config.protocol.max_outbound_capacity = 65536;
    let count = Rc::new(Cell::new(0));
    let seen = count.clone();
    let server = serve_connection_native(peer, config, move |request| {
        seen.set(seen.get() + 1);
        async move {
            let value = request
                .uri()
                .path()
                .trim_start_matches('/')
                .parse::<usize>()
                .unwrap();
            let text: String = (0..8192)
                .map(|i| char::from(b'a' + ((i * 17 + value * 13) % 26) as u8))
                .collect();
            let mut trailers = HeaderMap::new();
            trailers.insert("x-tail", text.parse().unwrap());
            let body = OutgoingBody::from_stream(futures::stream::iter([
                Ok(OutgoingFrame::Static(b"once")),
                Ok(OutgoingFrame::Trailers(trailers)),
            ]));
            Ok(Response::builder()
                .header("x-head", text)
                .body(body)
                .unwrap())
        }
    });
    let app = async {
        let mut tasks = Vec::new();
        for index in 0..24 {
            let client = client.clone();
            tasks.push(operations::spawn_task(async move {
                let mut response = client
                    .send(request(&format!("/{index}"), OutgoingBody::empty()))
                    .await
                    .unwrap();
                assert_eq!(response.headers()["x-head"].as_bytes().len(), 8192);
                let mut data = Vec::new();
                let mut trailers = 0;
                while let Some(frame) = response.body_mut().frame().await.unwrap() {
                    match frame {
                        IncomingFrame::Data(chunk) => data.extend_from_slice(&chunk),
                        IncomingFrame::Trailers(fields) => {
                            assert_eq!(fields["x-tail"].as_bytes().len(), 8192);
                            trailers += 1;
                        }
                    }
                }
                assert_eq!(data, b"once");
                assert_eq!(trailers, 1);
                response.body_mut().completion().await.unwrap();
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        client.control().graceful();
    };
    bounded(async {
        let ((), client, server) = futures::join!(app, connection.run(), server);
        client.unwrap();
        server.unwrap();
    })
    .await;
    assert_eq!(count.get(), 24);
}

#[kimojio::test]
async fn head_and_bodyless_statuses_do_not_poll_response_sources() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let polls = Rc::new(Cell::new(0));
    let checked = polls.clone();
    let server = serve_connection_native(peer, Config::default(), move |request| {
        let polls = checked.clone();
        async move {
            let status = request
                .uri()
                .path()
                .trim_start_matches('/')
                .parse::<u16>()
                .unwrap();
            let source = futures::stream::poll_fn(move |_| {
                polls.set(polls.get() + 1);
                std::task::Poll::Ready(Some(Ok(OutgoingFrame::Static(b"must-not-be-sent"))))
            });
            Ok(Response::builder()
                .status(status)
                .body(OutgoingBody::from_stream(source))
                .unwrap())
        }
    });
    let app = async {
        for (method, status) in [
            (Method::HEAD, 200),
            (Method::GET, 204),
            (Method::GET, 205),
            (Method::GET, 304),
        ] {
            let mut request = request(&format!("/{status}"), OutgoingBody::empty());
            *request.method_mut() = method;
            let mut response = client.send(request).await.unwrap();
            consume(response.body_mut(), 0, false).await;
        }
        client.control().graceful();
    };
    bounded(async {
        let ((), client, server) = futures::join!(app, connection.run(), server);
        client.unwrap();
        server.unwrap();
    })
    .await;
    assert_eq!(polls.get(), 0);
}

#[kimojio::test]
async fn server_capacity_rejects_only_the_excess_stream() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let config = Config {
        max_streams: 1,
        ..Config::default()
    };
    let (release, receive) = oneshot();
    let mut receive = Some(receive);
    let entered = Rc::new(Cell::new(false));
    let marker = entered.clone();
    let server = serve_connection_native(peer, config, move |request| {
        let wait = receive.take();
        let marker = marker.clone();
        async move {
            drop(request);
            if let Some(wait) = wait {
                marker.set(true);
                wait.recv().await.unwrap();
            }
            Ok(Response::new(OutgoingBody::empty()))
        }
    });
    let app = async {
        let mut accepted = Box::pin(client.send(request("/accepted", OutgoingBody::empty())));
        assert!(accepted.as_mut().now_or_never().is_none());
        while !entered.get() {
            operations::yield_io().await;
        }
        assert!(matches!(
            client
                .send(request("/refused", OutgoingBody::empty()))
                .await,
            Err(Error::Stream(StreamOutcome::Reset(7)))
        ));
        release.send(()).unwrap();
        consume(accepted.await.unwrap().body_mut(), 0, false).await;
        client.control().graceful();
    };
    bounded(async {
        let ((), client, server) = futures::join!(app, connection.run(), server);
        client.unwrap();
        server.unwrap();
    })
    .await;
}

#[cfg(feature = "virtual-clock")]
#[kimojio::test]
async fn graceful_server_deadline_revokes_a_pending_handler_in_virtual_time() {
    operations::virtual_clock_enable(true);
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let mut config = Config::default();
    config.protocol.shutdown_timeout = Duration::from_secs(1);
    let shutdown = Shutdown::default();
    let entered = Rc::new(Cell::new(false));
    let marker = entered.clone();
    let dropped = Rc::new(Cell::new(false));
    let ended = dropped.clone();
    let server =
        serve_connection_native_with_shutdown(peer, config, shutdown.clone(), move |request| {
            let marker = marker.clone();
            let ended = ended.clone();
            async move {
                let _guard = Dropped(ended);
                drop(request);
                marker.set(true);
                std::future::pending::<Result<Response<OutgoingBody>, Error>>().await
            }
        });
    let app = async {
        let mut pending = Box::pin(client.send(request("/deadline", OutgoingBody::empty())));
        assert!(pending.as_mut().now_or_never().is_none());
        while !entered.get() {
            operations::yield_io().await;
        }
        shutdown.graceful();
        while operations::virtual_clock_next_deadline()
            .is_none_or(|deadline| deadline > kimojio::clock_now() + Duration::from_secs(1))
        {
            operations::yield_io().await;
        }
        operations::virtual_clock_advance(Duration::from_secs(2));
        assert!(pending.await.is_err());
    };
    let ((), client, server) = futures::join!(app, connection.run(), server);
    assert!(client.is_err());
    server.unwrap();
    assert!(dropped.get());
    operations::virtual_clock_enable(false);
}

#[kimojio::test]
async fn handler_revocation_preserves_the_core_reset_outcome_while_a_lease_is_held() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let (send, receive) = oneshot();
    let mut send = Some(send);
    let dropped = Rc::new(Cell::new(false));
    let marker = dropped.clone();
    let server = serve_connection_native(
        peer,
        Config {
            turn_budget: 1,
            ..Config::default()
        },
        move |request| {
            let send = if request.uri().path() == "/retained" {
                send.take()
            } else {
                None
            };
            let marker = marker.clone();
            async move {
                if let Some(send) = send {
                    let _guard = Dropped(marker);
                    send.send(request.into_body()).unwrap();
                    std::future::pending::<()>().await;
                }
                Ok(Response::new(OutgoingBody::empty()))
            }
        },
    );
    let app = async {
        let mut pending =
            Box::pin(client.send(request("/retained", OutgoingBody::full(vec![0xa5; 1024]))));
        assert!(pending.as_mut().now_or_never().is_none());
        let mut body = receive.recv().await.unwrap();
        let Some(IncomingFrame::Data(held)) = body.frame().await.unwrap() else {
            panic!("data");
        };
        assert!(body.frame().await.unwrap().is_none());
        drop(pending);
        while !dropped.get() {
            operations::yield_io().await;
        }
        let mut sibling = client
            .send(request("/sibling", OutgoingBody::empty()))
            .await
            .unwrap();
        consume(sibling.body_mut(), 0, false).await;
        drop(held);
        assert_eq!(
            body.completion().await,
            Err(Error::Stream(StreamOutcome::Reset(8)))
        );
        assert_eq!(body.receive_outcome(), Some(StreamOutcome::Complete));
        client.control().graceful();
    };
    bounded(async {
        let ((), client, server) = futures::join!(app, connection.run(), server);
        client.unwrap();
        server.unwrap();
    })
    .await;
}

#[kimojio::test]
async fn reset_during_zero_copy_echo_releases_both_directions_and_preserves_sibling() {
    let (fd, peer) = kimojio::pipe::bipipe();
    rustix::net::sockopt::set_socket_send_buffer_size(&peer, 4096).unwrap();
    let (client, connection) = connect_native(fd, Config::default());
    let server = serve_connection_native(peer, Config::default(), |request| async move {
        Ok(Response::new(OutgoingBody::from_incoming(
            request.into_body(),
        )))
    });
    let app = async {
        let mut response = client
            .send(request("/reset-echo", streaming(96, true)))
            .await
            .unwrap();
        let Some(IncomingFrame::Data(held)) = response.body_mut().frame().await.unwrap() else {
            panic!("data");
        };
        response.body().cancel();
        assert!(held.iter().all(|byte| *byte == 0xa5));
        drop(held);
        assert!(response.body_mut().completion().await.is_err());
        let mut sibling = client
            .send(request("/sibling", streaming(12, true)))
            .await
            .unwrap();
        consume(sibling.body_mut(), 12 * 16 * 1024, true).await;
        client.control().graceful();
    };
    bounded(async {
        let ((), client, server) = futures::join!(app, connection.run(), server);
        client.unwrap();
        server.unwrap();
    })
    .await;
}
