mod schema;
mod server;

use futures::{
    FutureExt, StreamExt,
    future::{LocalBoxFuture, poll_fn},
    stream::FuturesUnordered,
};
use kimojio::{ReceiverOneshot, SenderOneshot, oneshot, operations, socket_helpers};
use kimojio_http2::{
    Client, Error, IncomingBody, IncomingFrame, OutgoingBody, OutgoingFrame, ReceiveEnd,
    RequestObserver, Shutdown, StreamId, StreamReport as RetirementReport, connect_native,
    http::{HeaderMap, Request, Response, header},
};
use schema::*;
use sha2::Digest;
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    fs::File,
    future::Future,
    io::Read,
    net::SocketAddr,
    rc::Rc,
    task::{Poll, Waker},
    time::Duration,
};

const CHUNK: usize = 16 * 1024;
type ResponseFuture = LocalBoxFuture<'static, Result<Response<IncomingBody>, Error>>;

struct Observer {
    context: Context,
    index: usize,
    admitted: Rc<Cell<Option<StreamId>>>,
    retired: Option<SenderOneshot<RetirementReport>>,
}

impl RequestObserver for Observer {
    fn admitted(&mut self, id: StreamId) {
        self.admitted.set(Some(id));
        {
            let mut report = self.context.report.borrow_mut();
            report.streams[self.index].admitted = true;
            report.streams[self.index].stream_id = id.get();
        }
        let expected = self.index as u32 * 2 + 1;
        if id.get() != expected {
            self.context.fail(format!(
                "request submission order changed: expected stream {expected}, got {}",
                id.get()
            ));
            self.context.shutdown.abort();
        }
    }
    fn informational(&mut self, head: Response<()>) {
        let mut report = self.context.report.borrow_mut();
        let stream = &mut report.streams[self.index];
        if stream.informational.len() == 64 {
            drop(report);
            self.context
                .fail("informational responses exceed fixture bound".into());
            self.context.shutdown.abort();
        } else {
            stream.informational.push(head.status().as_u16());
        }
    }
    fn receive_end(&mut self, end: ReceiveEnd) {
        self.context.report.borrow_mut().streams[self.index].receive(Some(end.outcome));
        self.context.progress.borrow_mut().finish(end.stream.get());
    }
    fn retired(&mut self, report: &RetirementReport) {
        self.context.report.borrow_mut().streams[self.index].retirement(report);
        if let Some(sender) = self.retired.take() {
            let _ = sender.send(report.clone());
        }
    }
}

#[derive(Default)]
struct Progress {
    receive_done: BTreeSet<u32>,
    waiters: BTreeMap<u32, Waker>,
}

impl Progress {
    fn finish(&mut self, id: u32) {
        self.receive_done.insert(id);
        for waker in std::mem::take(&mut self.waiters).into_values() {
            waker.wake();
        }
    }
}

#[derive(Clone)]
struct Context {
    input: Rc<Input>,
    report: Rc<RefCell<Report>>,
    progress: Rc<RefCell<Progress>>,
    held_capacity: Rc<Cell<usize>>,
    held_count: Rc<Cell<usize>>,
    metadata: Rc<Cell<usize>>,
    stop_admission: Rc<Cell<bool>>,
    shutdown: Shutdown,
}

impl Context {
    fn fail(&self, error: String) {
        let mut report = self.report.borrow_mut();
        if report.fixture_error.is_none() {
            report.fixture_error = Some(error);
        }
    }
}

struct Held {
    chunks: Vec<kimojio_http2::BodyChunk>,
    capacity: usize,
    context: Context,
}

impl Held {
    fn new(context: Context) -> Self {
        Self {
            chunks: Vec::new(),
            capacity: 0,
            context,
        }
    }
    fn push(&mut self, chunk: kimojio_http2::BodyChunk) -> Result<(), String> {
        let capacity = chunk.retained_capacity();
        if self.context.held_capacity.get() + capacity > MAX_METADATA
            || self.context.held_count.get() == 8192
        {
            return Err("paused body leases exceed fixture bounds".into());
        }
        self.context
            .held_capacity
            .set(self.context.held_capacity.get() + capacity);
        self.context
            .held_count
            .set(self.context.held_count.get() + 1);
        self.capacity += capacity;
        self.chunks.push(chunk);
        Ok(())
    }
    fn clear(&mut self) {
        self.context
            .held_capacity
            .set(self.context.held_capacity.get() - self.capacity);
        self.context
            .held_count
            .set(self.context.held_count.get() - self.chunks.len());
        self.capacity = 0;
        self.chunks.clear();
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        self.clear();
    }
}

pub fn main() -> Result<(), String> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() == 2 && args[0] == "server" {
        let input: ServerInput = read_input(&args[1])?;
        input.validate()?;
        return kimojio::run(0, server::run(input))
            .ok_or("native runtime stopped without a server result")?
            .map_err(|_| "native runtime panicked")?;
    }
    if args.len() != 3 || args[0] != "client" {
        return Err("usage: interop client REQUEST_JSON RESULT_JSON | server REQUEST_JSON".into());
    }
    let input: Input = read_input(&args[1])?;
    input.validate()?;
    let result = kimojio::run(0, execute(input))
        .ok_or("native runtime stopped without a fixture result")?
        .map_err(|_| "native runtime panicked")??;
    std::fs::write(&args[2], &result.0).map_err(|e| e.to_string())?;
    match result.1 {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn read_input<T: serde::de::DeserializeOwned>(path: &str) -> Result<T, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    file.take(MAX_INPUT + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_INPUT {
        return Err("request file exceeds fixture bound".into());
    }
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

async fn execute(input: Input) -> Result<(Vec<u8>, Option<String>), String> {
    operations::io_scope(async move || {
        let timeout = Duration::from_millis(input.timeout_ms);
        let address = SocketAddr::new(input.host.parse().map_err(|e| format!("{e}"))?, input.port);
        let socket = operations::timeout_at(
            kimojio::clock_now() + timeout,
            socket_helpers::create_client_socket(&address),
        )
        .await
        .map_err(|e| format!("native connect deadline: {e:?}"))?
        .map_err(|e| format!("native connect: {e}"))?;
        let (client, connection) = connect_native(socket, input.config.config()?);
        let control = client.control();
        let report = Rc::new(RefCell::new(Report {
            schema: 1,
            streams: Vec::new(),
            connection: ConnectionReport::pending(),
            fixture_error: None,
        }));
        let context = Context {
            input: Rc::new(input),
            report: report.clone(),
            progress: Rc::default(),
            held_capacity: Rc::default(),
            held_count: Rc::default(),
            metadata: Rc::default(),
            stop_admission: Rc::default(),
            shutdown: control.clone(),
        };
        let app_context = context.clone();
        let deadline = kimojio::clock_now() + timeout;
        let application = async move {
            match operations::timeout_at(deadline, requests(client, app_context.clone())).await {
                Ok(()) => app_context.shutdown.graceful(),
                Err(error) => {
                    app_context.fail(format!("application watchdog: {error:?}"));
                    app_context.shutdown.abort();
                }
            }
        };
        let driver_report = report.clone();
        let driver = async move {
            match ConnectionReport::driver(connection.run().await) {
                Ok(closed) => driver_report.borrow_mut().connection = closed,
                Err(error) => driver_report.borrow_mut().fixture_error = Some(error),
            }
        };
        if let Err(error) = operations::timeout_at(deadline + Duration::from_secs(3), async {
            futures::join!(application, driver);
        })
        .await
        {
            control.abort();
            context.fail(format!(
                "driver settlement watchdog: {error:?}; close is unconfirmed"
            ));
        }
        let mut report = report.borrow_mut();
        report.streams.retain(|stream| stream.admitted);
        for stream in &mut report.streams {
            stream.finish();
        }
        let bytes = serde_json::to_vec_pretty(&*report).map_err(|e| e.to_string())?;
        Ok((bytes, report.fixture_error.clone()))
    })
    .await
}

fn trailers(fields: &Fields) -> Result<HeaderMap, String> {
    let mut map = HeaderMap::new();
    for (name, value) in fields {
        map.append(
            header::HeaderName::from_bytes(name.as_bytes()).map_err(|e| e.to_string())?,
            header::HeaderValue::from_bytes(value.as_bytes()).map_err(|e| e.to_string())?,
        );
    }
    Ok(map)
}

fn trailer_fields(headers: &HeaderMap) -> Result<Fields, header::ToStrError> {
    headers
        .iter()
        .map(|(name, value)| {
            value
                .to_str()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
        })
        .collect()
}

fn outgoing(remaining: u64, id: u32, trailers: HeaderMap) -> OutgoingBody {
    if remaining == 0 && trailers.is_empty() {
        return OutgoingBody::empty();
    }
    OutgoingBody::from_stream(futures::stream::unfold(
        (remaining, (!trailers.is_empty()).then_some(trailers)),
        move |(remaining, trailers)| async move {
            if remaining != 0 {
                let n = production_size(remaining);
                Some((
                    Ok(OutgoingFrame::Data(vec![(id % 251) as u8; n])),
                    (remaining - n as u64, trailers),
                ))
            } else {
                trailers.map(|trailers| (Ok(OutgoingFrame::Trailers(trailers)), (0, None)))
            }
        },
    ))
}

fn production_size(remaining: u64) -> usize {
    remaining.min(CHUNK as u64) as usize
}

fn request(input: &Input, index: usize) -> Result<Request<OutgoingBody>, String> {
    let spec = &input.requests[index];
    let id = index as u32 * 2 + 1;
    let address = SocketAddr::new(input.host.parse().map_err(|e| format!("{e}"))?, input.port);
    let uri = if spec.method == "CONNECT" {
        address.to_string()
    } else {
        format!("http://{address}{}", spec.path)
    };
    let mut builder = Request::builder().method(spec.method.as_str()).uri(uri);
    if spec.method != "CONNECT" {
        builder = builder.header(header::CONTENT_LENGTH, spec.body_bytes.to_string());
    }
    builder
        .body(outgoing(spec.body_bytes, id, trailers(&spec.trailers)?))
        .map_err(|e| e.to_string())
}

/// A send future queues its request during this first poll, before any later send.
/// Merely inserting futures in FuturesUnordered does not establish submission order.
async fn prime<F: Future>(
    mut future: std::pin::Pin<Box<F>>,
) -> (Poll<F::Output>, std::pin::Pin<Box<F>>) {
    let first = poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx))).await;
    (first, future)
}

async fn requests(client: Client, context: Context) {
    let mut active = FuturesUnordered::new();
    let mut next = 0;
    let mut retired = 0;
    loop {
        while !context.stop_admission.get()
            && next < context.input.requests.len()
            && active.len() < context.input.concurrency
        {
            let request = match request(&context.input, next) {
                Ok(request) => request,
                Err(error) => {
                    context.fail(error);
                    context.stop_admission.set(true);
                    context.shutdown.abort();
                    break;
                }
            };
            context
                .report
                .borrow_mut()
                .streams
                .push(StreamReport::new(next as u32 * 2 + 1));
            let send_client = client.clone();
            let admitted = Rc::new(Cell::new(None));
            let (retired, retirement) = oneshot();
            let observer = Observer {
                context: context.clone(),
                index: next,
                admitted: admitted.clone(),
                retired: Some(retired),
            };
            let (first, future) = prime(Box::pin(async move {
                send_client.send_with_observer(request, observer).await
            }))
            .await;
            let future: ResponseFuture = match first {
                Poll::Ready(result) => futures::future::ready(result).boxed_local(),
                Poll::Pending => future,
            };
            active.push(consume(next, future, context.clone(), admitted, retirement).boxed_local());
            next += 1;
        }
        let Some(()) = active.next().await else {
            break;
        };
        retired += 1;
        if context.input.actions.iter().any(|action| {
            matches!(action,
            Action::GracefulClose { after_streams } if retired >= *after_streams)
        }) {
            context.stop_admission.set(true);
            context.shutdown.graceful();
        }
    }
}

async fn wait_receive(context: &Context, waiting: u32, target: u32) {
    poll_fn(|cx| {
        let mut progress = context.progress.borrow_mut();
        if progress.receive_done.contains(&target) {
            Poll::Ready(())
        } else {
            progress.waiters.insert(waiting, cx.waker().clone());
            Poll::Pending
        }
    })
    .await
}

async fn consume(
    index: usize,
    future: ResponseFuture,
    context: Context,
    admitted: Rc<Cell<Option<StreamId>>>,
    retirement: ReceiverOneshot<RetirementReport>,
) {
    let id = index as u32 * 2 + 1;
    let response = match future.await {
        Ok(response) => response,
        Err(error) => {
            context.report.borrow_mut().streams[index].note_error(&error);
            if admitted.get().is_some() {
                match retirement.recv().await {
                    Ok(report) => context.report.borrow_mut().streams[index].retirement(&report),
                    Err(error) => context.fail(format!(
                        "missing retirement after admitted request {id} failed: {error:?}"
                    )),
                }
            } else if !matches!(
                error,
                Error::Closed | Error::Connection(_) | Error::Cancelled
            ) {
                context.fail(format!("request rejected before admission: {error:?}"));
            }
            if matches!(error, Error::Closed | Error::Connection(_)) {
                context.stop_admission.set(true);
            }
            return;
        }
    };
    let (parts, mut body) = response.into_parts();
    {
        let mut report = context.report.borrow_mut();
        let stream = &mut report.streams[index];
        stream.status = Some(parts.status.as_u16());
        stream.content_length = parts
            .headers
            .get(header::CONTENT_LENGTH)
            .and_then(|v| kimojio_fsm_http2::parse_content_length(v.as_bytes()))
            .map(|n| n as u64);
    }
    if body.stream_id().get() != id {
        context.fail(format!(
            "request submission order changed: expected stream {id}, got {}",
            body.stream_id().get()
        ));
        context.shutdown.abort();
    }
    let pause = context
        .input
        .actions
        .iter()
        .find_map(|action| match *action {
            Action::Pause {
                stream_id,
                until_stream_ended,
            } if stream_id == id => Some(until_stream_ended),
            _ => None,
        });
    let reset_at = context
        .input
        .actions
        .iter()
        .find_map(|action| match *action {
            Action::Reset {
                stream_id,
                after_bytes,
                ..
            } if stream_id == id => Some(after_bytes),
            _ => None,
        });
    let mut reset = false;
    let mut held = Held::new(context.clone());
    loop {
        let paused =
            pause.filter(|target| !context.progress.borrow().receive_done.contains(target));
        if paused.is_none() {
            held.clear();
        }
        let frame = if let Some(target) = paused.filter(|_| !held.chunks.is_empty()) {
            let frame = body.frame().fuse();
            let resume = wait_receive(&context, id, target).fuse();
            futures::pin_mut!(frame, resume);
            futures::select! {
                frame = frame => Some(frame),
                () = resume => None,
            }
        } else {
            Some(body.frame().await)
        };
        let Some(frame) = frame else {
            held.clear();
            continue;
        };
        match frame {
            Ok(Some(IncomingFrame::Data(chunk))) => {
                let bytes = {
                    let mut report = context.report.borrow_mut();
                    let stream = &mut report.streams[index];
                    stream.bytes += chunk.len() as u64;
                    stream.digest.update(&*chunk);
                    stream.bytes
                };
                if paused.is_some() {
                    if let Err(error) = held.push(chunk) {
                        context.fail(error);
                        held.clear();
                        body.cancel();
                    }
                } else {
                    drop(chunk);
                }
                if !reset && reset_at.is_some_and(|limit| bytes >= limit) {
                    reset = true;
                    held.clear();
                    body.cancel();
                }
            }
            Ok(Some(IncomingFrame::Trailers(headers))) => match trailer_fields(&headers) {
                Ok(fields) => {
                    let n = fields.iter().map(|(n, v)| n.len() + v.len()).sum::<usize>();
                    if context.metadata.get() + n > MAX_METADATA {
                        context.fail("response metadata exceeds fixture bound".into());
                    } else {
                        context.metadata.set(context.metadata.get() + n);
                        context.report.borrow_mut().streams[index].trailers = fields;
                    }
                }
                Err(error) => context.fail(format!("non-text trailer: {error}")),
            },
            Ok(None) => break,
            Err(error) => {
                context.report.borrow_mut().streams[index].note_error(&error);
                break;
            }
        }
    }
    context.report.borrow_mut().streams[index].receive(body.receive_outcome());
    context.progress.borrow_mut().finish(id);
    held.clear();
    match body.retirement().await {
        Ok(report) => context.report.borrow_mut().streams[index].retirement(&report),
        Err(error) => {
            context.fail(format!(
                "missing retirement for response stream {id}: {error:?}"
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generator_allocation_does_not_grow_with_upload_length() {
        assert_eq!(production_size(16 * 1024 * 1024 + 17), CHUNK);
        assert_eq!(production_size(17), 17);
        assert_eq!(production_size(0), 0);
    }

    #[test]
    fn first_polls_establish_order_before_ready_queue_scheduling() {
        futures::executor::block_on(async {
            let order = Rc::new(RefCell::new(Vec::new()));
            let mut futures = Vec::new();
            for id in [1, 3, 5] {
                let seen = order.clone();
                let (first, future) = prime(Box::pin(async move {
                    seen.borrow_mut().push(id);
                    futures::future::pending::<()>().await;
                }))
                .await;
                assert!(first.is_pending());
                futures.push(future);
            }
            assert_eq!(*order.borrow(), [1, 3, 5]);
        });
    }

    #[kimojio::test]
    async fn observers_report_preheader_retirement_and_actual_informational_heads() {
        let input: Input = serde_json::from_str(
            r#"{"schema":1,"host":"127.0.0.1","port":1234,"timeout_ms":1000,
            "request_count":2,"concurrency":2,"requests":[
            {"method":"GET","path":"/reset"},
            {"method":"GET","path":"/informational/37"}]}"#,
        )
        .unwrap();
        let config = input.config.config().unwrap();
        let (fd, peer) = kimojio::pipe::bipipe();
        let (client, driver) = connect_native(fd, config.clone());
        let context = Context {
            input: Rc::new(input),
            report: Rc::new(RefCell::new(Report {
                schema: 1,
                streams: Vec::new(),
                connection: ConnectionReport::pending(),
                fixture_error: None,
            })),
            progress: Rc::default(),
            held_capacity: Rc::default(),
            held_count: Rc::default(),
            metadata: Rc::default(),
            stop_admission: Rc::default(),
            shutdown: client.control(),
        };
        let server = kimojio_http2::serve_connection_native(peer, config, |request| async {
            if request.uri().path() == "/reset" {
                Err(Error::Application("test rejection before headers".into()))
            } else {
                server::handle(request).await
            }
        });
        let app = async {
            requests(client, context.clone()).await;
            context.shutdown.graceful();
        };
        let ((), driver, server) =
            operations::timeout_at(kimojio::clock_now() + Duration::from_secs(10), async {
                futures::join!(app, driver.run(), server)
            })
            .await
            .unwrap();
        driver.unwrap();
        server.unwrap();
        let report = context.report.borrow();
        assert!(report.fixture_error.is_none());
        assert_eq!(report.streams.len(), 2);
        let reset = &report.streams[0];
        assert!(reset.admitted);
        assert_eq!(reset.stream_id, 1);
        assert_eq!(reset.status, None);
        assert_eq!(reset.outcome, Some(Outcome::Reset));
        assert_eq!(reset.error.as_ref().unwrap().code, 8);
        assert!(!reset.ended);
        let complete = &report.streams[1];
        assert!(complete.admitted);
        assert_eq!(complete.stream_id, 3);
        assert_eq!(complete.status, Some(200));
        assert_eq!(complete.bytes, 37);
        assert_eq!(complete.informational, [103]);
        assert_eq!(complete.outcome, Some(Outcome::Complete));
        assert!(complete.ended);
    }

    #[test]
    fn multi_name_trailers_preserve_every_same_name_occurrence_in_order() {
        fn grouped(fields: &Fields) -> BTreeMap<&str, Vec<&str>> {
            let mut names = BTreeMap::<&str, Vec<&str>>::new();
            for (name, value) in fields {
                names.entry(name).or_default().push(value);
            }
            names
        }

        let input: Fields = [
            ("x-a", "first"),
            ("x-b", "separate"),
            ("x-a", "repeated"),
            ("x-c", "other"),
            ("x-b", "separate"),
            ("x-a", "repeated"),
            ("x-a", "last, literal"),
        ]
        .into_iter()
        .map(|(name, value)| (name.into(), value.into()))
        .collect();
        let output = trailer_fields(&trailers(&input).unwrap()).unwrap();
        assert_eq!(output.len(), input.len());
        assert_eq!(grouped(&output), grouped(&input));
        assert_eq!(
            grouped(&output)["x-a"],
            ["first", "repeated", "repeated", "last, literal"]
        );
        assert_eq!(grouped(&output)["x-b"], ["separate", "separate"]);
    }
}
