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

use crate::body::{BodyDemand, OutgoingData};
use crate::{
    BodyChunk, Error, IncomingBody, OutgoingBody, OutgoingFrame,
    io::{self, Pending, WriteAction, WriteResult},
    metadata,
    transport::{NativeTransport, StreamTransport, Transport},
};

#[derive(Clone, Debug)]
pub struct Config {
    pub connection_id: core::ConnectionId,
    pub protocol: core::Config,
    pub turn_budget: usize,
}

impl Config {
    /// Identities come from the application's connection allocator.
    pub fn new(connection_id: core::ConnectionId) -> Self {
        Self {
            connection_id,
            protocol: core::Config::default(),
            turn_budget: 64,
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
    pub fn graceful(&self) {
        self.graceful.cancel();
    }

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

struct DemandQueue(Receiver<BodyDemand>);

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

    pub fn control(&self) -> Shutdown {
        self.shutdown.clone()
    }

    /// Stops admission, waits for the active exchange, and closes the transport.
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
}

/// Wraps an established transport. No I/O occurs until the driver is polled.
pub fn connect<S: SplittableStream>(stream: S, config: Config) -> (Client, Connection<S>) {
    connection(stream, config)
}

/// Caller-owned driver for one-shot native socket operations.
pub struct NativeConnection(Connection<NativeTransport>);

/// Takes ownership of an established socket without a buffered stream adapter.
///
/// Native reads use the core's receive storage directly. Each native write
/// reports the exact byte count from one writev completion.
pub fn connect_native(fd: kimojio::OwnedFd, config: Config) -> (Client, NativeConnection) {
    let (client, connection) = connection(NativeTransport(fd), config);
    (client, NativeConnection(connection))
}

fn connection<S>(stream: S, config: Config) -> (Client, Connection<S>) {
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
        },
    )
}

impl<S: SplittableStream> Connection<S> {
    pub async fn run(self) -> Result<(), Error> {
        self.run_transport(StreamTransport).await
    }
}

impl NativeConnection {
    /// Poll this driver concurrently with the client and through shutdown.
    pub async fn run(self) -> Result<(), Error> {
        self.0.run_transport(|stream| *stream).await
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
        ))
        .await;
        let _ = self.done.send(result.clone());
        result
    }
}

/// Serves one established transport with a sequential asynchronous handler.
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
    serve_transport(NativeTransport(fd), config, shutdown, handler)
}

async fn serve_transport<T, H, F>(
    transport: T,
    config: Config,
    shutdown: Shutdown,
    handler: H,
) -> Result<(), Error>
where
    T: Transport,
    H: FnMut(Request<IncomingBody>) -> F,
    F: Future<Output = Result<Response<OutgoingBody>, Error>> + 'static,
{
    let (_keep_open, requests) = async_channel();
    Box::pin(run(transport, config, true, &requests, shutdown, handler)).await
}

enum Machine {
    Client(core::Client<Vec<u8>, OutgoingData>),
    Server(core::Server<Vec<u8>, OutgoingData>),
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

struct Ports;

impl core::Ports<Vec<u8>, OutgoingData> for Ports {
    type Output = Event;

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
        Some(Event::Deadline(value))
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
    Source(Option<Result<OutgoingFrame, Error>>),
    Handler(Result<Response<OutgoingBody>, Error>),
    Timer(Result<(), kimojio::Errno>),
    Wake,
}

struct State {
    machine: Machine,
    active: Option<Active>,
    read_send: Option<Sender<Pending<core::ReadOp<Vec<u8>>>>>,
    write_send: Sender<WriteAction>,
    read_done: Receiver<core::ReadCompletion<Vec<u8>>>,
    write_done: Receiver<WriteResult>,
    release_send: Sender<core::BodyCompletion<Vec<u8>>>,
    released: Receiver<core::BodyCompletion<Vec<u8>>>,
    demand_send: Sender<BodyDemand>,
    demands: DemandQueue,
    pending_read: Option<(core::OperationId, Rc<CancellationToken>)>,
    pending_write: Option<(core::OperationId, Rc<CancellationToken>)>,
    pending_close: Option<core::CloseOp>,
    deadline: Option<core::Deadline>,
    timer: Option<Pin<Box<operations::SleepFuture<'static>>>>,
    epoch: Instant,
    shutdown: Shutdown,
    graceful_applied: bool,
    abort_applied: bool,
    server: bool,
    max_buffer: usize,
    receive_capacity: usize,
    max_headers: usize,
    rotation: usize,
}

impl State {
    fn observe_time(&mut self) -> Result<core::Tick, Error> {
        let nanos = kimojio::clock_now()
            .saturating_duration_since(self.epoch)
            .as_nanos();
        let now = core::Tick(u64::try_from(nanos).unwrap_or(u64::MAX));
        match &mut self.machine {
            Machine::Client(inner) => inner.observe_time(now),
            Machine::Server(inner) => inner.observe_time(now),
        }?;
        Ok(now)
    }

    fn observe(&mut self) -> Result<(), Error> {
        let now = self.observe_time()?;
        if let Some(deadline) = self.deadline
            && deadline.at <= now
        {
            self.deadline.take();
            self.timer.take();
            let result = match &mut self.machine {
                Machine::Client(inner) => inner.expire(deadline, now),
                Machine::Server(inner) => inner.expire(deadline, now),
            };
            match result {
                Ok(()) | Err(core::CommandError::StaleDeadline) => {}
                Err(error) => return Err(error.into()),
            }
        }
        if self.shutdown.abort.is_cancelled() && !self.abort_applied {
            self.abort_applied = true;
            match &mut self.machine {
                Machine::Client(inner) => inner.shutdown(core::ShutdownMode::Abort),
                Machine::Server(inner) => inner.shutdown(core::ShutdownMode::Abort),
            }
        } else if self.shutdown.graceful.is_cancelled() && !self.graceful_applied {
            self.graceful_applied = true;
            match &mut self.machine {
                Machine::Client(inner) => inner.shutdown(core::ShutdownMode::Graceful),
                Machine::Server(inner) => inner.shutdown(core::ShutdownMode::Graceful),
            }
        }
        if let Some(active) = self.active.as_mut()
            && active.cancel.is_cancelled()
            && !active.cancellation_applied
        {
            active.cancellation_applied = true;
            active.data.take();
            if !self.server {
                let _ = match &mut self.machine {
                    Machine::Client(inner) => inner.cancel_exchange(active.id),
                    Machine::Server(inner) => inner.cancel_exchange(active.id),
                };
            }
        }
        Ok(())
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
        let (mut parts, body) = command.request.into_parts();
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
                active.source = Some(body);
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
        let (parts, body) = response.into_parts();
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
        active.source = Some(body);
        Ok(())
    }

    fn source(&mut self, frame: Option<Result<OutgoingFrame, Error>>) -> Result<(), Error> {
        let active = self.active.as_mut().ok_or(Error::Closed)?;
        let capacity = active.capacity;
        active.capacity = 0;
        active.source_ready = false;
        match frame.transpose()? {
            Some(frame @ (OutgoingFrame::Data(_) | OutgoingFrame::Forward(_))) => {
                let buffer = match frame {
                    OutgoingFrame::Data(bytes) => OutgoingData::Owned(bytes),
                    OutgoingFrame::Forward(chunk) => OutgoingData::Forward(chunk),
                    OutgoingFrame::Trailers(_) => unreachable!(),
                };
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
                    Some(OutgoingFrame::Trailers(trailers)) => trailers,
                    None => HeaderMap::new(),
                    Some(OutgoingFrame::Data(_) | OutgoingFrame::Forward(_)) => unreachable!(),
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

    fn input(&mut self, input: Input) -> Result<(), Error> {
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
        }
        if self.pending_read.is_none()
            && self.pending_write.is_none()
            && let Some(close) = self.pending_close.take()
        {
            self.read_send.take();
            self.write_send
                .try_send(WriteAction::Close(close))
                .map_err(|_| Error::Closed)?;
        }
        Ok(())
    }

    fn event<H, F>(
        &mut self,
        event: Event,
        handler: &mut H,
    ) -> Result<Option<Result<(), Error>>, Error>
    where
        H: FnMut(Request<IncomingBody>) -> F,
        F: Future<Output = Result<Response<OutgoingBody>, Error>> + 'static,
    {
        match event {
            Event::Read(op) => {
                let cancel = Rc::new(CancellationToken::new());
                self.pending_read = Some((op.id(), cancel.clone()));
                self.read_send
                    .as_ref()
                    .ok_or(Error::Closed)?
                    .try_send(Pending { op, cancel })
                    .map_err(|_| Error::Closed)?;
            }
            Event::Write(op) => {
                let cancel = Rc::new(CancellationToken::new());
                self.pending_write = Some((op.id(), cancel.clone()));
                self.write_send
                    .try_send(WriteAction::Write(Pending { op, cancel }))
                    .map_err(|_| Error::Closed)?;
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
                for (id, token) in [&self.pending_read, &self.pending_write]
                    .into_iter()
                    .flatten()
                {
                    if *id == op.target {
                        token.cancel();
                    }
                }
            }
            Event::Close(op) => {
                self.pending_close = Some(op);
                self.input(Input::Wake)?;
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
                    self.timer = match deadline {
                        Some(deadline) => {
                            let at = self
                                .epoch
                                .checked_add(Duration::from_nanos(deadline.at.0))
                                .ok_or(Error::Limit)?;
                            Some(Box::pin(operations::sleep_until(at)))
                        }
                        None => None,
                    };
                    self.deadline = deadline;
                }
            }
            Event::Upgrade => return Err(Error::Application("HTTP upgrade is not exposed".into())),
            Event::Closed(result) => return Ok(Some(result.map_err(Error::Protocol))),
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
) -> Result<(), Error>
where
    T: Transport,
    H: FnMut(Request<IncomingBody>) -> F,
    F: Future<Output = Result<Response<OutgoingBody>, Error>> + 'static,
{
    let buffer = vec![0; config.protocol.max_buffer_bytes];
    let receive_capacity = buffer.capacity();
    let max_buffer = config.protocol.max_buffer_bytes;
    let max_headers = config.protocol.max_headers;
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
    let (reader, writer) = Box::pin(transport.split())
        .await
        .map_err(Error::Transport)?;
    let (read_send, reads) = async_channel();
    let (read_complete, read_done) = async_channel();
    let (write_send, writes) = async_channel();
    let (write_complete, write_done) = async_channel();
    let (release_send, released) = async_channel();
    let (demand_send, demands) = async_channel();
    let state = State {
        machine,
        active: None,
        read_send: Some(read_send),
        write_send,
        read_done,
        write_done,
        release_send,
        released,
        demand_send,
        demands: DemandQueue(demands),
        pending_read: None,
        pending_write: None,
        pending_close: None,
        deadline: None,
        timer: None,
        epoch: kimojio::clock_now(),
        shutdown,
        graceful_applied: false,
        abort_applied: false,
        server,
        max_buffer,
        receive_capacity,
        max_headers,
        rotation: 0,
    };
    let (result, (), ()) = futures::join!(
        drive(state, requests, &mut handler, config.turn_budget.max(1)),
        Box::pin(io::read_worker(Box::new(reader), reads, read_complete)),
        Box::pin(io::write_worker(Box::new(writer), writes, write_complete)),
    );
    result
}

async fn drive<H, F>(
    mut state: State,
    requests: &Receiver<SendRequest>,
    handler: &mut H,
    budget: usize,
) -> Result<(), Error>
where
    H: FnMut(Request<IncomingBody>) -> F,
    F: Future<Output = Result<Response<OutgoingBody>, Error>> + 'static,
{
    let mut turns = 0usize;
    loop {
        if let Err(error) = state.observe() {
            state.fail(error);
        }
        let event = match &mut state.machine {
            Machine::Client(inner) => inner.next(&mut Ports),
            Machine::Server(inner) => inner.next(&mut Ports),
        };
        let mut runnable = event.is_some();
        if let Some(event) = event {
            match state.event(event, handler) {
                Ok(Some(result)) => return result,
                Ok(None) => {}
                Err(error) => state.fail(error),
            }
        } else if state.cancel_abandoned_duplex() {
            // Source termination can drop the consumer before its last lease
            // returns. Only a drained core can distinguish that from abandonment.
            runnable = true;
        }
        turns += 1;
        if turns == budget {
            turns = 0;
            operations::yield_cpu().await;
        }
        if let Some(input) = next_input(&mut state, requests, runnable).await {
            if let Err(error) = state.observe_time() {
                state.fail(error);
            }
            if let Err(error) = state.input(input) {
                state.fail(error);
            }
        }
    }
}

async fn next_input(
    state: &mut State,
    requests: &Receiver<SendRequest>,
    runnable: bool,
) -> Option<Input> {
    let read = state.read_done.recv();
    let write = state.write_done.recv();
    let release = state.released.recv();
    let demand = state.demands.0.recv();
    let request = requests.recv();
    let graceful = state.shutdown.graceful.cancelled();
    let abort = state.shutdown.abort.cancelled();
    let active_cancel = state.active.as_ref().map(|active| active.cancel.clone());
    let cancelled = async {
        if let Some(cancel) = &active_cancel {
            let _ = cancel.cancelled().await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    futures::pin_mut!(
        read, write, release, demand, request, graceful, abort, cancelled
    );
    futures::future::poll_fn(|cx| {
        for offset in 0..10 {
            let index = (state.rotation + offset) % 10;
            let ready = match index {
                0 if state.read_send.is_some() => {
                    poll_receive(&state.read_done, read.as_mut(), cx, runnable)
                        .map(|r| r.ok().map(Input::Read))
                }
                1 => poll_receive(&state.write_done, write.as_mut(), cx, runnable)
                    .map(|r| r.ok().map(Input::Write)),
                2 => poll_receive(&state.released, release.as_mut(), cx, runnable)
                    .map(|r| r.ok().map(Input::Release)),
                3 if !state.server
                    && state.active.is_none()
                    && !state.graceful_applied
                    && !state.abort_applied =>
                {
                    poll_receive(requests, request.as_mut(), cx, runnable)
                        .map(|r| Some(Input::Request(r)))
                }
                // Drain notifications that can revoke previously advertised capacity.
                4 if !runnable => poll_source(&mut state.active, cx),
                5 => poll_handler(&mut state.active, cx),
                6 if !state.graceful_applied
                    && (!runnable || state.shutdown.graceful.is_cancelled()) =>
                {
                    graceful.as_mut().poll(cx).map(|_| Some(Input::Wake))
                }
                7 if !state.abort_applied && (!runnable || state.shutdown.abort.is_cancelled()) => {
                    abort.as_mut().poll(cx).map(|_| Some(Input::Wake))
                }
                8 => match state.timer.as_mut().map(|timer| timer.as_mut().poll(cx)) {
                    Some(Poll::Ready(result)) => {
                        state.timer.take();
                        Poll::Ready(Some(Input::Timer(result)))
                    }
                    _ => Poll::Pending,
                },
                9 => poll_receive(&state.demands.0, demand.as_mut(), cx, runnable)
                    .map(|r| r.ok().map(Input::Demand)),
                _ => Poll::Pending,
            };
            if let Poll::Ready(Some(input)) = ready {
                state.rotation = (index + 1) % 10;
                return Poll::Ready(Some(input));
            }
        }
        if let Some(active) = &state.active
            && !active.cancellation_applied
            && (!runnable || active.cancel.is_cancelled())
            && cancelled.as_mut().poll(cx).is_ready()
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

fn poll_receive<T>(
    receiver: &Receiver<T>,
    wait: Pin<&mut impl Future<Output = Result<T, kimojio::ChannelError>>>,
    cx: &mut Context<'_>,
    runnable: bool,
) -> Poll<Result<T, kimojio::ChannelError>> {
    if !runnable {
        return wait.poll(cx);
    }
    // A runnable turn cannot suspend, so an empty channel needs no wake registration.
    match receiver.try_recv() {
        Ok(Some(value)) => Poll::Ready(Ok(value)),
        Ok(None) => Poll::Pending,
        Err(error) => Poll::Ready(Err(error)),
    }
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
#[path = "forwarding_tests.rs"]
mod forwarding_tests;

#[cfg(test)]
mod tests {
    use super::*;

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
            match client.next(&mut Ports) {
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
