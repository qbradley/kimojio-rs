use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ops::{Deref, DerefMut},
    rc::Rc,
    time::Duration,
};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    H2HeaderField, HttpLimits,
    api::*,
    server::h2::{client::H2Client, events::*, flow::*, headers::*, server::H2Server, wire::*},
};

const PAGE_SIZE: usize = 32 * 1024;
const WINDOW_UPDATE_BYTES: usize = 13;

#[derive(Clone, Debug)]
pub struct Config {
    pub http: HttpLimits,
    pub stream_receive_window: u32,
    pub connection_receive_window: u32,
    pub max_receive_capacity: usize,
    pub max_stream_receive_capacity: usize,
    pub max_stream_fragments: usize,
    pub max_send_capacity: usize,
    pub max_send_buffer_capacity: usize,
    pub max_send_buffer_bytes: usize,
    pub max_outbound_items: usize,
    pub max_outbound_capacity: usize,
    pub turn_budget: usize,
    pub settings_timeout: Duration,
    pub shutdown_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            http: HttpLimits::new(),
            stream_receive_window: 65_535,
            connection_receive_window: 1024 * 1024,
            max_receive_capacity: 8 * 1024 * 1024,
            max_stream_receive_capacity: 256 * 1024,
            max_stream_fragments: 128,
            max_send_capacity: 8 * 1024 * 1024,
            max_send_buffer_capacity: 64 * 1024,
            max_send_buffer_bytes: 64 * 1024,
            max_outbound_items: 512,
            max_outbound_capacity: 2 * 1024 * 1024,
            turn_budget: 128,
            settings_timeout: Duration::from_secs(10),
            shutdown_timeout: Duration::from_secs(30),
        }
    }
}

impl Config {
    fn validate(&self) -> Result<(), CommandError> {
        if self.stream_receive_window > 0x7fff_ffff
            || !(65_535..=0x7fff_ffff).contains(&self.connection_receive_window)
            || self.max_receive_capacity < 2 * PAGE_SIZE
            || self.connection_receive_window as usize
                > self.max_receive_capacity.saturating_sub(2 * PAGE_SIZE)
            || self.max_stream_receive_capacity < PAGE_SIZE
            || self.max_stream_fragments == 0
            || self.max_send_buffer_capacity == 0
            || self.max_send_buffer_capacity > self.max_send_capacity
            || self.max_send_buffer_bytes == 0
            || self.max_outbound_items < 4
            || self.max_outbound_capacity < 65_536
            || self.http.max_active_streams() == 0
            || self.turn_budget == 0
            || self.settings_timeout.is_zero()
            || self.shutdown_timeout.is_zero()
        {
            return Err(CommandError::Capacity);
        }
        Ok(())
    }
}

enum Protocol {
    Client(H2Client),
    Server(H2Server),
}

impl Protocol {
    fn connection_send_available(&self) -> usize {
        match self {
            Self::Client(role) => role.endpoint.connection_send_available(),
            Self::Server(role) => role.endpoint.connection_send_available(),
        }
    }
    fn now(&mut self, now: Duration) {
        match self {
            Self::Client(role) => {
                role.endpoint.now = now;
                role.endpoint.control_budget.refill_if_due(now);
            }
            Self::Server(role) => {
                role.endpoint.now = now;
                role.endpoint.control_budget.refill_if_due(now);
            }
        }
    }
    fn fields(&self) -> Result<H2RawHeaderBlockRef<'_>, crate::ServerError> {
        match self {
            Self::Client(role) => role.compact_header_block(),
            Self::Server(role) => role.compact_header_block(),
        }
    }
    fn take_block(&mut self) -> Option<Vec<u8>> {
        match self {
            Self::Client(role) => role.endpoint.outbound_queue.blocks.pop_front(),
            Self::Server(role) => role.endpoint.outbound_queue.blocks.pop_front(),
        }
        .map(|block| block.bytes)
    }
    fn prepare_data(
        &self,
        id: StreamId,
        bytes: usize,
        end: bool,
    ) -> Result<Option<H2DataFramePlan>, crate::ServerError> {
        match self {
            Self::Client(role) => role.prepare_data_frame(id.0, bytes, end),
            Self::Server(role) => role.prepare_data_frame(id.0, bytes, end),
        }
    }
    fn commit_data(&mut self, plan: H2DataFramePlan) -> Result<(), crate::ServerError> {
        match self {
            Self::Client(role) => role.commit_data_frame(plan),
            Self::Server(role) => role.commit_data_frame(plan),
        }
    }
    fn close_send(&mut self, id: StreamId) {
        match self {
            Self::Client(role) => role.close_send(id.0),
            Self::Server(role) => role.finish_response_stream(id.0),
        }
    }
    fn reset(&mut self, id: StreamId) {
        match self {
            Self::Client(role) => role.close_stream(id.0),
            Self::Server(role) => role.close_stream(id.0),
        }
    }
    fn settings_owed(&self) -> bool {
        match self {
            Self::Client(role) => role.timer_obligations().settings_ack,
            Self::Server(role) => role.timer_obligations().settings_ack,
        }
    }
    fn highest_processed(&self) -> u32 {
        match self {
            Self::Client(_) => 0,
            Self::Server(role) => role.highest_processed_stream_id(),
        }
    }
}

struct Input {
    page: Rc<Page>,
    start: usize,
    end: usize,
}
struct Assembly {
    page: Rc<Page>,
    filled: usize,
}
struct Chunk<B> {
    buffer: B,
    offset: usize,
    end: bool,
}

enum Source {
    AwaitingResponse,
    Ready,
    Permitted(Token),
    Buffered,
    Stopped(SendStop),
}

struct Stream<B> {
    source: Source,
    chunk: Option<Chunk<B>>,
    receive_window: usize,
    leases: FxHashSet<u64>,
    receive_capacity: usize,
    writes: usize,
    returns: usize,
    receive_end: Option<StreamOutcome>,
    receive_notified: bool,
    stop_notified: bool,
    outcome: StreamOutcome,
    expected_send: Option<usize>,
    sent_bytes: usize,
    request_head: bool,
    connect: bool,
    deadline: Option<Duration>,
    demand_key: Option<u128>,
    ready_key: Option<u128>,
    body_limit: Option<usize>,
    response_seen: bool,
    outbound_complete: bool,
    receive_credit: usize,
    credit_key: Option<u128>,
}

impl<B> Stream<B> {
    fn new(source: Source, receive_window: u32) -> Self {
        Self {
            source,
            chunk: None,
            receive_window: receive_window as usize,
            leases: FxHashSet::default(),
            receive_capacity: 0,
            writes: 0,
            returns: 0,
            receive_end: None,
            receive_notified: false,
            stop_notified: false,
            outcome: StreamOutcome::Complete,
            expected_send: None,
            sent_bytes: 0,
            request_head: false,
            connect: false,
            deadline: None,
            demand_key: None,
            ready_key: None,
            body_limit: None,
            response_seen: false,
            outbound_complete: false,
            receive_credit: 0,
            credit_key: None,
        }
    }
    fn can_retire(&self) -> bool {
        self.receive_notified
            && self.stop_notified
            && self.leases.is_empty()
            && self.writes == 0
            && self.returns == 0
            && self.chunk.is_none()
    }
    fn succeeded(&self) -> bool {
        self.receive_end == Some(StreamOutcome::Complete) && self.outbound_complete
    }
}

enum Notice<B: SendBuffer> {
    Sent(Sent<B>),
    End(ReceiveEnd),
    Stop(StreamId, SendStop),
    Retired(StreamResult),
}

enum Life {
    Open,
    Draining {
        final_goaway: bool,
    },
    Settling(ConnectionResult),
    Closing {
        token: Token,
        result: ConnectionResult,
    },
    Closed {
        result: ConnectionResult,
        notified: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum Deadline {
    Settings,
    ShutdownPing,
    ShutdownEnd,
    Stream(StreamId),
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SettingsOutput {
    NotQueued,
    Queued,
    Flushed,
}

#[derive(Clone, Copy)]
enum Transition {
    Notice,
    Input,
    Write,
    Read,
    Permit,
    Alarm,
    CancelAlarm,
    CancelRead,
    CancelWrite,
    Close,
    Closed,
    FinishDrain,
    ExhaustIdentity,
    TimerDue,
}

/// Shared operations for the direct client and server.
///
/// Construction remains role-specific. Dereferencing a role exposes only these
/// shared commands, not another protocol or a materialized action interface.
pub struct Connection<B: SendBuffer = Vec<u8>> {
    protocol: Protocol,
    config: Config,
    owner: Rc<Owner>,
    sequence: u64,
    now: Duration,
    life: Life,
    streams: FxHashMap<StreamId, Stream<B>>,
    notices: VecDeque<Notice<B>>,
    ready: BTreeMap<u128, StreamId>,
    blocked: BTreeSet<StreamId>,
    probing: BTreeSet<StreamId>,
    probe_again: bool,
    prefer_probe: bool,
    demand: BTreeMap<u128, StreamId>,
    work_sequence: u128,
    send_capacity: usize,
    controls: VecDeque<(Vec<u8>, Option<StreamId>, ControlPurpose)>,
    control_capacity: usize,
    pending_write: Option<WriteOp<B>>,
    read: Option<Token>,
    write: Option<Token>,
    input: Option<Input>,
    assembly: Option<Assembly>,
    preface: usize,
    pool: Vec<Rc<Page>>,
    receive_capacity: usize,
    receive_window: usize,
    receive_credit: usize,
    credit_ready: BTreeMap<u128, StreamId>,
    // Queued control blocks ahead of the next coalesced credit write.
    credit_barrier: usize,
    data_after_credit: bool,
    deadlines: BTreeMap<(Duration, Deadline), ()>,
    settings_at: Option<Duration>,
    settings_output: SettingsOutput,
    alarm: Option<(Token, Duration)>,
    wake_tokens: FxHashSet<u64>,
    cancellations: FxHashSet<u64>,
    cancelled_read: bool,
    cancelled_write: bool,
    transport_broken: bool,
}

pub struct Client<B: SendBuffer = Vec<u8>>(Connection<B>);
pub struct Server<B: SendBuffer = Vec<u8>>(Connection<B>);

impl<B: SendBuffer> Deref for Client<B> {
    type Target = Connection<B>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl<B: SendBuffer> DerefMut for Client<B> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl<B: SendBuffer> Deref for Server<B> {
    type Target = Connection<B>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl<B: SendBuffer> DerefMut for Server<B> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<B: SendBuffer> Client<B> {
    pub fn new(config: Config, now: Duration) -> Result<Self, CommandError> {
        config.validate()?;
        let limits = H2Limits::from(config.http);
        let mut role = H2Client::with_local_flow_control_and_http_limits(
            config.stream_receive_window,
            config.connection_receive_window,
            limits,
            config.http,
        )?;
        let preface = role.connection_preface();
        let mut core = Connection::new(Protocol::Client(role), config, now);
        core.queue_control(preface, None)?;
        core.controls.back_mut().expect("client preface").2 = ControlPurpose::LocalSettings;
        core.settings_output = SettingsOutput::Queued;
        Ok(Self(core))
    }

    pub fn request(
        &mut self,
        fields: &[H2HeaderField],
        end: bool,
    ) -> Result<StreamId, CommandError> {
        self.request_by(fields.len(), |index| fields[index].as_ref(), end)
    }

    /// Encodes borrowed field views synchronously. No field borrow survives this call.
    pub fn request_ref(
        &mut self,
        fields: &[H2RawHeaderRef<'_>],
        end: bool,
    ) -> Result<StreamId, CommandError> {
        self.request_by(fields.len(), |index| fields[index], end)
    }

    fn request_by<'a>(
        &mut self,
        count: usize,
        field: impl Fn(usize) -> H2RawHeaderRef<'a> + Copy,
        end: bool,
    ) -> Result<StreamId, CommandError> {
        self.0.check_open()?;
        if self.streams.len() >= self.config.http.max_active_streams() {
            return Err(CommandError::Capacity);
        }
        let section =
            validate_decoded_header_fields_by(count, field, H2HeaderValidationRole::Request)
                .map_err(crate::ServerError::from)?
                .enforce_limits(self.config.http)?;
        if let Some(actual) = section.content_length
            && actual > self.config.http.max_body_bytes()
        {
            return Err(CommandError::Message(crate::ServerError::BodyTooLarge {
                limit: self.config.http.max_body_bytes(),
                actual,
            }));
        }
        if end && section.content_length.is_some_and(|n| n != 0) {
            return Err(CommandError::Message(
                crate::ServerError::InvalidContentLength,
            ));
        }
        self.0.preflight_headers(count, field)?;
        let Protocol::Client(role) = &mut self.0.protocol else {
            unreachable!()
        };
        let id = StreamId(role.next_stream_for_open()?);
        role.enqueue_outbound_header_block_by(id.0, count, field, end, 0, |_| {})?;
        role.next_stream_id = id.0.checked_add(2).ok_or(CommandError::SequenceExhausted)?;
        let head = (0..count)
            .map(field)
            .any(|f| f.name == b":method" && f.value == b"HEAD");
        let connect = (0..count)
            .map(field)
            .any(|f| f.name == b":method" && f.value == b"CONNECT");
        role.insert_client_stream(id.0, end, head);
        role.endpoint
            .streams
            .get_mut(&id.0)
            .expect("inserted stream")
            .request_is_connect = connect;
        let mut state = Stream::new(
            if end {
                Source::Stopped(SendStop::Finished)
            } else if connect {
                Source::AwaitingResponse
            } else {
                Source::Ready
            },
            self.config.stream_receive_window,
        );
        state.expected_send = section.content_length;
        state.body_limit = Some(self.config.http.max_body_bytes());
        state.request_head = head;
        state.connect = connect;
        self.streams.insert(id, state);
        self.0.collect_headers(id)?;
        if end {
            self.0.stop_notice(id, SendStop::Finished);
        } else if !connect {
            self.0.mark_demand(id);
        }
        Ok(id)
    }
}

impl<B: SendBuffer> Server<B> {
    pub fn new(config: Config, now: Duration) -> Result<Self, CommandError> {
        config.validate()?;
        let role = H2Server::with_local_flow_control_and_http_limits(
            config.stream_receive_window,
            config.connection_receive_window,
            H2Limits::from(config.http),
            config.http,
        )?;
        Ok(Self(Connection::new(Protocol::Server(role), config, now)))
    }

    pub fn respond(
        &mut self,
        id: StreamId,
        fields: &[H2HeaderField],
        end: bool,
    ) -> Result<(), CommandError> {
        self.respond_by(id, fields.len(), |index| fields[index].as_ref(), end)
    }

    /// Encodes borrowed field views synchronously. The caller keeps field storage.
    pub fn respond_ref(
        &mut self,
        id: StreamId,
        fields: &[H2RawHeaderRef<'_>],
        end: bool,
    ) -> Result<(), CommandError> {
        self.respond_by(id, fields.len(), |index| fields[index], end)
    }

    fn respond_by<'a>(
        &mut self,
        id: StreamId,
        count: usize,
        field: impl Fn(usize) -> H2RawHeaderRef<'a> + Copy,
        end: bool,
    ) -> Result<(), CommandError> {
        self.0.check_live()?;
        let state = self.streams.get(&id).ok_or(CommandError::InvalidState)?;
        if !matches!(state.source, Source::AwaitingResponse) {
            return Err(CommandError::InvalidState);
        }
        let section =
            validate_decoded_header_fields_by(count, field, H2HeaderValidationRole::Response)
                .map_err(crate::ServerError::from)?
                .enforce_limits(self.config.http)?;
        let status = section.response_status.ok_or(CommandError::InvalidState)?;
        let informational = status < 200;
        let tunnel = state.connect && (200..300).contains(&status);
        let no_body = !informational && (state.request_head || matches!(status, 204 | 205 | 304));
        if status == 101
            || (status == 204 && section.content_length.is_some())
            || (status == 205 && section.content_length.is_some_and(|n| n != 0))
            || (informational && end)
            || (informational && section.content_length.is_some())
            || (tunnel && section.content_length.is_some())
            || (end && !no_body && section.content_length.is_some_and(|n| n != 0))
        {
            return Err(CommandError::Message(crate::ServerError::InvalidResponse));
        }
        if !no_body
            && !tunnel
            && let Some(actual) = section.content_length
            && actual > self.config.http.max_body_bytes()
        {
            return Err(CommandError::Message(crate::ServerError::BodyTooLarge {
                limit: self.config.http.max_body_bytes(),
                actual,
            }));
        }
        self.0.preflight_headers(count, field)?;
        let Protocol::Server(role) = &mut self.0.protocol else {
            unreachable!()
        };
        role.enqueue_outbound_header_block_by(id.0, count, field, end || no_body, 0, |_| {})?;
        if !informational {
            let state = self.0.streams.get_mut(&id).expect("live stream");
            state.expected_send = if no_body {
                Some(0)
            } else {
                section.content_length
            };
            state.source = if end || no_body {
                Source::Stopped(SendStop::Finished)
            } else {
                Source::Ready
            };
            state.body_limit = if tunnel {
                None
            } else {
                Some(self.0.config.http.max_body_bytes())
            };
            if end || no_body {
                role.finish_response_stream(id.0);
            }
            if tunnel && let Some(inbound) = role.inbound_mut(id.0) {
                inbound.content_length = None;
                inbound.body_limit = None;
            }
            if tunnel && let Some(outbound) = role.outbound_mut(id.0) {
                outbound.sent.limit = None;
            }
        }
        self.0.collect_headers(id)?;
        if !informational {
            if end || no_body {
                self.0.stop_notice(id, SendStop::Finished);
            } else {
                self.0.mark_demand(id);
            }
        }
        Ok(())
    }
}

impl<B: SendBuffer> Connection<B> {
    fn new(mut protocol: Protocol, config: Config, now: Duration) -> Self {
        protocol.now(now);
        Self {
            receive_window: config.connection_receive_window as usize,
            receive_credit: 0,
            credit_ready: BTreeMap::new(),
            credit_barrier: 0,
            data_after_credit: false,
            protocol,
            config,
            owner: Rc::new(Owner),
            sequence: 0,
            now,
            life: Life::Open,
            streams: FxHashMap::default(),
            notices: VecDeque::new(),
            ready: BTreeMap::new(),
            blocked: BTreeSet::new(),
            probing: BTreeSet::new(),
            probe_again: false,
            prefer_probe: true,
            demand: BTreeMap::new(),
            work_sequence: 0,
            send_capacity: 0,
            controls: VecDeque::new(),
            control_capacity: 0,
            pending_write: None,
            read: None,
            write: None,
            input: None,
            assembly: None,
            preface: 0,
            pool: Vec::new(),
            receive_capacity: 0,
            deadlines: BTreeMap::new(),
            settings_at: None,
            settings_output: SettingsOutput::NotQueued,
            alarm: None,
            wake_tokens: FxHashSet::default(),
            cancellations: FxHashSet::default(),
            cancelled_read: false,
            cancelled_write: false,
            transport_broken: false,
        }
    }

    fn token(&mut self) -> Token {
        self.sequence = self
            .sequence
            .checked_add(1)
            .expect("operation sequence exhausted");
        Token {
            owner: self.owner.clone(),
            sequence: self.sequence,
        }
    }
    fn owns(&self, token: &Token) -> bool {
        Rc::ptr_eq(&self.owner, &token.owner)
    }
    fn check_open(&self) -> Result<(), CommandError> {
        if matches!(self.life, Life::Open) {
            Ok(())
        } else {
            Err(CommandError::InvalidState)
        }
    }
    fn check_live(&self) -> Result<(), CommandError> {
        if matches!(self.life, Life::Open | Life::Draining { .. }) {
            Ok(())
        } else {
            Err(CommandError::InvalidState)
        }
    }
    fn mark_demand(&mut self, id: StreamId) {
        if let Some(state) = self.streams.get_mut(&id)
            && state.demand_key.is_none()
        {
            self.work_sequence += 1;
            state.demand_key = Some(self.work_sequence);
            self.demand.insert(self.work_sequence, id);
        }
    }
    fn mark_ready(&mut self, id: StreamId) {
        self.blocked.remove(&id);
        self.probing.remove(&id);
        if let Some(state) = self.streams.get_mut(&id)
            && state.ready_key.is_none()
        {
            self.work_sequence += 1;
            state.ready_key = Some(self.work_sequence);
            self.ready.insert(self.work_sequence, id);
        }
    }
    fn settings_deadline(&mut self) {
        if self.protocol.settings_owed()
            && self.settings_at.is_none()
            && self.settings_output == SettingsOutput::Flushed
            && self.check_live().is_ok()
        {
            let at = self.now.saturating_add(self.config.settings_timeout);
            self.settings_at = Some(at);
            self.deadlines.insert((at, Deadline::Settings), ());
        } else if !self.protocol.settings_owed()
            && let Some(at) = self.settings_at.take()
        {
            self.deadlines.remove(&(at, Deadline::Settings));
        }
    }
    fn preflight_headers<'a>(
        &self,
        count: usize,
        field: impl Fn(usize) -> H2RawHeaderRef<'a>,
    ) -> Result<(), CommandError> {
        let payload_bound = (0..count)
            .map(field)
            .try_fold(16usize, |n, f| {
                n.checked_add(f.name.len())?
                    .checked_add(f.value.len())?
                    .checked_add(24)
            })
            .ok_or(CommandError::Capacity)?;
        let bound = payload_bound
            .checked_add(
                payload_bound
                    .div_ceil(16_384)
                    .max(1)
                    .checked_mul(9)
                    .ok_or(CommandError::Capacity)?,
            )
            .ok_or(CommandError::Capacity)?;
        self.preflight_control(bound)
    }

    fn preflight_control(&self, capacity: usize) -> Result<(), CommandError> {
        if self.controls.len() >= self.config.max_outbound_items
            || capacity
                > self
                    .config
                    .max_outbound_capacity
                    .saturating_sub(self.control_capacity)
        {
            Err(CommandError::Capacity)
        } else {
            Ok(())
        }
    }
    fn collect_headers(&mut self, id: StreamId) -> Result<(), CommandError> {
        while let Some(bytes) = self.protocol.take_block() {
            self.queue_control(bytes.into_boxed_slice().into_vec(), Some(id))?;
        }
        Ok(())
    }
    fn queue_control(
        &mut self,
        bytes: Vec<u8>,
        stream: Option<StreamId>,
    ) -> Result<(), CommandError> {
        if bytes.is_empty() {
            return Ok(());
        }
        if self.controls.len() >= self.config.max_outbound_items
            || bytes.capacity()
                > self
                    .config
                    .max_outbound_capacity
                    .saturating_sub(self.control_capacity)
        {
            return Err(CommandError::Capacity);
        }
        self.control_capacity += bytes.capacity();
        if let Some(state) = stream.and_then(|id| self.streams.get_mut(&id)) {
            state.writes += 1;
        }
        self.controls
            .push_back((bytes, stream, ControlPurpose::Ordinary));
        Ok(())
    }
    fn frame(&mut self, kind: H2FrameType, id: u32, payload: &[u8]) -> Result<(), CommandError> {
        let mut bytes = Vec::with_capacity(9 + payload.len());
        H2Frame {
            frame_type: kind,
            stream_id: id,
            flags: 0,
            payload: payload.to_vec(),
        }
        .encode(&mut bytes);
        let owner = (kind == H2FrameType::RstStream && self.streams.contains_key(&StreamId(id)))
            .then_some(StreamId(id));
        self.queue_control(bytes, owner)
    }

    pub fn send(
        &mut self,
        permit: SendPermit,
        buffer: B,
        end: bool,
    ) -> Result<(), Rejected<(SendPermit, B)>> {
        let valid = self.owns(&permit.token)
            && self.check_live().is_ok()
            && self.streams.get(&permit.stream).is_some_and(
                |state| matches!(&state.source, Source::Permitted(token) if token == &permit.token),
            );
        let error = if !valid {
            Some(CommandError::InvalidState)
        } else if buffer.retained_capacity() > permit.max_retained_capacity
            || (buffer.as_ref().is_empty() && !end)
        {
            Some(CommandError::Capacity)
        } else {
            None
        };
        if let Some(error) = error {
            return Err(Rejected {
                error,
                value: (permit, buffer),
            });
        }
        let state = self
            .streams
            .get_mut(&permit.stream)
            .expect("permit owns live source");
        let next = state.sent_bytes.checked_add(buffer.as_ref().len());
        if let (Some(next), Some(limit)) = (next, state.body_limit)
            && next > limit
        {
            return Err(Rejected {
                error: CommandError::Message(crate::ServerError::BodyTooLarge {
                    limit,
                    actual: next,
                }),
                value: (permit, buffer),
            });
        }
        if next.is_none()
            || state.expected_send.is_some_and(|expected| {
                next.is_some_and(|next| next > expected || (end && next != expected))
            })
        {
            return Err(Rejected {
                error: CommandError::Message(crate::ServerError::InvalidContentLength),
                value: (permit, buffer),
            });
        }
        if buffer.as_ref().len() > permit.max_bytes {
            return Err(Rejected {
                error: CommandError::Capacity,
                value: (permit, buffer),
            });
        }
        state.sent_bytes = next.expect("checked length");
        self.send_capacity -= permit.max_retained_capacity;
        self.send_capacity += buffer.retained_capacity();
        state.source = Source::Buffered;
        state.chunk = Some(Chunk {
            buffer,
            offset: 0,
            end,
        });
        self.mark_ready(permit.stream);
        Ok(())
    }

    pub fn trailers(&mut self, id: StreamId, fields: &[H2HeaderField]) -> Result<(), CommandError> {
        self.trailers_by(id, fields.len(), |index| fields[index].as_ref())
    }

    /// Encodes a borrowed trailer section without an owned-field staging list.
    pub fn trailers_ref(
        &mut self,
        id: StreamId,
        fields: &[H2RawHeaderRef<'_>],
    ) -> Result<(), CommandError> {
        self.trailers_by(id, fields.len(), |index| fields[index])
    }

    fn trailers_by<'a>(
        &mut self,
        id: StreamId,
        count: usize,
        field: impl Fn(usize) -> H2RawHeaderRef<'a> + Copy,
    ) -> Result<(), CommandError> {
        self.check_live()?;
        let state = self.streams.get(&id).ok_or(CommandError::InvalidState)?;
        if !matches!(state.source, Source::Ready | Source::Permitted(_))
            || state.chunk.is_some()
            || state.expected_send.is_some_and(|n| n != state.sent_bytes)
        {
            return Err(CommandError::InvalidState);
        }
        validate_decoded_header_fields_by(count, field, H2HeaderValidationRole::Trailers)
            .map_err(crate::ServerError::from)?
            .enforce_limits(self.config.http)?;
        self.preflight_headers(count, field)?;
        match &mut self.protocol {
            Protocol::Client(role) => {
                role.enqueue_outbound_header_block_by(id.0, count, field, true, 0, |_| {})?;
            }
            Protocol::Server(role) => {
                role.enqueue_outbound_header_block_by(id.0, count, field, true, 0, |_| {})?;
            }
        }
        self.protocol.close_send(id);
        self.collect_headers(id)?;
        self.stop_source(id, SendStop::Finished);
        self.maybe_retire(id);
        Ok(())
    }

    pub fn reset(&mut self, id: StreamId, code: H2ErrorCode) -> Result<(), CommandError> {
        self.check_live()?;
        if !self
            .streams
            .get(&id)
            .is_some_and(|state| state.outcome == StreamOutcome::Complete)
        {
            return Err(CommandError::InvalidState);
        }
        self.frame(H2FrameType::RstStream, id.0, &code.as_u32().to_be_bytes())?;
        self.protocol.reset(id);
        self.terminate_stream(id, StreamOutcome::Reset(code.as_u32()));
        Ok(())
    }

    pub fn set_deadline(
        &mut self,
        id: StreamId,
        deadline: Option<Duration>,
    ) -> Result<(), CommandError> {
        self.check_live()?;
        let state = self
            .streams
            .get_mut(&id)
            .ok_or(CommandError::InvalidState)?;
        if state.succeeded() || state.outcome != StreamOutcome::Complete {
            return Err(CommandError::InvalidState);
        }
        if let Some(old) = state.deadline.take() {
            self.deadlines.remove(&(old, Deadline::Stream(id)));
        }
        state.deadline = deadline;
        if let Some(at) = deadline {
            self.deadlines.insert((at, Deadline::Stream(id)), ());
        }
        Ok(())
    }

    pub fn advance_time(&mut self, now: Duration) -> Result<(), CommandError> {
        if now < self.now {
            return Err(CommandError::TimeReversed);
        }
        self.now = now;
        self.protocol.now(now);
        while let Some((&(at, kind), _)) = self.deadlines.first_key_value() {
            if at > now {
                break;
            }
            self.deadlines.pop_first();
            match kind {
                Deadline::Settings if self.protocol.settings_owed() => {
                    self.fail(ConnectionResult::Protocol(H2ProtocolError::connection(
                        H2ErrorCode::SettingsTimeout,
                        "SETTINGS acknowledgment deadline expired",
                    )))
                }
                Deadline::Stream(id) => {
                    if self
                        .streams
                        .get(&id)
                        .is_some_and(|s| s.deadline == Some(at))
                    {
                        let queued = self.frame(
                            H2FrameType::RstStream,
                            id.0,
                            &H2ErrorCode::Cancel.as_u32().to_be_bytes(),
                        );
                        self.protocol.reset(id);
                        self.terminate_stream(id, StreamOutcome::Deadline);
                        if queued.is_err() {
                            self.fail(ConnectionResult::ResourceExhausted);
                        }
                    }
                }
                Deadline::ShutdownPing => {
                    if self.final_goaway().is_err() {
                        self.fail(ConnectionResult::ResourceExhausted);
                    }
                }
                Deadline::ShutdownEnd => {
                    self.fail(ConnectionResult::Graceful);
                    self.fail(ConnectionResult::IoFailed);
                }
                Deadline::Settings => {}
            }
        }
        Ok(())
    }

    pub fn shutdown(&mut self) -> Result<(), CommandError> {
        self.check_open()?;
        self.preflight_control(if matches!(self.protocol, Protocol::Server(_)) {
            34
        } else {
            17
        })?;
        let bytes = match &mut self.protocol {
            Protocol::Server(role) => role.begin_graceful_shutdown()?,
            Protocol::Client(role) => role.goaway_frame(0, H2ErrorCode::NoError.as_u32())?,
        };
        self.queue_control(bytes.into_boxed_slice().into_vec(), None)?;
        self.life = Life::Draining {
            final_goaway: false,
        };
        if matches!(self.protocol, Protocol::Server(_)) {
            self.controls.back_mut().expect("shutdown output").2 = ControlPurpose::ShutdownPing;
        } else {
            self.life = Life::Draining { final_goaway: true };
        }
        self.deadlines.insert(
            (
                self.now.saturating_add(self.config.shutdown_timeout),
                Deadline::ShutdownEnd,
            ),
            (),
        );
        Ok(())
    }

    fn final_goaway(&mut self) -> Result<(), CommandError> {
        if !matches!(
            self.life,
            Life::Draining {
                final_goaway: false
            }
        ) {
            return Ok(());
        }
        self.preflight_control(17)?;
        if let Protocol::Server(role) = &mut self.protocol {
            let bytes = role.graceful_shutdown_ping_elapsed()?;
            self.queue_control(bytes.into_boxed_slice().into_vec(), None)?;
        }
        self.life = Life::Draining { final_goaway: true };
        Ok(())
    }

    fn stop_notice(&mut self, id: StreamId, reason: SendStop) {
        self.notices.push_back(Notice::Stop(id, reason));
    }
    fn stop_source(&mut self, id: StreamId, reason: SendStop) {
        let Some(state) = self.streams.get_mut(&id) else {
            return;
        };
        if matches!(state.source, Source::Stopped(_)) {
            return;
        }
        if matches!(state.source, Source::Permitted(_)) {
            self.send_capacity -= self.config.max_send_buffer_capacity;
        }
        state.source = Source::Stopped(reason);
        if let Some(chunk) = state.chunk.take() {
            self.send_capacity -= chunk.buffer.retained_capacity();
            state.returns += 1;
            self.notices.push_back(Notice::Sent(Sent {
                stream: id,
                buffer: chunk.buffer,
                accepted: chunk.offset,
                exact: true,
                result: Err(reason),
            }));
        }
        self.stop_notice(id, reason);
    }
    fn receive_end(&mut self, id: StreamId, outcome: StreamOutcome) {
        if let Some(state) = self.streams.get_mut(&id)
            && state.receive_end.is_none()
        {
            state.receive_end = Some(outcome);
            state.receive_credit = 0;
            if let Some(key) = state.credit_key.take() {
                self.credit_ready.remove(&key);
            }
            if state.outbound_complete
                && let Some(at) = state.deadline.take()
            {
                self.deadlines.remove(&(at, Deadline::Stream(id)));
            }
            self.notices.push_back(Notice::End(ReceiveEnd {
                stream: id,
                outcome,
            }));
        }
    }
    fn terminate_stream(&mut self, id: StreamId, outcome: StreamOutcome) {
        if let Some(state) = self.streams.get_mut(&id) {
            state.outcome = outcome;
            if let Some(at) = state.deadline.take() {
                self.deadlines.remove(&(at, Deadline::Stream(id)));
            }
        }
        let reason = match outcome {
            StreamOutcome::Unprocessed => SendStop::Unprocessed,
            StreamOutcome::Reset(code) => SendStop::Reset(code),
            StreamOutcome::Deadline => SendStop::Reset(H2ErrorCode::Cancel.as_u32()),
            _ => SendStop::ConnectionFailed,
        };
        self.stop_source(id, reason);
        self.receive_end(id, outcome);
        self.maybe_retire(id);
    }
    fn maybe_retire(&mut self, id: StreamId) {
        if self.streams.get(&id).is_some_and(Stream::can_retire) {
            let state = self.streams.remove(&id).expect("retirement join");
            if let Some(key) = state.demand_key {
                self.demand.remove(&key);
            }
            if let Some(key) = state.ready_key {
                self.ready.remove(&key);
            }
            if let Some(key) = state.credit_key {
                self.credit_ready.remove(&key);
            }
            if let Some(at) = state.deadline {
                self.deadlines.remove(&(at, Deadline::Stream(id)));
            }
            self.blocked.remove(&id);
            self.probing.remove(&id);
            self.notices.push_back(Notice::Retired(StreamResult {
                stream: id,
                outcome: state.outcome,
            }));
        }
    }

    fn fail(&mut self, result: ConnectionResult) {
        if matches!(
            result,
            ConnectionResult::IoFailed | ConnectionResult::PeerClosed
        ) {
            self.transport_broken = true;
            while let Some((bytes, id, _)) = self.controls.pop_front() {
                self.control_capacity -= bytes.capacity();
                if let Some(id) = id {
                    self.fail_unsettled_stream(id);
                    if let Some(state) = self.streams.get_mut(&id) {
                        state.writes -= 1;
                    }
                    self.maybe_retire(id);
                }
            }
            if let Some(op) = self.pending_write.take() {
                if let Some(id) = op.stream {
                    self.fail_unsettled_stream(id);
                }
                if let WriteStorage::Data { buffer, range, .. } = op.storage {
                    let id = op.stream.expect("DATA stream");
                    self.send_capacity -= buffer.retained_capacity();
                    self.streams
                        .get_mut(&id)
                        .expect("pending write owns stream")
                        .returns += 1;
                    self.notices.push_back(Notice::Sent(Sent {
                        stream: id,
                        buffer,
                        accepted: range.start + op.cursor.saturating_sub(9).min(range.len()),
                        exact: true,
                        result: Err(SendStop::ConnectionFailed),
                    }));
                } else if let WriteStorage::Control { bytes, .. } = op.storage {
                    self.control_capacity -= bytes.capacity();
                }
                if let Some(id) = op.stream {
                    self.streams
                        .get_mut(&id)
                        .expect("pending write owns stream")
                        .writes -= 1;
                    self.maybe_retire(id);
                }
            }
        }
        if !matches!(self.life, Life::Open | Life::Draining { .. }) {
            return;
        }
        let goaway_code = match result {
            ConnectionResult::Protocol(error) => Some(error.code),
            ConnectionResult::ResourceExhausted if !self.transport_broken => {
                Some(H2ErrorCode::EnhanceYourCalm)
            }
            _ => None,
        };
        if let Some(code) = goaway_code {
            let mut bytes = Vec::new();
            encode_h2_goaway(&mut bytes, self.protocol.highest_processed(), code.as_u32());
            // One bounded emergency GOAWAY slot is independent of ordinary output admission.
            self.control_capacity += bytes.capacity();
            self.controls
                .push_back((bytes, None, ControlPurpose::Ordinary));
        }
        self.life = Life::Settling(result);
        self.deadlines.clear();
        self.receive_credit = 0;
        let ids: Vec<_> = self.streams.keys().copied().collect();
        for id in ids {
            self.fail_unsettled_stream(id);
        }
        if let Some(input) = self.input.take() {
            self.recycle(input.page);
        }
        if let Some(assembly) = self.assembly.take() {
            self.recycle(assembly.page);
        }
    }

    fn fail_unsettled_stream(&mut self, id: StreamId) {
        if self
            .streams
            .get(&id)
            .is_some_and(|state| state.outcome == StreamOutcome::Complete && !state.succeeded())
        {
            self.terminate_stream(id, StreamOutcome::ConnectionFailed);
        }
    }

    fn page(&mut self) -> Option<Rc<Page>> {
        if let Some(page) = self.pool.pop() {
            return Some(page);
        }
        if self.receive_capacity + PAGE_SIZE > self.config.max_receive_capacity {
            return None;
        }
        self.receive_capacity += PAGE_SIZE;
        Some(Rc::new(Page {
            bytes: vec![0; PAGE_SIZE].into_boxed_slice(),
        }))
    }
    fn recycle(&mut self, page: Rc<Page>) {
        if Rc::strong_count(&page) == 1 {
            self.pool.push(page);
        }
    }
    fn refund(
        &mut self,
        id: StreamId,
        amount: usize,
        stream_credit: bool,
    ) -> Result<(), CommandError> {
        if amount == 0 {
            return Ok(());
        }
        if self.receive_credit == 0 && self.credit_ready.is_empty() {
            self.credit_barrier = self.controls.len();
        }
        self.receive_credit = self
            .receive_credit
            .checked_add(amount)
            .ok_or(CommandError::Capacity)?;
        if stream_credit
            && let Some(state) = self.streams.get_mut(&id)
            && state.receive_end.is_none()
        {
            state.receive_credit = state
                .receive_credit
                .checked_add(amount)
                .ok_or(CommandError::Capacity)?;
            if state.credit_key.is_none() {
                self.work_sequence += 1;
                state.credit_key = Some(self.work_sequence);
                self.credit_ready.insert(self.work_sequence, id);
            }
        }
        Ok(())
    }

    pub fn complete_read(
        &mut self,
        completion: ReadCompletion,
    ) -> Result<(), Rejected<ReadCompletion>> {
        let valid = self.owns(&completion.op.token)
            && self.read.as_ref() == Some(&completion.op.token)
            && match completion.outcome {
                ReadOutcome::Read(n) => n <= completion.op.page.bytes.len(),
                _ => true,
            };
        if !valid {
            return Err(Rejected {
                error: CommandError::InvalidCompletion,
                value: completion,
            });
        }
        self.read = None;
        self.cancelled_read = false;
        let ReadCompletion { op, outcome } = completion;
        if self.check_live().is_err() {
            self.recycle(op.page);
            return Ok(());
        }
        match outcome {
            ReadOutcome::Read(n) if n != 0 && self.check_live().is_ok() => {
                self.input = Some(Input {
                    page: op.page,
                    start: 0,
                    end: n,
                });
            }
            ReadOutcome::Read(_) | ReadOutcome::Eof => {
                self.recycle(op.page);
                self.fail(ConnectionResult::PeerClosed);
            }
            ReadOutcome::Failed(_) => {
                self.recycle(op.page);
                self.fail(ConnectionResult::IoFailed);
            }
        }
        Ok(())
    }

    pub fn release_body(&mut self, completion: BodyRelease) -> Result<(), Rejected<BodyRelease>> {
        let op = &completion.op;
        if !self.owns(&op.token)
            || !self
                .streams
                .get(&op.stream)
                .is_some_and(|s| s.leases.contains(&op.token.sequence))
        {
            return Err(Rejected {
                error: CommandError::InvalidCompletion,
                value: completion,
            });
        }
        let BodyRelease { op } = completion;
        let state = self.streams.get_mut(&op.stream).expect("live lease");
        state.leases.remove(&op.token.sequence);
        state.receive_capacity -= op.page.bytes.len();
        if self.check_live().is_ok() && self.refund(op.stream, op.range.len(), true).is_err() {
            self.fail(ConnectionResult::ResourceExhausted);
        }
        self.recycle(op.page);
        self.maybe_retire(op.stream);
        Ok(())
    }

    // Rejection returns the caller's original generic storage without an allocation.
    #[allow(clippy::result_large_err)]
    pub fn complete_write(
        &mut self,
        completion: WriteCompletion<B>,
    ) -> Result<(), Rejected<WriteCompletion<B>>> {
        let amount = match completion.outcome {
            WriteOutcome::Written(n) => n,
            WriteOutcome::Failed {
                progress: Progress::Exact(n) | Progress::AtLeast(n),
                ..
            } => n,
        };
        if !self.owns(&completion.op.token)
            || self.write.as_ref() != Some(&completion.op.token)
            || amount > completion.op.remaining()
        {
            return Err(Rejected {
                error: CommandError::InvalidCompletion,
                value: completion,
            });
        }
        self.write = None;
        self.cancelled_write = false;
        let WriteCompletion { mut op, outcome } = completion;
        op.cursor += amount;
        let success = matches!(outcome, WriteOutcome::Written(n) if n != 0);
        if success && op.remaining() != 0 && !self.transport_broken {
            self.pending_write = Some(op);
            return Ok(());
        }
        let exact = !matches!(
            outcome,
            WriteOutcome::Failed {
                progress: Progress::AtLeast(_),
                ..
            }
        );
        let frame_complete = op.remaining() == 0;
        if !success {
            self.fail(ConnectionResult::IoFailed);
        }
        let id = op.stream;
        if success
            && frame_complete
            && op.end
            && op.buffer_end
            && let Some(state) = id.and_then(|id| self.streams.get_mut(&id))
        {
            state.outbound_complete = true;
            if state.succeeded() && state.outcome == StreamOutcome::ConnectionFailed {
                state.outcome = StreamOutcome::Complete;
            }
            if state.succeeded()
                && let Some(at) = state.deadline.take()
            {
                self.deadlines
                    .remove(&(at, Deadline::Stream(id.expect("owned terminal write"))));
            }
        }
        match op.storage {
            WriteStorage::Control { bytes, purpose } => {
                self.control_capacity -= bytes.capacity();
                if success && frame_complete {
                    match purpose {
                        ControlPurpose::LocalSettings => {
                            self.settings_output = SettingsOutput::Flushed;
                            self.settings_deadline();
                        }
                        ControlPurpose::ShutdownPing
                            if matches!(
                                self.life,
                                Life::Draining {
                                    final_goaway: false
                                }
                            ) =>
                        {
                            self.deadlines.insert(
                                (
                                    self.now.saturating_add(Duration::from_secs(1)),
                                    Deadline::ShutdownPing,
                                ),
                                (),
                            );
                        }
                        ControlPurpose::Ordinary | ControlPurpose::ShutdownPing => {}
                    }
                }
            }
            WriteStorage::Data { buffer, range, .. } => {
                let id = id.expect("DATA has a stream");
                let accepted = range.start + op.cursor.saturating_sub(9).min(range.len());
                let stopped = self.streams.get(&id).and_then(|s| match s.source {
                    Source::Stopped(reason) => Some(reason),
                    _ => None,
                });
                if success && !op.buffer_end && stopped.is_none() {
                    self.streams
                        .get_mut(&id)
                        .expect("outstanding write keeps stream")
                        .chunk = Some(Chunk {
                        buffer,
                        offset: range.end,
                        end: op.end,
                    });
                    self.mark_ready(id);
                } else {
                    self.send_capacity -= buffer.retained_capacity();
                    let state = self
                        .streams
                        .get_mut(&id)
                        .expect("outstanding write keeps stream");
                    state.returns += 1;
                    self.notices.push_back(Notice::Sent(Sent {
                        stream: id,
                        buffer,
                        accepted,
                        exact,
                        result: if success && op.buffer_end && frame_complete {
                            Ok(())
                        } else if success {
                            stopped.map_or(Ok(()), Err)
                        } else {
                            Err(SendStop::ConnectionFailed)
                        },
                    }));
                    if stopped.is_none() {
                        if op.end {
                            self.stop_source(id, SendStop::Finished);
                        } else {
                            self.streams.get_mut(&id).expect("live stream").source = Source::Ready;
                            self.mark_demand(id);
                        }
                    }
                }
            }
        }
        if let Some(id) = id {
            if let Some(state) = self.streams.get_mut(&id) {
                state.writes -= 1;
            }
            self.maybe_retire(id);
        }
        Ok(())
    }

    pub fn complete_cancel(
        &mut self,
        completion: CancelCompletion,
    ) -> Result<(), Rejected<CancelCompletion>> {
        if !self.owns(&completion.op.token)
            || !self.cancellations.remove(&completion.op.token.sequence)
        {
            return Err(Rejected {
                error: CommandError::InvalidCompletion,
                value: completion,
            });
        }
        Ok(())
    }
    pub fn complete_wake(
        &mut self,
        completion: WakeCompletion,
    ) -> Result<(), Rejected<WakeCompletion>> {
        if !self.owns(&completion.op.token)
            || !self.wake_tokens.contains(&completion.op.token.sequence)
        {
            return Err(Rejected {
                error: CommandError::InvalidCompletion,
                value: completion,
            });
        }
        self.wake_tokens.remove(&completion.op.token.sequence);
        let current = self
            .alarm
            .as_ref()
            .is_some_and(|(token, _)| token == &completion.op.token);
        if current {
            self.alarm = None;
            let still_scheduled = self
                .deadlines
                .first_key_value()
                .is_some_and(|((at, _), _)| *at == completion.op.deadline);
            if !still_scheduled || completion.now < self.now {
                return Ok(());
            }
            if self.advance_time(completion.now).is_err() {
                self.fail(ConnectionResult::ResourceExhausted);
            }
        }
        Ok(())
    }
    pub fn complete_close(
        &mut self,
        completion: CloseCompletion,
    ) -> Result<(), Rejected<CloseCompletion>> {
        let Life::Closing { token, result } = &self.life else {
            return Err(Rejected {
                error: CommandError::InvalidCompletion,
                value: completion,
            });
        };
        if token != &completion.op.token {
            return Err(Rejected {
                error: CommandError::InvalidCompletion,
                value: completion,
            });
        }
        let result =
            if completion.result.is_err() && !matches!(result, ConnectionResult::Protocol(_)) {
                ConnectionResult::IoFailed
            } else {
                *result
            };
        self.life = Life::Closed {
            result,
            notified: false,
        };
        Ok(())
    }

    fn select(&self) -> Option<Transition> {
        if self.sequence >= u64::MAX - 8 && self.check_live().is_ok() {
            return Some(Transition::ExhaustIdentity);
        }
        if !self.notices.is_empty() {
            return Some(Transition::Notice);
        }
        if let Some((_, deadline)) = &self.alarm
            && self
                .deadlines
                .first_key_value()
                .is_none_or(|((next, _), _)| next != deadline)
        {
            return Some(Transition::CancelAlarm);
        }
        match &self.life {
            Life::Closed {
                notified: false, ..
            } => return Some(Transition::Closed),
            Life::Closed { .. } | Life::Closing { .. } => return None,
            Life::Settling(_) => {
                if self.read.is_some() && !self.cancelled_read {
                    return Some(Transition::CancelRead);
                }
                if self.transport_broken && self.write.is_some() && !self.cancelled_write {
                    return Some(Transition::CancelWrite);
                }
                if self.write.is_none()
                    && (self.pending_write.is_some() || !self.controls.is_empty())
                {
                    return Some(Transition::Write);
                }
                if self.read.is_none()
                    && self.write.is_none()
                    && self.controls.is_empty()
                    && self.pending_write.is_none()
                    && self.cancellations.is_empty()
                    && self.wake_tokens.is_empty()
                {
                    return Some(Transition::Close);
                }
                return None;
            }
            Life::Draining { final_goaway: true } if self.streams.is_empty() => {
                return Some(Transition::FinishDrain);
            }
            Life::Open | Life::Draining { .. } => {}
        }
        if self
            .deadlines
            .first_key_value()
            .is_some_and(|((at, _), _)| *at <= self.now)
        {
            return Some(Transition::TimerDue);
        }
        if self.input.is_some() {
            return Some(Transition::Input);
        }
        if self.write.is_none()
            && (self.pending_write.is_some()
                || !self.controls.is_empty()
                || self.receive_credit != 0
                || !self.credit_ready.is_empty()
                || !self.ready.is_empty()
                || (self.protocol.connection_send_available() != 0
                    && (!self.probing.is_empty() || self.probe_again)))
        {
            return Some(Transition::Write);
        }
        if self.read.is_none() {
            return Some(Transition::Read);
        }
        if !self.demand.is_empty()
            && self.send_capacity
                <= self.config.max_send_capacity - self.config.max_send_buffer_capacity
        {
            return Some(Transition::Permit);
        }
        if self.alarm.is_none() && !self.deadlines.is_empty() {
            return Some(Transition::Alarm);
        }
        None
    }

    pub fn next<P: Ports<B>>(&mut self, ports: &mut P) -> Option<P::Output> {
        for _ in 0..self.config.turn_budget {
            let transition = self.select()?;
            let output = match transition {
                Transition::TimerDue => {
                    if self.advance_time(self.now).is_err() {
                        self.fail(ConnectionResult::ResourceExhausted);
                    }
                    None
                }
                Transition::ExhaustIdentity => {
                    self.fail(ConnectionResult::ResourceExhausted);
                    self.fail(ConnectionResult::IoFailed);
                    None
                }
                Transition::FinishDrain => {
                    self.life = Life::Settling(ConnectionResult::Graceful);
                    self.deadlines.clear();
                    self.receive_credit = 0;
                    None
                }
                Transition::Notice => self.notify(ports),
                Transition::Input => self.process_input(ports),
                Transition::Write => self.issue_write(ports),
                Transition::Read => {
                    if let Some(page) = self.page() {
                        let token = self.token();
                        self.read = Some(token.clone());
                        ports.read(ReadOp { token, page })
                    } else {
                        self.fail(ConnectionResult::ResourceExhausted);
                        None
                    }
                }
                Transition::Permit => {
                    let (_, id) = self.demand.pop_first().expect("selected demand");
                    if let Some(state) = self.streams.get_mut(&id) {
                        state.demand_key = None;
                    }
                    if self
                        .streams
                        .get(&id)
                        .is_some_and(|s| matches!(s.source, Source::Ready))
                    {
                        let token = self.token();
                        let state = self.streams.get_mut(&id).expect("ready stream");
                        let mut max_bytes = self.config.max_send_buffer_bytes;
                        if let Some(expected) = state.expected_send {
                            max_bytes = max_bytes.min(expected.saturating_sub(state.sent_bytes));
                        }
                        if let Some(limit) = state.body_limit {
                            max_bytes = max_bytes.min(limit.saturating_sub(state.sent_bytes));
                        }
                        state.source = Source::Permitted(token.clone());
                        self.send_capacity += self.config.max_send_buffer_capacity;
                        ports.send_ready(SendPermit {
                            token,
                            stream: id,
                            max_bytes,
                            max_retained_capacity: self.config.max_send_buffer_capacity,
                        })
                    } else {
                        None
                    }
                }
                Transition::Alarm => {
                    if self
                        .wake_tokens
                        .len()
                        .saturating_add(self.cancellations.len())
                        >= self.config.max_outbound_items
                    {
                        self.fail(ConnectionResult::ResourceExhausted);
                        continue;
                    }
                    let (&(deadline, _), _) =
                        self.deadlines.first_key_value().expect("selected deadline");
                    let token = self.token();
                    self.wake_tokens.insert(token.sequence);
                    self.alarm = Some((token.clone(), deadline));
                    ports.wake(WakeOp { token, deadline })
                }
                Transition::CancelAlarm => {
                    let (original, _) = self.alarm.take().expect("selected alarm cancellation");
                    let token = self.token();
                    self.cancellations.insert(token.sequence);
                    ports.cancel(CancelOp { token, original })
                }
                Transition::CancelRead => {
                    let original = self.read.clone().expect("selected read cancellation");
                    let token = self.token();
                    self.cancelled_read = true;
                    self.cancellations.insert(token.sequence);
                    ports.cancel(CancelOp { token, original })
                }
                Transition::CancelWrite => {
                    let original = self.write.clone().expect("selected write cancellation");
                    let token = self.token();
                    self.cancelled_write = true;
                    self.cancellations.insert(token.sequence);
                    ports.cancel(CancelOp { token, original })
                }
                Transition::Close => {
                    let Life::Settling(result) = self.life else {
                        unreachable!()
                    };
                    let token = self.token();
                    self.life = Life::Closing {
                        token: token.clone(),
                        result,
                    };
                    ports.close(CloseOp { token })
                }
                Transition::Closed => {
                    let Life::Closed { result, notified } = &mut self.life else {
                        unreachable!()
                    };
                    *notified = true;
                    ports.closed(*result)
                }
            };
            if output.is_some() {
                return output;
            }
        }
        if self.select().is_some() {
            ports.reschedule()
        } else {
            None
        }
    }

    fn notify<P: Ports<B>>(&mut self, ports: &mut P) -> Option<P::Output> {
        match self.notices.pop_front().expect("selected notification") {
            Notice::Sent(sent) => {
                if let Some(state) = self.streams.get_mut(&sent.stream) {
                    state.returns -= 1;
                }
                self.maybe_retire(sent.stream);
                ports.sent(sent)
            }
            Notice::End(end) => {
                if let Some(state) = self.streams.get_mut(&end.stream) {
                    state.receive_notified = true;
                }
                self.maybe_retire(end.stream);
                ports.ended(end)
            }
            Notice::Stop(id, reason) => {
                if let Some(state) = self.streams.get_mut(&id) {
                    state.stop_notified = true;
                }
                self.maybe_retire(id);
                ports.send_stopped(id, reason)
            }
            Notice::Retired(result) => ports.retired(result),
        }
    }

    fn issue_write<P: Ports<B>>(&mut self, ports: &mut P) -> Option<P::Output> {
        if self.probing.is_empty()
            && self.probe_again
            && self.protocol.connection_send_available() != 0
        {
            std::mem::swap(&mut self.probing, &mut self.blocked);
            self.probe_again = false;
        }
        let data_turn = self.data_after_credit
            && self.controls.is_empty()
            && (!self.ready.is_empty()
                || (self.protocol.connection_send_available() != 0 && !self.probing.is_empty()));
        let op = if let Some(mut op) = self.pending_write.take() {
            op.token = self.token();
            op
        } else if !self.controls.is_empty()
            && (self.credit_barrier != 0
                || (self.receive_credit == 0 && self.credit_ready.is_empty())
                || self.control_capacity
                    > self.config.max_outbound_capacity - 2 * WINDOW_UPDATE_BYTES)
        {
            let (bytes, stream, purpose) =
                self.controls.pop_front().expect("queued control output");
            self.credit_barrier = self.credit_barrier.saturating_sub(1);
            let end = stream.is_some() && bytes.len() >= 9 && bytes[3] == 1 && bytes[4] & 1 != 0;
            WriteOp {
                token: self.token(),
                storage: WriteStorage::Control { bytes, purpose },
                cursor: 0,
                stream,
                end,
                buffer_end: true,
            }
        } else if (self.receive_credit != 0 || !self.credit_ready.is_empty()) && !data_turn {
            let frames =
                usize::from(self.receive_credit != 0) + usize::from(!self.credit_ready.is_empty());
            let mut bytes = Vec::with_capacity(WINDOW_UPDATE_BYTES * frames);
            let amount = std::mem::take(&mut self.receive_credit);
            if amount != 0 {
                self.receive_window += amount;
                encode_window_update(&mut bytes, 0, amount);
            }
            if let Some((_, id)) = self.credit_ready.pop_first() {
                let state = self.streams.get_mut(&id).expect("pending stream credit");
                state.credit_key = None;
                let amount = std::mem::take(&mut state.receive_credit);
                state.receive_window += amount;
                encode_window_update(&mut bytes, id.0, amount);
            }
            self.credit_barrier = self.controls.len();
            self.data_after_credit = true;
            debug_assert!(
                self.control_capacity + bytes.capacity() <= self.config.max_outbound_capacity
            );
            self.control_capacity += bytes.capacity();
            WriteOp {
                token: self.token(),
                storage: WriteStorage::Control {
                    bytes,
                    purpose: ControlPurpose::Ordinary,
                },
                cursor: 0,
                stream: None,
                end: false,
                buffer_end: true,
            }
        } else {
            let can_probe =
                self.protocol.connection_send_available() != 0 && !self.probing.is_empty();
            let id = if can_probe && (self.prefer_probe || self.ready.is_empty()) {
                self.prefer_probe = false;
                self.probing.pop_first()?
            } else {
                self.prefer_probe = true;
                self.ready.pop_first().map(|(_, id)| id)?
            };
            if let Some(state) = self.streams.get_mut(&id) {
                state.ready_key = None;
            }
            let chunk = self.streams.get(&id).and_then(|s| s.chunk.as_ref())?;
            let remaining = chunk.buffer.as_ref().len() - chunk.offset;
            let plan = match self.protocol.prepare_data(id, remaining, chunk.end) {
                Ok(Some(plan)) => plan,
                Ok(None) => {
                    self.blocked.insert(id);
                    return None;
                }
                Err(_) => {
                    if self.reset(id, H2ErrorCode::InternalError).is_err() {
                        self.fail(ConnectionResult::ResourceExhausted);
                    }
                    return None;
                }
            };
            if self.protocol.commit_data(plan).is_err() {
                self.fail(ConnectionResult::IoFailed);
                return None;
            }
            self.data_after_credit = false;
            let state = self.streams.get_mut(&id).expect("ready stream");
            let chunk = state.chunk.take().expect("ready chunk");
            state.writes += 1;
            WriteOp {
                token: self.token(),
                storage: WriteStorage::Data {
                    header: plan.header,
                    buffer: chunk.buffer,
                    range: chunk.offset..chunk.offset + plan.payload_len,
                },
                cursor: 0,
                stream: Some(id),
                end: chunk.end,
                buffer_end: remaining == plan.payload_len,
            }
        };
        self.write = Some(op.token.clone());
        ports.write(op)
    }

    fn process_input<P: Ports<B>>(&mut self, ports: &mut P) -> Option<P::Output> {
        let mut input = self.input.take().expect("selected input");
        if matches!(self.protocol, Protocol::Server(_)) && self.preface < CLIENT_PREFACE.len() {
            while input.start < input.end && self.preface < CLIENT_PREFACE.len() {
                if input.page.bytes[input.start] != CLIENT_PREFACE[self.preface] {
                    self.recycle(input.page);
                    self.fail(ConnectionResult::Protocol(H2ProtocolError::connection(
                        H2ErrorCode::ProtocolError,
                        "invalid client preface",
                    )));
                    return None;
                }
                input.start += 1;
                self.preface += 1;
            }
            if self.preface == CLIENT_PREFACE.len()
                && let Protocol::Server(role) = &mut self.protocol
            {
                role.preface_seen = true;
            }
            if input.start == input.end {
                self.recycle(input.page);
                return None;
            }
        }
        if let Some(mut assembly) = self.assembly.take() {
            let target = if assembly.filled < 9 {
                9
            } else {
                9 + payload_len(&assembly.page.bytes[..9])
            };
            let take = (target - assembly.filled).min(input.end - input.start);
            Rc::get_mut(&mut assembly.page)
                .expect("assembly page is exclusive")
                .bytes[assembly.filled..assembly.filled + take]
                .copy_from_slice(&input.page.bytes[input.start..input.start + take]);
            input.start += take;
            assembly.filled += take;
            if input.start != input.end {
                self.input = Some(input);
            } else {
                self.recycle(input.page);
            }
            if assembly.filled >= 9 && payload_len(&assembly.page.bytes[..9]) > 16_384 {
                self.recycle(assembly.page);
                self.fail(ConnectionResult::Protocol(H2ProtocolError::connection(
                    H2ErrorCode::FrameSizeError,
                    "frame exceeds advertised maximum",
                )));
                return None;
            }
            if assembly.filled >= 9 && assembly.filled == 9 + payload_len(&assembly.page.bytes[..9])
            {
                let output = self.dispatch_frame(&assembly.page, 0..assembly.filled, ports);
                self.recycle(assembly.page);
                output
            } else {
                self.assembly = Some(assembly);
                None
            }
        } else {
            let bytes = &input.page.bytes[input.start..input.end];
            if bytes.len() >= 9 && payload_len(bytes) > 16_384 {
                self.recycle(input.page);
                self.fail(ConnectionResult::Protocol(H2ProtocolError::connection(
                    H2ErrorCode::FrameSizeError,
                    "frame exceeds advertised maximum",
                )));
                return None;
            }
            if bytes.len() < 9 || bytes.len() < 9 + payload_len(bytes) {
                let Some(mut page) = self.page() else {
                    self.recycle(input.page);
                    self.fail(ConnectionResult::ResourceExhausted);
                    return None;
                };
                Rc::get_mut(&mut page).expect("new assembly page").bytes[..bytes.len()]
                    .copy_from_slice(bytes);
                self.assembly = Some(Assembly {
                    page,
                    filled: bytes.len(),
                });
                self.recycle(input.page);
                None
            } else {
                let end = input.start + 9 + payload_len(bytes);
                let output = self.dispatch_frame(&input.page, input.start..end, ports);
                input.start = end;
                if input.start < input.end && self.check_live().is_ok() {
                    self.input = Some(input);
                } else {
                    self.recycle(input.page);
                }
                output
            }
        }
    }

    fn dispatch_frame<P: Ports<B>>(
        &mut self,
        page: &Rc<Page>,
        range: std::ops::Range<usize>,
        ports: &mut P,
    ) -> Option<P::Output> {
        let bytes = &page.bytes[range.clone()];
        let id = StreamId(
            u32::from_be_bytes(bytes[5..9].try_into().expect("frame header")) & 0x7fff_ffff,
        );
        let is_data = bytes[3] == 0;
        let wire_len = bytes.len() - 9;
        if is_data {
            if wire_len > self.receive_window {
                self.fail(ConnectionResult::Protocol(H2ProtocolError::connection(
                    H2ErrorCode::FlowControlError,
                    "connection receive window exceeded",
                )));
                return None;
            }
            self.receive_window -= wire_len;
            if let Some(state) = self.streams.get_mut(&id)
                && state.receive_end.is_none()
            {
                if wire_len > state.receive_window {
                    if self.refund(id, wire_len, false).is_err()
                        || self.reset(id, H2ErrorCode::FlowControlError).is_err()
                    {
                        self.fail(ConnectionResult::ResourceExhausted);
                    }
                    return None;
                }
                state.receive_window -= wire_len;
            }
        }
        enum Event<'a> {
            Progress,
            Headers(StreamId, HeadKind, bool),
            Data(StreamId, &'a [u8], usize, bool),
            Discard(StreamId, usize),
            Reset(StreamId, u32),
            Goaway(u32, u32),
        }
        let result: Result<(Event<'_>, Vec<u8>), H2ProtocolError> = match &mut self.protocol {
            Protocol::Client(role) => role
                .accept_driver_bytes_ref(bytes)
                .map(|(event, _, output)| {
                    (
                        match event {
                            None | Some(H2DriverClientEvent::Progress) => Event::Progress,
                            Some(H2DriverClientEvent::ResponseHeaders {
                                stream_id,
                                section,
                                end_stream,
                            }) => {
                                let status = section.response_status.expect("validated status");
                                Event::Headers(
                                    StreamId(stream_id),
                                    if status < 200 {
                                        HeadKind::Informational(status)
                                    } else {
                                        HeadKind::Response(status)
                                    },
                                    end_stream,
                                )
                            }
                            Some(H2DriverClientEvent::Trailers { stream_id }) => {
                                Event::Headers(StreamId(stream_id), HeadKind::Trailers, true)
                            }
                            Some(H2DriverClientEvent::Data {
                                stream_id,
                                payload,
                                flow_control_len,
                                end_stream,
                            }) => Event::Data(
                                StreamId(stream_id),
                                payload,
                                flow_control_len,
                                end_stream,
                            ),
                            Some(H2DriverClientEvent::DiscardedData {
                                stream_id,
                                flow_control_len,
                            }) => Event::Discard(StreamId(stream_id), flow_control_len),
                            Some(H2DriverClientEvent::Reset {
                                stream_id,
                                error_code,
                            }) => Event::Reset(StreamId(stream_id), error_code),
                            Some(H2DriverClientEvent::Goaway {
                                last_stream_id,
                                error_code,
                            }) => Event::Goaway(last_stream_id, error_code),
                        },
                        output,
                    )
                })
                .map_err(|error| {
                    role.h2_error_from_server_error(
                        error,
                        Some(H2FrameHead {
                            payload_len: wire_len,
                            frame_type: H2FrameType::from_raw(bytes[3]),
                            flags: bytes[4],
                            stream_id: id.0,
                        }),
                    )
                }),
            Protocol::Server(role) => role
                .accept_driver_event_bytes_ref(bytes)
                .map(|progress| {
                    (
                        if let Some(error) = progress.handled_protocol_error {
                            match error.scope {
                                H2ErrorScope::Stream(id) => {
                                    Event::Reset(StreamId(id), error.code.as_u32())
                                }
                                _ => Event::Progress,
                            }
                        } else {
                            match progress.event {
                                None | Some(H2DriverServerEvent::Progress) => Event::Progress,
                                Some(H2DriverServerEvent::RequestHeaders {
                                    stream_id,
                                    end_stream,
                                    ..
                                }) => Event::Headers(
                                    StreamId(stream_id),
                                    HeadKind::Request,
                                    end_stream,
                                ),
                                Some(H2DriverServerEvent::Trailers { stream_id }) => {
                                    Event::Headers(StreamId(stream_id), HeadKind::Trailers, true)
                                }
                                Some(H2DriverServerEvent::Data {
                                    stream_id,
                                    payload,
                                    flow_control_len,
                                    end_stream,
                                }) => Event::Data(
                                    StreamId(stream_id),
                                    payload,
                                    flow_control_len,
                                    end_stream,
                                ),
                                Some(H2DriverServerEvent::DiscardedData {
                                    stream_id,
                                    flow_control_len,
                                }) => Event::Discard(StreamId(stream_id), flow_control_len),
                                Some(H2DriverServerEvent::Reset {
                                    stream_id,
                                    error_code,
                                }) => Event::Reset(StreamId(stream_id), error_code),
                                Some(H2DriverServerEvent::Goaway {
                                    last_stream_id,
                                    error_code,
                                }) => Event::Goaway(last_stream_id, error_code),
                            }
                        },
                        progress.output,
                    )
                })
                .map_err(|error| {
                    role.take_reported_protocol_error()
                        .unwrap_or_else(|| role.h2_error_from_server_error(error, None))
                }),
        };
        let (event, output) = match result {
            Ok(result) => result,
            Err(error) => {
                match error.scope {
                    H2ErrorScope::Connection => self.fail(ConnectionResult::Protocol(error)),
                    H2ErrorScope::Stream(stream) => {
                        if is_data && self.refund(id, wire_len, false).is_err() {
                            self.fail(ConnectionResult::ResourceExhausted);
                        }
                        if self
                            .frame(
                                H2FrameType::RstStream,
                                stream,
                                &error.code.as_u32().to_be_bytes(),
                            )
                            .is_err()
                        {
                            self.fail(ConnectionResult::ResourceExhausted);
                        }
                        self.protocol.reset(StreamId(stream));
                        self.terminate_stream(
                            StreamId(stream),
                            StreamOutcome::Reset(error.code.as_u32()),
                        );
                    }
                }
                return None;
            }
        };
        let output_owner = if output.len() >= 9 && output[3] == 3 {
            let id = StreamId(u32::from_be_bytes(
                output[5..9].try_into().expect("RST_STREAM header"),
            ));
            self.streams.contains_key(&id).then_some(id)
        } else {
            None
        };
        if self.queue_control(output, output_owner).is_err() {
            self.fail(ConnectionResult::ResourceExhausted);
            return None;
        }
        if self.settings_output == SettingsOutput::NotQueued && self.protocol.settings_owed() {
            self.controls.back_mut().expect("server SETTINGS output").2 =
                ControlPurpose::LocalSettings;
            self.settings_output = SettingsOutput::Queued;
        }
        self.settings_deadline();
        if bytes[3] == 8 || bytes[3] == 4 {
            if id.0 == 0 {
                // Each selected probe checks one previously blocked stream.
                // A connection update records work without a drive-time stream scan.
                if self.probing.is_empty() {
                    std::mem::swap(&mut self.probing, &mut self.blocked);
                } else {
                    self.probe_again = true;
                }
            } else if self.blocked.remove(&id) {
                self.mark_ready(id);
            }
        }
        if matches!(
            self.life,
            Life::Draining {
                final_goaway: false
            }
        ) && matches!(&self.protocol, Protocol::Server(role) if !role.timer_obligations().graceful_shutdown_ping)
        {
            self.life = Life::Draining { final_goaway: true };
        }
        match event {
            Event::Progress => None,
            Event::Headers(id, kind, end) => {
                if matches!(kind, HeadKind::Response(_) | HeadKind::Informational(_))
                    && let Some(state) = self.streams.get_mut(&id)
                {
                    state.response_seen = true;
                }
                if kind == HeadKind::Request {
                    if self.streams.len() >= self.config.http.max_active_streams() {
                        if self
                            .frame(
                                H2FrameType::RstStream,
                                id.0,
                                &H2ErrorCode::RefusedStream.as_u32().to_be_bytes(),
                            )
                            .is_err()
                        {
                            self.fail(ConnectionResult::ResourceExhausted);
                        }
                        self.protocol.reset(id);
                        return None;
                    }
                    let fields = self.protocol.fields().expect("validated compact fields");
                    let method = (0..fields.len())
                        .filter_map(|i| fields.get(i))
                        .find(|f| f.name == b":method");
                    let mut state =
                        Stream::new(Source::AwaitingResponse, self.config.stream_receive_window);
                    state.request_head = method.is_some_and(|f| f.value == b"HEAD");
                    state.connect = method.is_some_and(|f| f.value == b"CONNECT");
                    self.streams.insert(id, state);
                }
                if matches!(kind, HeadKind::Response(200..=299))
                    && self.streams.get(&id).is_some_and(|state| state.connect)
                {
                    let state = self.streams.get_mut(&id).expect("CONNECT response");
                    state.expected_send = None;
                    state.body_limit = None;
                    if let Protocol::Client(role) = &mut self.protocol
                        && let Some(send) = role.send_mut(id.0)
                    {
                        send.sent.limit = None;
                    }
                }
                if matches!(kind, HeadKind::Response(_))
                    && matches!(self.protocol, Protocol::Client(_))
                    && self
                        .streams
                        .get(&id)
                        .is_some_and(|state| matches!(state.source, Source::AwaitingResponse))
                {
                    self.streams.get_mut(&id).expect("CONNECT response").source = Source::Ready;
                    self.mark_demand(id);
                }
                if end {
                    self.receive_end(id, StreamOutcome::Complete);
                }
                let fields = self.protocol.fields().expect("validated compact fields");
                ports.headers(Head {
                    stream: id,
                    kind,
                    end_stream: end,
                    fields,
                })
            }
            Event::Data(id, payload, flow, end) => {
                let offset = payload.as_ptr() as usize - page.bytes.as_ptr() as usize;
                let len = payload.len();
                if end {
                    self.receive_end(id, StreamOutcome::Complete);
                }
                if self.refund(id, flow - len, true).is_err() {
                    self.fail(ConnectionResult::ResourceExhausted);
                    return None;
                }
                let pressure = self.streams.get(&id).is_none_or(|state| {
                    state.leases.len() >= self.config.max_stream_fragments
                        || state.receive_capacity + page.bytes.len()
                            > self.config.max_stream_receive_capacity
                });
                if pressure {
                    if self.refund(id, len, false).is_err()
                        || self.reset(id, H2ErrorCode::EnhanceYourCalm).is_err()
                    {
                        self.fail(ConnectionResult::ResourceExhausted);
                    }
                    return None;
                }
                let token = self.token();
                let state = self.streams.get_mut(&id).expect("DATA stream");
                state.leases.insert(token.sequence);
                state.receive_capacity += page.bytes.len();
                ports.body(BodyOp {
                    token,
                    stream: id,
                    page: page.clone(),
                    range: offset..offset + len,
                })
            }
            Event::Discard(id, flow) => {
                if self.refund(id, flow, false).is_err() {
                    self.fail(ConnectionResult::ResourceExhausted);
                }
                None
            }
            Event::Reset(id, code) => {
                if is_data && self.refund(id, wire_len, false).is_err() {
                    self.fail(ConnectionResult::ResourceExhausted);
                }
                self.terminate_stream(id, StreamOutcome::Reset(code));
                None
            }
            Event::Goaway(last, code) => {
                let excluded: Vec<_> = self
                    .streams
                    .keys()
                    .copied()
                    .filter(|id| matches!(self.protocol, Protocol::Client(_)) && id.0 > last)
                    .collect();
                for id in excluded {
                    let state = self.streams.get(&id).expect("GOAWAY stream");
                    if state.outcome == StreamOutcome::Complete && !state.succeeded() {
                        let outcome = if state.response_seen {
                            StreamOutcome::ConnectionFailed
                        } else {
                            StreamOutcome::Unprocessed
                        };
                        self.protocol.reset(id);
                        self.terminate_stream(id, outcome);
                    }
                }
                if code != 0 {
                    self.fail(ConnectionResult::PeerClosed);
                } else {
                    self.life = Life::Draining { final_goaway: true };
                }
                None
            }
        }
    }
}

fn payload_len(bytes: &[u8]) -> usize {
    (usize::from(bytes[0]) << 16) | (usize::from(bytes[1]) << 8) | usize::from(bytes[2])
}

fn encode_window_update(bytes: &mut Vec<u8>, stream: u32, amount: usize) {
    assert!(
        (1..=0x7fff_ffff).contains(&amount),
        "receive credit fits the configured window"
    );
    bytes.extend_from_slice(&[0, 0, 4, 8, 0]);
    bytes.extend_from_slice(&stream.to_be_bytes());
    bytes.extend_from_slice(&(amount as u32).to_be_bytes());
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
