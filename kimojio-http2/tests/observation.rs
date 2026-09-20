use std::{
    cell::{Cell, RefCell},
    future::Future,
    rc::Rc,
    time::Duration,
};

use futures::FutureExt;
use kimojio::{ReceiverOneshot, SenderOneshot, oneshot, operations};
use kimojio_http2::{
    Config, Error, IncomingFrame, OutgoingBody, OutgoingFrame, ReceiveEnd, RequestObserver,
    Shutdown, StreamId, StreamOutcome, StreamReport, connect_native,
    http::{Request, Response},
    serve_connection_native, serve_connection_native_with_shutdown,
};

struct Observer {
    id: Rc<Cell<Option<StreamId>>>,
    end: Rc<Cell<Option<StreamOutcome>>>,
    information: Rc<RefCell<Vec<u16>>>,
    report: Option<SenderOneshot<StreamReport>>,
}

impl RequestObserver for Observer {
    fn admitted(&mut self, stream: StreamId) {
        assert!(self.id.replace(Some(stream)).is_none());
    }
    fn informational(&mut self, head: Response<()>) {
        self.information.borrow_mut().push(head.status().as_u16());
    }
    fn receive_end(&mut self, end: ReceiveEnd) {
        assert_eq!(self.id.get(), Some(end.stream));
        assert!(self.end.replace(Some(end.outcome)).is_none());
    }
    fn retired(&mut self, report: &StreamReport) {
        assert_eq!(self.id.get(), Some(report.stream));
        assert_eq!(self.end.get(), report.receive_outcome);
        self.report.take().unwrap().send(report.clone()).ok();
    }
}

struct Observation {
    id: Rc<Cell<Option<StreamId>>>,
    end: Rc<Cell<Option<StreamOutcome>>>,
    information: Rc<RefCell<Vec<u16>>>,
    report: ReceiverOneshot<StreamReport>,
}

fn observer() -> (Observer, Observation) {
    let id = Rc::new(Cell::new(None));
    let end = Rc::new(Cell::new(None));
    let information = Rc::new(RefCell::new(Vec::new()));
    let (send, report) = oneshot();
    (
        Observer {
            id: id.clone(),
            end: end.clone(),
            information: information.clone(),
            report: Some(send),
        },
        Observation {
            id,
            end,
            information,
            report,
        },
    )
}

fn request(path: &str, body: OutgoingBody) -> Request<OutgoingBody> {
    Request::builder()
        .method("POST")
        .uri(format!("http://observe.test{path}"))
        .body(body)
        .unwrap()
}

async fn bounded(future: impl Future<Output = ()>) {
    operations::timeout_at(kimojio::clock_now() + Duration::from_secs(10), future)
        .await
        .unwrap();
}

#[kimojio::test]
async fn response_errors_before_headers_retain_the_admitted_id_and_retirement() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let server = serve_connection_native(peer, Config::default(), |request| async move {
        if request.uri().path() == "/reset" {
            return Err(Error::Application("rejected".into()));
        }
        request
            .body()
            .informational_sender()
            .unwrap()
            .send(Response::builder().status(103).body(()).unwrap())
            .await?;
        Ok(Response::new(OutgoingBody::from_static(b"body")))
    });
    let app = async {
        let (hook, observation) = observer();
        assert!(matches!(
            client
                .send_with_observer(request("/reset", OutgoingBody::empty()), hook)
                .await,
            Err(Error::Stream(StreamOutcome::Reset(8)))
        ));
        let admitted = observation.id.get().expect("actual admission callback");
        let report = observation.report.recv().await.unwrap();
        assert_eq!(report.stream, admitted);
        assert_eq!(report.outcome, StreamOutcome::Reset(8));
        assert_eq!(report.receive_outcome, Some(StreamOutcome::Reset(8)));
        assert!(report.send_failure.is_none());
        assert!(report.error.is_none());

        let (hook, observation) = observer();
        let mut response = client
            .send_with_observer(request("/valid", OutgoingBody::empty()), hook)
            .await
            .unwrap();
        assert_eq!(observation.id.get(), Some(response.body().stream_id()));
        assert_eq!(*observation.information.borrow(), [103]);
        assert_eq!(response.body_mut().collect(16).await.unwrap(), b"body");
        let report = response.body_mut().retirement().await.unwrap();
        assert_eq!(report, observation.report.recv().await.unwrap());
        assert_eq!(report.outcome, StreamOutcome::Complete);
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
async fn unadmitted_rejection_and_queued_cancellation_have_no_synthetic_identity_or_retirement() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let (hook, cancelled) = observer();
    assert!(
        client
            .send_with_observer(request("/must-not-arrive", OutgoingBody::empty()), hook)
            .now_or_never()
            .is_none()
    );
    let server = serve_connection_native(peer, Config::default(), |request| async move {
        assert_eq!(request.uri().path(), "/valid");
        Ok(Response::new(OutgoingBody::empty()))
    });
    let app = async {
        let (hook, rejected) = observer();
        let mut invalid = request("/invalid", OutgoingBody::empty());
        invalid
            .headers_mut()
            .insert("connection", "close".parse().unwrap());
        assert!(matches!(
            client.send_with_observer(invalid, hook).await,
            Err(Error::Command(_))
        ));
        assert!(rejected.report.recv().await.is_err());
        assert_eq!(rejected.id.get(), None);
        assert_eq!(rejected.end.get(), None);
        assert!(cancelled.report.recv().await.is_err());
        assert_eq!(cancelled.id.get(), None);
        assert_eq!(cancelled.end.get(), None);
        let mut response = client
            .send(request("/valid", OutgoingBody::empty()))
            .await
            .unwrap();
        response.body_mut().collect(0).await.unwrap();
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
async fn a_pre_header_connection_failure_still_reports_actual_core_retirement() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let shutdown = Shutdown::default();
    let control = shutdown.clone();
    let server =
        serve_connection_native_with_shutdown(peer, Config::default(), shutdown, move |_request| {
            let control = control.clone();
            async move {
                control.abort();
                std::future::pending::<Result<Response<OutgoingBody>, Error>>().await
            }
        });
    let app = async {
        let (hook, observation) = observer();
        assert!(
            client
                .send_with_observer(request("/close", OutgoingBody::full(vec![1; 65536])), hook)
                .await
                .is_err()
        );
        assert!(observation.id.get().is_some());
        let report = observation.report.recv().await.unwrap();
        assert_eq!(report.outcome, StreamOutcome::ConnectionFailed);
        assert_eq!(
            report.receive_outcome,
            Some(StreamOutcome::ConnectionFailed)
        );
    };
    bounded(async {
        let ((), client, server) = futures::join!(app, connection.run(), server);
        assert!(client.is_err());
        assert_eq!(
            server,
            Err(Error::Connection(kimojio_http2::ConnectionResult::Aborted))
        );
    })
    .await;
}

#[kimojio::test]
async fn observed_retirement_still_waits_for_leases_after_actual_descriptor_close() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (client, connection) = connect_native(fd, Config::default());
    let server = serve_connection_native(peer, Config::default(), |_request| async move {
        Ok(Response::new(OutgoingBody::from_stream(
            futures::stream::iter((0..96).map(|_| Ok(OutgoingFrame::Data(vec![0x35; 16384])))),
        )))
    });
    let closed = Cell::new(false);
    let app = async {
        let (hook, observation) = observer();
        let mut response = client
            .send_with_observer(request("/retain", OutgoingBody::empty()), hook)
            .await
            .unwrap();
        let Some(IncomingFrame::Data(held)) = response.body_mut().frame().await.unwrap() else {
            panic!("data");
        };
        client.control().abort();
        while !closed.get() {
            operations::yield_io().await;
        }
        assert!(observation.report.try_recv().unwrap().is_none());
        assert!(response.body_mut().retirement().now_or_never().is_none());
        assert!(held.iter().all(|byte| *byte == 0x35));
        drop(held);
        let report = observation.report.recv().await.unwrap();
        assert_eq!(report.outcome, StreamOutcome::ConnectionFailed);
        assert_eq!(response.body_mut().retirement().await.unwrap(), report);
    };
    bounded(async {
        futures::join!(
            app,
            async {
                assert!(connection.run().await.is_err());
            },
            async {
                assert!(server.await.is_err());
                closed.set(true);
            }
        );
    })
    .await;
}
