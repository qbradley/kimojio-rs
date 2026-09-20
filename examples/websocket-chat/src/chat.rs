//! Pure HTTP / WebSocket / hub composition. Only the root executes effects.
use std::collections::VecDeque;
use std::mem::size_of;

use kimojio_fsm_http1 as http;
use kimojio_fsm_websocket as ws;

use crate::hub::{self, ClientId, DeliveryCompletion, DeliveryResult, SharedPayload};

pub type Buffer = Box<[u8]>;
type Http = http::Server<Buffer, SharedPayload>;
type WebSocket = ws::Server<Buffer, SharedPayload>;

#[derive(Clone, Debug)]
pub struct Config {
    pub hub: hub::Config,
    pub http: http::Config,
    pub websocket: ws::Config,
    pub receive_bytes: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hub: hub::Config::default(),
            http: http::Config {
                max_head_bytes: 8192,
                max_buffer_bytes: 16384,
                head_timeout_ns: Some(5_000_000_000),
                idle_timeout_ns: Some(5_000_000_000),
                ..http::Config::default()
            },
            websocket: ws::Config {
                write_timeout_ns: Some(5_000_000_000),
                close_timeout_ns: Some(1_000_000_000),
                ..ws::Config::default()
            },
            receive_bytes: 16384,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Identity {
    Http(http::OperationId),
    WebSocket(ws::OperationId),
}

#[derive(Debug)]
pub enum Io {
    HttpRead(http::ReadOp<Buffer>),
    HttpWrite(http::WriteOp<SharedPayload>),
    HttpReadiness(http::ReadinessOp),
    HttpClose(http::CloseOp),
    WsRead(ws::ReadOp<Buffer>),
    WsWrite(ws::WriteOp<SharedPayload>),
    WsReadiness(ws::ReadinessOp),
    WsClose(ws::CloseOp),
}

impl Io {
    pub fn identity(&self) -> Identity {
        match self {
            Self::HttpRead(op) => Identity::Http(op.id()),
            Self::HttpWrite(op) => Identity::Http(op.id()),
            Self::HttpReadiness(op) => Identity::Http(op.id()),
            Self::HttpClose(op) => Identity::Http(op.id()),
            Self::WsRead(op) => Identity::WebSocket(op.id()),
            Self::WsWrite(op) => Identity::WebSocket(op.id()),
            Self::WsReadiness(op) => Identity::WebSocket(op.id()),
            Self::WsClose(op) => Identity::WebSocket(op.id()),
        }
    }
    pub fn lane(&self) -> usize {
        match self {
            Self::HttpRead(_) | Self::WsRead(_) => 0,
            Self::HttpWrite(_) | Self::WsWrite(_) => 1,
            Self::HttpReadiness(op) => usize::from(op.direction == http::Direction::Write),
            Self::WsReadiness(op) => usize::from(op.direction == ws::Direction::Write),
            Self::HttpClose(_) | Self::WsClose(_) => 0,
        }
    }
    pub fn is_close(&self) -> bool {
        matches!(self, Self::HttpClose(_) | Self::WsClose(_))
    }
}

#[derive(Debug)]
pub enum Completion {
    HttpRead(http::ReadCompletion<Buffer>),
    HttpWrite(http::WriteCompletion<SharedPayload>),
    HttpReadiness(http::ReadinessCompletion),
    HttpClose(http::CloseCompletion),
    WsRead(ws::ReadCompletion<Buffer>),
    WsWrite(ws::WriteCompletion<SharedPayload>),
    WsReadiness(ws::ReadinessCompletion),
    WsClose(ws::CloseCompletion),
}

pub trait Ports {
    type Output;
    fn io(&mut self, client: ClientId, operation: Io) -> Option<Self::Output>;
    fn cancel(&mut self, client: ClientId, target: Identity) -> Option<Self::Output>;
    fn deadline_changed(&mut self, at: Option<http::Tick>) -> Option<Self::Output>;
    fn retired(&mut self, client: ClientId) -> Option<Self::Output>;
    fn yield_turn(&mut self) -> Option<Self::Output>;
}

enum Phase {
    Http(Box<Http>),
    WebSocket {
        server: Box<WebSocket>,
        delivery: Option<(ws::MessageId, hub::DeliveryId)>,
    },
}
struct Connection {
    id: ClientId,
    phase: Phase,
    ready: bool,
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Ready {
    Client(ClientId),
    Hub,
}
#[derive(Clone, Copy)]
enum Deadline {
    Http(http::Deadline),
    WebSocket(ws::Deadline),
}
impl Deadline {
    fn at(self) -> http::Tick {
        match self {
            Self::Http(d) => d.at,
            Self::WebSocket(d) => d.at,
        }
    }
}
#[derive(Clone, Copy)]
struct Scheduled {
    client: ClientId,
    deadline: Deadline,
}

pub struct Chat {
    config: Config,
    hub: hub::Hub,
    connections: Box<[Option<Connection>]>,
    ready: VecDeque<Ready>,
    hub_ready: bool,
    deadlines: Vec<Scheduled>,
    deadline_dirty: bool,
    now: http::Tick,
}

impl Chat {
    pub fn new(mut config: Config) -> Result<Self, hub::Error> {
        let maximum = config.hub.max_clients;
        if maximum == 0
            || maximum > 4096
            || config.receive_bytes == 0
            || config.receive_bytes > config.http.max_buffer_bytes
            || config.receive_bytes > config.websocket.max_buffer_bytes
            || config.hub.max_message_bytes > config.websocket.max_buffer_bytes
            || config.http.max_head_bytes > 8192
            || config.hub.max_message_bytes as u64 != config.websocket.max_message_bytes
        {
            return Err(hub::Error::InvalidConfig);
        }
        let probe = http::ConnectionId {
            slot: 0,
            generation: 0,
        };
        if http::Server::<[u8; 1], SharedPayload>::with_output_type(
            probe,
            config.http.clone(),
            [0],
            http::Tick(0),
        )
        .is_err()
            || ws::Server::<[u8; 1], SharedPayload>::new(
                probe,
                config.websocket.clone(),
                [0],
                http::Tick(0),
            )
            .is_err()
        {
            return Err(hub::Error::InvalidConfig);
        }
        let ready = VecDeque::with_capacity(maximum + 1);
        let deadlines = Vec::with_capacity(maximum);
        let fixed = maximum
            .checked_mul(size_of::<Option<Connection>>())
            .and_then(|n| n.checked_add(ready.capacity() * size_of::<Ready>()))
            .and_then(|n| n.checked_add(deadlines.capacity() * size_of::<Scheduled>()))
            .and_then(|n| n.checked_add(size_of::<Self>()))
            .ok_or(hub::Error::InvalidConfig)?;
        config.hub.external_fixed_bytes = config
            .hub
            .external_fixed_bytes
            .checked_add(fixed)
            .ok_or(hub::Error::InvalidConfig)?;
        // HTTP head/token vectors can grow transiently and coexist with an
        // outgoing handshake head. Reserve their bounded worst-case storage,
        // plus both boxed protocol states during the upgrade transition.
        let protocol = config
            .receive_bytes
            .checked_add(16 * config.http.max_head_bytes)
            .and_then(|n| n.checked_add(size_of::<Http>() + size_of::<WebSocket>()))
            .ok_or(hub::Error::InvalidConfig)?;
        config.hub.external_bytes_per_client = config
            .hub
            .external_bytes_per_client
            .checked_add(protocol)
            .ok_or(hub::Error::InvalidConfig)?;
        let hub = hub::Hub::new(1, config.hub.clone())?;
        Ok(Self {
            config,
            hub,
            connections: hub::empty_slots(maximum),
            ready,
            hub_ready: false,
            deadlines,
            deadline_dirty: false,
            now: http::Tick(0),
        })
    }
    pub fn stats(&self) -> hub::Stats {
        self.hub.stats()
    }
    pub fn can_admit(&self) -> bool {
        self.hub.can_admit()
    }
    pub fn deadline(&self) -> Option<http::Tick> {
        self.deadlines.first().map(|entry| entry.deadline.at())
    }

    pub fn admit(&mut self) -> Result<ClientId, hub::Error> {
        let id = self.hub.admit()?;
        let connection = http::ConnectionId {
            slot: id.slot() as u64,
            generation: id.generation(),
        };
        let server = match Http::with_output_type(
            connection,
            self.config.http.clone(),
            vec![0; self.config.receive_bytes].into_boxed_slice(),
            self.now,
        ) {
            Ok(server) => server,
            Err(_) => {
                self.hub.closed(id)?;
                return Err(hub::Error::InvalidConfig);
            }
        };
        self.connections[id.slot()] = Some(Connection {
            id,
            phase: Phase::Http(Box::new(server)),
            ready: false,
        });
        self.mark_client(id);
        Ok(id)
    }
    fn client_mut(&mut self, id: ClientId) -> Option<&mut Connection> {
        self.connections
            .get_mut(id.slot())
            .and_then(Option::as_mut)
            .filter(|client| client.id == id)
    }
    fn mark_client(&mut self, id: ClientId) {
        if let Some(client) = self.client_mut(id)
            && !client.ready
        {
            client.ready = true;
            assert!(self.ready.len() < self.ready.capacity());
            self.ready.push_back(Ready::Client(id));
        }
    }
    fn mark_hub(&mut self) {
        if !self.hub_ready {
            self.hub_ready = true;
            assert!(self.ready.len() < self.ready.capacity());
            self.ready.push_back(Ready::Hub);
        }
    }
    fn schedule(&mut self, id: ClientId, deadline: Option<Deadline>) {
        let before = self.deadline();
        self.deadlines.retain(|entry| entry.client != id);
        if let Some(deadline) = deadline {
            let position = self
                .deadlines
                .partition_point(|entry| entry.deadline.at() <= deadline.at());
            assert!(self.deadlines.len() < self.deadlines.capacity());
            self.deadlines.insert(
                position,
                Scheduled {
                    client: id,
                    deadline,
                },
            );
        }
        self.deadline_dirty |= before != self.deadline();
    }
    pub fn observe_time(&mut self, now: http::Tick) {
        assert!(now >= self.now);
        self.now = now;
    }
    pub fn expire_due(&mut self, now: http::Tick) {
        self.observe_time(now);
        while self.deadline().is_some_and(|at| at <= now) {
            let entry = self.deadlines.remove(0);
            if let Some(client) = self.client_mut(entry.client) {
                match (&mut client.phase, entry.deadline) {
                    (Phase::Http(server), Deadline::Http(deadline)) => {
                        let _ = server.expire(deadline, now);
                    }
                    (Phase::WebSocket { server, .. }, Deadline::WebSocket(deadline)) => {
                        let _ = server.expire(deadline, now);
                    }
                    _ => {}
                }
            }
            self.mark_client(entry.client);
            self.deadline_dirty = true;
        }
    }
    pub fn shutdown(&mut self, abort: bool) {
        for slot in 0..self.connections.len() {
            if let Some(client) = self.connections[slot].as_mut() {
                let id = client.id;
                match &mut client.phase {
                    Phase::Http(server) => server.shutdown(if abort {
                        http::ShutdownMode::Abort
                    } else {
                        http::ShutdownMode::Graceful
                    }),
                    Phase::WebSocket { server, .. } if abort => {
                        server.abort(ws::Failure::Cancelled)
                    }
                    Phase::WebSocket { server, .. } => {
                        let _ = server.close(ws::CloseReason::new(1001, "").unwrap());
                    }
                }
                let _ = self.hub.remove(id, 1001);
                self.mark_client(id);
            }
        }
        self.mark_hub();
    }
    #[allow(
        clippy::result_large_err,
        reason = "rejected operations return ownership without allocating"
    )]
    pub fn complete(&mut self, id: ClientId, completion: Completion) -> Result<(), Completion> {
        let now = self.now;
        let Some(client) = self.client_mut(id) else {
            return Err(completion);
        };
        let result = match (&mut client.phase, completion) {
            (Phase::Http(server), Completion::HttpRead(c)) => {
                let _ = server.observe_time(now);
                server
                    .complete_read(c)
                    .map_err(|e| Completion::HttpRead(e.value))
            }
            (Phase::Http(server), Completion::HttpWrite(c)) => {
                let _ = server.observe_time(now);
                server
                    .complete_write(c)
                    .map_err(|e| Completion::HttpWrite(e.value))
            }
            (Phase::Http(server), Completion::HttpReadiness(c)) => server
                .complete_readiness(c)
                .map_err(|e| Completion::HttpReadiness(e.value)),
            (Phase::Http(server), Completion::HttpClose(c)) => server
                .complete_close(c)
                .map_err(|e| Completion::HttpClose(e.value)),
            (Phase::WebSocket { server, .. }, Completion::WsRead(c)) => {
                let _ = server.observe_time(now);
                server
                    .complete_read(c)
                    .map_err(|e| Completion::WsRead(e.value))
            }
            (Phase::WebSocket { server, .. }, Completion::WsWrite(c)) => {
                let _ = server.observe_time(now);
                server
                    .complete_write(c)
                    .map_err(|e| Completion::WsWrite(e.value))
            }
            (Phase::WebSocket { server, .. }, Completion::WsReadiness(c)) => server
                .complete_readiness(c)
                .map_err(|e| Completion::WsReadiness(e.value)),
            (Phase::WebSocket { server, .. }, Completion::WsClose(c)) => server
                .complete_close(c)
                .map_err(|e| Completion::WsClose(e.value)),
            (_, completion) => Err(completion),
        };
        self.mark_client(id);
        result
    }
    fn deliver(&mut self, delivery: hub::Delivery) {
        let id = delivery.id.client();
        let kind = match delivery.payload.kind() {
            hub::Kind::Text => ws::MessageKind::Text,
            hub::Kind::Binary => ws::MessageKind::Binary,
        };
        let command = ws::SendMessage {
            kind,
            range: 0..delivery.payload.as_ref().len(),
            buffer: delivery.payload,
        };
        let rejected = match self.client_mut(id).map(|client| &mut client.phase) {
            Some(Phase::WebSocket {
                server,
                delivery: pending,
            }) => match server.send_message(command) {
                Ok(message) => {
                    assert!(pending.is_none(), "one hub delivery per recipient");
                    *pending = Some((message, delivery.id));
                    None
                }
                Err(rejected) => Some(rejected.value.buffer),
            },
            _ => Some(command.buffer),
        };
        if let Some(payload) = rejected {
            // Admission is revocable: this is a normal typed rejection, even
            // when the protocol reported can_send before the hub issued work.
            self.hub
                .complete(DeliveryCompletion {
                    id: delivery.id,
                    payload,
                    result: DeliveryResult::Failed,
                })
                .unwrap();
            self.mark_hub();
        }
        self.mark_client(id);
    }
    fn close_recipient(&mut self, close: hub::Close) {
        if let Some(client) = self.client_mut(close.client) {
            match &mut client.phase {
                Phase::Http(server) => server.shutdown(http::ShutdownMode::Abort),
                Phase::WebSocket { server, .. } if !server.is_closing() => {
                    if server
                        .close(ws::CloseReason::new(close.code, "").unwrap())
                        .is_err()
                    {
                        server.abort(ws::Failure::Application);
                    }
                }
                Phase::WebSocket { .. } => {}
            }
        }
        self.mark_client(close.client);
    }

    pub fn next<P: Ports>(&mut self, ports: &mut P) -> Option<P::Output> {
        loop {
            for _ in 0..64 {
                if self.deadline_dirty {
                    self.deadline_dirty = false;
                    if let Some(output) = ports.deadline_changed(self.deadline()) {
                        return Some(output);
                    }
                }
                let ready = self.ready.pop_front()?;
                let id = match ready {
                    Ready::Hub => {
                        self.hub_ready = false;
                        match self.hub.next(&mut HubPorts) {
                            Some(HubEvent::Send(delivery)) => {
                                self.mark_hub();
                                self.deliver(delivery);
                            }
                            Some(HubEvent::Close(close)) => {
                                self.mark_hub();
                                self.close_recipient(close);
                            }
                            Some(HubEvent::Yield) => {
                                self.mark_hub();
                                if let Some(output) = ports.yield_turn() {
                                    return Some(output);
                                }
                            }
                            None => {}
                        }
                        continue;
                    }
                    Ready::Client(id) => id,
                };
                let now = self.now;
                let Some(client) = self.client_mut(id) else {
                    continue;
                };
                client.ready = false;
                let event = match &mut client.phase {
                    Phase::Http(server) => {
                        let _ = server.observe_time(now);
                        server.next(&mut HttpPorts)
                    }
                    Phase::WebSocket { server, .. } => {
                        let _ = server.observe_time(now);
                        server.next(&mut WsPorts)
                    }
                };
                if matches!(&client.phase, Phase::WebSocket { server, .. } if server.is_closing()) {
                    let _ = self.hub.remove(id, 1000);
                    self.mark_hub();
                }
                let Some(event) = event else { continue };
                self.mark_client(id);
                let output = match event {
                    Event::Io(operation) => ports.io(id, operation),
                    Event::Cancel(target) => ports.cancel(id, target),
                    Event::HttpDeadline(deadline) => {
                        self.schedule(id, deadline.map(Deadline::Http));
                        None
                    }
                    Event::WsDeadline(deadline) => {
                        self.schedule(id, deadline.map(Deadline::WebSocket));
                        None
                    }
                    Event::Request(exchange, handshake) => {
                        if let Phase::Http(server) = &mut self.client_mut(id).unwrap().phase {
                            match handshake {
                                Ok(handshake) => {
                                    if handshake.accept(server, exchange).is_err() {
                                        server.shutdown(http::ShutdownMode::Abort);
                                    }
                                }
                                Err(error) => {
                                    let _ = error.respond(server, exchange);
                                    server.shutdown(http::ShutdownMode::Graceful);
                                }
                            }
                        }
                        None
                    }
                    Event::Upgrade => {
                        let websocket_config = self.config.websocket.clone();
                        let client = self.client_mut(id).unwrap();
                        if let Phase::Http(server) = &mut client.phase {
                            match server.take_upgrade() {
                                Ok(handoff) => {
                                    let websocket = ws::Server::from_handoff(
                                        handoff,
                                        websocket_config,
                                        now,
                                    )
                                    .expect(
                                        "configuration and receive length checked before admission",
                                    );
                                    client.phase = Phase::WebSocket {
                                        server: Box::new(websocket),
                                        delivery: None,
                                    };
                                    self.schedule(id, None);
                                    self.hub.activate(id).unwrap();
                                }
                                Err(_) => server.shutdown(http::ShutdownMode::Abort),
                            }
                        }
                        None
                    }
                    Event::Started(info) => {
                        let kind = match info.kind {
                            ws::MessageKind::Text => hub::Kind::Text,
                            ws::MessageKind::Binary => hub::Kind::Binary,
                        };
                        if self.hub.begin(id, kind).is_err() {
                            self.close_recipient(hub::Close {
                                client: id,
                                code: 1008,
                            });
                        }
                        None
                    }
                    Event::Chunk(chunk) => {
                        let _ = self.hub.append(id, chunk.bytes());
                        if let Phase::WebSocket { server, .. } =
                            &mut self.client_mut(id).unwrap().phase
                        {
                            server.release_chunk(chunk.release()).unwrap();
                        }
                        self.mark_hub();
                        None
                    }
                    Event::Finished => {
                        let _ = self.hub.finish(id);
                        self.mark_hub();
                        None
                    }
                    Event::Sent(receipt) => {
                        let Phase::WebSocket { delivery, .. } =
                            &mut self.client_mut(id).unwrap().phase
                        else {
                            unreachable!("WebSocket receipt belongs to WebSocket phase")
                        };
                        let (message, delivery) =
                            delivery.take().expect("one owned hub delivery per WS send");
                        assert_eq!(message, receipt.id);
                        let result = if receipt.result.is_ok()
                            && receipt.accepted == receipt.buffer.as_ref().len()
                            && receipt.acceptance == http::Acceptance::Exact
                        {
                            DeliveryResult::Sent
                        } else {
                            DeliveryResult::Failed
                        };
                        self.hub
                            .complete(DeliveryCompletion {
                                id: delivery,
                                payload: receipt.buffer,
                                result,
                            })
                            .unwrap();
                        self.mark_hub();
                        None
                    }
                    Event::PeerClosed => {
                        let _ = self.hub.remove(id, 1000);
                        self.mark_hub();
                        None
                    }
                    Event::HttpBody(body) => {
                        let length = body.bytes().len();
                        if let Phase::Http(server) = &mut self.client_mut(id).unwrap().phase {
                            server.release_body(body.release(length)).unwrap();
                        }
                        None
                    }
                    Event::Closed => {
                        assert!(
                            !matches!(
                                &self.client_mut(id).unwrap().phase,
                                Phase::WebSocket {
                                    delivery: Some(_),
                                    ..
                                }
                            ),
                            "protocol close follows every delivery receipt"
                        );
                        self.schedule(id, None);
                        self.ready.retain(|ready| *ready != Ready::Client(id));
                        self.connections[id.slot()] = None;
                        self.hub.closed(id).unwrap();
                        ports.retired(id)
                    }
                };
                if output.is_some() {
                    return output;
                }
            }
            if let Some(output) = ports.yield_turn() {
                return Some(output);
            }
        }
    }
}

enum HubEvent {
    Send(hub::Delivery),
    Close(hub::Close),
    Yield,
}
struct HubPorts;
impl hub::Ports for HubPorts {
    type Output = HubEvent;
    fn send(&mut self, delivery: hub::Delivery) -> Option<Self::Output> {
        Some(HubEvent::Send(delivery))
    }
    fn close(&mut self, close: hub::Close) -> Option<Self::Output> {
        Some(HubEvent::Close(close))
    }
    fn yield_turn(&mut self) -> Option<Self::Output> {
        Some(HubEvent::Yield)
    }
}

enum Event {
    Io(Io),
    Cancel(Identity),
    HttpDeadline(Option<http::Deadline>),
    WsDeadline(Option<ws::Deadline>),
    Request(http::ExchangeId, Result<ws::Handshake, ws::HandshakeError>),
    Upgrade,
    Started(ws::MessageInfo),
    Chunk(ws::ChunkOp<Buffer>),
    Finished,
    Sent(ws::MessageSent<SharedPayload>),
    PeerClosed,
    HttpBody(http::BodyOp<Buffer>),
    Closed,
}
struct HttpPorts;
impl http::Ports<Buffer, SharedPayload> for HttpPorts {
    type Output = Event;
    fn read(&mut self, op: http::ReadOp<Buffer>) -> Option<Event> {
        Some(Event::Io(Io::HttpRead(op)))
    }
    fn write(&mut self, op: http::WriteOp<SharedPayload>) -> Option<Event> {
        Some(Event::Io(Io::HttpWrite(op)))
    }
    fn readiness(&mut self, op: http::ReadinessOp) -> Option<Event> {
        Some(Event::Io(Io::HttpReadiness(op)))
    }
    fn cancel(&mut self, op: http::CancelOp) -> Option<Event> {
        Some(Event::Cancel(Identity::Http(op.target)))
    }
    fn close(&mut self, op: http::CloseOp) -> Option<Event> {
        Some(Event::Io(Io::HttpClose(op)))
    }
    fn body(&mut self, op: http::BodyOp<Buffer>) -> Option<Event> {
        Some(Event::HttpBody(op))
    }
    fn trailers(&mut self, _: http::ExchangeId, _: http::Headers<'_>) -> Option<Event> {
        None
    }
    fn incoming_finished(&mut self, _: http::ExchangeId) -> Option<Event> {
        None
    }
    fn send_ready(&mut self, _: http::ExchangeId, _: usize) -> Option<Event> {
        None
    }
    fn body_sent(&mut self, _: http::BodySent<SharedPayload>) -> Option<Event> {
        unreachable!("handshakes have no response body")
    }
    fn exchange_finished(&mut self, _: http::ExchangeFinished) -> Option<Event> {
        None
    }
    fn deadline_changed(&mut self, d: Option<http::Deadline>) -> Option<Event> {
        Some(Event::HttpDeadline(d))
    }
    fn upgrade_ready(&mut self, _: http::ExchangeId) -> Option<Event> {
        Some(Event::Upgrade)
    }
    fn closed(&mut self, _: http::ConnectionResult) -> Option<Event> {
        Some(Event::Closed)
    }
}
impl http::ServerPorts<Buffer, SharedPayload> for HttpPorts {
    fn request(
        &mut self,
        exchange: http::ExchangeId,
        head: http::RequestHead<'_>,
    ) -> Option<Event> {
        Some(Event::Request(exchange, ws::Handshake::validate(head)))
    }
}
struct WsPorts;
impl ws::Ports<Buffer, SharedPayload> for WsPorts {
    type Output = Event;
    fn read(&mut self, op: ws::ReadOp<Buffer>) -> Option<Event> {
        Some(Event::Io(Io::WsRead(op)))
    }

    fn write(&mut self, op: ws::WriteOp<SharedPayload>) -> Option<Event> {
        Some(Event::Io(Io::WsWrite(op)))
    }
    fn readiness(&mut self, op: ws::ReadinessOp) -> Option<Event> {
        Some(Event::Io(Io::WsReadiness(op)))
    }
    fn cancel(&mut self, op: ws::CancelOp) -> Option<Event> {
        Some(Event::Cancel(Identity::WebSocket(op.target)))
    }
    fn close(&mut self, op: ws::CloseOp) -> Option<Event> {
        Some(Event::Io(Io::WsClose(op)))
    }
    fn message_started(&mut self, info: ws::MessageInfo) -> Option<Event> {
        Some(Event::Started(info))
    }
    fn chunk(&mut self, op: ws::ChunkOp<Buffer>) -> Option<Event> {
        Some(Event::Chunk(op))
    }
    fn message_finished(&mut self, _: ws::MessageInfo) -> Option<Event> {
        Some(Event::Finished)
    }
    fn message_sent(&mut self, receipt: ws::MessageSent<SharedPayload>) -> Option<Event> {
        Some(Event::Sent(receipt))
    }
    fn peer_closed(&mut self, _: ws::CloseReason) -> Option<Event> {
        Some(Event::PeerClosed)
    }
    fn deadline_changed(&mut self, d: Option<ws::Deadline>) -> Option<Event> {
        Some(Event::WsDeadline(d))
    }
    fn closed(&mut self, _: ws::ConnectionResult) -> Option<Event> {
        Some(Event::Closed)
    }
}

#[cfg(test)]
mod tests;
