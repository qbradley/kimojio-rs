//! Cooperative async drivers for one HTTP/1 client or server connection.
//!
//! Client creation returns a handle plus a caller-polled driver; server helpers
//! return a future that owns and drives one established transport.
use std::{
    cell::Cell,
    future::Future,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
    time::{Duration, Instant},
};

use futures::{FutureExt, StreamExt, future::LocalBoxFuture};
use http::{HeaderMap, Request, Response};
use kimojio::{
    CancellationToken, Receiver, ReceiverOneshot, Sender, SenderOneshot, SplittableStream,
    async_channel, oneshot, operations,
};
use kimojio_fsm_http1 as core;

use crate::body::{BodyDemand, OutgoingData, OutgoingSource, SourceFrame};
use crate::observation::BoundObservation;
use crate::{
    BodyChunk, Error, IncomingBody, OutgoingBody,
    io::{self, WriteResult},
    io_driver::{IoDriver, WorkerIo, native_io, poll_receive},
    metadata,
    transport::{NativeTransport, StreamTransport, Transport},
};

#[cfg(test)]
use crate::OutgoingFrame;
#[cfg(test)]
use crate::io::{Pending, WriteAction};

/// Protocol limits and scheduling options for one wrapped connection.
///
/// Create with [`Config::new`] to retain safe defaults, then customize the
/// protocol limits, cooperative turn budget, or optional observation handle.
#[derive(Clone, Debug)]
pub struct Config {
    /// Unique identity allocated by the application for this connection.
    pub connection_id: core::ConnectionId,
    /// HTTP framing, resource limits, and deadline policy.
    pub protocol: core::Config,
    /// Maximum number of synchronous core transitions before yielding to async work.
    pub turn_budget: usize,
    /// Combines eligible full bodies with metadata in one owned write.
    ///
    /// Disabled by default to retain separate metadata/payload completions.
    /// When enabled, the client upload deadline includes metadata, and generic
    /// write-all transports report coarser server progress for deadline refresh.
    pub coalesce_full_bodies: bool,
    /// An optional handle reserved by this driver, including before its first poll.
    pub observation: Option<crate::Observation>,
}

impl Config {
    /// Creates default wrapper settings for an application-allocated connection ID.
    pub fn new(connection_id: core::ConnectionId) -> Self {
        Self {
            connection_id,
            protocol: core::Config::default(),
            turn_budget: 64,
            coalesce_full_bodies: false,
            observation: None,
        }
    }
}

/// Cooperative control for a connection whose driver remains polled.
#[derive(Clone, Default)]
pub struct Shutdown {
    graceful: Rc<CancellationToken>,
    abort: Rc<CancellationToken>,
}

impl Shutdown {
    /// Stops accepting new exchanges and lets current protocol work drain.
    ///
    /// Keep polling the associated driver until it returns; this is cooperative.
    pub fn graceful(&self) {
        self.graceful.cancel();
    }

    /// Requests cancellation of active work and a safe transport close.
    ///
    /// The driver must remain polled to settle outstanding I/O operations.
    pub fn abort(&self) {
        self.abort.cancel();
    }
}

struct CancelOnDrop(Option<Rc<CancellationToken>>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(token) = self.0.take() {
            token.cancel();
        }
    }
}

struct SendRequest {
    request: Request<OutgoingBody>,
    response: SenderOneshot<Result<Response<IncomingBody>, Error>>,
    cancel: Rc<CancellationToken>,
}

struct RequestQueue(Receiver<SendRequest>);

impl Drop for RequestQueue {
    fn drop(&mut self) {
        while let Ok(Some(request)) = self.0.try_recv() {
            let _ = request.response.send(Err(Error::Closed));
        }
    }
}

struct DemandQueue(crate::receive_lane::ReceiveLane<BodyDemand>);

impl Drop for DemandQueue {
    fn drop(&mut self) {
        while let Ok(Some(demand)) = self.0.try_recv() {
            let _ = demand.accepted.send(Err(Error::Closed));
        }
    }
}

/// A single-connection client. It has no pool, retries, redirects, or URL resolver.
pub struct Client {
    requests: Sender<SendRequest>,
    shutdown: Shutdown,
    done: Option<ReceiverOneshot<Result<(), Error>>>,
}

impl Client {
    /// Returns the final response head while its body is still streaming.
    ///
    /// Poll the connection driver concurrently. Only one request can be queued;
    /// dropping this future cancels queued admission or the admitted exchange.
    /// Consume the response body before sending another request for reliable reuse.
    ///
    /// This helper assumes the associated driver is already being polled (see
    /// [`connect`] for the complete setup). Framing headers are derived from the
    /// outgoing body; supply the request target and `Host` explicitly.
    ///
    /// ```
    /// use kimojio_http1::{Client, Error, OutgoingBody, http::Request};
    ///
    /// async fn post_message(client: &mut Client) -> Result<Vec<u8>, Error> {
    ///     let request = Request::builder()
    ///         .method("POST")
    ///         .uri("/messages")
    ///         .header("host", "localhost:8080")
    ///         .header("content-type", "text/plain")
    ///         .body(OutgoingBody::full(b"hello".to_vec()))
    ///         .map_err(|_| Error::InvalidMetadata)?;
    ///     let mut response = client.send(request).await?;
    ///     response.body_mut().collect(64 * 1024).await
    /// }
    /// ```
    pub async fn send(
        &mut self,
        request: Request<OutgoingBody>,
    ) -> Result<Response<IncomingBody>, Error> {
        let cancel = Rc::new(CancellationToken::new());
        let mut guard = CancelOnDrop(Some(cancel.clone()));
        let (response, receive) = oneshot();
        self.requests
            .send(SendRequest {
                request,
                response,
                cancel,
            })
            .await
            .map_err(|_| Error::Closed)?;
        let result = receive.recv().await.map_err(|_| Error::Closed)?;
        if result.is_ok() {
            guard.0.take();
        }
        result
    }

    /// Clones the handle used to request graceful shutdown or abort.
    pub fn control(&self) -> Shutdown {
        self.shutdown.clone()
    }

    /// Stops admission, waits for the active exchange, and closes the transport.
    ///
    /// Finish consuming the active response first. Keep the driver polled while
    /// awaiting shutdown; simply dropping its future is not graceful shutdown.
    ///
    /// ```
    /// use kimojio_http1::{Client, Error, IncomingBody, http::Response};
    ///
    /// // The driver is running concurrently with this application helper.
    /// async fn finish(client: &mut Client, mut response: Response<IncomingBody>)
    ///     -> Result<Vec<u8>, Error>
    /// {
    ///     let bytes = response.body_mut().collect(64 * 1024).await?;
    ///     client.shutdown().await?;
    ///     Ok(bytes)
    /// }
    /// ```
    pub async fn shutdown(&mut self) -> Result<(), Error> {
        self.shutdown.graceful();
        match self.done.take() {
            Some(done) => done.recv().await.map_err(|_| Error::Closed)?,
            None => Ok(()),
        }
    }
}

/// Caller-owned connection driver. Keep `run` polled through shutdown.
pub struct Connection<S> {
    stream: Box<S>,
    config: Config,
    requests: RequestQueue,
    shutdown: Shutdown,
    done: SenderOneshot<Result<(), Error>>,
    observation: Result<Option<BoundObservation>, Error>,
}

/// Wraps an established transport. No I/O occurs until the driver is polled.
///
/// This does not resolve a host, connect a socket, or spawn a task. Under the
/// Kimojio runtime, poll both halves together and signal shutdown even if the
/// application fails. `join!` keeps the driver alive to settle I/O on that path.
///
/// ```
/// use kimojio::SplittableStream;
/// use kimojio_http1::{connect, Config, Error, OutgoingBody, http::Request};
///
/// async fn fetch<S: SplittableStream>(stream: S, config: Config) -> Result<Vec<u8>, Error> {
///     let (mut client, connection) = connect(stream, config);
///     let control = client.control();
///     let application = async {
///         let result = async {
///             let request = Request::builder()
///                 .uri("/")
///                 .header("host", "localhost:8080")
///                 .body(OutgoingBody::empty())
///                 .map_err(|_| Error::InvalidMetadata)?;
///             let mut response = client.send(request).await?;
///             let bytes = response.body_mut().collect(64 * 1024).await?;
///             client.shutdown().await?;
///             Ok::<_, Error>(bytes)
///         }.await;
///         if result.is_err() {
///             control.abort();
///         }
///         result
///     };
///     let (result, driver_result) = futures::join!(application, connection.run());
///     let bytes = result?;
///     driver_result?;
///     Ok(bytes)
/// }
/// ```
pub fn connect<S: SplittableStream>(stream: S, config: Config) -> (Client, Connection<S>) {
    connection(stream, config)
}

/// Caller-owned driver for one-shot native socket operations.
pub struct NativeConnection(Connection<NativeTransport>);

/// Takes ownership of an established socket without a buffered stream adapter.
///
/// Native reads use the core's receive storage directly. Each native write
/// reports the exact byte count from one writev completion.
///
/// Use the native driver in the same concurrent polling pattern as [`connect`].
/// Here a request with `Connection: close` lets the server end the connection
/// after the response, while the application requests graceful shutdown too.
///
/// ```
/// use kimojio_http1::{connect_native, Config, Error, OutgoingBody, http::Request};
///
/// async fn fetch_native(socket: kimojio::OwnedFd, config: Config) -> Result<Vec<u8>, Error> {
///     let (mut client, connection) = connect_native(socket, config);
///     let control = client.control();
///     let application = async {
///         let result = async {
///             let request = Request::builder()
///                 .uri("/")
///                 .header("host", "localhost:8080")
///                 .header("connection", "close")
///                 .body(OutgoingBody::empty())
///                 .map_err(|_| Error::InvalidMetadata)?;
///             let mut response = client.send(request).await?;
///             response.body_mut().collect(64 * 1024).await
///         }.await;
///         if result.is_err() { control.abort(); } else { control.graceful(); }
///         result
///     };
///     let (result, driver_result) = futures::join!(application, connection.run());
///     let bytes = result?;
///     driver_result?;
///     Ok(bytes)
/// }
/// ```
pub fn connect_native(fd: kimojio::OwnedFd, config: Config) -> (Client, NativeConnection) {
    let (client, connection) = connection(NativeTransport(fd), config);
    (client, NativeConnection(connection))
}

fn connection<S>(stream: S, config: Config) -> (Client, Connection<S>) {
    let observation = bind_observation(&config);
    let (send, requests) = async_channel();
    let (done, receive) = oneshot();
    let shutdown = Shutdown::default();
    (
        Client {
            requests: send,
            shutdown: shutdown.clone(),
            done: Some(receive),
        },
        Connection {
            stream: Box::new(stream),
            config,
            requests: RequestQueue(requests),
            shutdown,
            done,
            observation,
        },
    )
}

impl<S: SplittableStream> Connection<S> {
    /// Polls the client connection and its I/O until graceful close or failure.
    ///
    /// Run concurrently with `Client::send` and keep polling through shutdown.
    /// The example below composes an existing client/driver pair with application
    /// work, rather than spawning a separate task. See [`connect`] for creating it.
    ///
    /// ```
    /// use kimojio::SplittableStream;
    /// use kimojio_http1::{Client, Connection, Error, OutgoingBody, http::Request};
    ///
    /// async fn round_trip<S: SplittableStream>(
    ///     mut client: Client, connection: Connection<S>, request: Request<OutgoingBody>,
    /// ) -> Result<Vec<u8>, Error> {
    ///     let control = client.control();
    ///     let application = async {
    ///         let result = async {
    ///             let mut response = client.send(request).await?;
    ///             response.body_mut().collect(64 * 1024).await
    ///         }.await;
    ///         if result.is_err() { control.abort(); } else { control.graceful(); }
    ///         result
    ///     };
    ///     let (result, driver_result) = futures::join!(application, connection.run());
    ///     let bytes = result?;
    ///     driver_result?;
    ///     Ok(bytes)
    /// }
    /// ```
    pub async fn run(self) -> Result<(), Error> {
        self.run_transport(StreamTransport).await
    }
}

impl NativeConnection {
    /// Polls native socket I/O concurrently with the client through shutdown.
    ///
    /// See [`connect_native`] for a complete request, body consumption, and
    /// shutdown example using `futures::join!`.
    pub async fn run(self) -> Result<(), Error> {
        let connection = self.0;
        let result = Box::pin(run_native(
            connection.stream.0,
            connection.config,
            false,
            &connection.requests.0,
            connection.shutdown,
            |_| std::future::ready(Err(Error::Application("client handler".into()))),
            connection.observation,
        ))
        .await;
        let _ = connection.done.send(result.clone());
        result
    }
}

impl<S> Connection<S> {
    async fn run_transport<T: Transport>(
        self,
        transport: impl FnOnce(Box<S>) -> T,
    ) -> Result<(), Error> {
        let result = Box::pin(run(
            transport(self.stream),
            self.config,
            false,
            &self.requests.0,
            self.shutdown,
            |_| std::future::ready(Err(Error::Application("client handler".into()))),
            self.observation,
        ))
        .await;
        let _ = self.done.send(result.clone());
        result
    }
}

/// Serves one established transport with a sequential asynchronous handler.
///
/// Await this future under the Kimojio runtime; it drives both I/O and the
/// handler. This example consumes a bounded request body before responding, so
/// ordinary keep-alive requests can reuse the connection.
///
/// ```
/// use kimojio::SplittableStream;
/// use kimojio_http1::{serve_connection, Config, Error, OutgoingBody, http::Response};
///
/// async fn serve<S: SplittableStream>(stream: S, config: Config) -> Result<(), Error> {
///     serve_connection(stream, config, |mut request| async move {
///         let _ = request.body_mut().collect(64 * 1024).await?;
///         Ok(Response::new(OutgoingBody::full(b"hello\n".to_vec())))
///     }).await
/// }
/// ```
pub fn serve_connection<S, H, F>(
    stream: S,
    config: Config,
    handler: H,
) -> impl Future<Output = Result<(), Error>>
where
    S: SplittableStream,
    H: FnMut(Request<IncomingBody>) -> F,
    F: Future<Output = Result<Response<OutgoingBody>, Error>> + 'static,
{
    serve_connection_with_shutdown(stream, config, Shutdown::default(), handler)
}

/// Serves a stream with externally controlled cooperative shutdown.
///
/// Poll the returned future to drive I/O. The handler is called sequentially;
/// each response is started after the previous handler has settled.
///
/// Clone the shutdown handle before moving it into the server. Keep polling the
/// serving future after signaling shutdown so active work and I/O can settle.
///
/// ```
/// use kimojio::SplittableStream;
/// use kimojio_http1::{
///     serve_connection_with_shutdown, Config, Error, OutgoingBody, Shutdown, http::Response,
/// };
///
/// async fn serve_until<S: SplittableStream>(
///     stream: S, config: Config, stop: impl std::future::Future<Output = ()>,
/// ) -> Result<(), Error> {
///     let control = Shutdown::default();
///     let server = serve_connection_with_shutdown(
///         stream, config, control.clone(), |mut request| async move {
///             let _ = request.body_mut().collect(64 * 1024).await?;
///             Ok(Response::new(OutgoingBody::full(b"hello\n".to_vec())))
///         },
///     );
///     let stop_server = async { stop.await; control.graceful(); };
///     // Stop waiting for the signal if the connection finishes first.
///     futures::pin_mut!(server, stop_server);
///     match futures::future::select(server, stop_server).await {
///         futures::future::Either::Left((result, _)) => result,
///         futures::future::Either::Right(((), server)) => server.await,
///     }
/// }
/// ```
pub fn serve_connection_with_shutdown<S, H, F>(
    stream: S,
    config: Config,
    shutdown: Shutdown,
    handler: H,
) -> impl Future<Output = Result<(), Error>>
where
    S: SplittableStream,
    H: FnMut(Request<IncomingBody>) -> F,
    F: Future<Output = Result<Response<OutgoingBody>, Error>> + 'static,
{
    serve_transport(StreamTransport(Box::new(stream)), config, shutdown, handler)
}

/// Serves an established socket through exact one-shot native operations.
///
/// This takes ownership of the socket; do not read or write it elsewhere. The
/// handler below echoes a bounded body using a fixed-length response.
///
/// ```
/// use kimojio_http1::{serve_connection_native, Config, Error, OutgoingBody, http::Response};
///
/// async fn echo(socket: kimojio::OwnedFd, config: Config) -> Result<(), Error> {
///     serve_connection_native(socket, config, |mut request| async move {
///         let bytes = request.body_mut().collect(16 * 1024).await?;
///         Ok(Response::new(OutgoingBody::full(bytes)))
///     }).await
/// }
/// ```
pub fn serve_connection_native<H, F>(
    fd: kimojio::OwnedFd,
    config: Config,
    handler: H,
) -> impl Future<Output = Result<(), Error>>
where
    H: FnMut(Request<IncomingBody>) -> F,
    F: Future<Output = Result<Response<OutgoingBody>, Error>> + 'static,
{
    serve_connection_native_with_shutdown(fd, config, Shutdown::default(), handler)
}

/// Serves a native socket and settles its operations before explicit close.
///
/// Use an application-owned [`Shutdown`] handle for cooperative stop requests.
/// For example, request graceful shutdown after handling a particular path:
///
/// ```
/// use kimojio_http1::{
///     serve_connection_native_with_shutdown, Config, Error, OutgoingBody, Shutdown,
///     http::Response,
/// };
///
/// async fn serve(socket: kimojio::OwnedFd, config: Config) -> Result<(), Error> {
///     let control = Shutdown::default();
///     serve_connection_native_with_shutdown(socket, config, control.clone(), move |mut request| {
///         let control = control.clone();
///         async move {
///             let _ = request.body_mut().collect(64 * 1024).await?;
///             if request.uri().path() == "/last" {
///                 control.graceful(); // Drain this response, then close.
///             }
///             Ok(Response::new(OutgoingBody::full(b"hello\n".to_vec())))
///         }
///     }).await
/// }
/// ```
pub fn serve_connection_native_with_shutdown<H, F>(
    fd: kimojio::OwnedFd,
    config: Config,
    shutdown: Shutdown,
    handler: H,
) -> impl Future<Output = Result<(), Error>>
where
    H: FnMut(Request<IncomingBody>) -> F,
    F: Future<Output = Result<Response<OutgoingBody>, Error>> + 'static,
{
    let observation = bind_observation(&config);
    async move {
        let (_keep_open, requests) = async_channel();
        Box::pin(run_native(
            fd,
            config,
            true,
            &requests,
            shutdown,
            handler,
            observation,
        ))
        .await
    }
}

fn serve_transport<T, H, F>(
    transport: T,
    config: Config,
    shutdown: Shutdown,
    handler: H,
) -> impl Future<Output = Result<(), Error>>
where
    T: Transport,
    H: FnMut(Request<IncomingBody>) -> F,
    F: Future<Output = Result<Response<OutgoingBody>, Error>> + 'static,
{
    let observation = bind_observation(&config);
    async move {
        let (_keep_open, requests) = async_channel();
        Box::pin(run(
            transport,
            config,
            true,
            &requests,
            shutdown,
            handler,
            observation,
        ))
        .await
    }
}

fn bind_observation(config: &Config) -> Result<Option<BoundObservation>, Error> {
    config
        .observation
        .as_ref()
        .map(crate::Observation::bind)
        .transpose()
}

enum Machine {
    Client(core::Client<Vec<u8>, OutgoingData>),
    Server(core::Server<Vec<u8>, OutgoingData>),
}

fn admit_eager(
    machine: &mut Machine,
    exchange: core::ExchangeId,
    body: &mut OutgoingBody,
    max_buffer: usize,
    coalesce_full_bodies: bool,
) -> bool {
    if !coalesce_full_bodies {
        return false;
    }
    let bytes = match &mut body.source {
        OutgoingSource::Ready(slot)
            if slot
                .as_ref()
                .is_some_and(|bytes| bytes.capacity() <= max_buffer) =>
        {
            OutgoingData::Owned(slot.take().unwrap())
        }
        OutgoingSource::Shared(slot)
            if slot.as_ref().is_some_and(|bytes| bytes.len() <= max_buffer) =>
        {
            OutgoingData::Shared(slot.take().unwrap())
        }
        _ => return false,
    };
    let len = bytes.as_ref().len();
    let command = core::SendBody {
        exchange,
        buffer: bytes,
        range: 0..len,
        end: true,
    };
    let result = match machine {
        Machine::Client(client) => client.send_body_eager(command),
        Machine::Server(server) => server.send_body_eager(command),
    };
    match result {
        Ok(_) => true,
        Err(rejected) => {
            body.source = match rejected.value.buffer {
                OutgoingData::Owned(bytes) => OutgoingSource::Ready(Some(bytes)),
                OutgoingData::Shared(bytes) => OutgoingSource::Shared(Some(bytes)),
                OutgoingData::Forward(_) => unreachable!("only full bodies use eager admission"),
            };
            false
        }
    }
}

fn source_admitted(
    result: Result<core::BodyId, core::Rejected<core::SendBody<OutgoingData>>>,
) -> Result<bool, Error> {
    match result {
        Ok(_) => Ok(true),
        // The rejected command owns its unadmitted buffer. No I/O can reference it.
        Err(rejected) if rejected.reason == core::RejectReason::InvalidState => Ok(false),
        Err(_) => Err(Error::Limit),
    }
}

enum Event {
    Read(core::ReadOp<Vec<u8>>),
    Write(core::WriteOp<OutgoingData>),
    Readiness(core::ReadinessOp),
    Cancel(core::CancelOp),
    Close(core::CloseOp),
    Body(core::BodyOp<Vec<u8>>),
    Trailers(core::ExchangeId, Result<HeaderMap, Error>),
    IncomingFinished(core::ExchangeId),
    SendReady(core::ExchangeId, usize),
    SourceFinished(core::ExchangeId),
    BodySent(core::BodySent<OutgoingData>),
    ExchangeFinished(core::ExchangeFinished),
    Deadline(Option<core::Deadline>),
    Upgrade,
    Closed(core::ConnectionResult),
    Request(core::ExchangeId, Result<Request<()>, Error>),
    Response(core::ExchangeId, Result<Response<()>, Error>, bool),
}

#[derive(Default)]
struct Ports {
    observation: Option<crate::Observation>,
    // Clock-aware driving makes deadline callbacks wake hints, not mandatory
    // scheduling boundaries. Leave room for one ordinary callback/quiescence.
    deadline_budget: usize,
    deferred_deadline: Option<Option<core::Deadline>>,
    deferred_count: usize,
}

impl core::Ports<Vec<u8>, OutgoingData> for Ports {
    type Output = Event;

    fn log(&mut self, connection: core::ConnectionId, now: core::Tick, event: core::LogEvent) {
        if let Some(observation) = &self.observation {
            observation.log(connection, now, event);
        }
    }

    fn read(&mut self, value: core::ReadOp<Vec<u8>>) -> Option<Event> {
        Some(Event::Read(value))
    }

    fn write(&mut self, value: core::WriteOp<OutgoingData>) -> Option<Event> {
        Some(Event::Write(value))
    }

    fn readiness(&mut self, value: core::ReadinessOp) -> Option<Event> {
        Some(Event::Readiness(value))
    }

    fn cancel(&mut self, value: core::CancelOp) -> Option<Event> {
        Some(Event::Cancel(value))
    }

    fn close(&mut self, value: core::CloseOp) -> Option<Event> {
        Some(Event::Close(value))
    }

    fn body(&mut self, value: core::BodyOp<Vec<u8>>) -> Option<Event> {
        Some(Event::Body(value))
    }

    fn incoming_finished(&mut self, value: core::ExchangeId) -> Option<Event> {
        Some(Event::IncomingFinished(value))
    }

    fn source_finished(&mut self, value: core::ExchangeId) -> Option<Event> {
        Some(Event::SourceFinished(value))
    }

    fn body_sent(&mut self, value: core::BodySent<OutgoingData>) -> Option<Event> {
        Some(Event::BodySent(value))
    }

    fn exchange_finished(&mut self, value: core::ExchangeFinished) -> Option<Event> {
        Some(Event::ExchangeFinished(value))
    }

    fn deadline_changed(&mut self, value: Option<core::Deadline>) -> Option<Event> {
        if self.deferred_count + 1 < self.deadline_budget {
            self.deferred_deadline = Some(value);
            self.deferred_count += 1;
            None
        } else {
            Some(Event::Deadline(value))
        }
    }

    fn closed(&mut self, value: core::ConnectionResult) -> Option<Event> {
        Some(Event::Closed(value))
    }

    fn trailers(
        &mut self,
        exchange: core::ExchangeId,
        headers: core::Headers<'_>,
    ) -> Option<Event> {
        Some(Event::Trailers(exchange, metadata::headers(headers)))
    }

    fn send_ready(&mut self, exchange: core::ExchangeId, capacity: usize) -> Option<Event> {
        Some(Event::SendReady(exchange, capacity))
    }

    fn upgrade_ready(&mut self, _: core::ExchangeId) -> Option<Event> {
        Some(Event::Upgrade)
    }
}

impl core::ServerPorts<Vec<u8>, OutgoingData> for Ports {
    fn request(
        &mut self,
        exchange: core::ExchangeId,
        head: core::RequestHead<'_>,
    ) -> Option<Event> {
        Some(Event::Request(exchange, metadata::request(head, ())))
    }
}

impl core::ClientPorts<Vec<u8>, OutgoingData> for Ports {
    fn response(
        &mut self,
        exchange: core::ExchangeId,
        head: core::ResponseHead<'_>,
        informational: bool,
    ) -> Option<Event> {
        Some(Event::Response(
            exchange,
            metadata::response(head, ()),
            informational,
        ))
    }
}

struct Active {
    id: core::ExchangeId,
    cancel: Rc<CancellationToken>,
    cancellation_applied: bool,
    cancel_wait: crate::receive_lane::CancelLane,
    duplex: bool,
    incoming_lease: bool,
    abandonment_applied: bool,
    credit_started: bool,
    source: Option<OutgoingBody>,
    capacity: usize,
    source_ready: bool,
    data: Option<Sender<BodyChunk>>,
    terminal: Option<SenderOneshot<Result<HeaderMap, Error>>>,
    incoming: Option<IncomingBody>,
    incoming_finished: Rc<Cell<bool>>,
    trailers: HeaderMap,
    response: Option<SenderOneshot<Result<Response<IncomingBody>, Error>>>,
    handler: Option<LocalBoxFuture<'static, Result<Response<OutgoingBody>, Error>>>,
}

impl Active {
    fn new(
        id: core::ExchangeId,
        cancel: Rc<CancellationToken>,
        demand: Sender<BodyDemand>,
    ) -> Self {
        let (send, data) = async_channel();
        let (terminal, result) = oneshot();
        let incoming_finished = Rc::new(Cell::new(false));
        let incoming = IncomingBody::new(
            data,
            result,
            incoming_finished.clone(),
            cancel.clone(),
            demand,
            id,
        );
        Self {
            id,
            cancel_wait: crate::receive_lane::CancelLane::new(cancel.clone()),
            cancel,
            cancellation_applied: false,
            duplex: false,
            incoming_lease: false,
            abandonment_applied: false,
            credit_started: false,
            source: None,
            capacity: 0,
            source_ready: false,
            data: Some(send),
            terminal: Some(terminal),
            incoming: Some(incoming),
            incoming_finished,
            trailers: HeaderMap::new(),
            response: None,
            handler: None,
        }
    }

    fn finish_incoming(&mut self, result: Result<(), Error>) {
        self.incoming_finished.set(true);
        if let Some(terminal) = self.terminal.take() {
            let _ = terminal.send(result.map(|()| std::mem::take(&mut self.trailers)));
        }
        self.data.take();
    }
}

enum Input {
    Read(core::ReadCompletion<Vec<u8>>),
    Write(WriteResult),
    Release(core::BodyCompletion<Vec<u8>>),
    Demand(BodyDemand),
    Request(Result<SendRequest, kimojio::ChannelError>),
    Source(Option<Result<SourceFrame, Error>>),
    Handler(Result<Response<OutgoingBody>, Error>),
    Timer(Result<(), kimojio::Errno>),
    Wake,
    #[cfg(feature = "metrics")]
    Snapshot(crate::observation::SnapshotRequest),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShutdownProgress {
    Running,
    Draining,
    Aborting,
}

impl ShutdownProgress {
    fn advance(&mut self, abort: bool, graceful: bool) -> Option<core::ShutdownMode> {
        match (*self, abort, graceful) {
            (Self::Running | Self::Draining, true, _) => {
                *self = Self::Aborting;
                Some(core::ShutdownMode::Abort)
            }
            (Self::Running, false, true) => {
                *self = Self::Draining;
                Some(core::ShutdownMode::Graceful)
            }
            _ => None,
        }
    }
}

struct State {
    machine: Machine,
    active: Option<Active>,
    release_send: Sender<core::BodyCompletion<Vec<u8>>>,
    released: crate::receive_lane::ReceiveLane<core::BodyCompletion<Vec<u8>>>,
    demand_send: Sender<BodyDemand>,
    demands: DemandQueue,
    pending_read: Option<core::OperationId>,
    pending_write: Option<core::OperationId>,
    pending_close: Option<core::CloseOp>,
    deadline: Option<core::Deadline>,
    uses_deadlines: bool,
    timer: crate::timer::DeadlineTimer,
    epoch: Instant,
    shutdown: Shutdown,
    shutdown_progress: ShutdownProgress,
    graceful_wait: crate::receive_lane::CancelLane,
    abort_wait: crate::receive_lane::CancelLane,
    server: bool,
    max_buffer: usize,
    receive_capacity: usize,
    max_headers: usize,
    coalesce_full_bodies: bool,
    rotation: usize,
    observation: Option<BoundObservation>,
}

impl State {
    #[cfg(test)]
    fn new(config: Config, server: bool, shutdown: Shutdown) -> Result<Self, Error> {
        let observation = bind_observation(&config)?;
        Self::new_bound(config, server, shutdown, observation)
    }

    fn new_bound(
        config: Config,
        server: bool,
        shutdown: Shutdown,
        observation: Option<BoundObservation>,
    ) -> Result<Self, Error> {
        let buffer = vec![0; config.protocol.max_buffer_bytes];
        let receive_capacity = buffer.capacity();
        let max_buffer = config.protocol.max_buffer_bytes;
        let max_headers = config.protocol.max_headers;
        let uses_deadlines = [
            config.protocol.head_timeout_ns,
            config.protocol.body_timeout_ns,
            config.protocol.idle_timeout_ns,
            config.protocol.continue_timeout_ns,
        ]
        .iter()
        .any(Option::is_some);
        let machine = if server {
            Machine::Server(core::Server::with_output_type(
                config.connection_id,
                config.protocol,
                buffer,
                core::Tick(0),
            )?)
        } else {
            Machine::Client(core::Client::with_output_type(
                config.connection_id,
                config.protocol,
                buffer,
                core::Tick(0),
            )?)
        };
        let (release_send, released) = async_channel();
        let (demand_send, demands) = async_channel();
        Ok(Self {
            machine,
            active: None,
            release_send,
            released: crate::receive_lane::ReceiveLane::new(released),
            demand_send,
            demands: DemandQueue(crate::receive_lane::ReceiveLane::new(demands)),
            pending_read: None,
            pending_write: None,
            pending_close: None,
            deadline: None,
            uses_deadlines,
            timer: crate::timer::DeadlineTimer::default(),
            epoch: kimojio::clock_now(),
            graceful_wait: crate::receive_lane::CancelLane::new(shutdown.graceful.clone()),
            abort_wait: crate::receive_lane::CancelLane::new(shutdown.abort.clone()),
            shutdown,
            shutdown_progress: ShutdownProgress::Running,
            server,
            max_buffer,
            receive_capacity,
            max_headers,
            coalesce_full_bodies: config.coalesce_full_bodies,
            rotation: 0,
            observation,
        })
    }

    #[cfg(feature = "metrics")]
    fn metrics(&self) -> core::MetricsSnapshot {
        match &self.machine {
            Machine::Client(inner) => inner.metrics(),
            Machine::Server(inner) => inner.metrics(),
        }
    }

    fn current_tick(&self) -> core::Tick {
        let nanos = kimojio::clock_now()
            .saturating_duration_since(self.epoch)
            .as_nanos();
        core::Tick(u64::try_from(nanos).unwrap_or(u64::MAX))
    }

    fn observe_time(&mut self) -> Result<core::Tick, Error> {
        if !self.uses_deadlines && self.observation.is_none() {
            // No policy or observer can consume a timestamp on this connection.
            return Ok(core::Tick(0));
        }
        let now = self.current_tick();
        match &mut self.machine {
            Machine::Client(inner) => inner.observe_time(now),
            Machine::Server(inner) => inner.observe_time(now),
        }?;
        Ok(now)
    }

    fn observe(&mut self) -> Result<core::Tick, Error> {
        // Preserve expiry-before-shutdown/cancellation ordering at drive entry.
        // Completion processing below deliberately uses observe_time instead:
        // an accepted completion can refresh a deadline before the next drive.
        let now = if self.uses_deadlines {
            let now = self.current_tick();
            match &mut self.machine {
                Machine::Client(inner) => inner.advance_time(now),
                Machine::Server(inner) => inner.advance_time(now),
            }?;
            now
        } else {
            self.observe_time()?
        };
        if let Some(mode) = self.shutdown_progress.advance(
            self.shutdown.abort.is_cancelled(),
            self.shutdown.graceful.is_cancelled(),
        ) {
            self.graceful_wait.clear();
            if mode == core::ShutdownMode::Abort {
                self.abort_wait.clear();
            }
            match &mut self.machine {
                Machine::Client(inner) => inner.shutdown(mode),
                Machine::Server(inner) => inner.shutdown(mode),
            }
        }
        if let Some(active) = self.active.as_mut()
            && active.cancel.is_cancelled()
            && !active.cancellation_applied
        {
            active.cancellation_applied = true;
            active.cancel_wait.clear();
            active.data.take();
            if !self.server {
                let _ = match &mut self.machine {
                    Machine::Client(inner) => inner.cancel_exchange(active.id),
                    Machine::Server(inner) => inner.cancel_exchange(active.id),
                };
            }
        }
        Ok(now)
    }

    fn fail(&mut self, error: Error) {
        if let Some(active) = self.active.as_mut() {
            active.source.take();
            active.handler.take();
            active.finish_incoming(Err(error.clone()));
            if let Some(response) = active.response.take() {
                let _ = response.send(Err(error));
            }
            let _ = match &mut self.machine {
                Machine::Client(inner) => inner.fail_source(active.id, core::Failure::Application),
                Machine::Server(inner) => inner.fail_source(active.id, core::Failure::Application),
            };
        } else {
            match &mut self.machine {
                Machine::Client(inner) => inner.shutdown(core::ShutdownMode::Abort),
                Machine::Server(inner) => inner.shutdown(core::ShutdownMode::Abort),
            }
        }
    }

    fn cancel_abandoned_duplex(&mut self) -> bool {
        let Some(active) = self.active.as_mut() else {
            return false;
        };
        if active.duplex
            && active.cancellation_applied
            && !active.abandonment_applied
            && !active.incoming_lease
            && !active.incoming_finished.get()
        {
            active.abandonment_applied = true;
            if let Machine::Server(server) = &mut self.machine {
                let _ = server.cancel_exchange(active.id);
            }
            return true;
        }
        false
    }

    fn start_request(&mut self, command: SendRequest) {
        if command.cancel.is_cancelled() {
            return;
        }
        let (mut parts, mut body) = command.request.into_parts();
        let result = (|| {
            let Machine::Client(client) = &mut self.machine else {
                return Err(Error::Closed);
            };
            if body.continue_request {
                return Err(Error::InvalidMetadata);
            }
            if parts.headers.len() > self.max_headers {
                return Err(Error::Limit);
            }
            if parts.headers.get_all(http::header::EXPECT).iter().count() > 1 {
                return Err(Error::InvalidMetadata);
            }
            let expect_continue = match parts.headers.remove(http::header::EXPECT) {
                Some(value) if value.as_bytes().eq_ignore_ascii_case(b"100-continue") => true,
                Some(_) => return Err(Error::InvalidMetadata),
                None => false,
            };
            let target = parts.uri.to_string();
            let headers = metadata::borrowed_headers(&parts.headers);
            let id = client.request(core::Request {
                head: core::RequestHead {
                    method: parts.method.as_str(),
                    target: &target,
                    version: metadata::version(parts.version)?,
                    headers: &headers,
                },
                body: body.length,
                expect_continue,
            })?;
            Ok(id)
        })();
        match result {
            Ok(id) => {
                let mut active = Active::new(id, command.cancel, self.demand_send.clone());
                if !admit_eager(
                    &mut self.machine,
                    id,
                    &mut body,
                    self.max_buffer,
                    self.coalesce_full_bodies,
                ) {
                    active.source = Some(body);
                }
                active.response = Some(command.response);
                self.active = Some(active);
            }
            Err(error) => {
                let _ = command.response.send(Err(error));
            }
        }
    }

    fn respond(&mut self, response: Response<OutgoingBody>) -> Result<(), Error> {
        let active = self.active.as_mut().ok_or(Error::Closed)?;
        let (parts, mut body) = response.into_parts();
        if parts.headers.len() > self.max_headers {
            return Err(Error::Limit);
        }
        let headers = metadata::borrowed_headers(&parts.headers);
        let Machine::Server(server) = &mut self.machine else {
            return Err(Error::Closed);
        };
        let response = core::Response::new(
            parts.status.as_u16(),
            parts.status.canonical_reason().unwrap_or(""),
            &headers,
            body.length,
        );
        if body.continue_request {
            server.respond_duplex(active.id, response)?;
            active.duplex = true;
        } else {
            server.respond(active.id, response)?;
        }
        if !admit_eager(
            &mut self.machine,
            active.id,
            &mut body,
            self.max_buffer,
            self.coalesce_full_bodies,
        ) {
            active.source = Some(body);
        }
        Ok(())
    }

    fn source(&mut self, frame: Option<Result<SourceFrame, Error>>) -> Result<(), Error> {
        let active = self.active.as_mut().ok_or(Error::Closed)?;
        let capacity = active.capacity;
        active.capacity = 0;
        active.source_ready = false;
        match frame.transpose()? {
            Some(SourceFrame::Data(buffer)) => {
                if buffer.retained_capacity() > self.max_buffer || buffer.as_ref().len() > capacity
                {
                    return Err(Error::Limit);
                }
                if buffer.as_ref().is_empty() {
                    active.capacity = capacity;
                    active.source_ready = true;
                    return Ok(());
                }
                let len = buffer.as_ref().len();
                let command = core::SendBody {
                    exchange: active.id,
                    buffer,
                    range: 0..len,
                    end: false,
                };
                let admitted = source_admitted(match &mut self.machine {
                    Machine::Client(inner) => inner.send_body(command),
                    Machine::Server(inner) => inner.send_body(command),
                })?;
                if !admitted {
                    active.source.take();
                }
            }
            trailers => {
                active.source.take();
                let trailers = match trailers {
                    Some(SourceFrame::Trailers(trailers)) => trailers,
                    None => HeaderMap::new(),
                    Some(SourceFrame::Data(_)) => unreachable!(),
                };
                if trailers.len() > self.max_headers {
                    return Err(Error::Limit);
                }
                let headers = metadata::borrowed_headers(&trailers);
                match &mut self.machine {
                    Machine::Client(inner) => inner.finish_body(active.id, &headers),
                    Machine::Server(inner) => inner.finish_body(active.id, &headers),
                }?;
            }
        }
        Ok(())
    }

    fn input(&mut self, input: Input, io: &mut impl IoDriver) -> Result<(), Error> {
        match input {
            Input::Read(completion) => {
                self.pending_read.take();
                match &mut self.machine {
                    Machine::Client(inner) => inner.complete_read(completion),
                    Machine::Server(inner) => inner.complete_read(completion),
                }
                .map_err(|_| Error::Closed)?;
            }
            Input::Write(WriteResult::Write(completion)) => {
                self.pending_write.take();
                match &mut self.machine {
                    Machine::Client(inner) => inner.complete_write(completion),
                    Machine::Server(inner) => inner.complete_write(completion),
                }
                .map_err(|_| Error::Closed)?;
            }
            Input::Write(WriteResult::Close(completion)) => {
                match &mut self.machine {
                    Machine::Client(inner) => inner.complete_close(completion),
                    Machine::Server(inner) => inner.complete_close(completion),
                }
                .map_err(|_| Error::Closed)?;
            }
            Input::Release(completion) => {
                let (op, consumed) = completion.into_parts();
                let id = op.exchange();
                let result = match &mut self.machine {
                    Machine::Client(inner) => inner.release_body(op.release(consumed)),
                    Machine::Server(inner) => inner.release_body(op.release(consumed)),
                };
                if result.is_ok() {
                    let mut abandoned = false;
                    if let Some(active) = &mut self.active
                        && active.id == id
                    {
                        active.incoming_lease = false;
                        abandoned = active.duplex && active.cancel.is_cancelled();
                    }
                    if !abandoned {
                        let _ = match &mut self.machine {
                            Machine::Client(inner) => inner.grant_body_credit(id, consumed),
                            Machine::Server(inner) => inner.grant_body_credit(id, consumed),
                        };
                    }
                }
            }
            Input::Demand(demand) => {
                let result = match self
                    .active
                    .as_mut()
                    .filter(|active| active.id == demand.exchange)
                {
                    Some(active) if !active.credit_started && !active.incoming_finished.get() => {
                        let result = match &mut self.machine {
                            Machine::Client(inner) => {
                                inner.grant_body_credit(demand.exchange, self.max_buffer)
                            }
                            Machine::Server(inner) => {
                                inner.grant_body_credit(demand.exchange, self.max_buffer)
                            }
                        }
                        .map_err(Error::from);
                        active.credit_started = result.is_ok();
                        result
                    }
                    Some(_) => Ok(()),
                    None => Err(Error::Closed),
                };
                let _ = demand.accepted.send(result);
            }
            Input::Request(Ok(command)) => self.start_request(command),
            Input::Request(Err(_)) => self.shutdown.graceful(),
            Input::Source(frame) => self.source(frame)?,
            Input::Handler(response) => {
                if let Some(active) = &mut self.active {
                    active.handler.take();
                }
                self.respond(response?)?;
            }
            Input::Timer(result) => result.map_err(Error::Transport)?,
            Input::Wake => {}
            #[cfg(feature = "metrics")]
            Input::Snapshot(reply) => {
                let _ = reply.send(Ok(self.metrics()));
            }
        }
        if self.pending_read.is_none()
            && self.pending_write.is_none()
            && let Some(close) = self.pending_close.take()
        {
            io.close(close)?;
        }
        Ok(())
    }

    fn event<H, F>(
        &mut self,
        event: Event,
        handler: &mut H,
        io: &mut impl IoDriver,
    ) -> Result<Option<Result<(), Error>>, Error>
    where
        H: FnMut(Request<IncomingBody>) -> F,
        F: Future<Output = Result<Response<OutgoingBody>, Error>> + 'static,
    {
        match event {
            Event::Read(op) => {
                self.pending_read = Some(op.id());
                io.read(op)?;
            }
            Event::Write(op) => {
                self.pending_write = Some(op.id());
                io.write(op)?;
            }
            Event::Readiness(op) => {
                let completion = op.complete(Err(core::IoError {
                    kind: core::IoErrorKind::Other,
                    code: None,
                }));
                match &mut self.machine {
                    Machine::Client(inner) => inner.complete_readiness(completion),
                    Machine::Server(inner) => inner.complete_readiness(completion),
                }
                .map_err(|_| Error::Closed)?;
            }
            Event::Cancel(op) => {
                if self.pending_read == Some(op.target) {
                    io.cancel_read();
                }
                if self.pending_write == Some(op.target) {
                    io.cancel_write();
                }
            }
            Event::Close(op) => {
                self.pending_close = Some(op);
                self.input(Input::Wake, io)?;
            }
            Event::Body(op) => {
                let active = self.active.as_mut().ok_or(Error::Closed)?;
                active.incoming_lease = true;
                if active.duplex && active.cancel.is_cancelled() {
                    // A new delivery proves that the abandoned request was not complete.
                    active.abandonment_applied = true;
                    active.data.take();
                    if let Machine::Server(server) = &mut self.machine {
                        let _ = server.cancel_exchange(active.id);
                    }
                }
                let chunk = BodyChunk {
                    op: Some(op),
                    release: self.release_send.clone(),
                    retained_capacity: self.receive_capacity,
                };
                if let Some(data) = &active.data {
                    let _ = data.try_send(chunk);
                }
            }
            Event::Trailers(id, trailers) => {
                if let Some(active) = &mut self.active
                    && active.id == id
                {
                    active.trailers = trailers?;
                }
            }
            Event::IncomingFinished(id) => {
                if let Some(active) = &mut self.active
                    && active.id == id
                {
                    active.finish_incoming(Ok(()));
                }
            }
            Event::SendReady(id, capacity) => {
                if let Some(active) = &mut self.active
                    && active.id == id
                {
                    active.capacity = capacity;
                    active.source_ready = true;
                }
            }
            Event::SourceFinished(id) => {
                if let Some(active) = &mut self.active
                    && active.id == id
                {
                    active.source.take();
                    active.source_ready = false;
                    active.capacity = 0;
                }
            }
            Event::BodySent(sent) => {
                if let Err(failure) = sent.result
                    && let Some(active) = &mut self.active
                    && active.id == sent.exchange
                {
                    active.source.take();
                    if let Some(response) = active.response.take() {
                        let _ = response.send(Err(Error::Protocol(failure)));
                    }
                }
            }
            Event::ExchangeFinished(finished) => {
                if let Some(mut active) = self.active.take() {
                    if !active.incoming_finished.get() {
                        let error = finished
                            .result
                            .err()
                            .map_or(Error::Cancelled, Error::Protocol);
                        active.finish_incoming(Err(error));
                    }
                    if let Some(response) = active.response.take() {
                        let error = finished.result.err().map_or(Error::Closed, Error::Protocol);
                        let _ = response.send(Err(error));
                    }
                }
            }
            Event::Deadline(deadline) => {
                if self.deadline != deadline {
                    self.timer.set(match deadline {
                        Some(deadline) => {
                            let at = self
                                .epoch
                                .checked_add(Duration::from_nanos(deadline.at.0))
                                .ok_or(Error::Limit)?;
                            Some(at)
                        }
                        None => None,
                    });
                    self.deadline = deadline;
                }
            }
            Event::Upgrade => return Err(Error::Application("HTTP upgrade is not exposed".into())),
            Event::Closed(result) => {
                #[cfg(feature = "metrics")]
                if let Some(observation) = &self.observation {
                    observation.finish(self.metrics());
                }
                return Ok(Some(result.map_err(Error::Protocol)));
            }
            Event::Request(id, request) => {
                let mut active = Active::new(
                    id,
                    Rc::new(CancellationToken::new()),
                    self.demand_send.clone(),
                );
                let request = request?.map(|()| active.incoming.take().expect("new request body"));
                active.handler = Some(handler(request).boxed_local());
                self.active = Some(active);
            }
            Event::Response(id, response, informational) => {
                if !informational
                    && let Some(active) = &mut self.active
                    && active.id == id
                {
                    let response =
                        response?.map(|()| active.incoming.take().expect("first final head"));
                    if let Some(send) = active.response.take() {
                        let _ = send.send(Ok(response));
                    }
                }
            }
        }
        Ok(None)
    }
}

async fn run<T, H, F>(
    transport: T,
    config: Config,
    server: bool,
    requests: &Receiver<SendRequest>,
    shutdown: Shutdown,
    mut handler: H,
    observation: Result<Option<BoundObservation>, Error>,
) -> Result<(), Error>
where
    T: Transport,
    H: FnMut(Request<IncomingBody>) -> F,
    F: Future<Output = Result<Response<OutgoingBody>, Error>> + 'static,
{
    let budget = config.turn_budget.max(1);
    let mut state = State::new_bound(config, server, shutdown, observation?)?;
    let (reader, writer) = Box::pin(transport.split())
        .await
        .map_err(Error::Transport)?;
    state.epoch = kimojio::clock_now();
    let (read_send, reads) = async_channel();
    let (read_complete, read_done) = async_channel();
    let (write_send, writes) = async_channel();
    let (write_complete, write_done) = async_channel();
    let io = WorkerIo {
        read_send: Some(read_send),
        write_send,
        read_done,
        write_done,
        read_cancel: None,
        write_cancel: None,
    };
    let (result, (), ()) = futures::join!(
        drive(state, io, requests, &mut handler, budget),
        Box::pin(io::read_worker(reader, reads, read_complete)),
        Box::pin(io::write_worker(writer, writes, write_complete)),
    );
    result
}

async fn run_native<H, F>(
    fd: kimojio::OwnedFd,
    config: Config,
    server: bool,
    requests: &Receiver<SendRequest>,
    shutdown: Shutdown,
    mut handler: H,
    observation: Result<Option<BoundObservation>, Error>,
) -> Result<(), Error>
where
    H: FnMut(Request<IncomingBody>) -> F,
    F: Future<Output = Result<Response<OutgoingBody>, Error>> + 'static,
{
    let budget = config.turn_budget.max(1);
    let mut state = State::new_bound(config, server, shutdown, observation?)?;
    let io = native_io(fd);
    state.epoch = kimojio::clock_now();
    drive(state, io, requests, &mut handler, budget).await
}

async fn drive<H, F, I: IoDriver>(
    mut state: State,
    mut io: I,
    requests: &Receiver<SendRequest>,
    handler: &mut H,
    budget: usize,
) -> Result<(), Error>
where
    H: FnMut(Request<IncomingBody>) -> F,
    F: Future<Output = Result<Response<OutgoingBody>, Error>> + 'static,
{
    let mut turns = 0usize;
    let mut ports = Ports {
        observation: state
            .observation
            .as_ref()
            .map(|bound| bound.observation.clone()),
        ..Ports::default()
    };
    // A deferred hint must be convertible before an owned I/O event is issued.
    // If this unusual clock epoch cannot represent every core Tick, keep the
    // original yielding callback path so conversion errors precede issuance.
    let can_defer = state
        .epoch
        .checked_add(Duration::from_nanos(u64::MAX))
        .is_some();
    loop {
        ports.deadline_budget = if can_defer { budget - turns } else { 0 };
        let event = match state.observe() {
            Ok(now) if state.uses_deadlines => match &mut state.machine {
                Machine::Client(inner) => inner.next_at(now, &mut ports),
                Machine::Server(inner) => inner.next_at(now, &mut ports),
            },
            Ok(_) => Ok(match &mut state.machine {
                Machine::Client(inner) => inner.next(&mut ports),
                Machine::Server(inner) => inner.next(&mut ports),
            }),
            Err(error) => {
                // A bad clock must not strand an outstanding owned operation.
                state.fail(error);
                Ok(match &mut state.machine {
                    Machine::Client(inner) => inner.next(&mut ports),
                    Machine::Server(inner) => inner.next(&mut ports),
                })
            }
        }
        .expect("drive uses the time just accepted by advance_time");
        let deferred_count = std::mem::take(&mut ports.deferred_count);
        if let Some(deadline) = ports.deferred_deadline.take() {
            state
                .event(Event::Deadline(deadline), handler, &mut io)
                .expect("prechecked epoch can represent every deferred deadline");
        }
        let mut runnable = event.is_some() || deferred_count != 0;
        if let Some(event) = event {
            match state.event(event, handler, &mut io) {
                Ok(Some(result)) => return result,
                Ok(None) => {}
                Err(error) => state.fail(error),
            }
        } else if state.cancel_abandoned_duplex() {
            // Source termination can drop the consumer before its last lease
            // returns. Only a drained core can distinguish that from abandonment.
            runnable = true;
        }
        turns += 1 + deferred_count;
        if turns >= budget {
            turns = 0;
            operations::yield_cpu().await;
        }
        if let Some(input) = next_input(&mut state, &mut io, requests, runnable).await {
            if let Err(error) = state.observe_time() {
                state.fail(error);
            }
            if let Err(error) = state.input(input, &mut io) {
                state.fail(error);
            }
        }
    }
}

async fn next_input(
    state: &mut State,
    io: &mut impl IoDriver,
    requests: &Receiver<SendRequest>,
    runnable: bool,
) -> Option<Input> {
    let (read, write) = io.completions(runnable);
    let request = requests.recv();
    #[cfg(feature = "metrics")]
    let observation = state.observation.as_ref();
    #[cfg(feature = "metrics")]
    let snapshot = async {
        if let Some(observation) = observation {
            let mut wait = std::pin::pin!(observation.requests.recv());
            if let Ok(reply) = futures::future::poll_fn(|cx| {
                poll_receive(&observation.requests, wait.as_mut(), cx, runnable)
            })
            .await
            {
                return reply;
            }
        }
        std::future::pending().await
    };
    #[cfg(feature = "metrics")]
    futures::pin_mut!(snapshot);
    futures::pin_mut!(read, write, request);
    futures::future::poll_fn(|cx| {
        const LANES: usize = 10 + cfg!(feature = "metrics") as usize;
        for offset in 0..LANES {
            let index = (state.rotation + offset) % LANES;
            let ready = match index {
                0 => read.as_mut().poll(cx).map(|r| Some(Input::Read(r))),
                1 => write.as_mut().poll(cx).map(|r| Some(Input::Write(r))),
                2 => state
                    .released
                    .poll(cx, runnable)
                    .map(|r| r.ok().map(Input::Release)),
                3 if !state.server
                    && state.active.is_none()
                    && state.shutdown_progress == ShutdownProgress::Running =>
                {
                    poll_receive(requests, request.as_mut(), cx, runnable)
                        .map(|r| Some(Input::Request(r)))
                }
                // Drain notifications that can revoke previously advertised capacity.
                4 if !runnable => poll_source(&mut state.active, cx),
                5 => poll_handler(&mut state.active, cx),
                6 if state.shutdown_progress == ShutdownProgress::Running
                    && (!runnable || state.shutdown.graceful.is_cancelled()) =>
                {
                    state.graceful_wait.poll(cx).map(|_| Some(Input::Wake))
                }
                7 if state.shutdown_progress != ShutdownProgress::Aborting
                    && (!runnable || state.shutdown.abort.is_cancelled()) =>
                {
                    state.abort_wait.poll(cx).map(|_| Some(Input::Wake))
                }
                8 => state
                    .timer
                    .poll(cx)
                    .map(|result| Some(Input::Timer(result))),
                9 => state
                    .demands
                    .0
                    .poll(cx, runnable)
                    .map(|r| r.ok().map(Input::Demand)),
                #[cfg(feature = "metrics")]
                10 => snapshot
                    .as_mut()
                    .poll(cx)
                    .map(|reply| Some(Input::Snapshot(reply))),
                _ => Poll::Pending,
            };
            if let Poll::Ready(Some(input)) = ready {
                state.rotation = (index + 1) % LANES;
                return Poll::Ready(Some(input));
            }
        }
        if let Some(active) = &mut state.active
            && !active.cancellation_applied
            && (!runnable || active.cancel.is_cancelled())
            && active.cancel_wait.poll(cx).is_ready()
        {
            return Poll::Ready(Some(Input::Wake));
        }
        if runnable {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    })
    .await
}

fn poll_source(active: &mut Option<Active>, cx: &mut Context<'_>) -> Poll<Option<Input>> {
    if let Some(active) = active
        && active.source_ready
        && let Some(body) = &mut active.source
    {
        return body
            .source
            .poll_next_unpin(cx)
            .map(|frame| Some(Input::Source(frame)));
    }
    Poll::Pending
}

fn poll_handler(active: &mut Option<Active>, cx: &mut Context<'_>) -> Poll<Option<Input>> {
    if let Some(active) = active
        && let Some(handler) = &mut active.handler
    {
        return Pin::new(handler)
            .poll(cx)
            .map(|response| Some(Input::Handler(response)));
    }
    Poll::Pending
}

#[cfg(test)]
#[path = "shared_body_tests.rs"]
mod shared_body_tests;

#[cfg(test)]
#[path = "architecture_tests.rs"]
mod architecture_tests;

#[cfg(test)]
#[path = "forwarding_tests.rs"]
mod forwarding_tests;

#[cfg(test)]
#[path = "coalescing_tests.rs"]
mod coalescing_tests;

#[cfg(test)]
#[path = "combined_tests.rs"]
mod combined_tests;

#[cfg(test)]
#[path = "observation_tests.rs"]
mod observation_tests;

#[cfg(test)]
mod tests {
    use super::*;

    async fn move_and_replace_deadline() {
        let mut state = State::new(
            Config::new(core::ConnectionId {
                slot: 1,
                generation: 1,
            }),
            false,
            Shutdown::default(),
        )
        .unwrap();
        state
            .timer
            .set(Some(kimojio::clock_now() + Duration::from_secs(60)));
        assert!(
            futures::future::poll_fn(|cx| Poll::Ready(state.timer.poll(cx)))
                .await
                .is_pending()
        );
        let mut moved = std::hint::black_box(state);
        moved.timer.set(Some(kimojio::clock_now()));
        futures::future::poll_fn(|cx| moved.timer.poll(cx))
            .await
            .unwrap();
        moved.timer.set(None);
    }

    #[kimojio::test]
    async fn inline_deadline_can_move_after_poll_and_be_replaced() {
        move_and_replace_deadline().await;
    }

    #[cfg(feature = "virtual-clock")]
    #[kimojio::test]
    async fn inline_virtual_deadline_can_move_after_poll_and_be_replaced() {
        operations::virtual_clock_enable(true);
        move_and_replace_deadline().await;
        assert_eq!(operations::virtual_clock_pending_timers(), 0);
    }

    #[test]
    fn shutdown_progress_is_monotonic_for_all_five_turn_histories() {
        for history in 0..1024 {
            let mut progress = ShutdownProgress::Running;
            let mut rank = 0;
            for turn in 0..5 {
                let signals = (history >> (turn * 2)) & 3;
                let abort = signals & 1 != 0;
                let graceful = signals & 2 != 0;
                let requested = if abort {
                    2
                } else if graceful {
                    1
                } else {
                    0
                };
                let expected = if requested > rank {
                    Some(if abort {
                        core::ShutdownMode::Abort
                    } else {
                        core::ShutdownMode::Graceful
                    })
                } else {
                    None
                };
                rank = rank.max(requested);
                assert_eq!(progress.advance(abort, graceful), expected);
                assert_eq!(
                    progress,
                    [
                        ShutdownProgress::Running,
                        ShutdownProgress::Draining,
                        ShutdownProgress::Aborting,
                    ][rank]
                );
            }
        }
    }

    pub(super) fn full_body_client(expect_continue: bool) -> (Machine, core::ExchangeId) {
        let mut client = core::Client::with_output_type(
            core::ConnectionId {
                slot: 96,
                generation: 1,
            },
            core::Config::default(),
            vec![0; 1024],
            core::Tick(0),
        )
        .unwrap();
        let id = client
            .request(core::Request {
                head: core::RequestHead {
                    method: "POST",
                    target: "/",
                    version: core::Version::Http11,
                    headers: &[core::Header {
                        name: "host",
                        value: b"test",
                    }],
                },
                body: core::BodyLength::Known(3),
                expect_continue,
            })
            .unwrap();
        (Machine::Client(client), id)
    }

    #[test]
    fn full_body_eager_path_uses_one_write_and_no_producer_demand() {
        let (mut machine, id) = full_body_client(false);
        let bytes = b"abc".to_vec();
        let pointer = bytes.as_ptr();
        let mut body = OutgoingBody::full(bytes);
        assert!(admit_eager(&mut machine, id, &mut body, 1024, true));
        assert!(matches!(body.source, OutgoingSource::Ready(None)));
        let Machine::Client(mut client) = machine else {
            unreachable!()
        };
        let mut writes = 0;
        let mut source_finished = false;
        loop {
            match client.next(&mut Ports::default()) {
                Some(Event::Deadline(_)) => {}
                Some(Event::SourceFinished(exchange)) => {
                    assert_eq!(exchange, id);
                    source_finished = true;
                }
                Some(Event::Write(op)) => {
                    assert!(source_finished);
                    writes += 1;
                    assert_eq!(op.slices()[1].as_ptr(), pointer);
                    assert_eq!(op.slices()[1], b"abc");
                    let len = op.slices().iter().map(|slice| slice.len()).sum();
                    client.complete_write(op.complete(Ok(len))).unwrap();
                }
                Some(Event::BodySent(receipt)) => {
                    assert_eq!(receipt.accepted, 3);
                    assert_eq!(receipt.result, Ok(()));
                    break;
                }
                _ => panic!("eager full body must not require source demand or polling"),
            }
        }
        assert_eq!(writes, 1);
    }

    #[test]
    fn eager_fallback_retains_full_storage_and_never_polls_custom_streams() {
        for (expect, capacity) in [(true, 3), (false, 2048)] {
            let (mut machine, id) = full_body_client(expect);
            let mut bytes = Vec::with_capacity(capacity);
            bytes.extend_from_slice(b"abc");
            let pointer = bytes.as_ptr();
            let mut body = OutgoingBody::full(bytes);
            assert!(!admit_eager(&mut machine, id, &mut body, 1024, true));
            let Some(Ok(SourceFrame::Data(OutgoingData::Owned(bytes)))) =
                futures::executor::block_on(body.source.next())
            else {
                panic!()
            };
            assert_eq!(bytes.as_ptr(), pointer);
        }
        let (mut machine, id) = full_body_client(false);
        let mut body = OutgoingBody::from_stream(
            Some(3),
            futures::stream::poll_fn(|_| panic!("custom source must remain demand-driven")),
        );
        assert!(!admit_eager(&mut machine, id, &mut body, 1024, true));
    }

    #[test]
    fn runnable_receive_does_not_poll_the_wait_future() {
        let (sender, receiver) = async_channel();
        let mut wait = std::pin::pin!(std::future::poll_fn(
            |_| -> Poll<Result<usize, kimojio::ChannelError>> {
                panic!("a runnable turn registered a wait");
            }
        ));
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(poll_receive(&receiver, wait.as_mut(), &mut cx, true).is_pending());
        sender.try_send(17).unwrap();
        assert!(matches!(
            poll_receive(&receiver, wait.as_mut(), &mut cx, true),
            Poll::Ready(Ok(17))
        ));
        drop(sender);
        assert!(matches!(
            poll_receive(&receiver, wait.as_mut(), &mut cx, true),
            Poll::Ready(Err(_))
        ));
    }

    #[kimojio::test]
    async fn suspended_receive_registers_a_wake_after_a_runnable_probe() {
        let (sender, receiver) = async_channel();
        let mut wait = std::pin::pin!(receiver.recv());
        futures::future::poll_fn(|cx| {
            assert!(poll_receive(&receiver, wait.as_mut(), cx, true).is_pending());
            Poll::Ready(())
        })
        .await;
        let (received, ()) = futures::join!(
            futures::future::poll_fn(|cx| poll_receive(&receiver, wait.as_mut(), cx, false)),
            async {
                operations::yield_cpu().await;
                sender.try_send(29).unwrap();
            },
        );
        assert_eq!(received.unwrap(), 29);
    }

    #[kimojio::test]
    async fn ready_probe_can_consume_after_a_registered_wait() {
        let (sender, receiver) = async_channel();
        {
            let mut wait = std::pin::pin!(receiver.recv());
            futures::future::poll_fn(|cx| {
                assert!(poll_receive(&receiver, wait.as_mut(), cx, false).is_pending());
                sender.try_send(31).unwrap();
                assert!(matches!(
                    poll_receive(&receiver, wait.as_mut(), cx, true),
                    Poll::Ready(Ok(31))
                ));
                Poll::Ready(())
            })
            .await;
        }
        sender.try_send(37).unwrap();
        assert_eq!(receiver.recv().await.unwrap(), 37);
    }

    #[test]
    fn revoked_admission_returns_the_payload_without_aborting_the_response() {
        let config = core::Config::default();
        let buffer = vec![0; config.max_buffer_bytes];
        let mut client = core::Client::with_output_type(
            core::ConnectionId {
                slot: 95,
                generation: 1,
            },
            config,
            buffer,
            core::Tick(0),
        )
        .unwrap();
        let id = client
            .request(core::Request {
                head: core::RequestHead {
                    method: "POST",
                    target: "/",
                    version: core::Version::Http11,
                    headers: &[core::Header {
                        name: "host",
                        value: b"test",
                    }],
                },
                body: core::BodyLength::Streaming,
                expect_continue: false,
            })
            .unwrap();
        let mut read = None;
        let mut ready = false;
        let mut injected = false;
        let mut rejected = false;
        let mut source_finished = false;
        let mut incoming_finished = false;
        let mut closed = false;
        let mut body = Vec::new();
        for _ in 0..64 {
            match client.next(&mut Ports::default()) {
                Some(Event::Read(op)) => read = Some(op),
                Some(Event::Write(op)) => {
                    let count = op.slices().iter().map(|slice| slice.len()).sum();
                    client.complete_write(op.complete(Ok(count))).unwrap();
                }
                Some(Event::SendReady(exchange, capacity)) => {
                    assert_eq!(exchange, id);
                    assert!(capacity >= 8);
                    ready = true;
                }
                Some(Event::Response(exchange, response, false)) => {
                    assert_eq!(exchange, id);
                    assert_eq!(response.unwrap().status(), 413);
                    assert!(!source_finished);
                    let buffer = b"too late".to_vec();
                    let original = buffer.as_ptr();
                    let rejection = client
                        .send_body(core::SendBody {
                            exchange: id,
                            range: 0..buffer.len(),
                            buffer: OutgoingData::Owned(buffer),
                            end: false,
                        })
                        .unwrap_err();
                    assert_eq!(rejection.reason, core::RejectReason::InvalidState);
                    assert_eq!(rejection.value.buffer.as_ref().as_ptr(), original);
                    assert!(!source_admitted(Err(rejection)).unwrap());
                    rejected = true;
                    client.grant_body_credit(id, 64).unwrap();
                }
                Some(Event::SourceFinished(_)) => source_finished = true,
                Some(Event::Body(op)) => {
                    body.extend_from_slice(op.bytes());
                    let count = op.bytes().len();
                    client.release_body(op.release(count)).unwrap();
                }
                Some(Event::BodySent(_)) => panic!("a rejected buffer acquired a receipt"),
                Some(Event::IncomingFinished(_)) => incoming_finished = true,
                Some(Event::Close(op)) => client.complete_close(op.complete(Ok(()))).unwrap(),
                Some(Event::Closed(result)) => {
                    result.unwrap();
                    closed = true;
                    break;
                }
                Some(Event::Deadline(_) | Event::ExchangeFinished(_)) | None => {}
                _ => panic!("unexpected operation in revoked-admission regression"),
            }
            if ready
                && !injected
                && let Some(mut op) = read.take()
            {
                let response = b"HTTP/1.1 413 Content Too Large\r\nContent-Length: 16\r\nConnection: close\r\n\r\nrequest rejected";
                op.bytes_mut()[..response.len()].copy_from_slice(response);
                client
                    .complete_read(op.complete(Ok(response.len())))
                    .unwrap();
                injected = true;
            }
        }
        assert!(rejected && source_finished && incoming_finished && closed);
        assert_eq!(body, b"request rejected");
    }
}
