use std::{
    cell::{Cell, RefCell},
    future::Future,
    rc::Rc,
    task::Poll,
    time::Duration,
};

use futures::FutureExt;
use kimojio::{oneshot, operations};
use kimojio_http2::{
    Config, Error, IncomingBody, IncomingFrame, OutgoingBody, OutgoingFrame, ReceiveEnd,
    RequestObserver, StreamId, StreamOutcome, StreamReport, connect_native,
    http::{HeaderMap, Request, Response},
    serve_connection_native,
};

fn request(path: &str, body: OutgoingBody) -> Request<OutgoingBody> {
    Request::builder()
        .method("POST")
        .uri(format!("http://acceptance.test{path}"))
        .body(body)
        .unwrap()
}

async fn bounded(future: impl Future<Output = ()>) {
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(10), future)
        .await
        .unwrap();
}

#[derive(Default)]
struct Trace {
    admitted: Cell<Option<StreamId>>,
    end: Cell<Option<StreamOutcome>>,
    report: RefCell<Option<StreamReport>>,
    dropped: Cell<bool>,
}

struct Observer(Rc<Trace>);

impl RequestObserver for Observer {
    fn admitted(&mut self, stream: StreamId) {
        assert!(self.0.admitted.replace(Some(stream)).is_none());
    }
    fn receive_end(&mut self, end: ReceiveEnd) {
        assert_eq!(self.0.admitted.get(), Some(end.stream));
        assert!(self.0.end.replace(Some(end.outcome)).is_none());
    }
    fn retired(&mut self, report: &StreamReport) {
        assert_eq!(self.0.end.get(), report.receive_outcome);
        assert!(self.0.report.borrow_mut().replace(report.clone()).is_none());
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        self.0.dropped.set(true);
    }
}

fn counted_empty(polls: Rc<Cell<usize>>) -> OutgoingBody {
    OutgoingBody::from_stream(futures::stream::poll_fn(move |_| {
        polls.set(polls.get() + 1);
        Poll::Ready(None)
    }))
}

fn streaming(chunks: usize, produced: Rc<Cell<usize>>) -> OutgoingBody {
    let mut trailers = HeaderMap::new();
    trailers.insert("x-end", "complete".parse().unwrap());
    OutgoingBody::from_stream(futures::stream::iter(
        (0..chunks)
            .map(move |_| {
                produced.set(produced.get() + 1);
                Ok(OutgoingFrame::Data(vec![0xa5; 16 * 1024]))
            })
            .chain([Ok(OutgoingFrame::Trailers(trailers))]),
    ))
}

// Poll the application immediately after each driver poll. With turn_budget=1,
// this exposes real delivery before the next core notification, without hooks
// into the driver or a fabricated completion.
async fn drive_application(
    application: impl Future<Output = ()>,
    connection: impl Future<Output = Result<(), Error>>,
) -> Result<(), Error> {
    futures::pin_mut!(application, connection);
    let mut app_done = false;
    let mut result = None;
    futures::future::poll_fn(|cx| {
        if !app_done {
            app_done = application.as_mut().poll(cx).is_ready();
        }
        if result.is_none()
            && let Poll::Ready(done) = connection.as_mut().poll(cx)
        {
            result = Some(done);
        }
        if !app_done {
            app_done = application.as_mut().poll(cx).is_ready();
        }
        if app_done && let Some(result) = result.take() {
            Poll::Ready(result)
        } else {
            Poll::Pending
        }
    })
    .await
}

#[kimojio::test]
async fn terminal_response_drop_crossproduct_preserves_delayed_upload_and_retirement() {
    for shape in 0..3 {
        for (budget, after_end) in [(1, false), (1, true), (64, true)] {
            let (fd, peer) = kimojio::pipe::bipipe();
            let (client, connection) = connect_native(
                fd,
                Config {
                    turn_budget: budget,
                    ..Config::default()
                },
            );
            let (upload, receive_upload) = oneshot();
            let produced = Rc::new(Cell::new(0));
            let source = {
                let produced = produced.clone();
                futures::stream::once(async move {
                    receive_upload.recv().await.unwrap();
                    produced.set(produced.get() + 1);
                    Ok(OutgoingFrame::Data(vec![0xa5; 65536]))
                })
            };
            let received = Rc::new(Cell::new(0));
            let readers = Rc::new(RefCell::new(Vec::new()));
            let server = serve_connection_native(peer, Config::default(), {
                let received = received.clone();
                let readers = readers.clone();
                move |request| {
                    let received = received.clone();
                    let readers = readers.clone();
                    async move {
                        if request.uri().path() == "/sibling" {
                            return Ok(Response::new(OutgoingBody::empty()));
                        }
                        readers
                            .borrow_mut()
                            .push(operations::spawn_task(async move {
                                let mut body = request.into_body();
                                let bytes = body.collect(65536).await.unwrap();
                                assert!(bytes.iter().all(|byte| *byte == 0xa5));
                                received.set(bytes.len());
                                assert_eq!(
                                    body.retirement().await.unwrap().outcome,
                                    StreamOutcome::Complete
                                );
                            }));
                        let body = match shape {
                            0 => OutgoingBody::empty(),
                            1 => OutgoingBody::from_static(b"terminal"),
                            _ => {
                                let mut trailers = HeaderMap::new();
                                trailers.insert("x-end", "complete".parse().unwrap());
                                OutgoingBody::from_stream(futures::stream::iter([Ok(
                                    OutgoingFrame::Trailers(trailers),
                                )]))
                            }
                        };
                        Ok(Response::new(body))
                    }
                }
            });
            let trace = Rc::new(Trace::default());
            let app = async {
                let mut response = client
                    .send_with_observer(
                        request("/drop-end", OutgoingBody::from_stream(source)),
                        Observer(trace.clone()),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), 200);
                if shape == 1 {
                    let Some(IncomingFrame::Data(chunk)) =
                        response.body_mut().frame().await.unwrap()
                    else {
                        panic!("terminal DATA")
                    };
                    assert_eq!(&*chunk, b"terminal");
                    drop(chunk);
                } else if shape == 2 {
                    let Some(IncomingFrame::Trailers(headers)) =
                        response.body_mut().frame().await.unwrap()
                    else {
                        panic!("terminal trailers")
                    };
                    assert_eq!(headers["x-end"], "complete");
                }
                if after_end {
                    assert!(response.body_mut().frame().await.unwrap().is_none());
                    assert_eq!(
                        response.body().receive_outcome(),
                        Some(StreamOutcome::Complete)
                    );
                    assert_eq!(trace.end.get(), Some(StreamOutcome::Complete));
                } else {
                    assert_eq!(
                        response.body().receive_outcome(),
                        None,
                        "shape {shape} did not expose pending ReceiveEnd"
                    );
                    assert_eq!(trace.end.get(), None);
                }
                assert_eq!(
                    produced.get(),
                    0,
                    "upload completed before the response drop"
                );
                drop(response);
                upload.send(()).unwrap();
                while trace.report.borrow().is_none() || received.get() != 65536 {
                    operations::yield_io().await;
                }
                let report = trace.report.borrow().clone().unwrap();
                assert_eq!(report.outcome, StreamOutcome::Complete);
                assert_eq!(report.receive_outcome, Some(StreamOutcome::Complete));
                assert_eq!(report.send_failure, None);
                assert_eq!(report.error, None);
                assert_eq!(produced.get(), 1);
                let mut sibling = client
                    .send(request("/sibling", OutgoingBody::empty()))
                    .await
                    .unwrap();
                assert!(sibling.body_mut().frame().await.unwrap().is_none());
                assert_eq!(
                    sibling.body_mut().retirement().await.unwrap().outcome,
                    StreamOutcome::Complete
                );
                client.control().graceful();
            };
            bounded(async {
                let (client, server) =
                    futures::join!(drive_application(app, connection.run()), server);
                client.unwrap();
                server.unwrap();
                let readers = std::mem::take(&mut *readers.borrow_mut());
                for reader in readers {
                    reader.await.unwrap();
                }
            })
            .await;
        }
    }
}

#[kimojio::test]
async fn permanent_metadata_rejection_preserves_identity_sources_and_later_field_decoding() {
    for capacity in [false, true] {
        let (fd, peer) = kimojio::pipe::bipipe();
        let mut config = Config::default();
        config.protocol.max_outbound_capacity = 65536;
        if capacity {
            config.protocol.http = config.protocol.http.set_max_header_bytes(1024 * 1024);
        }
        let (client, connection) = connect_native(fd, config.clone());
        let paths = Rc::new(RefCell::new(Vec::new()));
        let server = serve_connection_native(peer, config, {
            let paths = paths.clone();
            move |request| {
                paths.borrow_mut().push(request.uri().path().to_owned());
                async move {
                    assert_ne!(request.uri().path(), "/oversized");
                    let value = request.headers()["x-dynamic"].clone();
                    Ok(Response::builder()
                        .header("x-observed", value)
                        .body(OutgoingBody::empty())
                        .unwrap())
                }
            }
        });
        let polls = Rc::new(Cell::new(0));
        let trace = Rc::new(Trace::default());
        let app = async {
            for (index, value) in ["before", "before", "after", "before"]
                .into_iter()
                .enumerate()
            {
                if index == 1 {
                    let mut oversized = request("/oversized", counted_empty(polls.clone()));
                    oversized
                        .headers_mut()
                        .insert("x-dynamic", "rejected".parse().unwrap());
                    oversized
                        .headers_mut()
                        .insert("x-large", "~".repeat(128 * 1024).parse().unwrap());
                    let result = client
                        .send_with_observer(oversized, Observer(trace.clone()))
                        .await;
                    if capacity {
                        assert!(
                            matches!(
                                result,
                                Err(Error::Command(kimojio_fsm_http2::CommandError::Capacity))
                            ),
                            "{result:?}"
                        );
                    } else {
                        assert!(
                            matches!(
                                result,
                                Err(Error::Command(kimojio_fsm_http2::CommandError::Message(_)))
                            ),
                            "{result:?}"
                        );
                    }
                    assert_eq!(trace.admitted.get(), None);
                    assert!(trace.report.borrow().is_none());
                    assert_eq!(polls.get(), 0);
                }
                let mut next = request(&format!("/valid-{index}"), OutgoingBody::empty());
                next.headers_mut()
                    .insert("x-dynamic", value.parse().unwrap());
                let mut response = client.send(next).await.unwrap();
                assert_eq!(response.headers()["x-observed"], value);
                assert_eq!(response.body().stream_id().get(), 2 * index as u32 + 1);
                response.body_mut().collect(0).await.unwrap();
                assert_eq!(
                    response.body_mut().retirement().await.unwrap().outcome,
                    StreamOutcome::Complete
                );
            }
            assert_eq!(
                *paths.borrow(),
                ["/valid-0", "/valid-1", "/valid-2", "/valid-3"]
            );
            assert_eq!(polls.get(), 0);
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
async fn cross_connection_duplex_forwarding_overlaps_both_windows_and_retires_all_streams() {
    let (front, relay) = kimojio::pipe::bipipe();
    let (back, destination) = kimojio::pipe::bipipe();
    let mut config = Config::default();
    config.protocol.connection_receive_window = 65535;
    config.protocol.max_stream_receive_capacity = 2 * 1024 * 1024;
    let (client, front_driver) = connect_native(front, config.clone());
    let (upstream, back_driver) = connect_native(back, config.clone());
    let backend = serve_connection_native(destination, config.clone(), |request| async move {
        Ok(Response::new(OutgoingBody::from_incoming(
            request.into_body(),
        )))
    });
    let traces = Rc::new(RefCell::new(Vec::new()));
    let relay_server = serve_connection_native(relay, config, {
        let upstream = upstream.clone();
        let traces = traces.clone();
        move |incoming: Request<IncomingBody>| {
            let upstream = upstream.clone();
            let trace = Rc::new(Trace::default());
            traces.borrow_mut().push(trace.clone());
            async move {
                let path = incoming.uri().path().to_owned();
                let response = upstream
                    .send_with_observer(
                        request(&path, OutgoingBody::from_incoming(incoming.into_body())),
                        Observer(trace),
                    )
                    .await?;
                Ok(Response::new(OutgoingBody::from_incoming(
                    response.into_body(),
                )))
            }
        }
    });
    let produced = Rc::new(Cell::new(0));
    let trace = Rc::new(Trace::default());
    let app = async {
        let mut response = client
            .send_with_observer(
                request("/duplex", streaming(96, produced.clone())),
                Observer(trace.clone()),
            )
            .await
            .unwrap();
        let Some(IncomingFrame::Data(held)) = response.body_mut().frame().await.unwrap() else {
            panic!("overlapping DATA")
        };
        let mut bytes = held.len();
        assert!(
            produced.get() < 96,
            "upload ended before the first echoed data"
        );
        assert_eq!(trace.end.get(), None);
        let mut sibling = client
            .send(request("/sibling", OutgoingBody::empty()))
            .await
            .unwrap();
        sibling.body_mut().collect(0).await.unwrap();
        assert_eq!(
            sibling.body_mut().retirement().await.unwrap().outcome,
            StreamOutcome::Complete
        );
        assert!(
            produced.get() < 96,
            "paused consumer did not bound the forwarded upload"
        );
        assert!(held.iter().all(|byte| *byte == 0xa5));
        drop(held);
        let mut trailers = 0;
        while let Some(frame) = response.body_mut().frame().await.unwrap() {
            match frame {
                IncomingFrame::Data(chunk) => {
                    assert!(chunk.iter().all(|byte| *byte == 0xa5));
                    bytes += chunk.len();
                }
                IncomingFrame::Trailers(fields) => {
                    assert_eq!(fields["x-end"], "complete");
                    trailers += 1;
                }
            }
        }
        assert_eq!(bytes, 96 * 16 * 1024);
        assert_eq!(trailers, 1);
        assert_eq!(produced.get(), 96);
        assert_eq!(
            response.body_mut().retirement().await.unwrap().outcome,
            StreamOutcome::Complete
        );
        while traces
            .borrow()
            .iter()
            .any(|trace| trace.report.borrow().is_none())
        {
            operations::yield_io().await;
        }
        assert_eq!(traces.borrow().len(), 2);
        for trace in traces.borrow().iter() {
            let report = trace.report.borrow();
            let report = report.as_ref().unwrap();
            assert_eq!(report.outcome, StreamOutcome::Complete);
            assert_eq!(report.receive_outcome, Some(StreamOutcome::Complete));
            assert_eq!(report.error, None);
            assert_eq!(report.send_failure, None);
        }
        client.control().graceful();
        upstream.control().graceful();
    };
    bounded(async {
        let ((), front, relay, back, backend) = futures::join!(
            app,
            front_driver.run(),
            relay_server,
            back_driver.run(),
            backend,
        );
        front.unwrap();
        relay.unwrap();
        back.unwrap();
        backend.unwrap();
    })
    .await;
}

#[kimojio::test]
async fn full_submission_budget_cancellation_has_no_headers_source_poll_or_sibling_reset() {
    for blocked in [false, true] {
        let (fd, peer) = kimojio::pipe::bipipe();
        let mut config = Config {
            max_queued_requests: 1,
            ..Config::default()
        };
        config.protocol.http = config.protocol.http.set_max_active_streams(1);
        let (client, connection) = connect_native(fd, config);
        let (release, wait) = oneshot();
        let mut wait = Some(wait);
        let active = Rc::new(Cell::new(false));
        let paths = Rc::new(RefCell::new(Vec::new()));
        let server = serve_connection_native(peer, Config::default(), {
            let active = active.clone();
            let paths = paths.clone();
            move |request| {
                let path = request.uri().path().to_owned();
                paths.borrow_mut().push(path.clone());
                assert!(
                    matches!(path.as_str(), "/active" | "/sibling"),
                    "cancelled or overflow request reached the peer"
                );
                let wait = if path == "/active" { wait.take() } else { None };
                let active = active.clone();
                async move {
                    if let Some(wait) = wait {
                        active.set(true);
                        wait.recv().await.unwrap();
                    }

                    Ok(Response::new(OutgoingBody::empty()))
                }
            }
        });
        let polls = Rc::new(Cell::new(0));
        let trace = Rc::new(Trace::default());
        let mut cancelled = Box::pin(client.send_with_observer(
            request("/cancelled", counted_empty(polls.clone())),
            Observer(trace.clone()),
        ));
        if !blocked {
            assert!(cancelled.as_mut().now_or_never().is_none());
        }
        let app = async {
            let mut admitted = Box::pin(client.send(request("/active", OutgoingBody::empty())));
            if blocked {
                assert!(admitted.as_mut().now_or_never().is_none());
                while !active.get() {
                    operations::yield_io().await;
                }
                assert!(cancelled.as_mut().now_or_never().is_none());
                for _ in 0..8 {
                    operations::yield_io().await;
                }
            }
            assert!(matches!(
                client
                    .send(request("/overflow", counted_empty(polls.clone())))
                    .await,
                Err(Error::Limit)
            ));
            assert_eq!(polls.get(), 0);
            assert_eq!(trace.admitted.get(), None);
            drop(cancelled);
            while !trace.dropped.get() {
                operations::yield_io().await;
            }
            assert_eq!(trace.admitted.get(), None);
            assert_eq!(trace.end.get(), None);
            assert!(trace.report.borrow().is_none());
            assert_eq!(polls.get(), 0);
            if blocked {
                assert!(
                    admitted.as_mut().now_or_never().is_none(),
                    "active sibling was cancelled"
                );
                release.send(()).unwrap();
                let mut response = admitted.await.unwrap();
                response.body_mut().collect(0).await.unwrap();
                assert_eq!(
                    response.body_mut().retirement().await.unwrap().outcome,
                    StreamOutcome::Complete
                );
            } else {
                drop(admitted);
                drop(release);
            }
            let mut response = client
                .send(request("/sibling", OutgoingBody::empty()))
                .await
                .unwrap();
            response.body_mut().collect(0).await.unwrap();
            assert_eq!(
                response.body_mut().retirement().await.unwrap().outcome,
                StreamOutcome::Complete
            );
            assert_eq!(
                *paths.borrow(),
                if blocked {
                    vec!["/active", "/sibling"]
                } else {
                    vec!["/sibling"]
                }
            );
            assert_eq!(polls.get(), 0);
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

#[cfg(feature = "virtual-clock")]
#[kimojio::test]
async fn virtual_application_deadlines_cancel_admission_and_body_without_masking_source_errors() {
    operations::virtual_clock_enable(true);
    let (fd, peer) = kimojio::pipe::bipipe();
    let mut config = Config::default();
    config.protocol.http = config.protocol.http.set_max_active_streams(1);
    let (client, connection) = connect_native(fd, config);
    let (release, wait) = oneshot();
    let mut wait = Some(wait);
    let paths = Rc::new(RefCell::new(Vec::new()));
    let server = serve_connection_native(peer, Config::default(), {
        let paths = paths.clone();
        move |request| {
            let path = request.uri().path().to_owned();
            paths.borrow_mut().push(path.clone());
            let wait = if path == "/hold" { wait.take() } else { None };
            async move {
                drop(request);
                if let Some(wait) = wait {
                    wait.recv().await.unwrap();
                }
                if path == "/source-error" {
                    return std::future::pending().await;
                }
                Ok(Response::new(if path == "/pending-body" {
                    OutgoingBody::from_stream(futures::stream::pending())
                } else {
                    OutgoingBody::empty()
                }))
            }
        }
    });
    let app = async {
        let mut warmup = client
            .send(request("/warmup", OutgoingBody::empty()))
            .await
            .unwrap();
        warmup.body_mut().collect(0).await.unwrap();
        assert_eq!(
            warmup.body_mut().retirement().await.unwrap().outcome,
            StreamOutcome::Complete
        );

        let mut active = Box::pin(client.send(request("/hold", OutgoingBody::empty())));
        assert!(active.as_mut().now_or_never().is_none());
        while !paths.borrow().iter().any(|path| path == "/hold") {
            operations::yield_io().await;
        }
        let trace = Rc::new(Trace::default());
        let polls = Rc::new(Cell::new(0));
        let mut timed = Box::pin(operations::timeout_at(
            kimojio::clock_now() + Duration::from_secs(1),
            client.send_with_observer(
                request("/timed-admission", counted_empty(polls.clone())),
                Observer(trace.clone()),
            ),
        ));
        assert!(timed.as_mut().now_or_never().is_none());
        for _ in 0..8 {
            operations::yield_io().await;
        }
        assert_eq!(trace.admitted.get(), None);
        operations::virtual_clock_advance(Duration::from_secs(2));
        assert!(matches!(timed.await, Err(kimojio::TimeoutError::Timeout)));
        while !trace.dropped.get() {
            operations::yield_io().await;
        }
        assert_eq!(polls.get(), 0);
        assert_eq!(trace.admitted.get(), None);
        assert!(trace.report.borrow().is_none());
        assert!(!paths.borrow().iter().any(|path| path == "/timed-admission"));
        assert!(active.as_mut().now_or_never().is_none());
        release.send(()).unwrap();
        let mut active = active.await.unwrap();
        active.body_mut().collect(0).await.unwrap();
        assert_eq!(
            active.body_mut().retirement().await.unwrap().outcome,
            StreamOutcome::Complete
        );

        let body_trace = Rc::new(Trace::default());
        let mut response = client
            .send_with_observer(
                request("/pending-body", OutgoingBody::empty()),
                Observer(body_trace.clone()),
            )
            .await
            .unwrap();
        let mut timed = Box::pin(operations::timeout_at(
            kimojio::clock_now() + Duration::from_secs(1),
            response.body_mut().frame(),
        ));
        assert!(timed.as_mut().now_or_never().is_none());
        operations::virtual_clock_advance(Duration::from_secs(2));
        assert!(matches!(timed.await, Err(kimojio::TimeoutError::Timeout)));
        assert_eq!(response.body().receive_outcome(), None);
        response.body().cancel();
        drop(response);
        while body_trace.report.borrow().is_none() {
            operations::yield_io().await;
        }
        assert_eq!(
            body_trace.report.borrow().as_ref().unwrap().outcome,
            StreamOutcome::Reset(8)
        );

        let error_trace = Rc::new(Trace::default());
        let before = kimojio::clock_now();
        let source = futures::stream::once(async {
            Err(Error::Application(
                "source failed before its deadline".into(),
            ))
        });
        let result = operations::timeout_at(
            before + Duration::from_secs(1),
            client.send_with_observer(
                request("/source-error", OutgoingBody::from_stream(source)),
                Observer(error_trace.clone()),
            ),
        )
        .await;
        assert!(
            matches!(
                result,
                Ok(Err(Error::Application(ref message))) if message == "source failed before its deadline"
            ),
            "{result:?}"
        );
        assert_eq!(kimojio::clock_now(), before);
        while error_trace.report.borrow().is_none() {
            operations::yield_io().await;
        }
        assert_eq!(
            error_trace.report.borrow().as_ref().unwrap().outcome,
            StreamOutcome::Reset(8)
        );
        let mut sibling = client
            .send(request("/sibling", OutgoingBody::empty()))
            .await
            .unwrap();
        sibling.body_mut().collect(0).await.unwrap();
        assert_eq!(
            sibling.body_mut().retirement().await.unwrap().outcome,
            StreamOutcome::Complete
        );
        client.control().graceful();
    };
    let ((), client, server) = futures::join!(app, connection.run(), server);
    client.unwrap();
    server.unwrap();
    assert_eq!(operations::virtual_clock_pending_timers(), 0);
    operations::virtual_clock_enable(false);
}
