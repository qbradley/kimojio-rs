use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    future::Future,
    rc::Rc,
    task::Poll,
    time::Duration,
};

use futures::FutureExt;
use http::{HeaderMap, Request};
use kimojio::operations;
use kimojio_fsm_http2 as core;

use crate::{
    Config, Error, IncomingFrame, OutgoingBody, OutgoingFrame, StreamOutcome,
    body::Data,
    connect_native,
    io::{self, Io, WriteDone},
};

#[derive(Default)]
struct Stats {
    requests: Vec<String>,
    bytes: usize,
    request_trailers: usize,
    ended: usize,
    resets: usize,
    closed: bool,
}

struct Reply {
    path: String,
    input: Vec<u8>,
    sent: usize,
}

struct Peer {
    stats: Rc<RefCell<Stats>>,
    streams: BTreeMap<core::StreamId, Reply>,
    closed: bool,
}

enum Action {
    Progress,
    Head(core::StreamId, String),
    Body(core::BodyOp),
    Permit(core::SendPermit),
    End(core::ReceiveEnd),
    Cancel(core::CancelCompletion),
}

struct PeerPorts<'a, I> {
    peer: &'a mut Peer,
    io: &'a mut I,
}

impl<I: Io> core::Ports<Data> for PeerPorts<'_, I> {
    type Output = Action;
    fn read(&mut self, op: core::ReadOp) -> Option<Action> {
        self.io.read(op);
        Some(Action::Progress)
    }
    fn write(&mut self, op: core::WriteOp<Data>) -> Option<Action> {
        self.io.write(op);
        Some(Action::Progress)
    }
    fn wake(&mut self, op: core::WakeOp) -> Option<Action> {
        self.io.wake(op);
        Some(Action::Progress)
    }
    fn close(&mut self, op: core::CloseOp) -> Option<Action> {
        self.io.close(op);
        Some(Action::Progress)
    }
    fn cancel(&mut self, op: core::CancelOp) -> Option<Action> {
        self.io.cancel(op.original());
        Some(Action::Cancel(op.complete()))
    }
    fn headers(&mut self, head: core::Head<'_>) -> Option<Action> {
        match head.kind {
            core::HeadKind::Request => {
                let path = head
                    .fields()
                    .find(|h| h.name == b":path")
                    .map(|h| String::from_utf8(h.value.to_vec()).unwrap())
                    .unwrap_or("/connect".into());
                Some(Action::Head(head.stream, path))
            }
            core::HeadKind::Trailers => {
                assert!(
                    head.fields()
                        .any(|field| matches!(field.name, b"x-upload" | b"x-download")
                            && field.value == b"done")
                );
                self.peer.stats.borrow_mut().request_trailers += 1;
                Some(Action::Progress)
            }
            _ => panic!("unexpected client headers"),
        }
    }
    fn body(&mut self, op: core::BodyOp) -> Option<Action> {
        Some(Action::Body(op))
    }
    fn send_ready(&mut self, permit: core::SendPermit) -> Option<Action> {
        Some(Action::Permit(permit))
    }
    fn send_stopped(&mut self, _: core::StreamId, _: core::SendStop) -> Option<Action> {
        Some(Action::Progress)
    }
    fn sent(&mut self, result: core::Sent<Data>) -> Option<Action> {
        assert!(result.exact);
        Some(Action::Progress)
    }
    fn ended(&mut self, end: core::ReceiveEnd) -> Option<Action> {
        Some(Action::End(end))
    }
    fn retired(&mut self, result: core::StreamResult) -> Option<Action> {
        if matches!(result.outcome, StreamOutcome::Reset(_)) {
            self.peer.stats.borrow_mut().resets += 1;
        }
        self.peer.streams.remove(&result.stream);
        Some(Action::Progress)
    }
    fn closed(&mut self, result: core::ConnectionResult) -> Option<Action> {
        assert!(
            matches!(
                result,
                core::ConnectionResult::Graceful
                    | core::ConnectionResult::PeerClosed
                    | core::ConnectionResult::IoFailed
                    | core::ConnectionResult::Aborted
            ),
            "{result:?}"
        );
        self.peer.closed = true;
        self.peer.stats.borrow_mut().closed = true;
        Some(Action::Progress)
    }
    fn reschedule(&mut self) -> Option<Action> {
        Some(Action::Progress)
    }
}

async fn reference(fd: kimojio::OwnedFd, stats: Rc<RefCell<Stats>>) {
    operations::io_scope(async move || {
        let config = core::Config::default();
        let mut machine = core::Server::<Data>::new(config, Duration::ZERO).unwrap();
        let epoch = kimojio::clock_now();
        let mut io = io::native(fd, epoch);
        let mut peer = Peer {
            stats,
            streams: BTreeMap::new(),
            closed: false,
        };
        loop {
            machine
                .advance_time(kimojio::clock_now().saturating_duration_since(epoch))
                .unwrap();
            let action = machine.next(&mut PeerPorts {
                peer: &mut peer,
                io: &mut io,
            });
            let runnable = action.is_some();
            match action {
                Some(Action::Head(id, path)) => {
                    peer.stats.borrow_mut().requests.push(path.clone());
                    peer.streams.insert(
                        id,
                        Reply {
                            path: path.clone(),
                            input: Vec::new(),
                            sent: 0,
                        },
                    );
                    match path.as_str() {
                        "/echo" | "/never" => {}
                        "/reset" => machine.reset(id, core::H2ErrorCode::Cancel).unwrap(),
                        _ => machine
                            .respond_ref(
                                id,
                                &[core::H2RawHeaderRef::new(b":status", b"200")],
                                matches!(path.as_str(), "/empty" | "/early-empty" | "/noerror"),
                            )
                            .unwrap(),
                    }
                }
                Some(Action::Body(op)) => {
                    peer.stats.borrow_mut().bytes += op.bytes().len();
                    if let Some(reply) = peer.streams.get_mut(&op.stream())
                        && reply.path == "/echo"
                    {
                        reply.input.extend_from_slice(op.bytes());
                    }
                    let id = op.stream();
                    machine.release_body(op.release()).unwrap();
                    if peer
                        .streams
                        .get(&id)
                        .is_some_and(|reply| reply.path == "/noerror")
                    {
                        machine.reset(id, core::H2ErrorCode::NoError).unwrap();
                    }
                }
                Some(Action::Permit(permit)) => {
                    let id = permit.stream();
                    let reply = peer.streams.get_mut(&id).unwrap();
                    match reply.path.as_str() {
                        "/early-data" => {
                            machine.send(permit, Data::Static(b"early"), true).unwrap()
                        }
                        "/early-trailers" => machine
                            .trailers_ref(id, &[core::H2RawHeaderRef::new(b"x-download", b"done")])
                            .unwrap(),
                        "/echo" => {
                            let n = (reply.input.len() - reply.sent).min(16 * 1024);
                            let data = reply.input[reply.sent..reply.sent + n].to_vec();
                            reply.sent += n;
                            machine
                                .send(permit, Data::Owned(data), reply.sent == reply.input.len())
                                .unwrap();
                        }
                        "/full" | "/trailers" | "/connect" => {
                            let total = 160 * 1024;
                            if reply.sent < total {
                                let n = (total - reply.sent).min(16 * 1024);
                                reply.sent += n;
                                machine
                                    .send(
                                        permit,
                                        Data::Owned(vec![0x5a; n]),
                                        reply.sent == total && reply.path != "/trailers",
                                    )
                                    .unwrap();
                            } else {
                                machine
                                    .trailers_ref(
                                        id,
                                        &[core::H2RawHeaderRef::new(b"x-download", b"done")],
                                    )
                                    .unwrap();
                            }
                        }
                        path => panic!("unexpected permit for {path}"),
                    }
                }
                Some(Action::End(end)) => {
                    if end.outcome == StreamOutcome::Complete {
                        peer.stats.borrow_mut().ended += 1;
                        if peer
                            .streams
                            .get(&end.stream)
                            .is_some_and(|r| r.path == "/echo")
                        {
                            machine
                                .respond_ref(
                                    end.stream,
                                    &[core::H2RawHeaderRef::new(b":status", b"200")],
                                    false,
                                )
                                .unwrap();
                        }
                    }
                }
                Some(Action::Cancel(done)) => machine.complete_cancel(done).unwrap(),
                _ => {}
            }
            if peer.closed {
                break;
            }
            futures::future::poll_fn(|cx| {
                if let Poll::Ready(done) = io.poll_read(cx) {
                    machine.complete_read(done).unwrap();
                    return Poll::Ready(());
                }
                if let Poll::Ready(done) = io.poll_write(cx) {
                    match done {
                        WriteDone::Data(done) => machine.complete_write(done).unwrap(),
                        WriteDone::Close(done, result) => {
                            result.unwrap();
                            machine.complete_close(done).unwrap();
                        }
                    }
                    return Poll::Ready(());
                }
                if let Poll::Ready(done) = io.poll_wake(cx) {
                    machine.complete_wake(done).unwrap();
                    return Poll::Ready(());
                }
                if runnable {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
            operations::yield_cpu().await;
        }
    })
    .await
}

fn request(path: &str, body: OutgoingBody) -> Request<OutgoingBody> {
    Request::builder()
        .method("POST")
        .uri(format!("http://test{path}"))
        .body(body)
        .unwrap()
}

fn streaming(chunks: usize, trailers: bool) -> OutgoingBody {
    let mut headers = HeaderMap::new();
    headers.insert("x-upload", "done".parse().unwrap());
    let data = (0..chunks).map(|_| Ok(OutgoingFrame::Data(vec![0xa5; 16 * 1024])));
    OutgoingBody::from_stream(futures::stream::iter(
        data.chain(trailers.then_some(Ok(OutgoingFrame::Trailers(headers)))),
    ))
}

async fn bounded(future: impl Future<Output = ()>) {
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(5), future)
        .await
        .unwrap();
}

#[kimojio::test]
async fn repeated_and_concurrent_requests_exceed_windows_in_both_directions() {
    let (fd, peer) = kimojio::pipe::bipipe();
    rustix::net::sockopt::set_socket_send_buffer_size(&fd, 4096).unwrap();
    rustix::net::sockopt::set_socket_send_buffer_size(&peer, 4096).unwrap();
    let (client, connection) = connect_native(fd, Config::default());
    let stats = Rc::new(RefCell::new(Stats::default()));
    let app = async {
        for _ in 0..3 {
            let first = async {
                let mut response = client
                    .send(request("/trailers", streaming(96, true)))
                    .await
                    .unwrap();
                assert_eq!(response.version(), http::Version::HTTP_2);
                let mut bytes = 0;
                let mut trailers = 0;
                while let Some(frame) = response.body_mut().frame().await.unwrap() {
                    match frame {
                        IncomingFrame::Data(chunk) => {
                            assert!(chunk.iter().all(|b| *b == 0x5a));
                            bytes += chunk.len();
                        }
                        IncomingFrame::Trailers(headers) => {
                            assert_eq!(headers["x-download"], "done");
                            trailers += 1;
                        }
                    }
                }
                assert_eq!(bytes, 160 * 1024);
                assert_eq!(trailers, 1);
                assert_eq!(
                    response.body_mut().completion().await.unwrap(),
                    StreamOutcome::Complete
                );
            };
            let second = async {
                let mut response = client
                    .clone()
                    .send(request("/echo", streaming(96, false)))
                    .await
                    .unwrap();
                assert_eq!(
                    response.body_mut().collect(2 * 1024 * 1024).await.unwrap(),
                    vec![0xa5; 96 * 16 * 1024]
                );
                response.body_mut().completion().await.unwrap();
            };
            futures::join!(first, second);
        }
        client.control().graceful();
    };
    bounded(async {
        let ((), result, ()) =
            futures::join!(app, connection.run(), reference(peer, stats.clone()));
        result.unwrap();
    })
    .await;
    assert_eq!(stats.borrow().requests.len(), 6);
    assert_eq!(stats.borrow().bytes, 6 * 96 * 16 * 1024);
    assert_eq!(stats.borrow().request_trailers, 3);
    assert!(stats.borrow().closed);
}

#[kimojio::test]
async fn dropping_complete_responses_does_not_cancel_uploads() {
    for path in ["/early-empty", "/early-data", "/early-trailers"] {
        let (fd, peer) = kimojio::pipe::bipipe();
        let (client, connection) = connect_native(
            fd,
            Config {
                turn_budget: 1,
                ..Config::default()
            },
        );
        let stats = Rc::new(RefCell::new(Stats::default()));
        let app = async {
            let response = client
                .send(request(path, streaming(12, false)))
                .await
                .unwrap();
            if path == "/early-empty" {
                drop(response);
            } else {
                let mut body = response.into_body();
                let frame = body.frame().await.unwrap().unwrap();
                drop(frame);
                // Deliberately do not await ReceiveEnd; it can still be in core notices.
                drop(body);
            }
            while stats.borrow().ended == 0 {
                operations::yield_io().await;
            }
            assert_eq!(stats.borrow().bytes, 12 * 16 * 1024);
            client.control().graceful();
        };
        bounded(async {
            let ((), result, ()) =
                futures::join!(app, connection.run(), reference(peer, stats.clone()));
            result.unwrap();
        })
        .await;
        assert_eq!(stats.borrow().resets, 0);
    }
}

#[kimojio::test]
async fn queued_cancellation_never_reaches_the_wire_and_admitted_cancel_is_local() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let stats = Rc::new(RefCell::new(Stats::default()));
    assert!(
        client
            .send(request("/must-not-arrive", OutgoingBody::empty()))
            .now_or_never()
            .is_none()
    );
    let app = async {
        let pending = Box::pin(client.send(request("/never", streaming(20, false))));
        let cancel = async {
            while !stats.borrow().requests.iter().any(|p| p == "/never") {
                operations::yield_io().await;
            }
        };
        match futures::future::select(pending, Box::pin(cancel)).await {
            futures::future::Either::Right(_) => {}
            _ => panic!("request completed before cancellation"),
        }
        let mut response = client
            .send(request("/empty", OutgoingBody::full(b"hello")))
            .await
            .unwrap();
        assert!(response.body_mut().frame().await.unwrap().is_none());
        response.body_mut().completion().await.unwrap();
        client.control().graceful();
    };
    bounded(async {
        let ((), result, ()) =
            futures::join!(app, connection.run(), reference(peer, stats.clone()));
        result.unwrap();
    })
    .await;
    assert!(
        !stats
            .borrow()
            .requests
            .iter()
            .any(|p| p == "/must-not-arrive")
    );
    assert!(stats.borrow().resets >= 1);
}

#[kimojio::test]
async fn limits_include_retained_capacity_and_zero_permits_are_not_eof() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let stats = Rc::new(RefCell::new(Stats::default()));
    let app = async {
        let mut oversized = Vec::with_capacity(128 * 1024);
        oversized.push(1);
        assert!(matches!(
            client
                .send(request("/empty", OutgoingBody::full(oversized)))
                .await,
            Err(Error::BufferTooLarge {
                capacity: 131072,
                ..
            })
        ));
        let mut bad = request(
            "/empty",
            OutgoingBody::from_stream(futures::stream::iter([Ok(OutgoingFrame::Data(vec![1]))])),
        );
        bad.headers_mut()
            .insert("content-length", "0".parse().unwrap());
        let result = client.send(bad).await;
        match result {
            Ok(mut response) => {
                let _ = response.body_mut().collect(0).await;
                assert!(matches!(
                    response.body_mut().completion().await,
                    Err(Error::BufferTooLarge { max_bytes: 0, .. })
                ));
            }
            Err(Error::BufferTooLarge { max_bytes: 0, .. }) => {}
            other => panic!("unexpected zero-permit result: {other:?}"),
        }
        let mut empty = request(
            "/empty",
            OutgoingBody::from_stream(futures::stream::empty()),
        );
        empty
            .headers_mut()
            .insert("content-length", "0".parse().unwrap());
        let mut response = client.send(empty).await.unwrap();
        response.body_mut().collect(0).await.unwrap();
        response.body_mut().completion().await.unwrap();
        client.control().graceful();
    };
    bounded(async {
        let ((), result, ()) = futures::join!(app, connection.run(), reference(peer, stats));
        result.unwrap();
    })
    .await;
}

#[kimojio::test]
async fn stream_reset_is_not_a_successful_response() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let stats = Rc::new(RefCell::new(Stats::default()));
    let app = async {
        assert!(matches!(
            client.send(request("/reset", OutgoingBody::empty())).await,
            Err(Error::Stream(StreamOutcome::Reset(_)))
        ));
        let mut response = client
            .send(request("/empty", OutgoingBody::empty()))
            .await
            .unwrap();
        response.body_mut().collect(0).await.unwrap();
        client.control().graceful();
    };
    bounded(async {
        let ((), result, ()) = futures::join!(app, connection.run(), reference(peer, stats));
        result.unwrap();
    })
    .await;
}

#[kimojio::test]
async fn forwarding_leases_and_trailers_across_connections() {
    let (origin, source) = kimojio::pipe::bipipe();
    let (destination, sink) = kimojio::pipe::bipipe();
    let (origin, origin_driver) = connect_native(origin, Config::default());
    let (destination, destination_driver) = connect_native(destination, Config::default());
    let source_stats = Rc::new(RefCell::new(Stats::default()));
    let sink_stats = Rc::new(RefCell::new(Stats::default()));
    let app = async {
        let response = origin
            .send(request("/trailers", OutgoingBody::empty()))
            .await
            .unwrap();
        let mut forwarded = destination
            .send(request(
                "/echo",
                OutgoingBody::from_incoming(response.into_body()),
            ))
            .await
            .unwrap();
        assert_eq!(
            forwarded.body_mut().collect(200 * 1024).await.unwrap(),
            vec![0x5a; 160 * 1024]
        );
        forwarded.body_mut().completion().await.unwrap();
        origin.control().graceful();
        destination.control().graceful();
    };
    bounded(async {
        let ((), a, b, (), ()) = futures::join!(
            app,
            origin_driver.run(),
            destination_driver.run(),
            reference(source, source_stats),
            reference(sink, sink_stats.clone())
        );
        a.unwrap();
        b.unwrap();
    })
    .await;
    assert_eq!(sink_stats.borrow().bytes, 160 * 1024);
    assert_eq!(sink_stats.borrow().request_trailers, 1);
}

#[kimojio::test]
async fn abandoned_body_drains_queued_leases_without_stopping_sibling() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let stats = Rc::new(RefCell::new(Stats::default()));
    let app = async {
        let response = client
            .send(request("/full", OutgoingBody::empty()))
            .await
            .unwrap();
        for _ in 0..10 {
            operations::yield_io().await;
        }
        drop(response);
        let mut sibling = client
            .send(request("/full", OutgoingBody::empty()))
            .await
            .unwrap();
        assert_eq!(
            sibling.body_mut().collect(200 * 1024).await.unwrap().len(),
            160 * 1024
        );
        sibling.body_mut().completion().await.unwrap();
        client.control().graceful();
    };
    bounded(async {
        let ((), result, ()) = futures::join!(app, connection.run(), reference(peer, stats));
        result.unwrap();
    })
    .await;
}

#[kimojio::test]
async fn held_chunk_outlives_actual_close_and_release_completes_retirement() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let stats = Rc::new(RefCell::new(Stats::default()));
    let returned = Cell::new(false);
    let driver = async {
        let result = connection.run().await;
        returned.set(true);
        assert_eq!(
            result,
            Err(Error::Connection(core::ConnectionResult::Aborted))
        );
    };
    let app = async {
        let mut response = client
            .send(request("/full", OutgoingBody::empty()))
            .await
            .unwrap();
        let Some(IncomingFrame::Data(chunk)) = response.body_mut().frame().await.unwrap() else {
            panic!("data expected");
        };
        assert!(chunk.retained_capacity() >= chunk.len());
        client.control().abort();
        while !stats.borrow().closed {
            operations::yield_io().await;
        }
        assert!(!returned.get(), "driver returned with a live core lease");
        assert!(chunk.iter().all(|b| *b == 0x5a));
        drop(chunk);
        assert!(response.body_mut().completion().await.is_err());
    };
    bounded(async {
        futures::join!(app, driver, reference(peer, stats.clone()));
    })
    .await;
    assert!(returned.get());
}

#[kimojio::test]
async fn native_io_producer_preserves_ordinary_combinator_scopes() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (read, write) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let stats = Rc::new(RefCell::new(Stats::default()));
    let entered = Rc::new(Cell::new(false));
    let source_entered = entered.clone();
    let source = futures::stream::once(async move {
        let mut bytes = vec![0; 16];
        source_entered.set(true);
        let result = operations::timeout_at(
            kimojio::clock_now() + Duration::from_secs(2),
            operations::read(&read, &mut bytes),
        )
        .await
        .unwrap()
        .unwrap();
        bytes.truncate(result);
        operations::close(read).await.unwrap();
        Ok(OutgoingFrame::Data(bytes))
    });
    let app = async {
        let mut response = client
            .send(request("/early-empty", OutgoingBody::from_stream(source)))
            .await
            .unwrap();
        assert!(response.body_mut().frame().await.unwrap().is_none());
        while !entered.get() {
            operations::yield_io().await;
        }
        assert!(response.body_mut().completion().now_or_never().is_none());
        operations::write_with_timeout(&write, b"native-source", None)
            .await
            .unwrap();
        operations::close(write).await.unwrap();
        response.body_mut().completion().await.unwrap();
        client.control().graceful();
    };
    bounded(async {
        let ((), result, ()) =
            futures::join!(app, connection.run(), reference(peer, stats.clone()));
        result.unwrap();
    })
    .await;
    assert_eq!(stats.borrow().bytes, b"native-source".len());
}

#[kimojio::test]
async fn producer_native_read_is_revoked_without_cancelling_sibling_io() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (read, write) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let stats = Rc::new(RefCell::new(Stats::default()));
    let entered = Rc::new(Cell::new(false));
    let source_entered = entered.clone();
    let source = futures::stream::once(async move {
        let mut bytes = vec![0; 64];
        source_entered.set(true);
        operations::read(&read, &mut bytes)
            .await
            .map_err(Error::Transport)?;
        Ok(OutgoingFrame::Data(bytes))
    });
    let app = async {
        let sending = Box::pin(client.send(request("/never", OutgoingBody::from_stream(source))));
        let cancel = async {
            while !entered.get() {
                operations::yield_io().await;
            }
        };
        assert!(matches!(
            futures::future::select(sending, Box::pin(cancel)).await,
            futures::future::Either::Right(_)
        ));
        let mut response = client
            .send(request("/full", streaming(10, false)))
            .await
            .unwrap();
        assert_eq!(
            response.body_mut().collect(200 * 1024).await.unwrap().len(),
            160 * 1024
        );
        response.body_mut().completion().await.unwrap();
        operations::close(write).await.unwrap();
        client.control().graceful();
    };
    bounded(async {
        let ((), result, ()) = futures::join!(app, connection.run(), reference(peer, stats));
        result.unwrap();
    })
    .await;
}

#[kimojio::test]
async fn classic_connect_and_ready_static_bodies() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let stats = Rc::new(RefCell::new(Stats::default()));
    let app = async {
        let request = Request::builder()
            .method("CONNECT")
            .uri("test:443")
            .body(OutgoingBody::from_static(b"tunnel"))
            .unwrap();
        let mut response = client.send(request).await.unwrap();
        assert_eq!(
            response.body_mut().collect(200 * 1024).await.unwrap().len(),
            160 * 1024
        );
        response.body_mut().completion().await.unwrap();
        client.control().graceful();
    };
    bounded(async {
        let ((), result, ()) =
            futures::join!(app, connection.run(), reference(peer, stats.clone()));
        result.unwrap();
    })
    .await;
    assert_eq!(stats.borrow().bytes, 6);
}

#[kimojio::test]
async fn ready_full_buffer_crosses_the_initial_stream_window() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let stats = Rc::new(RefCell::new(Stats::default()));
    let app = async {
        let mut response = client
            .send(request("/echo", OutgoingBody::full(vec![0x66; 64 * 1024])))
            .await
            .unwrap();
        assert_eq!(
            response.body_mut().collect(64 * 1024).await.unwrap(),
            vec![0x66; 64 * 1024]
        );
        response.body_mut().completion().await.unwrap();
        client.control().graceful();
    };
    bounded(async {
        let ((), result, ()) = futures::join!(app, connection.run(), reference(peer, stats));
        result.unwrap();
    })
    .await;
}

#[kimojio::test]
async fn dropped_unstarted_connection_rejects_queued_and_future_requests() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let mut send = Box::pin(client.send(request("/empty", OutgoingBody::empty())));
    assert!(send.as_mut().now_or_never().is_none());
    drop(connection);
    assert!(matches!(send.await, Err(Error::Closed)));
    assert!(matches!(
        client.send(request("/empty", OutgoingBody::empty())).await,
        Err(Error::Closed)
    ));
    operations::close(peer).await.unwrap();
}

#[kimojio::test]
async fn blocked_metadata_retries_and_queued_cancellation_preserve_siblings() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let mut config = Config::default();
    config.protocol.http = config.protocol.http.set_max_active_streams(1);
    let (client, connection) = connect_native(fd, config);
    let stats = Rc::new(RefCell::new(Stats::default()));
    let app = async {
        let mut first = Box::pin(client.send(request("/never", OutgoingBody::empty())));
        assert!(first.as_mut().now_or_never().is_none());
        while stats.borrow().requests.is_empty() {
            operations::yield_io().await;
        }
        let mut blocked = Box::pin(client.send(request("/must-not-arrive", OutgoingBody::empty())));
        assert!(blocked.as_mut().now_or_never().is_none());
        for _ in 0..20 {
            operations::yield_io().await;
        }
        assert_eq!(stats.borrow().requests.len(), 1);
        drop(blocked);
        let mut next = Box::pin(client.send(request("/empty", OutgoingBody::empty())));
        assert!(next.as_mut().now_or_never().is_none());
        for _ in 0..10 {
            operations::yield_io().await;
        }
        assert_eq!(stats.borrow().requests.len(), 1);
        drop(first);
        let mut response = next.await.unwrap();
        response.body_mut().collect(0).await.unwrap();
        response.body_mut().completion().await.unwrap();
        client.control().graceful();
    };
    bounded(async {
        let ((), result, ()) =
            futures::join!(app, connection.run(), reference(peer, stats.clone()));
        result.unwrap();
    })
    .await;
    assert_eq!(stats.borrow().requests, ["/never", "/empty"]);
}

#[kimojio::test]
async fn bounded_queue_reports_item_and_storage_overflow() {
    for config in [
        Config {
            max_queued_requests: 1,
            ..Config::default()
        },
        Config {
            max_queued_storage: 1,
            ..Config::default()
        },
    ] {
        let (fd, peer) = kimojio::pipe::bipipe();
        let (client, connection) = connect_native(fd, config);
        let mut first = Box::pin(client.send(request("/empty", OutgoingBody::empty())));
        let _ = first.as_mut().now_or_never();
        assert!(matches!(
            client.send(request("/empty", OutgoingBody::empty())).await,
            Err(Error::Limit)
        ));
        drop(first);
        drop(connection);
        operations::close(peer).await.unwrap();
    }
}

#[kimojio::test]
async fn permanent_invalid_trailers_fail_the_stream_instead_of_waiting_forever() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let stats = Rc::new(RefCell::new(Stats::default()));
    let app = async {
        let mut upload = request("/early-empty", streaming(0, true));
        upload
            .headers_mut()
            .insert("content-length", "1".parse().unwrap());
        match client.send(upload).await {
            Ok(mut response) => {
                let _ = response.body_mut().collect(0).await;
                assert!(response.body_mut().completion().await.is_err());
            }
            Err(Error::Command(core::CommandError::InvalidState)) => {}
            other => panic!("unexpected result: {other:?}"),
        }
        let mut response = client
            .send(request("/empty", OutgoingBody::empty()))
            .await
            .unwrap();
        response.body_mut().collect(0).await.unwrap();
        client.control().graceful();
    };
    bounded(async {
        let ((), result, ()) = futures::join!(app, connection.run(), reference(peer, stats));
        result.unwrap();
    })
    .await;
}

#[kimojio::test]
async fn peer_goaway_reports_unprocessed_without_success_fallback() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let app = async {
        assert!(matches!(
            client.send(request("/never", OutgoingBody::empty())).await,
            Err(Error::Stream(StreamOutcome::Unprocessed))
        ));
        client.control().graceful();
    };
    let server = async {
        let mut bytes = [0; 4096];
        assert!(operations::read(&peer, &mut bytes).await.unwrap() > 0);
        let settings_and_goaway = [
            0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 8, 7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        let mut sent = 0;
        while sent < settings_and_goaway.len() {
            sent += operations::write_with_timeout(&peer, &settings_and_goaway[sent..], None)
                .await
                .unwrap();
        }
        while let Ok(n) = operations::read(&peer, &mut bytes).await {
            if n == 0 {
                break;
            }
        }
        operations::close(peer).await.unwrap();
    };
    bounded(async {
        let ((), result, ()) = futures::join!(app, connection.run(), server);
        result.unwrap();
    })
    .await;
}

#[kimojio::test]
async fn explicit_body_cancel_stops_upload_after_receive_eof() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let stats = Rc::new(RefCell::new(Stats::default()));
    let app = async {
        let mut response = client
            .send(request(
                "/early-empty",
                OutgoingBody::from_stream(futures::stream::pending()),
            ))
            .await
            .unwrap();
        assert!(response.body_mut().frame().await.unwrap().is_none());
        response.body().cancel();
        response.body().cancel();
        assert!(matches!(
            response.body_mut().completion().await,
            Err(Error::Cancelled)
        ));
        assert!(response.body_mut().frame().await.unwrap().is_none());
        let mut sibling = client
            .send(request("/empty", OutgoingBody::empty()))
            .await
            .unwrap();
        sibling.body_mut().collect(0).await.unwrap();
        client.control().graceful();
    };
    bounded(async {
        let ((), result, ()) = futures::join!(app, connection.run(), reference(peer, stats));
        result.unwrap();
    })
    .await;
}

#[kimojio::test]
async fn no_error_reset_preserves_complete_response_but_reports_upload_outcome() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let stats = Rc::new(RefCell::new(Stats::default()));
    let (go, receive) = kimojio::oneshot();
    let app = async {
        let source = futures::stream::once(async move {
            receive.recv().await.unwrap();
            Ok(OutgoingFrame::Data(vec![42]))
        });
        let source = futures::StreamExt::chain(source, futures::stream::pending());
        let mut response = client
            .send(request("/noerror", OutgoingBody::from_stream(source)))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert!(response.body_mut().frame().await.unwrap().is_none());
        go.send(()).unwrap();
        assert!(matches!(
            response.body_mut().completion().await,
            Err(Error::Stream(StreamOutcome::Reset(0)))
                | Err(Error::Send {
                    reason: core::SendStop::Reset(0),
                    ..
                })
        ));
        assert_eq!(
            response.body().receive_outcome(),
            Some(StreamOutcome::Complete)
        );
        assert!(response.body_mut().frame().await.unwrap().is_none());
        client.control().graceful();
    };
    bounded(async {
        let ((), result, ()) = futures::join!(app, connection.run(), reference(peer, stats));
        result.unwrap();
    })
    .await;
}

#[cfg(feature = "virtual-clock")]
#[kimojio::test]
async fn settings_timeout_uses_virtual_clock_and_closes_descriptor() {
    operations::virtual_clock_enable(true);
    let (fd, peer) = kimojio::pipe::bipipe();
    let mut config = Config::default();
    config.protocol.settings_timeout = Duration::from_secs(1);
    let (_client, connection) = connect_native(fd, config);
    let control = async {
        while operations::virtual_clock_pending_timers() == 0 {
            operations::yield_io().await;
        }
        operations::virtual_clock_advance(Duration::from_secs(2));
        let mut bytes = [0; 4096];
        loop {
            match operations::read(&peer, &mut bytes).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        operations::close(peer).await.unwrap();
    };
    let (result, ()) = futures::join!(connection.run(), control);
    assert!(matches!(result, Err(Error::Connection(_))), "{result:?}");
    operations::virtual_clock_enable(false);
}
