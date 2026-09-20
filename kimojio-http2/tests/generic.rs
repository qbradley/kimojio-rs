use std::{
    cell::{Cell, RefCell},
    future::Future,
    rc::Rc,
    time::{Duration, Instant},
};

use futures::FutureExt;
use kimojio::{
    AsyncStreamRead, AsyncStreamWrite, Errno, OwnedFd, OwnedFdStream, SplittableStream, oneshot,
    operations,
};
use kimojio_http2::{
    Config, Error, IncomingBody, IncomingFrame, OutgoingBody, OutgoingFrame, RequestObserver,
    Shutdown, StreamOutcome, StreamReport, connect, connect_native,
    http::{HeaderMap, Method, Request, Response},
    serve_connection, serve_connection_native, serve_connection_with_shutdown,
};

fn request(path: &str, body: OutgoingBody) -> Request<OutgoingBody> {
    Request::builder()
        .method("POST")
        .uri(format!("http://generic.test{path}"))
        .body(body)
        .unwrap()
}

fn streaming(chunks: usize, produced: Rc<Cell<usize>>) -> OutgoingBody {
    let mut trailers = HeaderMap::new();
    trailers.append("x-end", "one".parse().unwrap());
    trailers.append("x-end", "two".parse().unwrap());
    OutgoingBody::from_stream(futures::stream::iter(
        (0..chunks)
            .map(move |_| {
                produced.set(produced.get() + 1);
                Ok(OutgoingFrame::Data(vec![0xa5; 16 * 1024]))
            })
            .chain([Ok(OutgoingFrame::Trailers(trailers))]),
    ))
}

async fn consume(body: &mut IncomingBody, expected: usize) {
    let mut size = 0;
    let mut trailers = 0;
    while let Some(frame) = body.frame().await.unwrap() {
        match frame {
            IncomingFrame::Data(chunk) => {
                assert!(chunk.iter().all(|byte| *byte == 0xa5));
                size += chunk.len();
            }
            IncomingFrame::Trailers(headers) => {
                let values: Vec<_> = headers.get_all("x-end").iter().collect();
                assert_eq!(values, ["one", "two"]);
                trailers += 1;
            }
        }
    }
    assert_eq!(size, expected);
    assert_eq!(trailers, 1);
    assert_eq!(
        body.retirement().await.unwrap().outcome,
        StreamOutcome::Complete
    );
}

async fn bounded(future: impl Future<Output = ()>) {
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(10), future)
        .await
        .unwrap();
}

struct Observer(Rc<RefCell<Vec<u16>>>, Rc<RefCell<Vec<StreamReport>>>);

impl RequestObserver for Observer {
    fn informational(&mut self, response: Response<()>) {
        self.0.borrow_mut().push(response.status().as_u16());
    }
    fn retired(&mut self, report: &StreamReport) {
        self.1.borrow_mut().push(report.clone());
    }
}

#[kimojio::test]
async fn generic_and_native_roles_share_concurrent_duplex_connect_and_observers() {
    for roles in 1..4 {
        let (fd, peer) = kimojio::pipe::bipipe();
        rustix::net::sockopt::set_socket_send_buffer_size(&fd, 4096).unwrap();
        rustix::net::sockopt::set_socket_send_buffer_size(&peer, 4096).unwrap();
        let mut config = Config::default();
        config.protocol.max_stream_receive_capacity = 2 * 1024 * 1024;
        let (client, driver) = if roles & 1 != 0 {
            let (client, connection) = connect(OwnedFdStream::new(fd), config.clone());
            (client, connection.run().boxed_local())
        } else {
            let (client, connection) = connect_native(fd, config.clone());
            (client, connection.run().boxed_local())
        };
        let handler = |request: Request<IncomingBody>| async move {
            if request.method() == Method::CONNECT {
                assert_eq!(
                    request.uri().authority().unwrap().as_str(),
                    "tunnel.test:443"
                );
            }
            request
                .body()
                .informational_sender()
                .unwrap()
                .send(Response::builder().status(103).body(()).unwrap())
                .await?;
            Ok(Response::new(OutgoingBody::from_incoming(
                request.into_body(),
            )))
        };
        let server = if roles & 2 != 0 {
            serve_connection(OwnedFdStream::new(peer), config, handler).boxed_local()
        } else {
            serve_connection_native(peer, config, handler).boxed_local()
        };
        let app = async {
            for _ in 0..2 {
                let send = |tunnel| {
                    let client = &client;
                    async move {
                        let body = streaming(32, Rc::new(Cell::new(0)));
                        let request = if tunnel {
                            Request::builder()
                                .method(Method::CONNECT)
                                .uri("tunnel.test:443")
                                .body(body)
                                .unwrap()
                        } else {
                            request("/echo", body)
                        };
                        let information = Rc::new(RefCell::new(Vec::new()));
                        let reports = Rc::new(RefCell::new(Vec::new()));
                        let mut response = client
                            .send_with_observer(
                                request,
                                Observer(information.clone(), reports.clone()),
                            )
                            .await
                            .unwrap();
                        consume(response.body_mut(), 32 * 16 * 1024).await;
                        assert_eq!(*information.borrow(), [103]);
                        let report = response.body_mut().retirement().await.unwrap();
                        assert_eq!(reports.borrow().as_slice(), &[report]);
                    }
                };
                futures::join!(send(false), send(true));
            }
            client.control().graceful();
        };
        bounded(async {
            let ((), client, server) = futures::join!(app, driver, server);
            client.unwrap();
            server.unwrap();
        })
        .await;
    }
}

#[kimojio::test]
async fn paused_generic_consumer_bounds_production_and_preserves_sibling_progress() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, driver) = connect(OwnedFdStream::new(fd), Config::default());
    let server = serve_connection(
        OwnedFdStream::new(peer),
        Config::default(),
        |request| async {
            if request.uri().path() == "/small" {
                Ok(Response::new(OutgoingBody::from_static(b"small")))
            } else {
                Ok(Response::new(OutgoingBody::from_incoming(
                    request.into_body(),
                )))
            }
        },
    );
    let (paused, wait_paused) = oneshot();
    let (resume, wait_resume) = oneshot();
    let produced = Rc::new(Cell::new(0));
    let slow = async {
        let mut response = client
            .send(request("/echo", streaming(128, produced.clone())))
            .await
            .unwrap();
        let Some(IncomingFrame::Data(chunk)) = response.body_mut().frame().await.unwrap() else {
            panic!("data")
        };
        let first = chunk.len();
        paused.send(()).unwrap();
        wait_resume.recv().await.unwrap();
        assert!(
            produced.get() < 128,
            "paused consumer did not bound upload production"
        );
        drop(chunk);
        consume(response.body_mut(), 128 * 16 * 1024 - first).await;
    };
    let fast = async {
        wait_paused.recv().await.unwrap();
        let mut response = client
            .send(request("/small", OutgoingBody::empty()))
            .await
            .unwrap();
        assert_eq!(response.body_mut().collect(16).await.unwrap(), b"small");
        resume.send(()).unwrap();
    };
    bounded(async {
        let app = async {
            futures::join!(slow, fast);
            client.control().graceful();
        };
        let ((), client, server) = futures::join!(app, driver.run(), server);
        client.unwrap();
        server.unwrap();
    })
    .await;
}

#[derive(Default)]
struct Probe {
    reading: Cell<bool>,
    writing: Cell<bool>,
    read_limit: Cell<usize>,
    read_calls: Cell<usize>,
    read_bytes: Cell<usize>,
    write_calls: Cell<usize>,
    reader_dropped: Cell<bool>,
    closed: Cell<bool>,
    payload_calls: Cell<usize>,
    partial_bytes: Cell<usize>,
    fail_payload: Cell<bool>,
    fail_read: Cell<Option<Errno>>,
    fail_close: Cell<Option<Errno>>,
}

struct Transport(OwnedFd, Rc<Probe>);
struct Reader(Rc<OwnedFd>, Rc<Probe>);
struct Writer(Option<Rc<OwnedFd>>, Rc<Probe>);

impl SplittableStream for Transport {
    type ReadStream = Reader;
    type WriteStream = Writer;

    async fn split(self) -> Result<(Reader, Writer), Errno> {
        let fd = Rc::new(self.0);
        Ok((Reader(fd.clone(), self.1.clone()), Writer(Some(fd), self.1)))
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        assert!(!self.1.reading.get(), "original read did not settle");
        self.1.reader_dropped.set(true);
    }
}

impl AsyncStreamRead for Reader {
    async fn try_read(
        &mut self,
        buffer: &mut [u8],
        deadline: Option<Instant>,
    ) -> Result<usize, Errno> {
        if let Some(error) = self.1.fail_read.take() {
            return Err(error);
        }
        let limit = self.1.read_limit.get();
        let length = if limit == 0 {
            buffer.len()
        } else {
            buffer.len().min(limit)
        };
        self.1.reading.set(true);
        self.1.read_calls.set(self.1.read_calls.get() + 1);
        let result =
            operations::read_with_deadline(self.0.as_ref(), &mut buffer[..length], deadline).await;
        self.1.reading.set(false);
        if let Ok(amount) = result {
            self.1.read_bytes.set(self.1.read_bytes.get() + amount);
        }
        result
    }

    async fn read(
        &mut self,
        mut buffer: &mut [u8],
        deadline: Option<Instant>,
    ) -> Result<(), Errno> {
        while !buffer.is_empty() {
            let n = self.try_read(buffer, deadline).await?;
            if n == 0 {
                return Err(Errno::PIPE);
            }
            buffer = &mut buffer[n..];
        }
        Ok(())
    }
}

impl AsyncStreamWrite for Writer {
    async fn write(&mut self, mut buffer: &[u8], deadline: Option<Instant>) -> Result<(), Errno> {
        self.1.write_calls.set(self.1.write_calls.get() + 1);
        let payload = buffer.len() >= 16 && buffer.iter().all(|byte| *byte == 0xa5);
        if payload {
            self.1.payload_calls.set(self.1.payload_calls.get() + 1);
        }
        self.1.writing.set(true);
        if payload && self.1.fail_payload.get() {
            let result = operations::write_with_deadline(
                self.0.as_ref().unwrap().as_ref(),
                &buffer[..7],
                deadline,
            )
            .await;
            self.1.writing.set(false);
            self.1.partial_bytes.set(result?);
            return Err(Errno::IO);
        }
        let result = async {
            while !buffer.is_empty() {
                let n = operations::write_with_deadline(
                    self.0.as_ref().unwrap().as_ref(),
                    buffer,
                    deadline,
                )
                .await?;
                if n == 0 {
                    return Err(Errno::PIPE);
                }
                buffer = &buffer[n..];
            }
            Ok(())
        }
        .await;
        self.1.writing.set(false);
        result
    }

    async fn shutdown(&mut self) -> Result<(), Errno> {
        rustix::net::shutdown(
            self.0.as_ref().unwrap().as_ref(),
            rustix::net::Shutdown::Write,
        )
    }

    async fn close(&mut self) -> Result<(), Errno> {
        assert!(self.1.reader_dropped.get(), "close preceded reader release");
        assert!(!self.1.reading.get() && !self.1.writing.get());
        let fd = Rc::try_unwrap(self.0.take().unwrap()).expect("original descriptor owner");
        operations::close(fd).await?;
        self.1.closed.set(true);
        self.1.fail_close.get().map_or(Ok(()), Err)
    }
}

#[kimojio::test]
async fn receive_page_exhaustion_is_terminal_not_release_backpressure() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let probe = Rc::new(Probe::default());
    probe.read_limit.set(1024);
    let mut config = Config::default();
    config.protocol.connection_receive_window = 65535;
    config.protocol.max_receive_capacity = 128 * 1024;
    let (client, driver) = connect(Transport(fd, probe.clone()), config);
    let server = serve_connection(
        OwnedFdStream::new(peer),
        Config::default(),
        |_request| async { Ok(Response::new(OutgoingBody::full(vec![0xa5; 65536]))) },
    );
    let app = async {
        let mut response = client
            .send(request("/capacity", OutgoingBody::empty()))
            .await
            .unwrap();
        let mut held = Vec::new();
        loop {
            match response.body_mut().frame().await {
                Ok(Some(IncomingFrame::Data(chunk))) => held.push(chunk),
                Err(Error::Stream(StreamOutcome::ConnectionFailed)) => break,
                other => panic!("expected terminal resource failure: {other:?}"),
            }
        }
        assert!(!held.is_empty());
        while !probe.closed.get() {
            operations::yield_io().await;
        }
        assert!(response.body_mut().retirement().now_or_never().is_none());
        for chunk in &held {
            assert!(chunk.iter().all(|byte| *byte == 0xa5));
        }
        drop(held);
        assert_eq!(
            response.body_mut().retirement().await.unwrap().outcome,
            StreamOutcome::ConnectionFailed
        );
    };
    bounded(async {
        let ((), client, server) = futures::join!(app, driver.run(), server);
        assert_eq!(
            client,
            Err(Error::Connection(
                kimojio_http2::ConnectionResult::ResourceExhausted
            ))
        );
        assert!(server.is_err());
    })
    .await;
    assert!(probe.closed.get());
}

#[kimojio::test]
async fn lease_release_alone_wakes_blocked_admission_without_new_read_completion() {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct TransportState {
        read_bytes: usize,
        read_calls: usize,
        write_calls: usize,
        reading: bool,
    }
    impl TransportState {
        fn capture(probe: &Probe) -> Self {
            Self {
                read_bytes: probe.read_bytes.get(),
                read_calls: probe.read_calls.get(),
                write_calls: probe.write_calls.get(),
                reading: probe.reading.get(),
            }
        }
    }
    struct Admission {
        probe: Rc<Probe>,
        at_admission: Rc<Cell<Option<TransportState>>>,
    }
    impl RequestObserver for Admission {
        fn admitted(&mut self, _: kimojio_http2::StreamId) {
            self.at_admission
                .set(Some(TransportState::capture(&self.probe)));
        }
    }
    let (fd, peer) = kimojio::pipe::bipipe();
    let probe = Rc::new(Probe::default());
    let mut config = Config::default();
    config.protocol.http = config.protocol.http.set_max_active_streams(1);
    let (client, driver) = connect(Transport(fd, probe.clone()), config);
    let paths = Rc::new(RefCell::new(Vec::new()));
    let server = serve_connection(OwnedFdStream::new(peer), Config::default(), {
        let paths = paths.clone();
        move |request| {
            paths.borrow_mut().push(request.uri().path().to_owned());
            async move { Ok(Response::new(OutgoingBody::from_static(b"retained"))) }
        }
    });
    let admitted = Rc::new(Cell::new(None));
    let app = async {
        let mut first = client
            .send(request("/held", OutgoingBody::empty()))
            .await
            .unwrap();
        let Some(IncomingFrame::Data(held)) = first.body_mut().frame().await.unwrap() else {
            panic!("held DATA")
        };
        assert!(first.body_mut().frame().await.unwrap().is_none());
        assert!(first.body_mut().retirement().now_or_never().is_none());
        let mut next = Box::pin(client.send_with_observer(
            request("/blocked", OutgoingBody::empty()),
            Admission {
                probe: probe.clone(),
                at_admission: admitted.clone(),
            },
        ));
        assert!(next.as_mut().now_or_never().is_none());
        for _ in 0..8 {
            operations::yield_io().await;
        }
        assert_eq!(*paths.borrow(), ["/held"]);
        assert_eq!(admitted.get(), None);
        assert!(
            probe.reading.get(),
            "idle original read is the only transport wait"
        );
        assert!(!probe.writing.get());
        let before = TransportState::capture(&probe);
        // The second command is already blocked. Only this release wakes it.
        drop(held);
        let mut next = next.await.unwrap();
        assert_eq!(
            admitted.get(),
            Some(before),
            "admission needed new transport progress"
        );
        assert_eq!(next.body_mut().collect(16).await.unwrap(), b"retained");
        assert_eq!(
            first.body_mut().retirement().await.unwrap().outcome,
            StreamOutcome::Complete
        );
        assert_eq!(
            next.body_mut().retirement().await.unwrap().outcome,
            StreamOutcome::Complete
        );
        assert_eq!(*paths.borrow(), ["/held", "/blocked"]);
        client.control().graceful();
    };
    bounded(async {
        let ((), client, server) = futures::join!(app, driver.run(), server);
        client.unwrap();
        server.unwrap();
    })
    .await;
    assert!(probe.closed.get());
}

#[kimojio::test]
async fn partial_write_failure_keeps_lower_bound_receipt_errno_and_close_error() {
    for close_fails in [false, true] {
        let (fd, peer) = kimojio::pipe::bipipe();
        let probe = Rc::new(Probe::default());
        probe.fail_payload.set(true);
        probe.fail_close.set(close_fails.then_some(Errno::BUSY));
        let (client, driver) = connect(Transport(fd, probe.clone()), Config::default());
        let server = serve_connection(
            OwnedFdStream::new(peer),
            Config::default(),
            |request| async move {
                // Preserve valid receive END even though the later upload fails.
                drop(request);
                Ok(Response::new(OutgoingBody::empty()))
            },
        );
        let (start, wait) = oneshot();
        let app = async {
            let source = futures::stream::once(async {
                wait.recv().await.unwrap();
                Ok(OutgoingFrame::Data(vec![0xa5; 16 * 1024]))
            });
            let mut response = client
                .send(request("/early", OutgoingBody::from_stream(source)))
                .await
                .unwrap();
            assert!(response.body_mut().frame().await.unwrap().is_none());
            start.send(()).unwrap();
            let report = response.body_mut().retirement().await.unwrap();
            assert_eq!(report.receive_outcome, Some(StreamOutcome::Complete));
            let receipt = report.send_failure.expect("original failed buffer receipt");
            assert!(!receipt.exact, "write-all error cannot claim exact zero");
            assert!(receipt.accepted <= probe.partial_bytes.get());
            assert!(probe.partial_bytes.get() > 0);
            assert_ne!(report.outcome, StreamOutcome::Complete);
        };
        bounded(async {
            let ((), result, server) = futures::join!(app, driver.run(), server);
            assert_eq!(
                std::error::Error::source(result.as_ref().unwrap_err())
                    .unwrap()
                    .downcast_ref::<Errno>(),
                Some(&Errno::IO),
            );
            assert_eq!(
                result,
                Err(if close_fails {
                    Error::TransportAndClose {
                        transport: Errno::IO,
                        close: Errno::BUSY,
                    }
                } else {
                    Error::Transport(Errno::IO)
                })
            );
            assert!(server.is_err());
        })
        .await;
        assert!(probe.closed.get());
        assert_eq!(probe.payload_calls.get(), 1, "failed buffer replayed");
    }
}

#[kimojio::test]
async fn read_failure_preserves_source_errno_and_closes_original_halves() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let probe = Rc::new(Probe::default());
    probe.fail_read.set(Some(Errno::NOENT));
    let (_client, driver) = connect(Transport(fd, probe.clone()), Config::default());
    bounded(async {
        assert_eq!(driver.run().await, Err(Error::Transport(Errno::NOENT)));
    })
    .await;
    assert!(probe.closed.get());
    operations::close(peer).await.unwrap();
}

#[kimojio::test]
async fn invalid_configuration_still_closes_generic_halves_in_both_roles() {
    for server in [false, true] {
        let (fd, peer) = kimojio::pipe::bipipe();
        let probe = Rc::new(Probe::default());
        let config = Config {
            max_streams: 0,
            ..Config::default()
        };
        let result = if server {
            serve_connection(Transport(fd, probe.clone()), config, |_request| async {
                panic!("invalid configuration admitted a request")
            })
            .await
        } else {
            let (_client, driver) = connect(Transport(fd, probe.clone()), config);
            driver.run().await
        };
        assert_eq!(result, Err(Error::Limit));
        assert!(probe.reader_dropped.get() && probe.closed.get());
        operations::close(peer).await.unwrap();
    }
}

#[kimojio::test]
async fn graceful_close_failure_is_not_success_in_either_role() {
    for server_failure in [false, true] {
        let (fd, peer) = kimojio::pipe::bipipe();
        let client_probe = Rc::new(Probe::default());
        let server_probe = Rc::new(Probe::default());
        if server_failure {
            server_probe.fail_close.set(Some(Errno::BUSY));
        } else {
            client_probe.fail_close.set(Some(Errno::BUSY));
        }
        let (client, driver) = connect(Transport(fd, client_probe.clone()), Config::default());
        let server = serve_connection(
            Transport(peer, server_probe.clone()),
            Config::default(),
            |_request| async { Ok(Response::new(OutgoingBody::empty())) },
        );
        let app = async {
            let mut response = client
                .send(request("/empty", OutgoingBody::empty()))
                .await
                .unwrap();
            assert!(response.body_mut().frame().await.unwrap().is_none());
            assert_eq!(
                response.body_mut().retirement().await.unwrap().outcome,
                StreamOutcome::Complete
            );
            client.control().graceful();
        };
        bounded(async {
            let ((), client, server) = futures::join!(app, driver.run(), server);
            assert_eq!(
                if server_failure { server } else { client },
                Err(Error::Transport(Errno::BUSY))
            );
        })
        .await;
        assert!(client_probe.closed.get() && server_probe.closed.get());
    }
}

#[kimojio::test]
async fn generic_close_precedes_held_chunk_release_and_retirement() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let probe = Rc::new(Probe::default());
    let (client, driver) = connect(Transport(fd, probe.clone()), Config::default());
    let shutdown = Shutdown::default();
    let server = serve_connection_with_shutdown(
        OwnedFdStream::new(peer),
        Config::default(),
        shutdown.clone(),
        |_request| async { Ok(Response::new(OutgoingBody::from_static(b"held"))) },
    );
    let finished = Cell::new(false);
    let run = async {
        let result = driver.run().await;
        finished.set(true);
        result
    };
    let app = async {
        let mut response = client
            .send(request("/held", OutgoingBody::empty()))
            .await
            .unwrap();
        let Some(IncomingFrame::Data(chunk)) = response.body_mut().frame().await.unwrap() else {
            panic!("chunk")
        };
        assert!(response.body_mut().frame().await.unwrap().is_none());
        client.control().abort();
        while !probe.closed.get() {
            operations::yield_cpu().await;
        }
        assert_eq!(&*chunk, b"held");
        assert!(!finished.get());
        assert!(response.body_mut().retirement().now_or_never().is_none());
        drop(chunk);
        let report = response.body_mut().retirement().await.unwrap();
        assert_eq!(report.receive_outcome, Some(StreamOutcome::Complete));
        shutdown.abort();
    };
    bounded(async {
        let ((), client, _server) = futures::join!(app, run, server);
        assert!(client.is_err());
    })
    .await;
    assert!(finished.get());
}

#[kimojio::test]
async fn server_partial_write_failure_preserves_its_original_receipt_and_source() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let probe = Rc::new(Probe::default());
    probe.fail_payload.set(true);
    let (client, driver) = connect(OwnedFdStream::new(fd), Config::default());
    let (send_body, receive_body) = oneshot();
    let mut send_body = Some(send_body);
    let server = serve_connection(
        Transport(peer, probe.clone()),
        Config::default(),
        move |request| {
            let send = send_body.take().unwrap();
            async move {
                send.send(request.into_body()).unwrap();
                Ok(Response::new(OutgoingBody::full(vec![0xa5; 16 * 1024])))
            }
        },
    );
    let app = async {
        let mut response = client
            .send(request("/fail", OutgoingBody::empty()))
            .await
            .unwrap();
        assert!(response.body_mut().collect(16384).await.is_err());
        let mut request = receive_body.recv().await.unwrap();
        assert!(request.frame().await.unwrap().is_none());
        let report = request.retirement().await.unwrap();
        assert_eq!(report.receive_outcome, Some(StreamOutcome::Complete));
        let receipt = report.send_failure.unwrap();
        assert!(!receipt.exact);
        assert!(receipt.accepted <= probe.partial_bytes.get());
        assert!(probe.partial_bytes.get() > 0);
        assert_ne!(report.outcome, StreamOutcome::Complete);
    };
    bounded(async {
        let ((), client, server) = futures::join!(app, driver.run(), server);
        assert!(client.is_err());
        assert_eq!(server, Err(Error::Transport(Errno::IO)));
    })
    .await;
    assert_eq!(probe.payload_calls.get(), 1);
    assert!(probe.closed.get());
}

#[kimojio::test]
async fn server_close_precedes_held_request_release_and_retirement() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let probe = Rc::new(Probe::default());
    let (client, driver) = connect(OwnedFdStream::new(fd), Config::default());
    let shutdown = Shutdown::default();
    let (send_body, receive_body) = oneshot();
    let mut send_body = Some(send_body);
    let server = serve_connection_with_shutdown(
        Transport(peer, probe.clone()),
        Config::default(),
        shutdown.clone(),
        move |request| {
            let send = send_body.take().unwrap();
            async move {
                send.send(request.into_body()).unwrap();
                Ok(Response::new(OutgoingBody::empty()))
            }
        },
    );
    let finished = Cell::new(false);
    let run = async {
        let result = server.await;
        finished.set(true);
        result
    };
    let app = async {
        let mut response = client
            .send(request("/held", OutgoingBody::from_static(b"held")))
            .await
            .unwrap();
        assert!(response.body_mut().frame().await.unwrap().is_none());
        let mut request = receive_body.recv().await.unwrap();
        let Some(IncomingFrame::Data(chunk)) = request.frame().await.unwrap() else {
            panic!("chunk")
        };
        assert!(request.frame().await.unwrap().is_none());
        shutdown.abort();
        while !probe.closed.get() {
            operations::yield_cpu().await;
        }
        assert_eq!(&*chunk, b"held");
        assert!(!finished.get());
        assert!(request.retirement().now_or_never().is_none());
        drop(chunk);
        assert_eq!(
            request.retirement().await.unwrap().receive_outcome,
            Some(StreamOutcome::Complete)
        );
    };
    bounded(async {
        let ((), _client, server) = futures::join!(app, driver.run(), run);
        assert!(server.is_err());
    })
    .await;
    assert!(finished.get());
}

struct Dropped(Rc<Cell<bool>>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.set(true);
    }
}

#[kimojio::test]
async fn stream_cancellation_releases_handler_and_source_io_without_stopping_siblings() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, driver) = connect(OwnedFdStream::new(fd), Config::default());
    let handler_started = Rc::new(Cell::new(false));
    let source_started = Rc::new(Cell::new(false));
    let handler_dropped = Rc::new(Cell::new(false));
    let source_dropped = Rc::new(Cell::new(false));
    let (handler_fd, handler_peer) = kimojio::pipe::bipipe();
    let handler_fd = Rc::new(handler_fd);
    let server = serve_connection(OwnedFdStream::new(peer), Config::default(), {
        let started = handler_started.clone();
        let dropped = handler_dropped.clone();
        move |request| {
            let fd = handler_fd.clone();
            let started = started.clone();
            let dropped = dropped.clone();
            async move {
                if request.uri().path() == "/pending" {
                    let _guard = Dropped(dropped);
                    started.set(true);
                    let mut byte = [0];
                    operations::read(fd.as_ref(), &mut byte)
                        .await
                        .map_err(Error::Transport)?;
                    panic!("handler I/O unexpectedly succeeded");
                }
                Ok(Response::new(OutgoingBody::empty()))
            }
        }
    });
    let (source_fd, source_peer) = kimojio::pipe::bipipe();
    let source = {
        let started = source_started.clone();
        let dropped = source_dropped.clone();
        futures::stream::once(async move {
            let _guard = Dropped(dropped);
            started.set(true);
            let mut byte = [0];
            operations::read(&source_fd, &mut byte)
                .await
                .map_err(Error::Transport)?;
            Ok(OutgoingFrame::Data(vec![0xa5]))
        })
    };
    let app = async {
        let mut pending =
            Box::pin(client.send(request("/pending", OutgoingBody::from_stream(source))));
        assert!(pending.as_mut().now_or_never().is_none());
        while !handler_started.get() || !source_started.get() {
            operations::yield_io().await;
        }
        drop(pending);
        while !handler_dropped.get() || !source_dropped.get() {
            operations::yield_io().await;
        }
        let mut response = client
            .send(request("/sibling", OutgoingBody::empty()))
            .await
            .unwrap();
        assert!(response.body_mut().frame().await.unwrap().is_none());
        assert_eq!(
            response.body_mut().retirement().await.unwrap().outcome,
            StreamOutcome::Complete
        );
        client.control().graceful();
    };
    bounded(async {
        let ((), client, server) = futures::join!(app, driver.run(), server);
        client.unwrap();
        server.unwrap();
    })
    .await;
    operations::close(handler_peer).await.unwrap();
    operations::close(source_peer).await.unwrap();
}

#[kimojio::test]
async fn unread_requests_do_not_cancel_generic_early_responses_or_uploads() {
    for response_shape in 0..3 {
        let (fd, peer) = kimojio::pipe::bipipe();
        let (client, driver) = connect(OwnedFdStream::new(fd), Config::default());
        let server = serve_connection(
            OwnedFdStream::new(peer),
            Config::default(),
            move |request| async move {
                drop(request);
                let body = match response_shape {
                    0 => OutgoingBody::empty(),
                    1 => OutgoingBody::full(vec![0xa5; 4096]),
                    _ => streaming(2, Rc::new(Cell::new(0))),
                };
                Ok(Response::new(body))
            },
        );
        let produced = Rc::new(Cell::new(0));
        let app = async {
            let mut response = client
                .send(request("/early", streaming(64, produced.clone())))
                .await
                .unwrap();
            if response_shape == 2 {
                consume(response.body_mut(), 32768).await;
            } else {
                let bytes = response.body_mut().collect(4096).await.unwrap();
                assert_eq!(bytes.len(), if response_shape == 0 { 0 } else { 4096 });
            }
            assert_eq!(
                response.body_mut().retirement().await.unwrap().outcome,
                StreamOutcome::Complete
            );
            assert_eq!(produced.get(), 64);
            client.control().graceful();
        };
        bounded(async {
            let ((), client, server) = futures::join!(app, driver.run(), server);
            client.unwrap();
            server.unwrap();
        })
        .await;
    }
}

#[cfg(feature = "virtual-clock")]
#[kimojio::test]
async fn generic_graceful_deadline_uses_virtual_time_and_closes_after_handler_settlement() {
    operations::virtual_clock_enable(true);
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, driver) = connect(OwnedFdStream::new(fd), Config::default());
    let probe = Rc::new(Probe::default());
    let mut config = Config::default();
    config.protocol.shutdown_timeout = Duration::from_secs(1);
    let shutdown = Shutdown::default();
    let entered = Rc::new(Cell::new(false));
    let dropped = Rc::new(Cell::new(false));
    let server =
        serve_connection_with_shutdown(Transport(peer, probe.clone()), config, shutdown.clone(), {
            let entered = entered.clone();
            let dropped = dropped.clone();
            move |_request| {
                let entered = entered.clone();
                let dropped = dropped.clone();
                async move {
                    let _guard = Dropped(dropped);
                    entered.set(true);
                    std::future::pending::<Result<Response<OutgoingBody>, Error>>().await
                }
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
    let ((), client, server) = futures::join!(app, driver.run(), server);
    assert!(client.is_err());
    server.unwrap();
    assert!(dropped.get() && probe.closed.get());
    operations::virtual_clock_enable(false);
}
