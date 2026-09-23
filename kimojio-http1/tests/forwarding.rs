use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};

use kimojio::{
    AsyncStreamRead, AsyncStreamWrite, Errno, OwnedFdStream, OwnedFdStreamRead, OwnedFdStreamWrite,
    SplittableStream, operations,
};
use kimojio_http1::{
    Config, ConnectionId, IncomingBody, IncomingFrame, OutgoingBody, OutgoingFrame, connect,
    http::{HeaderMap, Request, Response},
    serve_connection,
};

#[derive(Default)]
struct Allocation {
    start: Cell<usize>,
    capacity: Cell<usize>,
    forwarded: Cell<usize>,
}

struct ObservedStream {
    inner: OwnedFdStream,
    receive: Option<Rc<Allocation>>,
    forward: Option<Rc<Allocation>>,
}

struct Reader(OwnedFdStreamRead, Option<Rc<Allocation>>);
struct Writer(OwnedFdStreamWrite, Option<Rc<Allocation>>);

impl SplittableStream for ObservedStream {
    type ReadStream = Reader;
    type WriteStream = Writer;

    async fn split(self) -> Result<(Reader, Writer), Errno> {
        let (read, write) = self.inner.split().await?;
        Ok((Reader(read, self.receive), Writer(write, self.forward)))
    }
}

impl AsyncStreamRead for Reader {
    async fn try_read(
        &mut self,
        bytes: &mut [u8],
        deadline: Option<Instant>,
    ) -> Result<usize, Errno> {
        if let Some(allocation) = &self.1
            && allocation.start.get() == 0
        {
            allocation.start.set(bytes.as_ptr() as usize);
            allocation.capacity.set(bytes.len());
        }
        self.0.try_read(bytes, deadline).await
    }

    async fn read(&mut self, bytes: &mut [u8], deadline: Option<Instant>) -> Result<(), Errno> {
        self.0.read(bytes, deadline).await
    }
}

impl AsyncStreamWrite for Writer {
    async fn write(&mut self, bytes: &[u8], deadline: Option<Instant>) -> Result<(), Errno> {
        if bytes.first() == Some(&0xfe)
            && let Some(allocation) = &self.1
        {
            let pointer = bytes.as_ptr() as usize;
            assert!(pointer >= allocation.start.get());
            assert!(pointer + bytes.len() <= allocation.start.get() + allocation.capacity.get());
            assert!(bytes.iter().all(|&byte| byte == 0xfe));
            allocation
                .forwarded
                .set(allocation.forwarded.get() + bytes.len());
        }
        self.0.write(bytes, deadline).await
    }

    async fn shutdown(&mut self) -> Result<(), Errno> {
        self.0.shutdown().await
    }
    async fn close(&mut self) -> Result<(), Errno> {
        self.0.close().await
    }
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

fn config(slot: u64, capacity: usize) -> Config {
    let mut config = Config::new(ConnectionId {
        slot,
        generation: 1,
    });
    config.protocol.max_buffer_bytes = capacity;
    config.protocol.max_head_bytes = capacity / 2;
    config
}

fn request(body: OutgoingBody) -> Request<OutgoingBody> {
    Request::builder()
        .method("POST")
        .uri("/")
        .header("host", "test")
        .body(body)
        .unwrap()
}

#[kimojio::test]
async fn cross_connection_forwarding_retains_receive_allocation_and_trailers() {
    let (source, origin) = pair();
    let (destination, sink) = pair();
    let allocation = Rc::new(Allocation::default());
    let (mut source, source_driver) = connect(
        ObservedStream {
            inner: source,
            receive: Some(allocation.clone()),
            forward: None,
        },
        config(110, 16 * 1024),
    );
    let (mut destination, destination_driver) = connect(
        ObservedStream {
            inner: destination,
            receive: None,
            forward: Some(allocation.clone()),
        },
        config(111, 16 * 1024),
    );
    let total = 4 * 16 * 1024 + 37;
    let origin = serve_connection(origin, config(112, 16 * 1024), move |_| async move {
        let mut trailers = HeaderMap::new();
        trailers.insert("x-forwarded", "yes".parse().unwrap());
        let mut frames = vec![Ok(OutgoingFrame::Data(Vec::new()))];
        for size in [16 * 1024, 16 * 1024, 16 * 1024, 16 * 1024, 37] {
            frames.push(Ok(OutgoingFrame::Data(vec![0xfe; size])));
        }
        frames.push(Ok(OutgoingFrame::Trailers(trailers)));
        Ok(Response::new(OutgoingBody::from_stream(
            None,
            futures::stream::iter(frames),
        )))
    });
    let sink = serve_connection(
        sink,
        config(113, 16 * 1024),
        move |mut request: Request<IncomingBody>| async move {
            let mut bytes = Vec::new();
            let mut trailers = false;
            while let Some(frame) = request.body_mut().frame().await? {
                match frame {
                    IncomingFrame::Data(chunk) => bytes.extend_from_slice(&chunk),
                    IncomingFrame::Trailers(headers) => {
                        assert_eq!(headers["x-forwarded"], "yes");
                        trailers = true;
                    }
                }
            }
            assert_eq!(bytes, vec![0xfe; total]);
            assert!(trailers);
            Ok(Response::new(OutgoingBody::full(b"forwarded")))
        },
    );
    let app = async {
        let response = source.send(request(OutgoingBody::empty())).await.unwrap();
        let mut response = destination
            .send(request(OutgoingBody::from_incoming(response.into_body())))
            .await
            .unwrap();
        assert_eq!(response.body_mut().collect(32).await.unwrap(), b"forwarded");
        assert_eq!(allocation.forwarded.get(), total);
        source.shutdown().await.unwrap();
        destination.shutdown().await.unwrap();
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
        let ((), source, destination, origin, sink) = futures::join!(
            app,
            source_driver.run(),
            destination_driver.run(),
            origin,
            sink
        );
        source.unwrap();
        destination.unwrap();
        origin.unwrap();
        sink.unwrap();
    })
    .await
    .unwrap();
}

#[kimojio::test]
async fn short_lease_from_larger_receive_allocation_is_rejected_without_hanging() {
    let (source, origin) = pair();
    let (destination, sink) = pair();
    let (mut source, source_driver) = connect(source, config(120, 32 * 1024));
    let (mut destination, destination_driver) = connect(destination, config(121, 16 * 1024));
    let origin = serve_connection(origin, config(122, 32 * 1024), |_| async {
        Ok(Response::new(OutgoingBody::full(b"small")))
    });
    let sink = serve_connection(
        sink,
        config(123, 16 * 1024),
        |mut request: Request<IncomingBody>| async move {
            assert!(request.body_mut().collect(32).await.is_err());
            Ok(Response::new(OutgoingBody::empty()))
        },
    );
    let app = async {
        let response = source.send(request(OutgoingBody::empty())).await.unwrap();
        let result = destination
            .send(request(OutgoingBody::from_incoming(response.into_body())))
            .await;
        assert!(matches!(result, Err(kimojio_http1::Error::Limit)));
        assert!(matches!(
            source.shutdown().await,
            Err(kimojio_http1::Error::Protocol(
                kimojio_fsm_http1::Failure::Cancelled
            ))
        ));
        let _ = destination.shutdown().await;
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
        let ((), _, _, _, _) = futures::join!(
            app,
            source_driver.run(),
            destination_driver.run(),
            origin,
            sink
        );
    })
    .await
    .unwrap();
}

struct SourceDropped(Rc<Cell<bool>>);

impl Drop for SourceDropped {
    fn drop(&mut self) {
        assert!(!self.0.replace(true));
    }
}

#[kimojio::test]
async fn early_destination_response_drops_the_forwarded_source_without_deadlock() {
    let (source, origin) = pair();
    let (destination, sink) = pair();
    let (mut source, source_driver) = connect(source, config(130, 16 * 1024));
    let (mut destination, destination_driver) = connect(destination, config(131, 16 * 1024));
    let dropped = Rc::new(Cell::new(false));
    let producer = SourceDropped(dropped.clone());
    let mut producer = Some(producer);
    let origin = serve_connection(origin, config(132, 16 * 1024), move |_| {
        let producer = producer.take().unwrap();
        async move {
            let frames = futures::stream::unfold(producer, |producer| async move {
                Some((Ok(OutgoingFrame::Data(vec![0xfe; 16 * 1024])), producer))
            });
            Ok(Response::new(OutgoingBody::from_stream(None, frames)))
        }
    });
    let sink = serve_connection(sink, config(133, 16 * 1024), |request| async move {
        drop(request);
        Ok(Response::builder()
            .status(413)
            .body(OutgoingBody::empty())
            .unwrap())
    });
    let app = async {
        let response = source.send(request(OutgoingBody::empty())).await.unwrap();
        let mut response = destination
            .send(request(OutgoingBody::from_incoming(response.into_body())))
            .await
            .unwrap();
        assert_eq!(response.status(), 413);
        assert!(response.body_mut().collect(32).await.unwrap().is_empty());
        let _ = source.shutdown().await;
        destination.shutdown().await.unwrap();
    };
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(3), async {
        let ((), source, destination, origin, sink) = futures::join!(
            app,
            source_driver.run(),
            destination_driver.run(),
            origin,
            sink
        );
        assert!(source.is_err());
        destination.unwrap();
        assert!(origin.is_err());
        sink.unwrap();
    })
    .await
    .unwrap();
    assert!(dropped.get());
}
