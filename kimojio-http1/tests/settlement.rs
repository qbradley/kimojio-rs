use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};

use futures::{FutureExt, StreamExt};
use kimojio::{
    AsyncStreamRead, AsyncStreamWrite, Errno, OwnedFdStream, OwnedFdStreamRead, OwnedFdStreamWrite,
    SplittableStream, operations,
};
use kimojio_http1::{
    Config, ConnectionId, Error, IncomingBody, OutgoingBody, OutgoingFrame, Shutdown, connect,
    http::{Request, Response},
    serve_connection_with_shutdown,
};

#[derive(Default)]
struct Counts {
    reads: Cell<usize>,
    writes: Cell<usize>,
    pending_writes: Cell<usize>,
    closes: Cell<usize>,
    reader_address: Cell<usize>,
    writer_address: Cell<usize>,
    reader_drops: Cell<usize>,
    writer_drops: Cell<usize>,
}

fn record_address(previous: &Cell<usize>, address: usize) {
    let old = previous.replace(address);
    if old != 0 {
        assert_eq!(old, address, "worker moved its transport half");
    }
}

struct ProducerDropped(Rc<Cell<bool>>);

impl Drop for ProducerDropped {
    fn drop(&mut self) {
        self.0.set(true);
    }
}

struct Stream {
    stream: OwnedFdStream,
    counts: Rc<Counts>,
    fail_write: bool,
}

struct Reader {
    stream: OwnedFdStreamRead,
    counts: Rc<Counts>,
}

struct Writer {
    stream: OwnedFdStreamWrite,
    counts: Rc<Counts>,
    fail_write: bool,
}

impl Drop for Reader {
    fn drop(&mut self) {
        record_address(
            &self.counts.reader_address,
            std::ptr::from_ref(self) as usize,
        );
        self.counts
            .reader_drops
            .set(self.counts.reader_drops.get() + 1);
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        record_address(
            &self.counts.writer_address,
            std::ptr::from_ref(self) as usize,
        );
        self.counts
            .writer_drops
            .set(self.counts.writer_drops.get() + 1);
    }
}

impl SplittableStream for Stream {
    type ReadStream = Reader;
    type WriteStream = Writer;

    async fn split(self) -> Result<(Reader, Writer), Errno> {
        let (reader, writer) = self.stream.split().await?;
        Ok((
            Reader {
                stream: reader,
                counts: self.counts.clone(),
            },
            Writer {
                stream: writer,
                counts: self.counts,
                fail_write: self.fail_write,
            },
        ))
    }
}

struct Reading(Rc<Counts>);
impl Drop for Reading {
    fn drop(&mut self) {
        self.0.reads.set(self.0.reads.get() - 1);
    }
}

struct Writing(Rc<Counts>);
impl Drop for Writing {
    fn drop(&mut self) {
        self.0.pending_writes.set(self.0.pending_writes.get() - 1);
    }
}

impl AsyncStreamRead for Reader {
    async fn try_read(
        &mut self,
        buffer: &mut [u8],
        deadline: Option<Instant>,
    ) -> Result<usize, Errno> {
        record_address(
            &self.counts.reader_address,
            std::ptr::from_ref(self) as usize,
        );
        self.counts.reads.set(self.counts.reads.get() + 1);
        let _reading = Reading(self.counts.clone());
        self.stream.try_read(buffer, deadline).await
    }

    async fn read(&mut self, buffer: &mut [u8], deadline: Option<Instant>) -> Result<(), Errno> {
        self.stream.read(buffer, deadline).await
    }
}

impl AsyncStreamWrite for Writer {
    async fn write(&mut self, buffer: &[u8], deadline: Option<Instant>) -> Result<(), Errno> {
        record_address(
            &self.counts.writer_address,
            std::ptr::from_ref(self) as usize,
        );
        self.counts.writes.set(self.counts.writes.get() + 1);
        self.counts
            .pending_writes
            .set(self.counts.pending_writes.get() + 1);
        let _writing = Writing(self.counts.clone());
        if self.fail_write {
            self.stream
                .write(&buffer[..buffer.len().min(13)], deadline)
                .await?;
            Err(Errno::INTR)
        } else {
            self.stream.write(buffer, deadline).await
        }
    }

    async fn shutdown(&mut self) -> Result<(), Errno> {
        self.stream.shutdown().await
    }

    async fn close(&mut self) -> Result<(), Errno> {
        record_address(
            &self.counts.writer_address,
            std::ptr::from_ref(self) as usize,
        );
        assert_eq!(self.counts.reads.get(), 0, "close preceded read settlement");
        assert_eq!(
            self.counts.pending_writes.get(),
            0,
            "close preceded write settlement"
        );
        self.counts.closes.set(self.counts.closes.get() + 1);
        self.stream.close().await
    }
}

fn pair(fail_write: bool) -> (Stream, OwnedFdStream, Rc<Counts>) {
    let (a, b) = rustix::net::socketpair(
        rustix::net::AddressFamily::UNIX,
        rustix::net::SocketType::STREAM,
        rustix::net::SocketFlags::CLOEXEC,
        None,
    )
    .unwrap();
    rustix::net::sockopt::set_socket_send_buffer_size(&a, 4096).unwrap();
    let counts = Rc::new(Counts::default());
    (
        Stream {
            stream: OwnedFdStream::new(a),
            counts: counts.clone(),
            fail_write,
        },
        OwnedFdStream::new(b),
        counts,
    )
}

#[kimojio::test]
async fn workers_retain_transport_halves_through_streaming_reuse_and_drop_once() {
    let (stream, peer, counts) = pair(false);
    let (mut client, driver) = connect(stream, config(90));
    let server = serve_connection_with_shutdown(
        peer,
        config(91),
        Shutdown::default(),
        |mut request: Request<IncomingBody>| async move {
            assert_eq!(request.body_mut().collect(3).await?, b"abc");
            Ok(Response::new(OutgoingBody::full(b"done".to_vec())))
        },
    );
    let app = async {
        for _ in 0..3 {
            let body = OutgoingBody::from_stream(
                Some(3),
                futures::stream::iter([Ok(OutgoingFrame::Data(b"abc".to_vec()))]),
            );
            let mut response = client
                .send(
                    Request::builder()
                        .method("POST")
                        .uri("/")
                        .header("host", "test")
                        .body(body)
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.body_mut().collect(4).await.unwrap(), b"done");
        }
        client.shutdown().await.unwrap();
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
        let ((), driver, server) = futures::join!(app, driver.run(), server);
        driver.unwrap();
        server.unwrap();
    })
    .await
    .unwrap();
    assert!(counts.writes.get() >= 3);
    assert_eq!(counts.closes.get(), 1);
    assert_eq!(counts.reader_drops.get(), 1);
    assert_eq!(counts.writer_drops.get(), 1);
}

#[kimojio::test]
async fn response_head_arrives_while_native_write_is_blocked() {
    let (stream, mut peer, counts) = pair(false);
    let (mut client, driver) = connect(stream, config(8));
    let body = OutgoingBody::from_stream(
        None,
        futures::stream::repeat_with(|| Ok(OutgoingFrame::Data(vec![b'x'; 64 * 1024]))),
    );
    let app = async {
        let response = client
            .send(
                Request::builder()
                    .method("POST")
                    .uri("/")
                    .header("host", "test")
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 413);
        drop(response);
        let _ = client.shutdown().await;
    };
    let raw = async {
        let mut buffer = [0; 1024];
        let mut head = Vec::new();
        while !head.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let n = peer.try_read(&mut buffer, None).await.unwrap();
            assert_ne!(n, 0);
            head.extend_from_slice(&buffer[..n]);
        }
        operations::sleep(Duration::from_millis(10)).await.unwrap();
        assert_eq!(
            counts.pending_writes.get(),
            1,
            "upload did not reach a blocked native write"
        );
        peer.write(
            b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            None,
        )
        .await
        .unwrap();
        operations::sleep(Duration::from_millis(10)).await.unwrap();
        peer.close().await.unwrap();
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
        let ((), _, ()) = futures::join!(app, driver.run(), raw);
    })
    .await
    .unwrap();
    assert_eq!(counts.closes.get(), 1);
}

#[kimojio::test]
async fn continuously_ready_empty_source_does_not_starve_response() {
    let (stream, mut peer, counts) = pair(false);
    let (mut client, driver) = connect(stream, config(9));
    let polls = Rc::new(Cell::new(0));
    let source_polls = polls.clone();
    let body = OutgoingBody::from_stream(
        None,
        futures::stream::repeat_with(move || {
            source_polls.set(source_polls.get() + 1);
            Ok(OutgoingFrame::Data(Vec::new()))
        }),
    );
    let app = async {
        let response = client
            .send(
                Request::builder()
                    .method("POST")
                    .uri("/")
                    .header("host", "test")
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 413);
        drop(response);
        client.shutdown().await.unwrap();
    };
    let raw = async {
        let mut buffer = [0; 1024];
        let mut head = Vec::new();
        while !head.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let n = peer.try_read(&mut buffer, None).await.unwrap();
            assert_ne!(n, 0);
            head.extend_from_slice(&buffer[..n]);
        }
        operations::yield_io().await;
        peer.write(
            b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
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
    assert!(polls.get() > 0);
    assert_eq!(counts.closes.get(), 1);
}

#[kimojio::test]
async fn cancelled_upload_does_not_discard_the_final_response_body() {
    let (stream, mut peer, counts) = pair(false);
    let (mut client, driver) = connect(stream, config(12));
    let body = OutgoingBody::from_stream(
        None,
        futures::stream::repeat_with(|| Ok(OutgoingFrame::Data(vec![b'x'; 64 * 1024]))),
    );
    let (finished, closed) = kimojio::oneshot();
    let app = async {
        let mut response = client
            .send(
                Request::builder()
                    .method("POST")
                    .uri("/")
                    .header("host", "test")
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 413);
        let bytes = response.body_mut().collect(256 * 1024).await.unwrap();
        assert_eq!(bytes, vec![b'y'; 128 * 1024]);
        client.shutdown().await.unwrap();
        finished.send(()).unwrap();
    };
    let raw = async {
        let mut buffer = [0; 1024];
        let mut head = Vec::new();
        while !head.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let n = peer.try_read(&mut buffer, None).await.unwrap();
            assert_ne!(n, 0);
            head.extend_from_slice(&buffer[..n]);
        }
        operations::sleep(Duration::from_millis(10)).await.unwrap();
        assert_eq!(counts.pending_writes.get(), 1);
        peer.write(b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 131072\r\nConnection: close\r\n\r\n", None).await.unwrap();
        let body = vec![b'y'; 128 * 1024];
        peer.write(&body, None).await.unwrap();
        closed.recv().await.unwrap();
        peer.close().await.unwrap();
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
        let ((), driver, ()) = futures::join!(app, driver.run(), raw);
        driver.unwrap();
    })
    .await
    .unwrap();
    assert_eq!(counts.closes.get(), 1);
}

#[kimojio::test]
async fn producer_release_does_not_release_an_in_flight_payload() {
    let (stream, mut peer, counts) = pair(false);
    let (mut client, driver) = connect(stream, config(14));
    let released = Rc::new(Cell::new(false));
    let guard = ProducerDropped(released.clone());
    let source = futures::stream::iter([Ok(OutgoingFrame::Data(vec![b'x'; 64 * 1024]))]).inspect(
        move |_| {
            let _ = &guard;
        },
    );
    let body = OutgoingBody::from_stream(Some(64 * 1024), source);
    let app = async {
        let mut response = client
            .send(
                Request::builder()
                    .method("POST")
                    .uri("/")
                    .header("host", "test")
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert!(response.body_mut().frame().await.unwrap().is_none());
        client.shutdown().await.unwrap();
    };
    let raw = async {
        let mut buffer = [0; 1024];
        let mut head = Vec::new();
        let end = loop {
            if let Some(end) = head.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                break end + 4;
            }
            let n = peer.try_read(&mut buffer, None).await.unwrap();
            assert_ne!(n, 0);
            head.extend_from_slice(&buffer[..n]);
        };
        operations::sleep(Duration::from_millis(10)).await.unwrap();
        assert!(released.get(), "producer outlived the core's final demand");
        assert_eq!(counts.pending_writes.get(), 1);
        let mut payload = head[end..].to_vec();
        while payload.len() < 64 * 1024 {
            let n = peer.try_read(&mut buffer, None).await.unwrap();
            assert_ne!(n, 0);
            payload.extend_from_slice(&buffer[..n]);
        }
        assert_eq!(payload, vec![b'x'; 64 * 1024]);
        peer.write(
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            None,
        )
        .await
        .unwrap();
        peer.close().await.unwrap();
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
        let ((), driver, ()) = futures::join!(app, driver.run(), raw);
        driver.unwrap();
    })
    .await
    .unwrap();
    assert_eq!(counts.closes.get(), 1);
}

fn config(slot: u64) -> Config {
    Config::new(ConnectionId {
        slot,
        generation: 1,
    })
}

#[kimojio::test]
async fn write_all_failure_with_partial_progress_is_never_retried() {
    let (stream, mut peer, counts) = pair(true);
    let (mut client, driver) = connect(stream, config(1));
    let app = async {
        assert!(
            client
                .send(
                    Request::builder()
                        .uri("/")
                        .header("host", "test")
                        .body(OutgoingBody::empty())
                        .unwrap()
                )
                .await
                .is_err()
        );
    };
    let raw = async {
        let mut received = Vec::new();
        let mut buffer = [0; 256];
        loop {
            let count = peer.try_read(&mut buffer, None).await.unwrap();
            if count == 0 {
                break;
            }
            received.extend_from_slice(&buffer[..count]);
        }
        assert_eq!(received.len(), 13);
        peer.close().await.unwrap();
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
        let ((), result, ()) = futures::join!(app, driver.run(), raw);
        assert!(matches!(
            result,
            Err(Error::Protocol(kimojio_fsm_http1::Failure::Transport(
                kimojio_fsm_http1::IoError {
                    kind: kimojio_fsm_http1::IoErrorKind::UnknownProgress,
                    ..
                }
            )))
        ));
    })
    .await
    .unwrap();
    assert_eq!(counts.writes.get(), 1);
    assert_eq!(counts.closes.get(), 1);
}

#[kimojio::test]
async fn abort_settles_pending_read_and_closes_once() {
    let (stream, mut peer, counts) = pair(false);
    let control = Shutdown::default();
    let server = serve_connection_with_shutdown(
        stream,
        config(2),
        control.clone(),
        |_: Request<IncomingBody>| std::future::pending::<Result<Response<OutgoingBody>, Error>>(),
    );
    let raw = async {
        peer.write(b"GET / HTTP/1.1\r\nHost: test\r\n\r\n", None)
            .await
            .unwrap();
        operations::sleep(Duration::from_millis(5)).await.unwrap();
        control.abort();
        let mut buffer = [0; 1];
        assert_eq!(peer.try_read(&mut buffer, None).await.unwrap(), 0);
        peer.close().await.unwrap();
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
        let (result, ()) = futures::join!(server, raw);
        assert!(result.is_err());
    })
    .await
    .unwrap();
    assert_eq!(counts.closes.get(), 1);
}

#[kimojio::test]
async fn dropped_admitted_send_settles_transport_operations() {
    let (stream, mut peer, counts) = pair(false);
    let (mut client, driver) = connect(stream, config(10));
    let (head_seen, seen) = kimojio::oneshot();
    let app = async {
        {
            let send = client
                .send(
                    Request::builder()
                        .uri("/")
                        .header("host", "test")
                        .body(OutgoingBody::empty())
                        .unwrap(),
                )
                .fuse();
            let seen = seen.recv().fuse();
            futures::pin_mut!(send, seen);
            futures::select_biased! {
                _ = seen => {}
                response = send => panic!("unexpected response: {response:?}"),
            }
        }
        assert!(client.shutdown().await.is_err());
    };
    let raw = async {
        let mut head = Vec::new();
        let mut buffer = [0; 1024];
        while !head.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let n = peer.try_read(&mut buffer, None).await.unwrap();
            assert_ne!(n, 0);
            head.extend_from_slice(&buffer[..n]);
        }
        head_seen.send(()).unwrap();
        assert_eq!(peer.try_read(&mut buffer, None).await.unwrap(), 0);
        peer.close().await.unwrap();
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
        let ((), result, ()) = futures::join!(app, driver.run(), raw);
        assert!(result.is_err());
    })
    .await
    .unwrap();
    assert_eq!(counts.closes.get(), 1);
}

#[kimojio::test]
async fn dropped_response_returns_queued_body_lease() {
    let (stream, mut peer, counts) = pair(false);
    let (mut client, driver) = connect(stream, config(11));
    let (continue_body, body_allowed) = kimojio::oneshot();
    let app = async {
        let mut response = client
            .send(
                Request::builder()
                    .uri("/")
                    .header("host", "test")
                    .body(OutgoingBody::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let first = response.body_mut().frame().await.unwrap().unwrap();
        drop(first);
        continue_body.send(()).unwrap();
        operations::sleep(Duration::from_millis(5)).await.unwrap();
        drop(response);
        assert!(client.shutdown().await.is_err());
    };
    let raw = async {
        let mut head = Vec::new();
        let mut buffer = [0; 1024];
        while !head.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let n = peer.try_read(&mut buffer, None).await.unwrap();
            assert_ne!(n, 0);
            head.extend_from_slice(&buffer[..n]);
        }
        peer.write(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n", None)
            .await
            .unwrap();
        peer.write(&[b'x'; 100], None).await.unwrap();
        body_allowed.recv().await.unwrap();
        peer.write(&[b'y'; 100], None).await.unwrap();
        assert_eq!(peer.try_read(&mut buffer, None).await.unwrap(), 0);
        peer.close().await.unwrap();
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
        let ((), result, ()) = futures::join!(app, driver.run(), raw);
        assert!(result.is_err());
    })
    .await
    .unwrap();
    assert_eq!(counts.closes.get(), 1);
}

#[cfg(feature = "virtual-clock")]
#[kimojio::test]
async fn protocol_deadline_uses_virtual_clock_domain() {
    operations::virtual_clock_enable(true);
    let epoch = kimojio::clock_now();
    let (stream, mut peer, counts) = pair(false);
    let mut config = config(3);
    config.protocol.head_timeout_ns = Some(1_000_000_000);
    config.protocol.idle_timeout_ns = Some(1_000_000_000);
    let server = serve_connection_with_shutdown(
        stream,
        config,
        Shutdown::default(),
        |_: Request<IncomingBody>| async { Ok(Response::new(OutgoingBody::empty())) },
    );
    let mut server = std::pin::pin!(server);
    assert!(operations::poll_once(server.as_mut()).await.is_none());
    operations::virtual_clock_advance(Duration::from_secs(2));
    assert!(matches!(
        server.await,
        Err(Error::Protocol(kimojio_fsm_http1::Failure::Timeout))
    ));
    assert_eq!(
        kimojio::clock_now().duration_since(epoch),
        Duration::from_secs(2)
    );
    assert_eq!(counts.closes.get(), 1);
    peer.close().await.unwrap();
}

#[cfg(feature = "virtual-clock")]
#[kimojio::test]
async fn a_request_after_idle_uses_its_actual_admission_time() {
    operations::virtual_clock_enable(true);
    let epoch = kimojio::clock_now();
    let (stream, mut peer, counts) = pair(false);
    let mut config = config(13);
    config.protocol.idle_timeout_ns = Some(100_000_000_000);
    config.protocol.head_timeout_ns = Some(1_000_000_000);
    let (mut client, driver) = connect(stream, config);
    let mut driver = std::pin::pin!(driver.run());
    assert!(operations::poll_once(driver.as_mut()).await.is_none());
    operations::virtual_clock_advance(Duration::from_secs(20));
    let app = async {
        let mut response = client
            .send(
                Request::builder()
                    .uri("/")
                    .header("host", "test")
                    .body(OutgoingBody::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert!(response.body_mut().frame().await.unwrap().is_none());
        client.shutdown().await.unwrap();
    };
    let raw = async {
        let mut buffer = [0; 1024];
        let mut head = Vec::new();
        while !head.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let n = peer.try_read(&mut buffer, None).await.unwrap();
            assert_ne!(n, 0, "new request expired using stale pre-idle time");
            head.extend_from_slice(&buffer[..n]);
        }
        peer.write(
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            None,
        )
        .await
        .unwrap();
        peer.close().await.unwrap();
    };
    let ((), driver, ()) = futures::join!(app, driver, raw);
    driver.unwrap();
    assert_eq!(
        kimojio::clock_now().duration_since(epoch),
        Duration::from_secs(20)
    );
    assert_eq!(operations::virtual_clock_pending_timers(), 0);
    assert_eq!(counts.closes.get(), 1);
}
