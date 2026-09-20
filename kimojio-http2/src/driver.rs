use std::{
    cell::{Cell, OnceCell},
    collections::{BTreeMap, BTreeSet},
    future::Future,
    ops::{Deref, DerefMut},
    rc::Rc,
    task::Poll,
    time::Duration,
};

use futures::{FutureExt, StreamExt, future::LocalBoxFuture};
use http::{Request, Response};
use kimojio::{
    AsyncStreamWrite, CancellationToken, Receiver, ReceiverUnbounded, Sender, SenderOneshot,
    SenderUnbounded, SplittableStream, async_channel, async_channel_unbounded, oneshot, operations,
};
use kimojio_fsm_http2 as core;

use crate::{
    BodyChunk, Error, IncomingBody, IncomingFrame, OutgoingBody, OutgoingFrame, RequestObserver,
    SendFailure, StreamReport,
    body::{Data, Delivery, Source},
    informational::{InformationState, PendingInformation},
    io::{self, Io, WriteDone},
    metadata,
    observation::{self, Observer},
};

#[derive(Clone, Debug)]
pub struct Config {
    pub protocol: core::Config,
    /// Pending requests, excluding requests already admitted by the core.
    pub max_queued_requests: usize,
    /// Owned field allocations and ready body backing allocations in that queue.
    pub max_queued_storage: usize,
    /// Owned storage for one producer's pending trailer section.
    pub max_trailer_storage: usize,
    /// Owned fields and ready body storage for one pending server response.
    /// Also bounds one optional informational section, independently.
    pub max_response_storage: usize,
    /// Wrapper stream records and producer tasks, including unsettled streams.
    pub max_streams: usize,
    pub turn_budget: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            protocol: core::Config::default(),
            max_queued_requests: 64,
            max_queued_storage: 8 * 1024 * 1024,
            max_trailer_storage: 128 * 1024,
            max_response_storage: 256 * 1024,
            max_streams: 128,
            turn_budget: 64,
        }
    }
}

/// Monotonic cooperative shutdown. Abort supersedes graceful shutdown.
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

    fn policy(&self) -> u8 {
        if self.abort.is_cancelled() {
            2
        } else if self.graceful.is_cancelled() {
            1
        } else {
            0
        }
    }
}

#[derive(Default)]
struct Budget {
    items: Cell<usize>,
    bytes: Cell<usize>,
}

struct Reservation {
    budget: Rc<Budget>,
    bytes: usize,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.items.set(self.budget.items.get() - 1);
        self.budget.bytes.set(self.budget.bytes.get() - self.bytes);
    }
}

pub(crate) struct RequestControl {
    pub(crate) cancelled: Cell<bool>,
    pub(crate) retired: Cell<bool>,
    stream: Cell<Option<core::StreamId>>,
    pub(crate) informational: OnceCell<Rc<InformationState>>,
    pub(crate) response_state: Cell<ResponseState>,
    pub(crate) metadata_limit: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResponseState {
    Awaiting,
    Queued,
    Committed,
}

impl RequestControl {
    pub(crate) fn new(stream: Option<core::StreamId>, metadata_limit: usize) -> Self {
        Self {
            cancelled: Cell::new(false),
            retired: Cell::new(false),
            stream: Cell::new(stream),
            informational: OnceCell::new(),
            response_state: Cell::new(ResponseState::Awaiting),
            metadata_limit,
        }
    }
}

struct CancelSend {
    control: Option<Rc<RequestControl>>,
    events: SenderUnbounded<Event>,
}

impl Drop for CancelSend {
    fn drop(&mut self) {
        if let Some(control) = self.control.take()
            && !control.retired.get()
        {
            control.cancelled.set(true);
            let _ = self.events.send(Event::Cancel(control));
        }
    }
}

struct Queued {
    fields: Vec<core::H2HeaderField>,
    body: OutgoingBody,
    response: SenderOneshot<Result<Response<IncomingBody>, Error>>,
    control: Rc<RequestControl>,
    _reservation: Reservation,
    informational: Option<InformationalCallback>,
    observer: Option<Observer>,
}

type InformationalCallback = Box<dyn FnMut(Response<()>)>;

struct Requests {
    receive: ReceiverUnbounded<Queued>,
    shutdown: Shutdown,
}

impl Deref for Requests {
    type Target = ReceiverUnbounded<Queued>;
    fn deref(&self) -> &Self::Target {
        &self.receive
    }
}

impl Drop for Requests {
    fn drop(&mut self) {
        self.shutdown.abort();
        while let Ok(Some(request)) = self.receive.try_recv() {
            request.control.retired.set(true);
            let _ = request.response.send(Err(Error::Closed));
        }
    }
}

struct Events {
    receive: ReceiverUnbounded<Event>,
    send: SenderUnbounded<Event>,
}

impl Deref for Events {
    type Target = ReceiverUnbounded<Event>;
    fn deref(&self) -> &Self::Target {
        &self.receive
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        self.send.close();
        while let Ok(Some(event)) = self.receive.try_recv() {
            drop(event);
        }
    }
}

/// A clonable, single-connection client with concurrent `send(&self)` calls.
#[derive(Clone)]
pub struct Client {
    requests: SenderUnbounded<Queued>,
    shutdown: Shutdown,
    budget: Rc<Budget>,
    config: Rc<Config>,
    events: SenderUnbounded<Event>,
}

impl Client {
    /// Returns final response headers, not upload completion or stream retirement.
    ///
    /// Queue overflow returns `Error::Limit`. Dropping a pending future prevents
    /// queued admission or cancels only its admitted stream.
    pub async fn send(
        &self,
        request: Request<OutgoingBody>,
    ) -> Result<Response<IncomingBody>, Error> {
        self.send_request(request, None, None).await
    }

    /// Observes actual informational heads before returning the final response.
    ///
    /// The synchronous callback runs on the connection driver and must not block.
    /// Ordinary `send` calls allocate neither a callback nor an informational queue.
    pub async fn send_with_informational<H>(
        &self,
        request: Request<OutgoingBody>,
        on_informational: H,
    ) -> Result<Response<IncomingBody>, Error>
    where
        H: FnMut(Response<()>) + 'static,
    {
        self.send_request(request, Some(Box::new(on_informational)), None)
            .await
    }

    /// Observes admission, informational heads, receive END, and actual retirement.
    ///
    /// The observer remains attached after a pre-header response error and does
    /// not require a body to expose the admitted ID or retirement. Ordinary sends
    /// allocate no observer. The application chooses how to retain notifications.
    pub async fn send_with_observer<O>(
        &self,
        request: Request<OutgoingBody>,
        observer: O,
    ) -> Result<Response<IncomingBody>, Error>
    where
        O: RequestObserver + 'static,
    {
        self.send_request(request, None, Some(Box::new(observer)))
            .await
    }

    async fn send_request(
        &self,
        request: Request<OutgoingBody>,
        informational: Option<InformationalCallback>,
        observer: Option<Observer>,
    ) -> Result<Response<IncomingBody>, Error> {
        if self.shutdown.policy() != 0 {
            return Err(Error::Closed);
        }
        let (fields, body) = metadata::request(request)?;
        if let Source::Full(data) = &body.source {
            data.check(
                self.config.protocol.max_send_buffer_bytes,
                self.config.protocol.max_send_buffer_capacity,
            )?;
        }
        let bytes = metadata::storage(&fields)
            .checked_add(body.retained_capacity())
            .ok_or(Error::Limit)?;
        if self.budget.items.get() >= self.config.max_queued_requests
            || bytes
                > self
                    .config
                    .max_queued_storage
                    .saturating_sub(self.budget.bytes.get())
        {
            return Err(Error::Limit);
        }
        self.budget.items.set(self.budget.items.get() + 1);
        self.budget.bytes.set(self.budget.bytes.get() + bytes);
        let reservation = Reservation {
            budget: self.budget.clone(),
            bytes,
        };
        let control = Rc::new(RequestControl::new(None, self.config.max_response_storage));
        let (response, receive) = oneshot();
        self.requests
            .send(Queued {
                fields,
                body,
                response,
                control: control.clone(),
                _reservation: reservation,
                informational,
                observer,
            })
            .map_err(|_| Error::Closed)?;
        let mut guard = CancelSend {
            control: Some(control),
            events: self.events.clone(),
        };
        let result = receive.recv().await.map_err(|_| Error::Closed)?;
        // A resolved error is not abandonment of a pending request.
        guard.control.take();
        result
    }

    pub fn control(&self) -> Shutdown {
        self.shutdown.clone()
    }
}

/// The caller must keep `run` polled through stream retirement.
pub struct Connection<S> {
    stream: Box<S>,
    state: State,
}

/// Caller-owned connection driver with exact native one-shot write receipts.
pub struct NativeConnection(Connection<kimojio::OwnedFd>);

/// Uses an established descriptor. The peer must speak HTTP/2 prior knowledge.
pub fn connect_native(fd: kimojio::OwnedFd, config: Config) -> (Client, NativeConnection) {
    let (client, connection) = connection(fd, config);
    (client, NativeConnection(connection))
}

/// Uses an established HTTP/2 stream. No I/O occurs until `run` is polled.
pub fn connect<S: SplittableStream>(stream: S, config: Config) -> (Client, Connection<S>) {
    connection(stream, config)
}

fn connection<S>(stream: S, config: Config) -> (Client, Connection<S>) {
    let (requests, receive) = async_channel_unbounded();
    let (event_send, events) = async_channel_unbounded();
    let shutdown = Shutdown::default();
    let client = Client {
        requests,
        shutdown: shutdown.clone(),
        budget: Rc::new(Budget::default()),
        config: Rc::new(config.clone()),
        events: event_send.clone(),
    };
    (
        client,
        Connection {
            stream: Box::new(stream),
            state: State::new(
                config,
                Some(Requests {
                    receive,
                    shutdown: shutdown.clone(),
                }),
                Events {
                    receive: events,
                    send: event_send.clone(),
                },
                event_send,
                shutdown,
            ),
        },
    )
}

impl NativeConnection {
    /// Returns only after descriptor closure, producer settlement and retirement.
    ///
    /// Held body chunks remain readable after close but delay this return.
    pub async fn run(self) -> Result<(), Error> {
        Box::pin(run_native(*self.0.stream, self.0.state, None)).await
    }
}

impl<S: SplittableStream> Connection<S> {
    /// Returns after transport close, task settlement, and stream retirement.
    ///
    /// A failed write-all operation has unknown additional progress. Held body
    /// chunks remain readable after close and delay this return.
    pub async fn run(self) -> Result<(), Error> {
        Box::pin(run_stream(self.stream, self.state, None)).await
    }
}

/// Serves concurrent requests over an established HTTP/2 stream.
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

/// Serves concurrent requests with externally controlled graceful or hard stop.
pub fn serve_connection_with_shutdown<S, H, F>(
    stream: S,
    config: Config,
    shutdown: Shutdown,
    mut handler: H,
) -> impl Future<Output = Result<(), Error>>
where
    S: SplittableStream,
    H: FnMut(Request<IncomingBody>) -> F,
    F: Future<Output = Result<Response<OutgoingBody>, Error>> + 'static,
{
    let stream = Box::new(stream);
    async move {
        let (event_send, receive) = async_channel_unbounded();
        let events = Events {
            receive,
            send: event_send.clone(),
        };
        let state = State::new(config, None, events, event_send, shutdown);
        let mut handler = |request| handler(request).boxed_local();
        Box::pin(run_stream(stream, state, Some(&mut handler))).await
    }
}

/// Serves concurrent requests over an established native HTTP/2 descriptor.
///
/// Each handler has its own native task and cancellation scope. Dropping an
/// unread request body discards input without cancelling the response.
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

/// Serves concurrent requests with an externally owned shutdown control.
///
/// Handlers return final responses. A request body's optional
/// `informational_sender` admits interim heads before the final response.
pub async fn serve_connection_native_with_shutdown<H, F>(
    fd: kimojio::OwnedFd,
    config: Config,
    shutdown: Shutdown,
    mut handler: H,
) -> Result<(), Error>
where
    H: FnMut(Request<IncomingBody>) -> F,
    F: Future<Output = Result<Response<OutgoingBody>, Error>> + 'static,
{
    let (event_send, receive) = async_channel_unbounded();
    let events = Events {
        receive,
        send: event_send.clone(),
    };
    let state = State::new(config, None, events, event_send, shutdown);
    let mut handler = |request| handler(request).boxed_local();
    Box::pin(run_native(fd, state, Some(&mut handler))).await
}

type Handler<'a> = dyn FnMut(Request<IncomingBody>) -> LocalBoxFuture<'static, Result<Response<OutgoingBody>, Error>>
    + 'a;

enum Machine {
    Client(core::Client<Data>),
    Server(core::Server<Data>),
}

impl Deref for Machine {
    type Target = core::Connection<Data>;
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Client(core) => core,
            Self::Server(core) => core,
        }
    }
}

impl DerefMut for Machine {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Self::Client(core) => core,
            Self::Server(core) => core,
        }
    }
}

async fn run_native(
    fd: kimojio::OwnedFd,
    state: State,
    handler: Option<&mut Handler<'_>>,
) -> Result<(), Error> {
    operations::io_scope(async move || {
        let machine = match new_machine(&state) {
            Ok(machine) => machine,
            Err(error) => {
                operations::close(fd).await.map_err(Error::Transport)?;
                return Err(error);
            }
        };
        let epoch = kimojio::clock_now();
        run_io(io::native(fd, epoch), machine, state, handler, epoch).await
    })
    .await
}

async fn run_stream<S: SplittableStream>(
    stream: Box<S>,
    state: State,
    handler: Option<&mut Handler<'_>>,
) -> Result<(), Error> {
    operations::io_scope(async move || {
        let (reader, mut writer) = (*stream).split().await.map_err(Error::Transport)?;
        let machine = match new_machine(&state) {
            Ok(machine) => machine,
            Err(error) => {
                drop(reader);
                writer.close().await.map_err(Error::Transport)?;
                return Err(error);
            }
        };
        let epoch = kimojio::clock_now();
        run_io(
            io::stream(reader, writer, epoch),
            machine,
            state,
            handler,
            epoch,
        )
        .await
    })
    .await
}

fn new_machine(state: &State) -> Result<Machine, Error> {
    if state.config.turn_budget == 0
        || state.config.max_streams == 0
        || state.config.max_queued_requests == 0
        || state.config.max_queued_storage == 0
        || state.config.max_response_storage == 0
    {
        return Err(Error::Limit);
    }
    let protocol = state.config.protocol.clone();
    if state.server {
        core::Server::new(protocol, Duration::ZERO).map(Machine::Server)
    } else {
        core::Client::new(protocol, Duration::ZERO).map(Machine::Client)
    }
    .map_err(Error::from)
}

async fn run_io(
    mut io: impl Io,
    mut machine: Machine,
    mut state: State,
    mut handler: Option<&mut Handler<'_>>,
    epoch: std::time::Instant,
) -> Result<(), Error> {
    let mut transport_error = None;
    let mut turns = 0;
    loop {
        transport_error = transport_error.or(io.take_error());
        machine.advance_time(kimojio::clock_now().saturating_duration_since(epoch))?;
        state.control(&mut machine);
        state.admit(&mut machine);
        let output = machine.next(&mut Ports {
            state: &mut state,
            io: &mut io,
        });
        let mut runnable = output.is_some();
        if let Some(output) = output {
            match output {
                Command::Request(id) => state.start_handler(
                    id,
                    handler.as_deref_mut().expect("server handler"),
                    &mut machine,
                ),
                output => state.command(output, &mut machine),
            }
        } else {
            // ReceiveEnd can follow the delivery that caused a body drop.
            runnable = state.abandon(&mut machine);
        }
        if state.closed.is_some()
            && state.streams.is_empty()
            && state.producers.is_empty()
            && state.handlers.is_empty()
        {
            state.reject_queued();
            if let Some(error) = transport_error {
                return Err(match state.close_error.take() {
                    Some(Error::Transport(close)) => Error::TransportAndClose {
                        transport: error,
                        close,
                    },
                    _ => Error::Transport(error),
                });
            }
            return state.close_error.take().map_or_else(
                || match state.closed.expect("closed") {
                    core::ConnectionResult::Graceful => Ok(()),
                    result => Err(Error::Connection(result)),
                },
                Err,
            );
        }
        if let Some(input) = next_input(&mut state, &mut io, runnable).await {
            state.input(input, &mut machine).await?;
        }
        turns += 1;
        if turns >= state.config.turn_budget {
            turns = 0;
            operations::yield_cpu().await;
        }
    }
}

pub(crate) enum Event {
    Release(core::BodyRelease),
    Cancel(Rc<RequestControl>),
    Abandon(core::StreamId),
    Produced(core::SendPermit, Result<Produced, Error>),
    ProducerDone(core::StreamId, Result<(), Error>),
    Handled(core::StreamId, Result<PreparedResponse, Error>),
    Informational(PendingInformation),
    CancelInformation(core::StreamId, u64),
}

pub(crate) enum Produced {
    Data(Data, bool),
    Trailers(Vec<core::H2HeaderField>),
}

pub(crate) struct PreparedResponse {
    fields: Vec<core::H2HeaderField>,
    body: OutgoingBody,
}

enum Upload {
    Empty,
    Ready(Option<Data>),
    Producer(Sender<core::SendPermit>),
}

struct Stream {
    control: Rc<RequestControl>,
    response: Option<SenderOneshot<Result<Response<IncomingBody>, Error>>>,
    incoming: Option<IncomingBody>,
    request: Option<Request<IncomingBody>>,
    frames: SenderUnbounded<Delivery>,
    queued: Rc<ReceiverUnbounded<Delivery>>,
    received: Rc<Cell<Option<core::StreamOutcome>>>,
    completion: Option<SenderOneshot<Result<StreamReport, Error>>>,
    upload: Upload,
    cancel: Rc<CancellationToken>,
    handler_cancel: Rc<CancellationToken>,
    error: Option<Error>,
    informational: Option<InformationalCallback>,
    observer: Option<Observer>,
    send_failure: Option<SendFailure>,
}

struct ScopedTask {
    handle: operations::TaskHandle<()>,
    cancel: Rc<CancellationToken>,
}

struct State {
    config: Config,
    requests: Option<Requests>,
    server: bool,
    requests_open: bool,
    pending: Option<Queued>,
    admission: bool,
    events: Events,
    event_send: SenderUnbounded<Event>,
    streams: BTreeMap<core::StreamId, Stream>,
    producers: BTreeMap<core::StreamId, ScopedTask>,
    handlers: BTreeMap<core::StreamId, ScopedTask>,
    // Core retirement does not release a slot while its native task still runs.
    slots: usize,
    responses: BTreeMap<core::StreamId, PreparedResponse>,
    response_retry: bool,
    informational: BTreeMap<core::StreamId, PendingInformation>,
    informational_retry: bool,
    trailers: BTreeMap<core::StreamId, Vec<core::H2HeaderField>>,
    trailer_retry: bool,
    abandoned: BTreeSet<core::StreamId>,
    shutdown: Shutdown,
    policy: u8,
    closed: Option<core::ConnectionResult>,
    close_error: Option<Error>,
    rotation: usize,
}

impl State {
    fn new(
        config: Config,
        requests: Option<Requests>,
        events: Events,
        event_send: SenderUnbounded<Event>,
        shutdown: Shutdown,
    ) -> Self {
        Self {
            config,
            server: requests.is_none(),
            requests_open: requests.is_some(),
            requests,
            pending: None,
            admission: true,
            events,
            event_send,
            streams: BTreeMap::new(),
            producers: BTreeMap::new(),
            handlers: BTreeMap::new(),
            slots: 0,
            responses: BTreeMap::new(),
            response_retry: false,
            informational: BTreeMap::new(),
            informational_retry: false,
            trailers: BTreeMap::new(),
            trailer_retry: false,
            abandoned: BTreeSet::new(),
            shutdown,
            policy: 0,
            closed: None,
            close_error: None,
            rotation: 0,
        }
    }

    fn control(&mut self, machine: &mut Machine) {
        let policy = self.shutdown.policy();
        if policy > self.policy {
            self.policy = policy;
            self.reject_queued();
            if policy == 2 {
                machine.abort();
            } else {
                match machine.shutdown() {
                    Ok(()) | Err(core::CommandError::InvalidState) => {}
                    Err(error) => {
                        self.close_error.get_or_insert(error.into());
                        machine.abort();
                    }
                }
            }
        }
    }

    fn reject_queued(&mut self) {
        if let Some(request) = self.pending.take() {
            request.control.retired.set(true);
            let _ = request.response.send(Err(Error::Closed));
        }
        if let Some(requests) = &self.requests {
            while let Ok(Some(request)) = requests.try_recv() {
                request.control.retired.set(true);
                let _ = request.response.send(Err(Error::Closed));
            }
        }
    }

    fn admit(&mut self, machine: &mut Machine) {
        if self.informational_retry {
            self.informational_retry = false;
            for (_, information) in std::mem::take(&mut self.informational) {
                self.inform(machine, information);
            }
        }
        if self.trailer_retry {
            self.trailer_retry = false;
            let trailers = std::mem::take(&mut self.trailers);
            for (id, fields) in trailers {
                self.submit_trailers(machine, id, fields);
            }
        }
        if self.response_retry {
            self.response_retry = false;
            for (id, response) in std::mem::take(&mut self.responses) {
                self.respond(machine, id, response);
            }
        }
        if self.server {
            return;
        }
        if self.policy != 0 || self.closed.is_some() {
            self.reject_queued();
            return;
        }
        if self.slots >= self.config.max_streams {
            return;
        }
        if !self.admission {
            return;
        }
        let Some(request) = self.pending.take() else {
            return;
        };
        if request.control.cancelled.get() {
            request.control.retired.set(true);
            return;
        }
        let end = request.body.is_empty();
        let Machine::Client(client) = machine else {
            unreachable!("client admission")
        };
        match client.request_ref(&metadata::borrowed(&request.fields), end) {
            Err(core::CommandError::Blocked) => {
                self.pending = Some(request);
                self.admission = false;
            }
            Err(error) => {
                request.control.retired.set(true);
                let _ = request.response.send(Err(error.into()));
            }
            Ok(id) => {
                request.control.stream.set(Some(id));
                let mut stream = self.stream(id, request.control);
                stream.response = Some(request.response);
                stream.informational = request.informational;
                stream.observer = request.observer;
                stream.upload = self.upload(id, request.body, stream.cancel.clone());
                self.slots += 1;
                self.streams.insert(id, stream);
                if let Err(error) = observation::notify(
                    &mut self.streams.get_mut(&id).expect("admitted stream").observer,
                    |observer| observer.admitted(id),
                ) {
                    self.fail(machine, id, error);
                }
            }
        }
    }

    fn release_slot(&mut self, id: core::StreamId) {
        if !self.streams.contains_key(&id)
            && !self.handlers.contains_key(&id)
            && !self.producers.contains_key(&id)
        {
            self.slots -= 1;
        }
    }

    fn stream(&self, id: core::StreamId, control: Rc<RequestControl>) -> Stream {
        let (frames, receive) = async_channel_unbounded();
        let receive = Rc::new(receive);
        let (completion, done) = oneshot();
        let received = Rc::new(Cell::new(None));
        let incoming = IncomingBody {
            stream: id,
            frames: receive.clone(),
            close: frames.clone(),
            events: self.event_send.clone(),
            received: received.clone(),
            completion: Some(done),
            completion_wait: None,
            completed: None,
            eof: false,
            control: control.clone(),
            abandon_on_drop: !self.server,
        };
        Stream {
            control,
            response: None,
            incoming: Some(incoming),
            request: None,
            frames,
            queued: receive,
            received,
            completion: Some(completion),
            upload: Upload::Empty,
            cancel: Rc::new(CancellationToken::new()),
            handler_cancel: Rc::new(CancellationToken::new()),
            error: None,
            informational: None,
            observer: None,
            send_failure: None,
        }
    }

    fn upload(
        &mut self,
        id: core::StreamId,
        body: OutgoingBody,
        cancel: Rc<CancellationToken>,
    ) -> Upload {
        if body.is_empty() {
            return Upload::Empty;
        }
        match body.source {
            Source::Empty => Upload::Empty,
            Source::Full(data) => Upload::Ready(Some(data)),
            Source::Stream(source) => {
                let (send, permits) = async_channel();
                let events = self.event_send.clone();
                let stop = cancel.clone();
                let max_metadata = self.config.max_trailer_storage;
                let handle = operations::spawn_task(async move {
                    let produced = events.clone();
                    let result =
                        std::panic::AssertUnwindSafe(operations::io_scope(async move || {
                            producer(source, permits, stop, produced, max_metadata).await
                        }))
                        .catch_unwind()
                        .await
                        .unwrap_or_else(|_| {
                            Err(Error::Application("body producer panicked".into()))
                        });
                    let _ = events.send(Event::ProducerDone(id, result));
                });
                self.producers.insert(id, ScopedTask { handle, cancel });
                Upload::Producer(send)
            }
        }
    }

    fn start_handler(
        &mut self,
        id: core::StreamId,
        handler: &mut Handler<'_>,
        machine: &mut Machine,
    ) {
        let Some(stream) = self.streams.get_mut(&id) else {
            return;
        };
        let Some(request) = stream.request.take() else {
            return;
        };
        let cancel = stream.handler_cancel.clone();
        let future =
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(request))) {
                Ok(future) => future,
                Err(_) => {
                    self.fail(
                        machine,
                        id,
                        Error::Application("handler construction panicked".into()),
                    );
                    return;
                }
            };
        let events = self.event_send.clone();
        let stop = cancel.clone();
        let config = self.config.clone();
        let handle = operations::spawn_task(async move {
            let result = std::panic::AssertUnwindSafe(operations::io_scope(async move || {
                let stopped = stop.cancelled();
                futures::pin_mut!(stopped);
                match futures::future::select(stopped, future).await {
                    futures::future::Either::Left(_) => Err(Error::Cancelled),
                    futures::future::Either::Right((response, _)) => {
                        let (fields, body) = metadata::response(response?)?;
                        if let Source::Full(data) = &body.source {
                            data.check(
                                config.protocol.max_send_buffer_bytes,
                                config.protocol.max_send_buffer_capacity,
                            )?;
                        }
                        if metadata::storage(&fields).saturating_add(body.retained_capacity())
                            > config.max_response_storage
                        {
                            return Err(Error::Limit);
                        }
                        Ok(PreparedResponse { fields, body })
                    }
                }
            }))
            .catch_unwind()
            .await
            .unwrap_or_else(|_| Err(Error::Application("handler panicked".into())));
            let _ = events.send(Event::Handled(id, result));
        });
        self.handlers.insert(id, ScopedTask { handle, cancel });
    }

    fn respond(&mut self, machine: &mut Machine, id: core::StreamId, response: PreparedResponse) {
        let Some(stream) = self.streams.get(&id) else {
            return;
        };
        if stream.handler_cancel.is_cancelled() {
            return;
        }
        stream.control.response_state.set(ResponseState::Queued);
        if stream
            .control
            .informational
            .get()
            .is_some_and(|state| state.is_pending())
        {
            self.responses.insert(id, response);
            return;
        }
        let cancel = stream.cancel.clone();
        let Machine::Server(server) = machine else {
            unreachable!("server response")
        };
        match server.respond_ref(
            id,
            &metadata::borrowed(&response.fields),
            response.body.is_empty(),
        ) {
            Ok(()) => {
                self.streams
                    .get(&id)
                    .expect("accepted response")
                    .control
                    .response_state
                    .set(ResponseState::Committed);
                let upload = self.upload(id, response.body, cancel);
                self.streams.get_mut(&id).expect("accepted response").upload = upload;
            }
            Err(core::CommandError::Blocked) => {
                self.responses.insert(id, response);
            }
            Err(error) => self.fail(machine, id, error.into()),
        }
    }

    fn inform(&mut self, machine: &mut Machine, information: PendingInformation) {
        let id = information.stream;
        let error = if information.cancelled() {
            Some(Error::Cancelled)
        } else {
            match self.streams.get(&id) {
                None => Some(Error::Closed),
                Some(stream) if stream.handler_cancel.is_cancelled() => Some(Error::Cancelled),
                Some(stream)
                    if !self.server
                        || stream.control.response_state.get() == ResponseState::Committed =>
                {
                    Some(Error::Command(core::CommandError::InvalidState))
                }
                Some(_) => None,
            }
        };
        if let Some(error) = error {
            information.complete(Err(error));
            self.response_retry = true;
            return;
        }
        let Machine::Server(server) = machine else {
            unreachable!("server informational response")
        };
        match server.respond_ref(id, &metadata::borrowed(&information.fields), false) {
            Ok(()) => {
                information.complete(Ok(()));
                self.response_retry = true;
            }
            Err(core::CommandError::Blocked) => {
                self.informational.insert(id, information);
            }
            Err(error) => {
                information.complete(Err(error.into()));
                self.response_retry = true;
            }
        }
    }

    fn fail(&mut self, machine: &mut Machine, id: core::StreamId, error: Error) {
        if let Some(stream) = self.streams.get_mut(&id) {
            stream.error.get_or_insert(error.clone());
            stream.control.response_state.set(ResponseState::Committed);
            stream.informational.take();
            stream.cancel.cancel();
            stream.handler_cancel.cancel();
            if let Some(response) = stream.response.take() {
                let _ = response.send(Err(error.clone()));
            }
            stream.incoming.take();
        }
        let _ = machine.reset(id, core::H2ErrorCode::Cancel);
        self.trailers.remove(&id);
        self.responses.remove(&id);
        if let Some(information) = self.informational.remove(&id) {
            information.complete(Err(error));
        }
    }

    fn abandon(&mut self, machine: &mut Machine) -> bool {
        let changed = !self.abandoned.is_empty();
        for id in std::mem::take(&mut self.abandoned) {
            if self
                .streams
                .get(&id)
                .is_some_and(|stream| stream.received.get().is_none())
            {
                self.fail(machine, id, Error::Cancelled);
            }
        }
        changed
    }

    fn submit(
        &mut self,
        machine: &mut Machine,
        permit: core::SendPermit,
        produced: Result<Produced, Error>,
    ) {
        let id = permit.stream();
        if self
            .streams
            .get(&id)
            .is_none_or(|stream| stream.cancel.is_cancelled())
        {
            return;
        }
        match produced {
            Err(error) => self.fail(machine, id, error),
            Ok(Produced::Trailers(fields)) => self.submit_trailers(machine, id, fields),
            Ok(Produced::Data(data, end)) => {
                if let Err(error) = data.check(permit.max_bytes(), permit.max_retained_capacity()) {
                    self.fail(machine, id, error);
                } else if let Err(rejected) = machine.send(permit, data, end) {
                    // Each permit is submitted once. InvalidState therefore means
                    // revocation; the pending core notices carry its actual cause.
                    if rejected.error != core::CommandError::InvalidState {
                        self.fail(machine, id, rejected.error.into());
                    }
                }
            }
        }
    }

    fn submit_trailers(
        &mut self,
        machine: &mut Machine,
        id: core::StreamId,
        fields: Vec<core::H2HeaderField>,
    ) {
        match machine.trailers_ref(id, &metadata::borrowed(&fields)) {
            Ok(()) => {}
            Err(core::CommandError::Blocked) => {
                self.trailers.insert(id, fields);
            }
            Err(error) => self.fail(machine, id, error.into()),
        }
    }

    fn command(&mut self, command: Command, machine: &mut Machine) {
        match command {
            Command::Progress => {}
            Command::Request(_) => unreachable!("handler dispatch belongs to the driver"),
            Command::Reject(id) => {
                let _ = machine.reset(id, core::H2ErrorCode::RefusedStream);
            }
            Command::Cancel(done) => {
                if machine.complete_cancel(done).is_err() {
                    machine.abort();
                }
            }
            Command::Fail(id, error) => self.fail(machine, id, error),
            Command::Permit(permit) => {
                let id = permit.stream();
                if let Some(stream) = self.streams.get_mut(&id) {
                    match &mut stream.upload {
                        Upload::Ready(data) => {
                            if let Some(data) = data.take() {
                                self.submit(machine, permit, Ok(Produced::Data(data, true)));
                            }
                        }
                        Upload::Producer(send) => {
                            if send.try_send(permit).is_err() {
                                self.fail(
                                    machine,
                                    id,
                                    Error::Application("producer permit channel closed".into()),
                                );
                            }
                        }
                        Upload::Empty => {}
                    }
                }
            }
        }
    }

    async fn input(&mut self, input: Input, machine: &mut Machine) -> Result<(), Error> {
        match input {
            Input::Read(done) => machine
                .complete_read(done)
                .map_err(|r| Error::Command(r.error))?,
            Input::Write(WriteDone::Data(done)) => machine
                .complete_write(done)
                .map_err(|r| Error::Command(r.error))?,
            Input::Write(WriteDone::Close(done, result)) => {
                if let Err(error) = result {
                    self.close_error = Some(Error::Transport(error));
                }
                machine
                    .complete_close(done)
                    .map_err(|r| Error::Command(r.error))?;
            }
            Input::Wake(done) => machine
                .complete_wake(done)
                .map_err(|r| Error::Command(r.error))?,
            Input::Request(Some(request)) => {
                self.pending = Some(request);
                self.admission = true;
            }
            Input::Request(None) => {
                self.requests_open = false;
                self.shutdown.graceful();
            }
            Input::Event(Event::Release(done)) => machine
                .release_body(done)
                .map_err(|r| Error::Command(r.error))?,
            Input::Control => {}
            Input::Event(Event::Abandon(id)) => {
                self.abandoned.insert(id);
            }
            Input::Event(Event::Cancel(control)) => {
                if let Some(id) = control.stream.get() {
                    self.fail(machine, id, Error::Cancelled);
                } else if self
                    .pending
                    .as_ref()
                    .is_some_and(|p| Rc::ptr_eq(&p.control, &control))
                {
                    if let Some(request) = self.pending.take() {
                        request.control.retired.set(true);
                    }
                    self.admission = true;
                }
            }
            Input::Event(Event::Produced(permit, produced)) => {
                self.submit(machine, permit, produced)
            }
            Input::Event(Event::ProducerDone(id, result)) => {
                if let Some(producer) = self.producers.remove(&id) {
                    producer
                        .handle
                        .await
                        .map_err(|e| Error::Application(format!("producer task: {e:?}")))?;
                    self.release_slot(id);
                }
                if let Err(error) = result {
                    self.fail(machine, id, error);
                }
            }
            Input::Event(Event::Handled(id, result)) => {
                let mut stopped = false;
                if let Some(handler) = self.handlers.remove(&id) {
                    stopped = handler.cancel.is_cancelled();
                    handler
                        .handle
                        .await
                        .map_err(|error| Error::Application(format!("handler task: {error:?}")))?;
                    self.release_slot(id);
                }
                // Revocation must not replace the core reset or connection outcome.
                if !stopped {
                    match result {
                        Ok(response) => self.respond(machine, id, response),
                        Err(error) => self.fail(machine, id, error),
                    }
                }
            }
            Input::Event(Event::Informational(information)) => self.inform(machine, information),
            Input::Event(Event::CancelInformation(id, generation)) => {
                if self
                    .informational
                    .get(&id)
                    .is_some_and(|pending| pending.generation == generation)
                {
                    self.informational.remove(&id);
                    self.response_retry = true;
                }
            }
        }
        Ok(())
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.reject_queued();
        for producer in self.producers.values() {
            producer.cancel.cancel();
        }
        for handler in self.handlers.values() {
            handler.cancel.cancel();
        }
        for stream in self.streams.values_mut() {
            stream.control.retired.set(true);
            stream.cancel.cancel();
            stream.handler_cancel.cancel();
            stream.frames.close();
            while let Ok(Some(delivery)) = stream.queued.try_recv() {
                drop(delivery);
            }
            if let Some(response) = stream.response.take() {
                let _ = response.send(Err(Error::Closed));
            }
            if let Some(done) = stream.completion.take() {
                let _ = done.send(Err(Error::Closed));
            }
        }
        while let Ok(Some(event)) = self.events.try_recv() {
            drop(event);
        }
    }
}

enum Command {
    Progress,
    Request(core::StreamId),
    Reject(core::StreamId),
    Cancel(core::CancelCompletion),
    Permit(core::SendPermit),
    Fail(core::StreamId, Error),
}

struct Ports<'a, I> {
    state: &'a mut State,
    io: &'a mut I,
}

impl<I: Io> core::Ports<Data> for Ports<'_, I> {
    type Output = Command;
    fn admission_changed(&mut self) -> Option<Command> {
        self.state.admission = true;
        self.state.trailer_retry = true;
        self.state.response_retry = true;
        self.state.informational_retry = true;
        Some(Command::Progress)
    }
    fn read(&mut self, op: core::ReadOp) -> Option<Command> {
        self.io.read(op);
        Some(Command::Progress)
    }
    fn write(&mut self, op: core::WriteOp<Data>) -> Option<Command> {
        self.io.write(op);
        Some(Command::Progress)
    }
    fn wake(&mut self, op: core::WakeOp) -> Option<Command> {
        self.io.wake(op);
        Some(Command::Progress)
    }
    fn cancel(&mut self, op: core::CancelOp) -> Option<Command> {
        self.io.cancel(op.original());
        Some(Command::Cancel(op.complete()))
    }
    fn close(&mut self, op: core::CloseOp) -> Option<Command> {
        self.io.close(op);
        Some(Command::Progress)
    }
    fn closed(&mut self, result: core::ConnectionResult) -> Option<Command> {
        self.state.closed = Some(result);
        Some(Command::Progress)
    }
    fn reschedule(&mut self) -> Option<Command> {
        Some(Command::Progress)
    }
    fn send_ready(&mut self, permit: core::SendPermit) -> Option<Command> {
        Some(Command::Permit(permit))
    }
    fn send_stopped(&mut self, id: core::StreamId, reason: core::SendStop) -> Option<Command> {
        if let Some(stream) = self.state.streams.get_mut(&id) {
            if self.state.server {
                stream.control.response_state.set(ResponseState::Committed);
            }
            stream.cancel.cancel();
            stream.upload = Upload::Empty;
            if reason != core::SendStop::Finished {
                stream.handler_cancel.cancel();
            }
        }
        self.state.trailers.remove(&id);
        self.state.responses.remove(&id);
        if let Some(information) = self.state.informational.remove(&id) {
            let error = match reason {
                core::SendStop::Finished => Error::Command(core::CommandError::InvalidState),
                core::SendStop::Reset(code) => Error::Stream(core::StreamOutcome::Reset(code)),
                core::SendStop::Unprocessed => Error::Stream(core::StreamOutcome::Unprocessed),
                core::SendStop::ConnectionFailed => {
                    Error::Stream(core::StreamOutcome::ConnectionFailed)
                }
            };
            information.complete(Err(error));
        }
        Some(Command::Progress)
    }
    fn sent(&mut self, result: core::Sent<Data>) -> Option<Command> {
        if let Err(reason) = result.result
            && let Some(stream) = self.state.streams.get_mut(&result.stream)
        {
            stream.send_failure.get_or_insert(SendFailure {
                accepted: result.accepted,
                exact: result.exact,
                reason,
            });
            stream.error.get_or_insert(Error::Send {
                accepted: result.accepted,
                exact: result.exact,
                reason,
            });
        }
        drop(result);
        Some(Command::Progress)
    }
    fn headers(&mut self, head: core::Head<'_>) -> Option<Command> {
        if head.kind == core::HeadKind::Request {
            if !self.state.server {
                return Some(Command::Fail(head.stream, Error::InvalidMetadata));
            }
            if self.state.slots >= self.state.config.max_streams {
                return Some(Command::Reject(head.stream));
            }
            let control = Rc::new(RequestControl::new(
                Some(head.stream),
                self.state.config.max_response_storage,
            ));
            let mut stream = self.state.stream(head.stream, control);
            let body = stream.incoming.take().expect("new request body");
            match metadata::incoming_request(head, body) {
                Ok(request) => stream.request = Some(request),
                Err(error) => return Some(Command::Fail(head.stream, error)),
            }
            self.state.slots += 1;
            self.state.streams.insert(head.stream, stream);
            return Some(Command::Request(head.stream));
        }
        let Some(stream) = self.state.streams.get_mut(&head.stream) else {
            return Some(Command::Progress);
        };
        match head.kind {
            core::HeadKind::Response(status) => {
                stream.informational.take();
                let headers = match metadata::headers(head) {
                    Ok(headers) => headers,
                    Err(error) => return Some(Command::Fail(head.stream, error)),
                };
                if let (Some(send), Some(body)) = (stream.response.take(), stream.incoming.take()) {
                    let mut response = Response::new(body);
                    *response.status_mut() =
                        http::StatusCode::from_u16(status).expect("validated status");
                    *response.version_mut() = http::Version::HTTP_2;
                    *response.headers_mut() = headers;
                    let _ = send.send(Ok(response));
                }
            }
            core::HeadKind::Trailers => match metadata::headers(head) {
                Ok(headers) => {
                    let _ = stream
                        .frames
                        .send(Delivery::Frame(IncomingFrame::Trailers(headers)));
                }
                Err(error) => return Some(Command::Fail(head.stream, error)),
            },
            core::HeadKind::Informational(status) => {
                if !stream.control.cancelled.get()
                    && (stream.informational.is_some() || stream.observer.is_some())
                {
                    let headers = match metadata::headers(head) {
                        Ok(headers) => headers,
                        Err(error) => return Some(Command::Fail(head.stream, error)),
                    };
                    let mut response = Response::new(());
                    *response.status_mut() =
                        http::StatusCode::from_u16(status).expect("validated status");
                    *response.version_mut() = http::Version::HTTP_2;
                    *response.headers_mut() = headers;
                    if let Some(callback) = &mut stream.informational {
                        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            callback(response)
                        }))
                        .is_err()
                        {
                            return Some(Command::Fail(
                                head.stream,
                                Error::Application("informational callback panicked".into()),
                            ));
                        }
                    } else if let Err(error) =
                        observation::notify(&mut stream.observer, |observer| {
                            observer.informational(response)
                        })
                    {
                        return Some(Command::Fail(head.stream, error));
                    }
                }
            }
            core::HeadKind::Request => {
                return Some(Command::Fail(head.stream, Error::InvalidMetadata));
            }
        }
        Some(Command::Progress)
    }
    fn body(&mut self, op: core::BodyOp) -> Option<Command> {
        let id = op.stream();
        let chunk = BodyChunk {
            op: Some(op),
            events: self.state.event_send.clone(),
        };
        if let Some(stream) = self.state.streams.get(&id) {
            let _ = stream
                .frames
                .send(Delivery::Frame(IncomingFrame::Data(chunk)));
        }
        Some(Command::Progress)
    }
    fn ended(&mut self, end: core::ReceiveEnd) -> Option<Command> {
        if let Some(stream) = self.state.streams.get_mut(&end.stream) {
            stream.received.set(Some(end.outcome));
            if end.outcome != core::StreamOutcome::Complete {
                while let Ok(Some(delivery)) = stream.queued.try_recv() {
                    drop(delivery);
                }
            }
            let _ = stream.frames.send(Delivery::End(end.outcome));
            if let Some(response) = stream.response.take() {
                let _ = response.send(Err(stream
                    .error
                    .clone()
                    .unwrap_or(Error::Stream(end.outcome))));
            }
            if let Err(error) =
                observation::notify(&mut stream.observer, |observer| observer.receive_end(end))
            {
                return Some(Command::Fail(end.stream, error));
            }
        }
        Some(Command::Progress)
    }
    fn retired(&mut self, result: core::StreamResult) -> Option<Command> {
        if let Some(mut stream) = self.state.streams.remove(&result.stream) {
            stream.control.retired.set(true);
            stream.cancel.cancel();
            stream.handler_cancel.cancel();
            stream.frames.close();
            let mut report = StreamReport {
                stream: result.stream,
                outcome: result.outcome,
                receive_outcome: stream.received.get(),
                send_failure: stream.send_failure,
                error: stream.error.take(),
            };
            if let Err(error) =
                observation::notify(&mut stream.observer, |observer| observer.retired(&report))
            {
                report.error.get_or_insert(error);
            }
            if let Some(done) = stream.completion.take() {
                let _ = done.send(Ok(report));
            }
            self.state.release_slot(result.stream);
        }
        self.state.trailers.remove(&result.stream);
        self.state.responses.remove(&result.stream);
        self.state.informational.remove(&result.stream);
        Some(Command::Progress)
    }
}

enum Input {
    Read(core::ReadCompletion),
    Write(WriteDone),
    Wake(core::WakeCompletion),
    Request(Option<Queued>),
    Event(Event),
    Control,
}

async fn next_input(state: &mut State, io: &mut impl Io, runnable: bool) -> Option<Input> {
    let request = async {
        match &state.requests {
            Some(requests) => requests.recv().await,
            None => std::future::pending().await,
        }
    };
    let event = state.events.recv();
    let graceful = state.shutdown.graceful.cancelled();
    let abort = state.shutdown.abort.cancelled();
    futures::pin_mut!(request, event, graceful, abort);
    futures::future::poll_fn(|cx| {
        for offset in 0..7 {
            let index = (state.rotation + offset) % 7;
            let ready = match index {
                0 => io.poll_read(cx).map(Input::Read),
                1 => io.poll_write(cx).map(Input::Write),
                2 => io.poll_wake(cx).map(Input::Wake),
                3 if state.requests_open && state.pending.is_none() => {
                    let result = if runnable {
                        match state.requests.as_ref().expect("client requests").try_recv() {
                            Ok(Some(request)) => Poll::Ready(Some(request)),
                            Err(_) => Poll::Ready(None),
                            Ok(None) => Poll::Pending,
                        }
                    } else {
                        request.as_mut().poll(cx).map(Result::ok)
                    };
                    result.map(Input::Request)
                }
                4 => {
                    let result = if runnable {
                        match state.events.try_recv() {
                            Ok(Some(event)) => Poll::Ready(event),
                            _ => Poll::Pending,
                        }
                    } else {
                        match event.as_mut().poll(cx) {
                            Poll::Ready(Ok(event)) => Poll::Ready(event),
                            _ => Poll::Pending,
                        }
                    };
                    result.map(Input::Event)
                }
                5 if state.policy == 0 && (!runnable || state.shutdown.graceful.is_cancelled()) => {
                    graceful.as_mut().poll(cx).map(|_| Input::Control)
                }
                6 if state.policy < 2 && (!runnable || state.shutdown.abort.is_cancelled()) => {
                    abort.as_mut().poll(cx).map(|_| Input::Control)
                }
                _ => Poll::Pending,
            };
            if let Poll::Ready(input) = ready {
                state.rotation = (index + 1) % 7;
                return Poll::Ready(Some(input));
            }
        }
        if runnable {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    })
    .await
}

async fn producer(
    mut source: futures::stream::LocalBoxStream<'static, Result<OutgoingFrame, Error>>,
    permits: Receiver<core::SendPermit>,
    cancel: Rc<CancellationToken>,
    events: SenderUnbounded<Event>,
    max_metadata: usize,
) -> Result<(), Error> {
    let work = async {
        loop {
            let permit = permits.recv().await.map_err(|_| Error::Closed)?;
            let produced = loop {
                match source.next().await {
                    None => break Produced::Data(Data::Static(&[]), true),
                    Some(Err(error)) => return Err(error),
                    Some(Ok(OutgoingFrame::Trailers(headers))) => {
                        let fields = metadata::trailers(headers);
                        if metadata::storage(&fields) > max_metadata {
                            return Err(Error::Limit);
                        }
                        break Produced::Trailers(fields);
                    }
                    Some(Ok(frame)) => {
                        let data = match frame {
                            OutgoingFrame::Data(data) => Data::Owned(data),
                            OutgoingFrame::Static(data) => Data::Static(data),
                            OutgoingFrame::Forward(chunk) => Data::Forward(chunk),
                            OutgoingFrame::Trailers(_) => unreachable!(),
                        };
                        data.check(permit.max_bytes(), permit.max_retained_capacity())?;
                        if data.as_ref().is_empty() {
                            operations::yield_cpu().await;
                            continue;
                        }
                        break Produced::Data(data, false);
                    }
                }
            };
            let terminal = matches!(&produced, Produced::Data(_, true) | Produced::Trailers(_));
            events
                .send(Event::Produced(permit, Ok(produced)))
                .map_err(|_| Error::Closed)?;
            if terminal {
                return Ok(());
            }
        }
    };
    futures::pin_mut!(work);
    let stopped = cancel.cancelled();
    futures::pin_mut!(stopped);
    match futures::future::select(stopped, work).await {
        futures::future::Either::Left(_) => Ok(()),
        futures::future::Either::Right((result, _)) => result,
    }
}
