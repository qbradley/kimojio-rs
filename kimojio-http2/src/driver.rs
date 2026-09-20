use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    future::Future,
    ops::Deref,
    rc::Rc,
    task::Poll,
    time::Duration,
};

use futures::{FutureExt, StreamExt};
use http::{Request, Response};
use kimojio::{
    CancellationToken, Receiver, ReceiverUnbounded, Sender, SenderOneshot, SenderUnbounded,
    async_channel, async_channel_unbounded, oneshot, operations,
};
use kimojio_fsm_http2 as core;

use crate::{
    BodyChunk, Error, IncomingBody, IncomingFrame, OutgoingBody, OutgoingFrame,
    body::{Data, Delivery, Source},
    io::{self, Io, WriteDone},
    metadata,
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
            max_streams: 128,
            turn_budget: 64,
        }
    }
}

/// Monotonic cooperative shutdown. Abort supersedes graceful shutdown.
#[derive(Clone)]
pub struct Shutdown {
    policy: Rc<Cell<u8>>,
    notified: Rc<Cell<bool>>,
    events: SenderUnbounded<Event>,
}

impl Shutdown {
    pub fn graceful(&self) {
        self.set(1);
    }
    pub fn abort(&self) {
        self.set(2);
    }

    fn set(&self, policy: u8) {
        if policy > self.policy.get() {
            self.policy.set(policy);
            if !self.notified.replace(true) {
                let _ = self.events.send(Event::Control);
            }
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
}

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
        self.shutdown.policy.set(2);
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
        if self.shutdown.policy.get() != 0 {
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
        let control = Rc::new(RequestControl {
            cancelled: Cell::new(false),
            retired: Cell::new(false),
            stream: Cell::new(None),
        });
        let (response, receive) = oneshot();
        self.requests
            .send(Queued {
                fields,
                body,
                response,
                control: control.clone(),
                _reservation: reservation,
            })
            .map_err(|_| Error::Closed)?;
        let mut guard = CancelSend {
            control: Some(control),
            events: self.shutdown.events.clone(),
        };
        let result = receive.recv().await.map_err(|_| Error::Closed)?;
        if result.is_ok() {
            guard.control.take();
        }
        result
    }

    pub fn control(&self) -> Shutdown {
        self.shutdown.clone()
    }
}

/// The caller must keep `run` polled through stream retirement.
pub struct NativeConnection {
    fd: kimojio::OwnedFd,
    config: Config,
    requests: Requests,
    events: Events,
    event_send: SenderUnbounded<Event>,
    shutdown: Shutdown,
}

/// Uses an established descriptor. The peer must speak HTTP/2 prior knowledge.
pub fn connect_native(fd: kimojio::OwnedFd, config: Config) -> (Client, NativeConnection) {
    let (requests, receive) = async_channel_unbounded();
    let (event_send, events) = async_channel_unbounded();
    let shutdown = Shutdown {
        policy: Rc::new(Cell::new(0)),
        notified: Rc::new(Cell::new(false)),
        events: event_send.clone(),
    };
    let client = Client {
        requests,
        shutdown: shutdown.clone(),
        budget: Rc::new(Budget::default()),
        config: Rc::new(config.clone()),
    };
    (
        client,
        NativeConnection {
            fd,
            config,
            requests: Requests {
                receive,
                shutdown: shutdown.clone(),
            },
            events: Events {
                receive: events,
                send: event_send.clone(),
            },
            event_send,
            shutdown,
        },
    )
}

impl NativeConnection {
    /// Returns only after descriptor closure, producer settlement and retirement.
    ///
    /// Held body chunks remain readable after close but delay this return.
    pub async fn run(self) -> Result<(), Error> {
        operations::io_scope(async move || {
            let Self {
                fd,
                config,
                requests,
                events,
                event_send,
                shutdown,
            } = self;
            if config.turn_budget == 0
                || config.max_streams == 0
                || config.max_queued_requests == 0
                || config.max_queued_storage == 0
            {
                operations::close(fd).await.map_err(Error::Transport)?;
                return Err(Error::Limit);
            }
            let mut machine =
                match core::Client::<Data>::new(config.protocol.clone(), Duration::ZERO) {
                    Ok(machine) => machine,
                    Err(error) => {
                        operations::close(fd).await.map_err(Error::Transport)?;
                        return Err(error.into());
                    }
                };
            let epoch = kimojio::clock_now();
            let mut io = io::native(fd, epoch);
            let mut state = State::new(config, requests, events, event_send, shutdown);
            let mut turns = 0;
            loop {
                machine.advance_time(kimojio::clock_now().saturating_duration_since(epoch))?;
                state.control(&mut machine);
                state.admit(&mut machine);
                let output = machine.next(&mut Ports {
                    state: &mut state,
                    io: &mut io,
                });
                let mut runnable = output.is_some();
                if let Some(output) = output {
                    state.command(output, &mut machine);
                } else {
                    // ReceiveEnd can follow the delivery that caused a body drop.
                    runnable = state.abandon(&mut machine);
                }
                if state.closed.is_some() && state.streams.is_empty() && state.producers.is_empty()
                {
                    state.reject_queued();
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
        })
        .await
    }
}

pub(crate) enum Event {
    Release(core::BodyRelease),
    Cancel(Rc<RequestControl>),
    Abandon(core::StreamId),
    Produced(core::SendPermit, Result<Produced, Error>),
    ProducerDone(core::StreamId, Result<(), Error>),
    Control,
}

pub(crate) enum Produced {
    Data(Data, bool),
    Trailers(Vec<core::H2HeaderField>),
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
    frames: SenderUnbounded<Delivery>,
    queued: Rc<ReceiverUnbounded<Delivery>>,
    received: Rc<Cell<Option<core::StreamOutcome>>>,
    completion: Option<SenderOneshot<Result<core::StreamOutcome, Error>>>,
    upload: Upload,
    cancel: Rc<CancellationToken>,
    error: Option<Error>,
}

struct Producer {
    handle: operations::TaskHandle<()>,
    cancel: Rc<CancellationToken>,
}

struct State {
    config: Config,
    requests: Requests,
    requests_open: bool,
    pending: Option<Queued>,
    admission: bool,
    events: Events,
    event_send: SenderUnbounded<Event>,
    streams: BTreeMap<core::StreamId, Stream>,
    producers: BTreeMap<core::StreamId, Producer>,
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
        requests: Requests,
        events: Events,
        event_send: SenderUnbounded<Event>,
        shutdown: Shutdown,
    ) -> Self {
        Self {
            config,
            requests,
            requests_open: true,
            pending: None,
            admission: true,
            events,
            event_send,
            streams: BTreeMap::new(),
            producers: BTreeMap::new(),
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

    fn control(&mut self, machine: &mut core::Client<Data>) {
        let policy = self.shutdown.policy.get();
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
        while let Ok(Some(request)) = self.requests.try_recv() {
            request.control.retired.set(true);
            let _ = request.response.send(Err(Error::Closed));
        }
    }

    fn admit(&mut self, machine: &mut core::Client<Data>) {
        if self.trailer_retry {
            self.trailer_retry = false;
            let trailers = std::mem::take(&mut self.trailers);
            for (id, fields) in trailers {
                self.submit_trailers(machine, id, fields);
            }
        }
        if self.policy != 0 || self.closed.is_some() {
            self.reject_queued();
            return;
        }
        if self.streams.len().max(self.producers.len()) >= self.config.max_streams {
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
        match machine.request_ref(&metadata::borrowed(&request.fields), end) {
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
                    control: request.control.clone(),
                };
                let cancel = Rc::new(CancellationToken::new());
                let upload = if end {
                    Upload::Empty
                } else {
                    match request.body.source {
                        Source::Empty => Upload::Empty,
                        Source::Full(data) => Upload::Ready(Some(data)),
                        Source::Stream(source) => {
                            let (send, permits) = async_channel();
                            let events = self.event_send.clone();
                            let stop = cancel.clone();
                            let max_metadata = self.config.max_trailer_storage;
                            let handle = operations::spawn_task(async move {
                                let produced = events.clone();
                                let result = std::panic::AssertUnwindSafe(operations::io_scope(
                                    async move || {
                                        producer(source, permits, stop, produced, max_metadata)
                                            .await
                                    },
                                ))
                                .catch_unwind()
                                .await
                                .unwrap_or_else(|_| {
                                    Err(Error::Application("body producer panicked".into()))
                                });
                                let _ = events.send(Event::ProducerDone(id, result));
                            });
                            self.producers.insert(
                                id,
                                Producer {
                                    handle,
                                    cancel: cancel.clone(),
                                },
                            );
                            Upload::Producer(send)
                        }
                    }
                };
                self.streams.insert(
                    id,
                    Stream {
                        control: request.control,
                        response: Some(request.response),
                        incoming: Some(incoming),
                        frames,
                        queued: receive,
                        received,
                        completion: Some(completion),
                        upload,
                        cancel,
                        error: None,
                    },
                );
            }
        }
    }

    fn fail(&mut self, machine: &mut core::Client<Data>, id: core::StreamId, error: Error) {
        if let Some(stream) = self.streams.get_mut(&id) {
            stream.error.get_or_insert(error.clone());
            stream.cancel.cancel();
            if let Some(response) = stream.response.take() {
                let _ = response.send(Err(error));
            }
            stream.incoming.take();
            let _ = machine.reset(id, core::H2ErrorCode::Cancel);
        }
        self.trailers.remove(&id);
    }

    fn abandon(&mut self, machine: &mut core::Client<Data>) -> bool {
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
        machine: &mut core::Client<Data>,
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
        machine: &mut core::Client<Data>,
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

    fn command(&mut self, command: Command, machine: &mut core::Client<Data>) {
        match command {
            Command::Progress => {}
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

    async fn input(&mut self, input: Input, machine: &mut core::Client<Data>) -> Result<(), Error> {
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
            Input::Event(Event::Control) => self.shutdown.notified.set(false),
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
                }
                if let Err(error) = result {
                    self.fail(machine, id, error);
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
        for stream in self.streams.values_mut() {
            stream.control.retired.set(true);
            stream.cancel.cancel();
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
    fn send_stopped(&mut self, id: core::StreamId, _reason: core::SendStop) -> Option<Command> {
        if let Some(stream) = self.state.streams.get_mut(&id) {
            stream.cancel.cancel();
            stream.upload = Upload::Empty;
        }
        self.state.trailers.remove(&id);
        Some(Command::Progress)
    }
    fn sent(&mut self, result: core::Sent<Data>) -> Option<Command> {
        if let Err(reason) = result.result
            && let Some(stream) = self.state.streams.get_mut(&result.stream)
        {
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
        let Some(stream) = self.state.streams.get_mut(&head.stream) else {
            return Some(Command::Progress);
        };
        match head.kind {
            core::HeadKind::Response(status) => {
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
            core::HeadKind::Informational(_) => {}
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
        }
        Some(Command::Progress)
    }
    fn retired(&mut self, result: core::StreamResult) -> Option<Command> {
        if let Some(mut stream) = self.state.streams.remove(&result.stream) {
            stream.control.retired.set(true);
            stream.cancel.cancel();
            stream.frames.close();
            if let Some(done) = stream.completion.take() {
                let outcome = if let Some(error) = stream.error {
                    Err(error)
                } else if result.outcome == core::StreamOutcome::Complete {
                    Ok(result.outcome)
                } else {
                    Err(Error::Stream(result.outcome))
                };
                let _ = done.send(outcome);
            }
        }
        self.state.trailers.remove(&result.stream);
        Some(Command::Progress)
    }
}

enum Input {
    Read(core::ReadCompletion),
    Write(WriteDone),
    Wake(core::WakeCompletion),
    Request(Option<Queued>),
    Event(Event),
}

async fn next_input(state: &mut State, io: &mut impl Io, runnable: bool) -> Option<Input> {
    let request = state.requests.recv();
    let event = state.events.recv();
    futures::pin_mut!(request, event);
    futures::future::poll_fn(|cx| {
        for offset in 0..5 {
            let index = (state.rotation + offset) % 5;
            let ready = match index {
                0 => io.poll_read(cx).map(Input::Read),
                1 => io.poll_write(cx).map(Input::Write),
                2 => io.poll_wake(cx).map(Input::Wake),
                3 if state.requests_open && state.pending.is_none() => {
                    let result = if runnable {
                        match state.requests.try_recv() {
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
                _ => Poll::Pending,
            };
            if let Poll::Ready(input) = ready {
                state.rotation = (index + 1) % 5;
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
