use crate::codec::{self, EncodedHead, Framing};
use crate::operations::{MetadataKind, WriteStorage};
use crate::state::{
    Admission, ArmedDeadline, Closing, ContinueGate, IoState, Lifecycle, MethodSemantics,
    Notification, ReceiveStorage, TimerPhase, Timers, Transmit, Upgrade,
};
use crate::*;
use std::mem::MaybeUninit;

#[path = "coordinator.rs"]
mod coordinator;
use crate::observation::Metric;
use coordinator::Boundary;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Rx {
    Head,
    Fixed(u64),
    Size,
    Chunk(u64),
    ChunkCrlf,
    Trailers,
    Eof,
    Done,
    Paused,
    AwaitingRequest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResponseMode {
    Conservative,
    Duplex,
    Upgrade,
}

#[derive(Debug)]
struct Exchange {
    id: ExchangeId,
    version: Version,
    method: MethodSemantics,
    persistent: bool,
    upgrade: Option<Vec<u8>>,
    expect: bool,
    continue_gate: ContinueGate,
    incoming_notification: Notification,
    source_notification: Notification,
    consume_request: bool,
}

#[derive(Debug)]
struct Core<B, W, const SERVER: bool> {
    id: ConnectionId,
    config: Config,
    sequence: u64,
    exchanges: u64,
    now: Tick,
    timers: Timers,
    receive: ReceiveStorage<B>,
    start: usize,
    end: usize,
    head: Vec<u8>,
    rx: Rx,
    received: u64,
    no_content: bool,
    outgoing_bytes: u64,
    metadata_bytes: usize,
    metadata_fields: usize,
    chunk_metadata_bytes: usize,
    informational: usize,
    outgoing_metadata_bytes: usize,
    outgoing_metadata_fields: usize,
    outgoing_informational: usize,
    outgoing_chunk_metadata_bytes: usize,
    incoming_connection_fields: Vec<u8>,
    outgoing_connection_fields: Vec<u8>,
    credit: usize,
    eof: bool,
    exchange: Option<Exchange>,
    tx: Transmit,
    output: Option<WriteOp<W>>,
    pending_body: Option<(BodyId, SendBody<W>)>,
    body_result: Option<BodySent<W>>,
    read: IoState,
    write: IoState,
    close_after: bool,
    failure: Option<Failure>,
    lifecycle: Lifecycle,
    boundary: Boundary,
    #[cfg(feature = "metrics")]
    counters: Counters,
    failure_logged: bool,
}

/// A single-exchange HTTP/1 server with independently owned I/O operations.
#[derive(Debug)]
pub struct Server<B: Buffer, W: AsRef<[u8]> = B> {
    core: Core<B, W, true>,
}

/// A single-exchange HTTP/1 client. Requests are never automatically replayed.
#[derive(Debug)]
pub struct Client<B: Buffer, W: AsRef<[u8]> = B> {
    core: Core<B, W, false>,
}

enum ParsedHead<'a> {
    Request(RequestHead<'a>),
    Response(ResponseHead<'a>, bool),
}

impl<B: Buffer, W: AsRef<[u8]>, const SERVER: bool> Core<B, W, SERVER> {
    fn new(
        id: ConnectionId,
        config: Config,
        mut input: B,
        now: Tick,
    ) -> Result<Self, CommandError> {
        if input.as_ref().is_empty()
            || input.as_ref().len() != input.as_mut().len()
            || input.as_ref().len() > config.max_buffer_bytes
            || config.max_head_bytes < 16
            || config.max_headers == 0
            || config.max_headers > 128
            || config.max_chunk_line_bytes < 3
            || config.max_chunk_metadata_bytes < config.max_chunk_line_bytes
            || config.max_requests == 0
        {
            return Err(CommandError::InvalidConfig);
        }
        let mut core = Self {
            id,
            config,
            sequence: 0,
            exchanges: 0,
            now,
            timers: Timers::new(),
            receive: ReceiveStorage::Available(input),
            start: 0,
            end: 0,
            head: Vec::new(),
            rx: Rx::Head,
            received: 0,
            no_content: false,
            outgoing_bytes: 0,
            metadata_bytes: 0,
            metadata_fields: 0,
            chunk_metadata_bytes: 0,
            informational: 0,
            outgoing_metadata_bytes: 0,
            outgoing_metadata_fields: 0,
            outgoing_informational: 0,
            outgoing_chunk_metadata_bytes: 0,
            incoming_connection_fields: Vec::new(),
            outgoing_connection_fields: Vec::new(),
            credit: 0,
            eof: false,
            exchange: None,
            tx: Transmit::Idle,
            output: None,
            pending_body: None,
            body_result: None,
            read: IoState::Idle,
            write: IoState::Idle,
            close_after: false,
            failure: None,
            lifecycle: Lifecycle::Http(Admission::Accepting),
            boundary: Boundary::None,
            #[cfg(feature = "metrics")]
            counters: Counters::default(),
            failure_logged: false,
        };
        core.set_deadline(
            if SERVER {
                TimerPhase::Head
            } else {
                TimerPhase::Idle
            },
            if SERVER {
                core.config.head_timeout_ns
            } else {
                core.config.idle_timeout_ns
            },
        )?;
        Ok(core)
    }

    #[cfg(feature = "metrics")]
    fn metrics(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            connection: self.id,
            server: SERVER,
            observed_at: self.now,
            phase: match self.lifecycle {
                Lifecycle::Http(Admission::Accepting) => SnapshotPhase::Http,
                Lifecycle::Http(Admission::Draining) => SnapshotPhase::Draining,
                Lifecycle::Upgrade(_) => SnapshotPhase::Upgrading,
                Lifecycle::ErrorResponse => SnapshotPhase::ErrorResponse,
                Lifecycle::Closing(_) => SnapshotPhase::Closing,
                Lifecycle::Closed(_) => SnapshotPhase::Closed,
                Lifecycle::HandedOff => SnapshotPhase::HandedOff,
            },
            exchange: self.exchange.as_ref().map(|exchange| exchange.id),
            buffered_input_bytes: self.end - self.start,
            body_credit: self.credit,
            metadata_buffer_bytes: self.head.len(),
            metadata_buffer_capacity: self.head.capacity(),
            read_outstanding: self.read.operation().is_some(),
            write_outstanding: self.write.operation().is_some(),
            body_lease_outstanding: self.receive.lease().is_some(),
            failure: self.failure,
            counters: self.counters,
        }
    }

    #[inline]
    fn count(&mut self, metric: Metric, amount: u64) {
        #[cfg(feature = "metrics")]
        self.counters.add(metric, amount);
        #[cfg(not(feature = "metrics"))]
        let _ = (metric, amount);
    }

    #[inline]
    fn log<P: Ports<B, W>>(&self, ports: &mut P, event: LogEvent) {
        ports.log(self.id, self.now, event);
    }

    fn sequence(&mut self) -> Result<u64, CommandError> {
        let sequence = self
            .sequence
            .checked_add(1)
            .filter(|sequence| *sequence < u64::MAX)
            .ok_or(CommandError::SequenceExhausted)?;
        self.sequence = sequence;
        Ok(sequence)
    }

    fn operation(&mut self, kind: OperationKind) -> Option<OperationId> {
        match self.sequence() {
            Ok(sequence) => Some(OperationId {
                connection: self.id,
                sequence,
                kind,
            }),
            Err(_) => {
                self.fail(Failure::SequenceExhausted);
                None
            }
        }
    }

    fn set_deadline(
        &mut self,
        kind: TimerPhase,
        duration: Option<u64>,
    ) -> Result<(), CommandError> {
        let phase_timer = duration
            .map(|duration| {
                self.now
                    .0
                    .checked_add(duration)
                    .map(|at| (kind, Tick(at)))
                    .ok_or(CommandError::InvalidConfig)
            })
            .transpose()?;
        self.update_deadline(phase_timer, self.timers.continue_at)
    }

    fn update_deadline(
        &mut self,
        phase_timer: Option<(TimerPhase, Tick)>,
        continue_at: Option<Tick>,
    ) -> Result<(), CommandError> {
        let mut earliest = match (phase_timer, continue_at) {
            (Some(phase), Some(at)) if at < phase.1 => Some((TimerPhase::Continue, at)),
            (Some(phase), _) => Some(phase),
            (None, Some(at)) => Some((TimerPhase::Continue, at)),
            _ => None,
        };
        if let Some(at) = self.timers.upload_at
            && earliest.is_none_or(|(_, prior)| at < prior)
        {
            earliest = Some((TimerPhase::Upload, at));
        }
        if earliest
            != self
                .timers
                .armed
                .map(|armed| (armed.kind, armed.deadline.at))
        {
            let armed = match earliest {
                Some((kind, at)) => Some(ArmedDeadline {
                    deadline: Deadline {
                        connection: self.id,
                        sequence: self.sequence()?,
                        at,
                    },
                    kind,
                }),
                None => None,
            };
            self.timers.armed = armed;
            self.timers.notification = Notification::Pending;
        }
        self.timers.phase = phase_timer;
        self.timers.continue_at = continue_at;
        Ok(())
    }

    fn progress(&mut self) {
        if !self.lifecycle.is_closing() && !self.tx.stopped() && self.timers.upload_at.is_some() {
            self.arm_upload();
        }
        if !self.lifecycle.is_closing()
            && self
                .timers
                .phase
                .is_some_and(|(kind, _)| kind == TimerPhase::Body)
            && self
                .set_deadline(TimerPhase::Body, self.config.body_timeout_ns)
                .is_err()
        {
            self.fail(Failure::SequenceExhausted);
        }
    }

    fn clear_deadlines(&mut self) {
        self.timers.clear();
    }

    fn arm_upload(&mut self) {
        self.timers.upload_at = self
            .config
            .body_timeout_ns
            .and_then(|duration| self.now.0.checked_add(duration))
            .map(Tick);
        if self.config.body_timeout_ns.is_some() && self.timers.upload_at.is_none()
            || self
                .update_deadline(self.timers.phase, self.timers.continue_at)
                .is_err()
        {
            self.fail(Failure::SequenceExhausted);
        }
    }

    fn check_exchange(&self, exchange: ExchangeId) -> Result<(), CommandError> {
        if self.exchange.as_ref().map(|e| e.id) != Some(exchange) {
            Err(CommandError::StaleExchange)
        } else if !self.lifecycle.accepts_exchange_commands() {
            Err(CommandError::InvalidState)
        } else {
            Ok(())
        }
    }

    fn waiting_continue(&self) -> bool {
        self.exchange
            .as_ref()
            .is_some_and(|exchange| exchange.continue_gate == ContinueGate::Waiting)
    }

    fn release_continue(&mut self) {
        if let Some(exchange) = &mut self.exchange {
            exchange.continue_gate = ContinueGate::Open;
        }
    }

    fn fail(&mut self, failure: Failure) {
        let first_failure = self.failure.is_none();
        let incomplete_request =
            self.exchange.is_some() || !self.head.is_empty() || self.start < self.end;
        let status = match failure {
            Failure::Protocol | Failure::UnexpectedEof => Some((400, "Bad Request")),
            Failure::ExpectationFailed => Some((417, "Expectation Failed")),
            Failure::Limit if matches!(self.rx, Rx::Head | Rx::Trailers) => {
                Some((431, "Request Header Fields Too Large"))
            }
            Failure::Limit => Some((413, "Content Too Large")),
            Failure::Timeout if incomplete_request => Some((408, "Request Timeout")),
            _ => None,
        };
        self.failure.get_or_insert(failure);
        self.boundary = Boundary::None;
        self.credit = 0;
        self.rx = Rx::Paused;
        self.release_continue();
        self.clear_deadlines();
        if first_failure
            && SERVER
            && !self.tx.started()
            && let Some((status, reason)) = status
        {
            let response = Response {
                head: ResponseHead {
                    version: self
                        .exchange
                        .as_ref()
                        .map_or(Version::Http11, |exchange| exchange.version),
                    status,
                    reason,
                    headers: &[],
                },
                body: BodyLength::Empty,
            };
            if let Ok(EncodedHead { bytes, fields, .. }) =
                codec::encode_response(response, false, true, false, &self.config)
                && self.check_outgoing_metadata(bytes.len(), fields).is_ok()
            {
                self.outgoing_metadata_bytes += bytes.len();
                self.outgoing_metadata_fields += fields;
                self.tx = Transmit::begin(Framing::Empty);
                self.lifecycle = Lifecycle::ErrorResponse;
                self.close_after = true;
                if let Some(op) = self.output.as_mut() {
                    let WriteStorage::Head {
                        bytes: queued,
                        kind,
                    } = &mut op.storage
                    else {
                        unreachable!()
                    };
                    queued.reserve_exact(bytes.len());
                    queued.extend_from_slice(&bytes);
                    *kind = MetadataKind::FinalHead;
                } else {
                    self.queue_head(bytes, MetadataKind::FinalHead);
                }
                if self
                    .set_deadline(TimerPhase::Body, self.config.body_timeout_ns)
                    .is_ok()
                {
                    return;
                }
            }
        }
        self.lifecycle.begin_closing();
        self.clear_deadlines();
    }

    fn queue_head(&mut self, bytes: Vec<u8>, kind: MetadataKind) {
        // A queued operation receives its external identity only at issuance.
        self.output = Some(WriteOp {
            id: OperationId {
                connection: self.id,
                sequence: 0,
                kind: OperationKind::Write,
            },
            storage: WriteStorage::Head { bytes, kind },
            cursor: 0,
        });
    }

    fn request(&mut self, request: Request<'_>) -> Result<ExchangeId, CommandError> {
        if self.exchange.is_some()
            || self.start < self.end
            || self.lifecycle != Lifecycle::Http(Admission::Accepting)
        {
            return Err(CommandError::InvalidState);
        }
        let EncodedHead {
            bytes: head,
            framing,
            fields,
        } = codec::encode_request(request, &self.config)?;
        self.check_outgoing_metadata(
            head.len()
                .saturating_add(if framing == Framing::Chunked { 5 } else { 0 }),
            fields,
        )?;
        let upgrade = codec::upgrade_protocols(request.head.headers)
            .map_err(|_| CommandError::InvalidHead)?;
        if upgrade.is_some() && request.head.version != Version::Http11 {
            return Err(CommandError::InvalidHead);
        }
        let connection_fields = codec::connection_fields(request.head.headers)
            .map_err(|_| CommandError::InvalidHead)?;
        self.sequence
            .checked_add(2)
            .filter(|sequence| *sequence < u64::MAX)
            .ok_or(CommandError::SequenceExhausted)?;
        self.set_deadline(TimerPhase::Head, self.config.head_timeout_ns)?;
        let id = ExchangeId {
            connection: self.id,
            sequence: self.sequence()?,
        };
        self.exchange = Some(Exchange {
            id,
            version: request.head.version,
            method: MethodSemantics::from_method(request.head.method),
            persistent: codec::persistent(request.head.version, request.head.headers),
            upgrade,
            expect: request.expect_continue,
            continue_gate: if request.expect_continue
                && request.head.version == Version::Http11
                && framing != Framing::Empty
            {
                ContinueGate::Waiting
            } else {
                ContinueGate::Open
            },
            incoming_notification: Notification::Pending,
            source_notification: Notification::Pending,
            consume_request: false,
        });
        self.exchanges += 1;
        self.count(Metric::ExchangesStarted, 1);
        self.tx = Transmit::begin(framing);
        self.outgoing_connection_fields = connection_fields;
        self.outgoing_metadata_bytes = head.len();
        self.outgoing_metadata_fields = fields;
        self.queue_head(head, MetadataKind::FinalHead);
        Ok(id)
    }

    fn respond(
        &mut self,
        exchange: ExchangeId,
        mut response: Response<'_>,
        mode: ResponseMode,
    ) -> Result<(), CommandError> {
        self.check_exchange(exchange)?;
        let upgrade = mode == ResponseMode::Upgrade;
        let current = self.exchange.as_ref().unwrap();
        if self.tx.started()
            || (!upgrade && response.head.status < 200)
            || (upgrade && self.rx != Rx::Done)
            || (upgrade && self.lifecycle.is_draining())
        {
            return Err(CommandError::InvalidState);
        }
        response.head.version = current.version;
        let tunnel = current.method == MethodSemantics::Connect
            && (200..300).contains(&response.head.status);
        if upgrade {
            if !tunnel
                && (current.version != Version::Http11
                    || response.head.status != 101
                    || !current.upgrade.as_ref().is_some_and(|requested| {
                        codec::valid_upgrade(requested, response.head.headers)
                    }))
            {
                return Err(CommandError::InvalidHead);
            }
        } else if tunnel || response.head.status == 101 {
            return Err(CommandError::InvalidState);
        }
        let connection_fields = codec::connection_fields(response.head.headers)
            .map_err(|_| CommandError::InvalidHead)?;
        let early = self.rx != Rx::Done;
        let close = !current.persistent
            || (early && mode != ResponseMode::Duplex)
            || self.lifecycle.is_draining()
            || self.exchanges >= self.config.max_requests;
        let EncodedHead {
            bytes: final_bytes,
            framing,
            fields,
        } = codec::encode_response(
            response,
            current.method == MethodSemantics::Head,
            close && !upgrade,
            tunnel,
            &self.config,
        )?;
        let continue_first = current.expect
            && self.start == self.end
            && !self.eof
            && !matches!(self.rx, Rx::Done | Rx::Paused)
            && (mode == ResponseMode::Duplex
                || (self.credit != 0
                    && (200..300).contains(&response.head.status)
                    && framing != Framing::Empty));
        let bytes = if continue_first {
            if self.outgoing_informational >= self.config.max_informational_responses {
                return Err(CommandError::Limit);
            }
            let mut bytes = codec::encode_response(
                Response {
                    head: ResponseHead {
                        version: current.version,
                        status: 100,
                        reason: "Continue",
                        headers: &[],
                    },
                    body: BodyLength::Empty,
                },
                false,
                false,
                false,
                &self.config,
            )?
            .bytes;
            // The generated 100 Continue has no fields to add to `fields`.
            if bytes.len().saturating_add(final_bytes.len()) > self.config.max_head_bytes {
                return Err(CommandError::Limit);
            }
            bytes.reserve_exact(final_bytes.len());
            bytes.extend_from_slice(&final_bytes);
            bytes
        } else {
            final_bytes
        };
        self.check_outgoing_metadata(
            bytes
                .len()
                .saturating_add(if framing == Framing::Chunked { 5 } else { 0 }),
            fields,
        )?;
        self.close_after = !upgrade
            && (close
                || framing == Framing::Eof
                || codec::has_token(response.head.headers, "connection", b"close"));
        if upgrade {
            self.lifecycle.begin_upgrade();
        }
        self.tx = Transmit::begin(framing);
        self.exchange.as_mut().unwrap().consume_request = mode == ResponseMode::Duplex;
        self.outgoing_connection_fields = connection_fields;
        if continue_first {
            self.exchange.as_mut().unwrap().expect = false;
            self.outgoing_informational += 1;
        }
        self.outgoing_metadata_bytes += bytes.len();
        self.outgoing_metadata_fields += fields;
        if let Some(op) = self.output.as_mut() {
            let WriteStorage::Head {
                bytes: queued,
                kind,
            } = &mut op.storage
            else {
                unreachable!()
            };
            queued.reserve_exact(bytes.len());
            queued.extend_from_slice(&bytes);
            *kind = MetadataKind::FinalHead;
        } else {
            self.queue_head(bytes, MetadataKind::FinalHead);
        }
        Ok(())
    }

    fn inform(
        &mut self,
        exchange: ExchangeId,
        mut head: ResponseHead<'_>,
    ) -> Result<(), CommandError> {
        self.check_exchange(exchange)?;
        head.version = self.exchange.as_ref().unwrap().version;
        if self.tx.started()
            || self.output.is_some()
            || self.write.operation().is_some()
            || !(100..=199).contains(&head.status)
            || head.status == 101
            || head.version != Version::Http11
            || self.outgoing_informational >= self.config.max_informational_responses
            || (head.status == 100
                && (!self.exchange.as_ref().unwrap().expect
                    || self.start < self.end
                    || matches!(self.rx, Rx::Done | Rx::Paused)))
        {
            return Err(CommandError::InvalidState);
        }
        let EncodedHead { bytes, fields, .. } = codec::encode_response(
            Response {
                head,
                body: BodyLength::Empty,
            },
            false,
            false,
            false,
            &self.config,
        )?;
        self.check_outgoing_metadata(bytes.len(), fields)?;
        if head.status == 100 {
            self.exchange.as_mut().unwrap().expect = false;
        }
        self.outgoing_metadata_bytes += bytes.len();
        self.outgoing_metadata_fields += fields;
        self.outgoing_informational += 1;
        self.queue_head(bytes, MetadataKind::Informational);
        Ok(())
    }

    fn send_body(&mut self, command: SendBody<W>) -> Result<BodyId, Rejected<SendBody<W>>> {
        let reason = if self.check_exchange(command.exchange).is_err() || !self.tx.accepts_data() {
            Some(RejectReason::InvalidState)
        } else if self.pending_body.is_some()
            || self.body_result.is_some()
            || self.output.is_some()
            || self.write.operation().is_some()
            || self.waiting_continue()
        {
            Some(RejectReason::NoCapacity)
        } else if command.range.start > command.range.end
            || command.range.end > command.buffer.as_ref().len()
            || command.buffer.as_ref().len() > self.config.max_buffer_bytes
            || command.range.len() > self.config.max_buffer_bytes
            || command.range.is_empty()
        {
            Some(RejectReason::InvalidRange)
        } else {
            match self.tx.framing() {
                Framing::Empty => Some(RejectReason::InvalidState),
                Framing::Fixed(left)
                    if command.range.len() as u64 > left
                        || (command.end && command.range.len() as u64 != left) =>
                {
                    Some(RejectReason::InvalidCount)
                }
                _ => None,
            }
        };
        let reason = reason.or_else(|| {
            self.outgoing_bytes
                .checked_add(command.range.len() as u64)
                .is_none_or(|n| n > self.config.max_body_bytes)
                .then_some(RejectReason::Limit)
        });
        let chunk_metadata = if self.tx.framing() == Framing::Chunked {
            command.range.len().max(1).ilog(16) as usize + 5
        } else {
            0
        };
        let reason = reason.or_else(|| {
            (self.tx.framing() == Framing::Chunked
                && (chunk_metadata - 2 > self.config.max_chunk_line_bytes
                    || self
                        .outgoing_chunk_metadata_bytes
                        .saturating_add(chunk_metadata)
                        .saturating_add(3)
                        > self.config.max_chunk_metadata_bytes))
                .then_some(RejectReason::Limit)
        });
        let reason = reason.or_else(|| {
            (self.tx.framing() == Framing::Chunked
                && command.end
                && self.check_outgoing_metadata(5, 0).is_err())
            .then_some(RejectReason::Limit)
        });
        if let Some(reason) = reason {
            return Err(Rejected {
                reason,
                value: command,
            });
        }
        let sequence = match self.sequence() {
            Ok(sequence) => sequence,
            Err(_) => {
                return Err(Rejected {
                    reason: RejectReason::InvalidState,
                    value: command,
                });
            }
        };
        let id = BodyId {
            connection: self.id,
            sequence,
        };
        self.tx.accept_data(command.range.len(), command.end);
        self.outgoing_bytes += command.range.len() as u64;
        self.count(Metric::ProducerAccepted, command.range.len() as u64);
        self.outgoing_chunk_metadata_bytes += chunk_metadata;
        if self.tx.framing() == Framing::Chunked && command.end {
            self.outgoing_chunk_metadata_bytes += 3;
        }
        self.pending_body = Some((id, command));
        Ok(id)
    }

    fn send_body_eager(&mut self, command: SendBody<W>) -> Result<BodyId, Rejected<SendBody<W>>> {
        let eligible = matches!(self.tx.framing(), Framing::Fixed(left) if left == command.range.len() as u64)
            && command.end
            && self.write == IoState::Idle
            && self.output.as_ref().is_some_and(|op| {
                op.cursor == 0
                    && matches!(
                        op.storage,
                        WriteStorage::Head {
                            kind: MetadataKind::FinalHead,
                            ..
                        }
                    )
            });
        if !eligible {
            return Err(Rejected {
                reason: RejectReason::NoCapacity,
                value: command,
            });
        }
        let head = self.output.take().unwrap();
        let admitted = self.send_body(command);
        self.output = Some(head);
        let id = admitted?;
        let WriteStorage::Head { bytes, .. } = self.output.take().unwrap().storage else {
            unreachable!()
        };
        let (body_id, command) = self.pending_body.take().unwrap();
        self.output = Some(WriteOp {
            id: OperationId {
                connection: self.id,
                sequence: 0,
                kind: OperationKind::Write,
            },
            storage: WriteStorage::HeadBody {
                bytes,
                command,
                body_id,
            },
            cursor: 0,
        });
        if !SERVER {
            self.arm_upload();
        }
        Ok(id)
    }

    fn finish_body(
        &mut self,
        exchange: ExchangeId,
        trailers: Headers<'_>,
    ) -> Result<(), CommandError> {
        self.check_exchange(exchange)?;
        if !self.tx.accepts_data()
            || self.pending_body.is_some()
            || self.output.is_some()
            || self.write.operation().is_some()
            || self.body_result.is_some()
        {
            return Err(CommandError::InvalidState);
        }
        if matches!(self.tx.framing(), Framing::Fixed(n) if n != 0) {
            return Err(CommandError::InvalidFraming);
        }
        if !trailers.is_empty() && self.tx.framing() != Framing::Chunked {
            return Err(CommandError::InvalidFraming);
        }
        let bytes = if self.tx.framing() == Framing::Chunked {
            if self.outgoing_chunk_metadata_bytes.saturating_add(3)
                > self.config.max_chunk_metadata_bytes
            {
                return Err(CommandError::Limit);
            }
            codec::encode_trailers(trailers, &self.outgoing_connection_fields, &self.config)?
        } else {
            Vec::new()
        };
        self.check_outgoing_metadata(bytes.len(), trailers.len())?;
        self.outgoing_metadata_bytes += bytes.len();
        self.outgoing_metadata_fields += trailers.len();
        if self.tx.framing() == Framing::Chunked {
            self.outgoing_chunk_metadata_bytes += 3;
        }
        self.tx.finish_source();
        if bytes.is_empty() {
            self.settle_transmit();
        } else {
            self.queue_head(bytes, MetadataKind::BodyEnd);
        }
        Ok(())
    }

    fn validate(&self, id: OperationId, expected: Option<OperationId>) -> Result<(), RejectReason> {
        if id.connection != self.id {
            Err(RejectReason::WrongConnection)
        } else if expected != Some(id) {
            Err(RejectReason::Stale)
        } else {
            Ok(())
        }
    }

    fn complete_read(
        &mut self,
        completion: ReadCompletion<B>,
    ) -> Result<(), Rejected<ReadCompletion<B>>> {
        let reason = self
            .validate(completion.op.id, self.read.operation())
            .err()
            .or_else(|| {
                completion
                    .result
                    .ok()
                    .filter(|n| *n > completion.op.range.len())
                    .map(|_| RejectReason::InvalidCount)
            });
        if let Some(reason) = reason {
            return Err(Rejected {
                reason,
                value: completion,
            });
        }
        self.read.complete();
        let ReadCompletion { op, result } = completion;
        self.receive.complete_read(op.buffer);
        self.count(Metric::ReadCompletions, 1);
        match result {
            Ok(n) => {
                self.count(Metric::ReadBytes, n as u64);
                self.start = op.range.start;
                self.end = op.range.start + n;
                self.eof |= n == 0;
                if n != 0 {
                    if self.rx == Rx::AwaitingRequest && self.start_request_head().is_err() {
                        self.fail(Failure::SequenceExhausted);
                    } else {
                        self.progress();
                    }
                }
            }
            Err(error) if self.lifecycle.is_closing() || self.rx == Rx::Paused => {
                let _ = error;
            }
            Err(IoError {
                kind: IoErrorKind::WouldBlock,
                ..
            }) => self.read.wait_for_readiness(),
            Err(IoError {
                kind: IoErrorKind::Interrupted,
                ..
            }) => {}
            Err(error) => self.fail(Failure::Transport(error)),
        }
        Ok(())
    }

    // Rejection returns the exclusive lease without an allocation.
    #[allow(clippy::result_large_err)]
    fn complete_write(
        &mut self,
        completion: WriteCompletion<W>,
    ) -> Result<(), Rejected<WriteCompletion<W>>> {
        let reason = self
            .validate(completion.op.id, self.write.operation())
            .err()
            .or_else(|| {
                completion
                    .result
                    .ok()
                    .filter(|n| *n > completion.op.remaining())
                    .map(|_| RejectReason::InvalidCount)
            });
        if let Some(reason) = reason {
            return Err(Rejected {
                reason,
                value: completion,
            });
        }
        self.write.complete();
        let WriteCompletion { mut op, result } = completion;
        let acceptance = if matches!(
            result,
            Err(IoError {
                kind: IoErrorKind::UnknownProgress | IoErrorKind::CancelledUnknownProgress,
                ..
            })
        ) {
            Acceptance::LowerBound
        } else {
            Acceptance::Exact
        };
        self.count(Metric::WriteCompletions, 1);
        if let Ok(n) = result {
            self.count(Metric::WrittenBytes, n as u64);
        }
        if acceptance == Acceptance::LowerBound {
            self.count(Metric::UncertainWrites, 1);
        }
        match result {
            Ok(0) => self.fail(Failure::WriteZero),
            Ok(n) => {
                op.cursor += n;
                self.progress();
            }
            Err(IoError {
                kind: IoErrorKind::WouldBlock,
                ..
            }) if !self.lifecycle.is_closing() => self.write.wait_for_readiness(),
            Err(IoError {
                kind: IoErrorKind::Interrupted,
                ..
            }) if !self.lifecycle.is_closing() => {}
            Err(IoError {
                kind: IoErrorKind::Cancelled | IoErrorKind::CancelledUnknownProgress,
                ..
            }) if self.tx.stopped() => {}
            Err(error) => self.fail(Failure::Transport(error)),
        }
        if self.lifecycle.is_closing() || self.tx.stopped() || op.remaining() == 0 {
            self.settle_write(op, acceptance);
        } else {
            self.output = Some(op);
        }
        Ok(())
    }

    fn settle_write(&mut self, op: WriteOp<W>, acceptance: Acceptance) {
        match op.storage {
            WriteStorage::Head { kind, .. } => {
                if kind != MetadataKind::Informational
                    && self.tx.started()
                    && self.tx.source_finished()
                {
                    self.settle_transmit();
                }
                if !SERVER
                    && !self.tx.source_finished()
                    && !self.tx.stopped()
                    && !self.lifecycle.is_closing()
                {
                    self.arm_upload();
                }
                if !SERVER
                    && self.waiting_continue()
                    && !self.lifecycle.is_closing()
                    && !self.tx.stopped()
                {
                    let at = self
                        .config
                        .continue_timeout_ns
                        .and_then(|duration| self.now.0.checked_add(duration))
                        .map(Tick);
                    if self.config.continue_timeout_ns.is_some() && at.is_none()
                        || self.update_deadline(self.timers.phase, at).is_err()
                    {
                        self.fail(Failure::SequenceExhausted);
                    }
                }
            }
            WriteStorage::HeadBody {
                bytes,
                command,
                body_id,
            } => {
                self.settle_body(command, body_id, op.cursor, bytes.len(), false, acceptance);
            }
            WriteStorage::Body {
                command,
                body_id,
                prefix_len,
                chunked,
                ..
            } => {
                self.settle_body(command, body_id, op.cursor, prefix_len, chunked, acceptance);
            }
        }
        if self.tx.settled()
            && self.timers.upload_at.take().is_some()
            && self
                .update_deadline(self.timers.phase, self.timers.continue_at)
                .is_err()
        {
            self.fail(Failure::SequenceExhausted);
        }
    }

    fn settle_body(
        &mut self,
        command: SendBody<W>,
        body_id: BodyId,
        cursor: usize,
        prefix_len: usize,
        chunked: bool,
        acceptance: Acceptance,
    ) {
        let accepted = cursor.saturating_sub(prefix_len).min(command.range.len());
        self.body_result = Some(BodySent {
            exchange: command.exchange,
            id: body_id,
            buffer: command.buffer,
            accepted,
            acceptance,
            result: self
                .failure
                .or(self.tx.stopped().then_some(Failure::EarlyResponse))
                .map_or(Ok(()), Err),
        });
        if self.tx.source_finished() && !self.lifecycle.is_closing() && !self.tx.stopped() {
            if chunked {
                self.outgoing_metadata_bytes += 5;
                self.queue_head(b"0\r\n\r\n".to_vec(), MetadataKind::BodyEnd);
            } else {
                self.settle_transmit();
            }
        }
    }

    fn complete_readiness(
        &mut self,
        completion: ReadinessCompletion,
    ) -> Result<(), Rejected<ReadinessCompletion>> {
        let live = match completion.op.direction {
            Direction::Read => self.read.operation(),
            Direction::Write => self.write.operation(),
        };
        if let Err(reason) = self.validate(completion.op.id, live) {
            return Err(Rejected {
                reason,
                value: completion,
            });
        }
        match completion.op.direction {
            Direction::Read => self.read.complete(),
            Direction::Write => self.write.complete(),
        }
        if let Err(error) = completion.result {
            if error.kind == IoErrorKind::Interrupted && !self.lifecycle.is_closing() {
                match completion.op.direction {
                    Direction::Read => self.read.wait_for_readiness(),
                    Direction::Write => self.write.wait_for_readiness(),
                }
            } else if !self.lifecycle.is_closing()
                && self.rx != Rx::Paused
                && !(self.tx.stopped()
                    && completion.op.direction == Direction::Write
                    && error.kind == IoErrorKind::Cancelled)
            {
                self.fail(Failure::Transport(error));
            }
        }
        Ok(())
    }

    fn release_body(
        &mut self,
        completion: BodyCompletion<B>,
    ) -> Result<(), Rejected<BodyCompletion<B>>> {
        let reason = self
            .validate(completion.op.id, self.receive.lease())
            .err()
            .or_else(|| {
                (completion.consumed > completion.op.range.len())
                    .then_some(RejectReason::InvalidCount)
            });
        if let Some(reason) = reason {
            return Err(Rejected {
                reason,
                value: completion,
            });
        }
        self.receive.release(completion.op.buffer);
        self.start = completion.op.range.start + completion.consumed;
        self.end = completion.op.buffered_end;
        if completion.consumed == 0 {
            self.credit = 0;
        }
        self.received += completion.consumed as u64;
        self.count(Metric::BodyConsumed, completion.consumed as u64);
        if completion.consumed != 0 {
            self.progress();
        }
        self.rx = match self.rx {
            Rx::Fixed(n) => {
                if n == completion.consumed as u64 {
                    Rx::Done
                } else {
                    Rx::Fixed(n - completion.consumed as u64)
                }
            }
            Rx::Chunk(n) => {
                if n == completion.consumed as u64 {
                    Rx::ChunkCrlf
                } else {
                    Rx::Chunk(n - completion.consumed as u64)
                }
            }
            other => other,
        };
        if self.rx == Rx::Done {
            self.incoming_ended();
        }
        Ok(())
    }

    fn grant_body_credit(
        &mut self,
        exchange: ExchangeId,
        bytes: usize,
    ) -> Result<(), CommandError> {
        self.check_exchange(exchange)?;
        let credit = self.credit.checked_add(bytes).ok_or(CommandError::Limit)?;
        if bytes > self.config.max_buffer_bytes || credit > self.config.max_buffer_bytes {
            return Err(CommandError::Limit);
        }
        self.credit = credit;
        Ok(())
    }

    fn complete_close(
        &mut self,
        completion: CloseCompletion,
    ) -> Result<(), Rejected<CloseCompletion>> {
        if let Err(reason) = self.validate(completion.op.id, self.lifecycle.close_operation()) {
            return Err(Rejected {
                reason,
                value: completion,
            });
        }
        if let Err(error) = completion.result {
            self.failure.get_or_insert(Failure::Transport(error));
        }
        self.lifecycle.complete_close();
        Ok(())
    }

    fn shutdown(&mut self, mode: ShutdownMode) {
        if self.lifecycle.is_terminal() {
            return;
        }
        if mode == ShutdownMode::Abort || self.lifecycle.is_upgrade() {
            self.fail(Failure::Cancelled);
        } else {
            if matches!(self.lifecycle, Lifecycle::Http(_)) {
                self.lifecycle = Lifecycle::Http(Admission::Draining);
            }
            if self.exchange.is_none() {
                self.lifecycle.begin_closing();
                self.clear_deadlines();
            }
        }
    }

    fn observe_time(&mut self, now: Tick) -> Result<(), CommandError> {
        if now < self.now {
            return Err(CommandError::TimeRegression);
        }
        self.now = now;
        Ok(())
    }

    fn expire(&mut self, deadline: Deadline, now: Tick) -> Result<(), CommandError> {
        if self.timers.deadline() != Some(deadline) {
            return Err(CommandError::StaleDeadline);
        }
        if now < deadline.at {
            return Err(CommandError::EarlyDeadline);
        }
        if self.timers.kind() == Some(TimerPhase::Continue) && self.timers.phase.is_some() {
            self.sequence
                .checked_add(1)
                .filter(|n| *n < u64::MAX)
                .ok_or(CommandError::SequenceExhausted)?;
        }
        self.observe_time(now)?;
        self.count(Metric::Expirations, 1);
        if self.timers.kind() == Some(TimerPhase::Continue) {
            self.release_continue();
            self.update_deadline(self.timers.phase, None)?;
        } else {
            self.fail(Failure::Timeout);
        }
        Ok(())
    }

    fn take_upgrade(&mut self) -> Result<Handoff<B>, CommandError> {
        if self.lifecycle != Lifecycle::Upgrade(Upgrade::Ready)
            || self.receive.buffer().is_none()
            || !self.tx.settled()
            || self.rx != Rx::Done
            || !self.io_settled()
            || self.output.is_some()
            || self.pending_body.is_some()
            || self.body_result.is_some()
        {
            return Err(CommandError::NotReady);
        }
        self.lifecycle = Lifecycle::HandedOff;
        Ok(Handoff {
            connection: self.id,
            buffered: BufferedInput {
                buffer: self.receive.handoff(),
                range: self.start..self.end,
            },
        })
    }

    fn io_settled(&self) -> bool {
        self.read.operation().is_none()
            && self.write.operation().is_none()
            && self.receive.lease().is_none()
    }

    fn assert_invariants(&self) {
        // Metadata borrowing must end before a drive boundary, including yields.
        debug_assert!(!matches!(self.receive, ReceiveStorage::Parsing));
        if self.rx == Rx::AwaitingRequest {
            debug_assert!(SERVER && self.exchange.is_none() && self.head.is_empty());
        }
        match self.boundary {
            Boundary::None => {}
            Boundary::IncomingEnded => {
                debug_assert!(!SERVER);
                debug_assert_eq!(self.rx, Rx::Done);
                debug_assert!(matches!(self.lifecycle, Lifecycle::Http(_)));
            }
            Boundary::OutgoingSettled => {
                debug_assert!(SERVER && self.tx.settled());
                debug_assert!(matches!(self.lifecycle, Lifecycle::Http(_)));
            }
        }
        let read_owns_input = self
            .read
            .operation()
            .is_some_and(|id| id.kind() == OperationKind::Read);
        debug_assert_eq!(
            matches!(self.receive, ReceiveStorage::Reading),
            read_owns_input
        );
        debug_assert_eq!(
            matches!(self.receive, ReceiveStorage::Transferred),
            self.lifecycle == Lifecycle::HandedOff
        );
        debug_assert!(self.receive.lease().is_none() || self.read.operation().is_none());
        if let Some(buffer) = self.receive.buffer() {
            debug_assert!(self.start <= self.end && self.end <= buffer.as_ref().len());
        }
        if self.tx.settled() && self.lifecycle.accepts_exchange_commands() {
            debug_assert!(self.output.is_none());
            debug_assert!(self.pending_body.is_none());
        }
        if self
            .write
            .operation()
            .is_some_and(|id| id.kind() == OperationKind::Write)
            && let Some(queued) = &self.output
        {
            // A final head can queue behind an outstanding informational write.
            debug_assert!(matches!(queued.storage, WriteStorage::Head { .. }));
        }
        match self.lifecycle {
            Lifecycle::Http(_) | Lifecycle::Upgrade(_) => {
                debug_assert!(self.failure.is_none());
            }
            Lifecycle::ErrorResponse => {
                debug_assert!(self.failure.is_some());
                debug_assert_eq!(self.rx, Rx::Paused);
                debug_assert!(self.tx.source_finished());
                debug_assert!(self.close_after);
            }
            _ => {}
        }
        if matches!(
            self.lifecycle,
            Lifecycle::Closing(Closing::AwaitingClose(_)) | Lifecycle::Closed(_)
        ) {
            debug_assert!(self.io_settled());
            debug_assert!(self.exchange.is_none());
            debug_assert!(self.output.is_none());
            debug_assert!(self.pending_body.is_none());
            debug_assert!(self.body_result.is_none());
            debug_assert!(self.timers.armed.is_none());
        }
        if matches!(
            self.lifecycle,
            Lifecycle::Upgrade(Upgrade::Ready) | Lifecycle::HandedOff
        ) {
            debug_assert!(self.io_settled());
            debug_assert!(self.tx.settled());
            debug_assert_eq!(self.rx, Rx::Done);
            debug_assert!(self.output.is_none());
            debug_assert!(self.pending_body.is_none());
            debug_assert!(self.body_result.is_none());
            debug_assert!(self.timers.armed.is_none());
        }
    }

    fn process_metadata<P: Ports<B, W>>(
        &mut self,
        ports: &mut P,
        callback: fn(&mut P, ExchangeId, ParsedHead<'_>) -> Option<P::Output>,
    ) -> Option<P::Output> {
        let mut bytes = std::mem::take(&mut self.head);
        let result = self.checked_metadata(&bytes, ports, callback);
        bytes.clear();
        self.head = bytes;
        self.finish_metadata(result)
    }

    fn checked_metadata<P: Ports<B, W>>(
        &mut self,
        bytes: &[u8],
        ports: &mut P,
        callback: fn(&mut P, ExchangeId, ParsedHead<'_>) -> Option<P::Output>,
    ) -> Result<Option<P::Output>, Failure> {
        // Both buffered and direct input have passed metadata_span's strict
        // CRLF validation, including fragment joins and limit error precedence.
        #[cfg(debug_assertions)]
        debug_assert!(codec::strict_lines(bytes));
        self.account_metadata(bytes.len())?;
        self.metadata(bytes, ports, callback)
    }

    fn account_metadata(&mut self, len: usize) -> Result<(), Failure> {
        let accounting = if matches!(self.rx, Rx::Head | Rx::Trailers) {
            self.metadata_bytes = self.metadata_bytes.saturating_add(len);
            self.metadata_bytes <= self.config.max_head_bytes
        } else {
            self.chunk_metadata_bytes = self.chunk_metadata_bytes.saturating_add(len);
            self.chunk_metadata_bytes <= self.config.max_chunk_metadata_bytes
        };
        if accounting {
            Ok(())
        } else {
            Err(Failure::Limit)
        }
    }

    fn accept_chunk_size(&mut self, size: u64) -> Result<(), Failure> {
        if self.no_content && size != 0 {
            return Err(Failure::Protocol);
        }
        if self
            .received
            .checked_add(size)
            .is_none_or(|n| n > self.config.max_body_bytes)
        {
            return Err(Failure::Limit);
        }
        self.rx = if size == 0 {
            Rx::Trailers
        } else {
            Rx::Chunk(size)
        };
        Ok(())
    }

    fn finish_metadata<T>(&mut self, result: Result<Option<T>, Failure>) -> Option<T> {
        match result {
            Ok(output) => output,
            Err(error) => {
                self.fail(error);
                None
            }
        }
    }

    fn metadata<P: Ports<B, W>>(
        &mut self,
        bytes: &[u8],
        ports: &mut P,
        callback: fn(&mut P, ExchangeId, ParsedHead<'_>) -> Option<P::Output>,
    ) -> Result<Option<P::Output>, Failure> {
        match self.rx {
            Rx::Size => {
                let line = &bytes[..bytes.len() - 2];
                let size = codec::chunk_size(line)?;
                self.accept_chunk_size(size)?;
                Ok(None)
            }
            Rx::ChunkCrlf => {
                if bytes != b"\r\n" {
                    return Err(Failure::Protocol);
                }
                self.rx = Rx::Size;
                Ok(None)
            }
            Rx::Trailers => {
                // Standalone trailer parsing still requires initialized storage.
                let mut headers = [httparse::EMPTY_HEADER; 128];
                let httparse::Status::Complete((count, trailers)) =
                    httparse::parse_headers(bytes, &mut headers[..self.config.max_headers])
                        .map_err(parse_error)?
                else {
                    return Err(Failure::Protocol);
                };
                self.account_fields(trailers.len())?;
                if count != bytes.len()
                    || trailers
                        .iter()
                        .any(|h| !codec::valid_trailer(h, &self.incoming_connection_fields))
                {
                    return Err(Failure::Protocol);
                }
                self.rx = Rx::Done;
                self.incoming_ended();
                self.log(
                    ports,
                    LogEvent::TrailersReceived {
                        exchange: self.exchange.as_ref().unwrap().id,
                        fields: trailers.len(),
                    },
                );
                Ok(ports.trailers(self.exchange.as_ref().unwrap().id, trailers))
            }
            Rx::Head if SERVER => {
                // Initialize only parsed headers, and no header scratch for chunk metadata.
                let mut headers = [MaybeUninit::uninit(); 128];
                let mut request = httparse::Request::new(&mut []);
                let httparse::Status::Complete(count) = request
                    .parse_with_uninit_headers(bytes, &mut headers[..self.config.max_headers])
                    .map_err(parse_error)?
                else {
                    return Err(Failure::Protocol);
                };
                if count != bytes.len() {
                    return Err(Failure::Protocol);
                }
                let version = match request.version {
                    Some(0) => Version::Http10,
                    Some(1) => Version::Http11,
                    _ => return Err(Failure::Protocol),
                };
                self.account_fields(request.headers.len())?;
                if (version == Version::Http11 || codec::field_present(request.headers, "host"))
                    && !codec::host(request.headers)
                {
                    return Err(Failure::Protocol);
                }
                let framing = codec::framing(request.headers, false)?;
                if version == Version::Http10 && framing == Framing::Chunked {
                    return Err(Failure::Protocol);
                }
                let method = request.method.ok_or(Failure::Protocol)?;
                let expect =
                    codec::expect_continue(request.headers, version)? && framing != Framing::Empty;
                let upgrade = codec::upgrade_protocols(request.headers)?;
                self.incoming_connection_fields = codec::connection_fields(request.headers)?;
                let id = ExchangeId {
                    connection: self.id,
                    sequence: self.sequence().map_err(|_| Failure::SequenceExhausted)?,
                };
                self.exchange = Some(Exchange {
                    id,
                    version,
                    method: MethodSemantics::from_method(method),
                    persistent: codec::persistent(version, request.headers),
                    upgrade,
                    expect,
                    continue_gate: ContinueGate::Open,
                    incoming_notification: Notification::Pending,
                    source_notification: Notification::Pending,
                    consume_request: false,
                });
                self.exchanges += 1;
                self.count(Metric::ExchangesStarted, 1);
                self.set_rx(framing)?;
                let head = RequestHead {
                    method,
                    target: request.path.ok_or(Failure::Protocol)?,
                    version,
                    headers: request.headers,
                };
                self.log(
                    ports,
                    LogEvent::RequestReceived {
                        exchange: id,
                        version,
                    },
                );
                Ok(callback(ports, id, ParsedHead::Request(head)))
            }
            Rx::Head => {
                let mut headers = [MaybeUninit::uninit(); 128];
                let mut response = httparse::Response::new(&mut []);
                let httparse::Status::Complete(count) = httparse::ParserConfig::default()
                    .parse_response_with_uninit_headers(
                        &mut response,
                        bytes,
                        &mut headers[..self.config.max_headers],
                    )
                    .map_err(parse_error)?
                else {
                    return Err(Failure::Protocol);
                };
                if count != bytes.len() {
                    return Err(Failure::Protocol);
                }
                let version = match response.version {
                    Some(0) => Version::Http10,
                    Some(1) => Version::Http11,
                    _ => return Err(Failure::Protocol),
                };
                let status = response.code.ok_or(Failure::Protocol)?;
                if !(100..=599).contains(&status) {
                    return Err(Failure::Protocol);
                }
                if version == Version::Http10 && status < 200 {
                    return Err(Failure::Protocol);
                }
                self.account_fields(response.headers.len())?;
                let exchange = self.exchange.as_ref().ok_or(Failure::Protocol)?;
                let tunnel =
                    exchange.method == MethodSemantics::Connect && (200..300).contains(&status);
                let framing = if tunnel {
                    Framing::Empty
                } else {
                    if (status < 200 || status == 204)
                        && (codec::field_present(response.headers, "content-length")
                            || codec::field_present(response.headers, "transfer-encoding"))
                    {
                        return Err(Failure::Protocol);
                    }
                    codec::framing(response.headers, true)?
                };
                if version == Version::Http10 && framing == Framing::Chunked {
                    return Err(Failure::Protocol);
                }
                let informational = status < 200 && status != 101;
                if informational {
                    self.informational += 1;
                    if self.informational > self.config.max_informational_responses {
                        return Err(Failure::Limit);
                    }
                    if status == 100 {
                        self.release_continue();
                        self.update_deadline(self.timers.phase, None)
                            .map_err(|_| Failure::SequenceExhausted)?;
                    }
                } else {
                    self.no_content = status == 205;
                    if self.no_content && matches!(framing, Framing::Fixed(n) if n != 0) {
                        return Err(Failure::Protocol);
                    }
                    self.incoming_connection_fields = codec::connection_fields(response.headers)?;
                    self.close_after |= !codec::persistent(version, response.headers);
                    if status == 101 {
                        if version != Version::Http11
                            || !exchange.upgrade.as_ref().is_some_and(|requested| {
                                codec::valid_upgrade(requested, response.headers)
                            })
                            || !self.tx.source_finished()
                        {
                            return Err(Failure::Protocol);
                        }
                        self.lifecycle.begin_upgrade();
                        self.rx = Rx::Done;
                    } else if tunnel {
                        if !self.tx.source_finished() {
                            return Err(Failure::Protocol);
                        }
                        self.lifecycle.begin_upgrade();
                        self.rx = Rx::Done;
                    } else {
                        let framing = if exchange.method == MethodSemantics::Head
                            || status == 204
                            || status == 304
                        {
                            Framing::Empty
                        } else {
                            framing
                        };
                        self.set_rx(framing)?;
                    }
                    if !self.tx.settled()
                        && self.tx.framing() != Framing::Empty
                        && !self.lifecycle.is_upgrade()
                        && (status >= 300 || self.waiting_continue() || self.rx == Rx::Done)
                    {
                        self.stop_upload();
                    }
                    self.update_deadline(self.timers.phase, None)
                        .map_err(|_| Failure::SequenceExhausted)?;
                }
                self.log(
                    ports,
                    LogEvent::ResponseReceived {
                        exchange: self.exchange.as_ref().unwrap().id,
                        status,
                        informational,
                    },
                );
                Ok(callback(
                    ports,
                    self.exchange.as_ref().unwrap().id,
                    ParsedHead::Response(
                        ResponseHead {
                            version,
                            status,
                            reason: response.reason.unwrap_or(""),
                            headers: response.headers,
                        },
                        informational,
                    ),
                ))
            }
            _ => Err(Failure::Protocol),
        }
    }

    fn set_rx(&mut self, framing: Framing) -> Result<(), Failure> {
        if let Framing::Fixed(length) = framing
            && length > self.config.max_body_bytes
        {
            self.rx = Rx::Fixed(length);
            return Err(Failure::Limit);
        }
        self.rx = match framing {
            Framing::Empty => Rx::Done,
            Framing::Fixed(n) => Rx::Fixed(n),
            Framing::Chunked => Rx::Size,
            Framing::Eof => {
                self.close_after = true;
                Rx::Eof
            }
        };
        if self.rx == Rx::Done {
            self.incoming_ended();
        }
        self.set_deadline(TimerPhase::Body, self.config.body_timeout_ns)
            .map_err(|_| Failure::SequenceExhausted)?;
        Ok(())
    }

    fn account_fields(&mut self, count: usize) -> Result<(), Failure> {
        self.metadata_fields = self.metadata_fields.saturating_add(count);
        if self.metadata_fields > self.config.max_headers {
            Err(Failure::Limit)
        } else {
            Ok(())
        }
    }

    fn check_outgoing_metadata(&self, bytes: usize, fields: usize) -> Result<(), CommandError> {
        if self
            .outgoing_metadata_bytes
            .checked_add(bytes)
            .is_none_or(|n| n > self.config.max_head_bytes)
            || self
                .outgoing_metadata_fields
                .checked_add(fields)
                .is_none_or(|n| n > self.config.max_headers)
        {
            Err(CommandError::Limit)
        } else {
            Ok(())
        }
    }
}

fn parse_error(error: httparse::Error) -> Failure {
    if error == httparse::Error::TooManyHeaders {
        Failure::Limit
    } else {
        Failure::Protocol
    }
}

impl<B: Buffer> Server<B> {
    pub fn new(
        id: ConnectionId,
        config: Config,
        receive_buffer: B,
        now: Tick,
    ) -> Result<Self, CommandError> {
        Self::with_output_type(id, config, receive_buffer, now)
    }
}
impl<B: Buffer, W: AsRef<[u8]>> Server<B, W> {
    /// Constructs a connection with separate input and output storage.
    ///
    /// `W` needs only read access. It can be an exclusive buffer, a
    /// borrowed slice, or caller-selected shared storage.
    pub fn with_output_type(
        id: ConnectionId,
        config: Config,
        receive_buffer: B,
        now: Tick,
    ) -> Result<Self, CommandError> {
        Ok(Self {
            core: Core::new(id, config, receive_buffer, now)?,
        })
    }
    pub fn connection_id(&self) -> ConnectionId {
        self.core.id
    }

    /// Returns a snapshot without driving the machine or resetting counters.
    #[cfg(feature = "metrics")]
    pub fn metrics(&self) -> MetricsSnapshot {
        self.core.metrics()
    }
    /// Bytes already received but not yet consumed by HTTP or a body consumer.
    ///
    /// This view is empty while the receive buffer belongs to an operation.
    pub fn buffered_input(&self) -> &[u8] {
        self.core.receive.buffer().map_or(&[], |buffer| {
            &buffer.as_ref()[self.core.start..self.core.end]
        })
    }
    pub fn send_body(&mut self, command: SendBody<W>) -> Result<BodyId, Rejected<SendBody<W>>> {
        self.core.send_body(command)
    }
    /// Attaches a complete fixed-length body to an unstarted final-head write.
    ///
    /// Call after `respond` or `respond_duplex`, before driving the machine.
    /// The payload remains separately owned and is not copied into the head.
    /// Admission ends the source, but storage returns only through `body_sent`.
    /// A rejection returns the command unchanged for the normal demand path.
    /// Suppressed, streaming, incomplete, or already-issued bodies cannot attach.
    pub fn send_body_eager(
        &mut self,
        command: SendBody<W>,
    ) -> Result<BodyId, Rejected<SendBody<W>>> {
        self.core.send_body_eager(command)
    }
    pub fn finish_body(
        &mut self,
        exchange: ExchangeId,
        trailers: Headers<'_>,
    ) -> Result<(), CommandError> {
        self.core.finish_body(exchange, trailers)
    }
    pub fn grant_body_credit(
        &mut self,
        exchange: ExchangeId,
        bytes: usize,
    ) -> Result<(), CommandError> {
        self.core.grant_body_credit(exchange, bytes)
    }
    pub fn release_body(
        &mut self,
        completion: BodyCompletion<B>,
    ) -> Result<(), Rejected<BodyCompletion<B>>> {
        self.core.release_body(completion)
    }
    pub fn complete_read(
        &mut self,
        completion: ReadCompletion<B>,
    ) -> Result<(), Rejected<ReadCompletion<B>>> {
        self.core.complete_read(completion)
    }
    #[allow(clippy::result_large_err)]
    pub fn complete_write(
        &mut self,
        completion: WriteCompletion<W>,
    ) -> Result<(), Rejected<WriteCompletion<W>>> {
        self.core.complete_write(completion)
    }
    pub fn complete_readiness(
        &mut self,
        completion: ReadinessCompletion,
    ) -> Result<(), Rejected<ReadinessCompletion>> {
        self.core.complete_readiness(completion)
    }
    pub fn complete_close(
        &mut self,
        completion: CloseCompletion,
    ) -> Result<(), Rejected<CloseCompletion>> {
        self.core.complete_close(completion)
    }
    /// Reports a handler or body-source failure, including pending demand.
    pub fn fail_source(
        &mut self,
        exchange: ExchangeId,
        failure: Failure,
    ) -> Result<(), CommandError> {
        self.core.check_exchange(exchange)?;
        self.core.fail(failure);
        Ok(())
    }
    pub fn cancel_exchange(&mut self, exchange: ExchangeId) -> Result<(), CommandError> {
        self.fail_source(exchange, Failure::Cancelled)
    }
    pub fn shutdown(&mut self, mode: ShutdownMode) {
        self.core.shutdown(mode);
    }
    pub fn observe_time(&mut self, now: Tick) -> Result<(), CommandError> {
        self.core.observe_time(now)
    }
    pub fn expire(&mut self, deadline: Deadline, now: Tick) -> Result<(), CommandError> {
        self.core.expire(deadline, now)
    }
    pub fn take_upgrade(&mut self) -> Result<Handoff<B>, CommandError> {
        self.core.take_upgrade()
    }
}

impl<B: Buffer> Client<B> {
    pub fn new(
        id: ConnectionId,
        config: Config,
        receive_buffer: B,
        now: Tick,
    ) -> Result<Self, CommandError> {
        Self::with_output_type(id, config, receive_buffer, now)
    }
}

impl<B: Buffer, W: AsRef<[u8]>> Client<B, W> {
    /// Constructs a connection with separate input and output storage.
    ///
    /// `W` needs only read access. It can be an exclusive buffer, a
    /// borrowed slice, or caller-selected shared storage.
    pub fn with_output_type(
        id: ConnectionId,
        config: Config,
        receive_buffer: B,
        now: Tick,
    ) -> Result<Self, CommandError> {
        Ok(Self {
            core: Core::new(id, config, receive_buffer, now)?,
        })
    }
    pub fn connection_id(&self) -> ConnectionId {
        self.core.id
    }

    /// Returns a snapshot without driving the machine or resetting counters.
    #[cfg(feature = "metrics")]
    pub fn metrics(&self) -> MetricsSnapshot {
        self.core.metrics()
    }
    /// Bytes already received but not yet consumed by HTTP or a body consumer.
    ///
    /// This view is empty while the receive buffer belongs to an operation.
    pub fn buffered_input(&self) -> &[u8] {
        self.core.receive.buffer().map_or(&[], |buffer| {
            &buffer.as_ref()[self.core.start..self.core.end]
        })
    }
    pub fn send_body(&mut self, command: SendBody<W>) -> Result<BodyId, Rejected<SendBody<W>>> {
        self.core.send_body(command)
    }
    /// Attaches a complete fixed-length body to an unstarted request-head write.
    ///
    /// Call after `request`, before driving the machine.
    /// The payload remains separately owned and is not copied into the head.
    /// Admission ends the source, but storage returns only through `body_sent`.
    /// An unresolved `Expect: 100-continue` gate rejects eager admission.
    /// The upload deadline starts at admission and includes the combined head.
    /// A rejection returns the command unchanged for the normal demand path.
    pub fn send_body_eager(
        &mut self,
        command: SendBody<W>,
    ) -> Result<BodyId, Rejected<SendBody<W>>> {
        self.core.send_body_eager(command)
    }
    pub fn finish_body(
        &mut self,
        exchange: ExchangeId,
        trailers: Headers<'_>,
    ) -> Result<(), CommandError> {
        self.core.finish_body(exchange, trailers)
    }
    pub fn grant_body_credit(
        &mut self,
        exchange: ExchangeId,
        bytes: usize,
    ) -> Result<(), CommandError> {
        self.core.grant_body_credit(exchange, bytes)
    }
    pub fn release_body(
        &mut self,
        completion: BodyCompletion<B>,
    ) -> Result<(), Rejected<BodyCompletion<B>>> {
        self.core.release_body(completion)
    }
    pub fn complete_read(
        &mut self,
        completion: ReadCompletion<B>,
    ) -> Result<(), Rejected<ReadCompletion<B>>> {
        self.core.complete_read(completion)
    }
    #[allow(clippy::result_large_err)]
    pub fn complete_write(
        &mut self,
        completion: WriteCompletion<W>,
    ) -> Result<(), Rejected<WriteCompletion<W>>> {
        self.core.complete_write(completion)
    }
    pub fn complete_readiness(
        &mut self,
        completion: ReadinessCompletion,
    ) -> Result<(), Rejected<ReadinessCompletion>> {
        self.core.complete_readiness(completion)
    }
    pub fn complete_close(
        &mut self,
        completion: CloseCompletion,
    ) -> Result<(), Rejected<CloseCompletion>> {
        self.core.complete_close(completion)
    }
    /// Reports a handler or body-source failure, including pending demand.
    pub fn fail_source(
        &mut self,
        exchange: ExchangeId,
        failure: Failure,
    ) -> Result<(), CommandError> {
        self.core.check_exchange(exchange)?;
        self.core.fail(failure);
        Ok(())
    }
    pub fn cancel_exchange(&mut self, exchange: ExchangeId) -> Result<(), CommandError> {
        self.fail_source(exchange, Failure::Cancelled)
    }
    pub fn shutdown(&mut self, mode: ShutdownMode) {
        self.core.shutdown(mode);
    }
    pub fn observe_time(&mut self, now: Tick) -> Result<(), CommandError> {
        self.core.observe_time(now)
    }
    pub fn expire(&mut self, deadline: Deadline, now: Tick) -> Result<(), CommandError> {
        self.core.expire(deadline, now)
    }
    pub fn take_upgrade(&mut self) -> Result<Handoff<B>, CommandError> {
        self.core.take_upgrade()
    }
}

impl<B: Buffer, W: AsRef<[u8]>> Server<B, W> {
    pub fn next<P: ServerPorts<B, W>>(&mut self, ports: &mut P) -> Option<P::Output> {
        let output = self.core.next(ports, |ports, id, head| match head {
            ParsedHead::Request(head) => ports.request(id, head),
            ParsedHead::Response(..) => unreachable!(),
        });
        self.core.assert_invariants();
        output
    }
    /// Starts a final response, closing after an early response by default.
    ///
    /// Use [`Self::respond_duplex`] when the application will continue consuming
    /// the request even after the response finishes.
    pub fn respond(
        &mut self,
        exchange: ExchangeId,
        response: Response<'_>,
    ) -> Result<(), CommandError> {
        self.core
            .respond(exchange, response, ResponseMode::Conservative)
    }
    /// Starts a final response without abandoning the incoming request.
    ///
    /// The caller must continue granting body credit and releasing body leases
    /// until `incoming_finished`, or cancel the exchange if consumption stops.
    /// Response completion does not cancel pending reads or retire the exchange.
    /// Reuse requires both directions and all external operations to settle.
    /// Other close policies, limits, timeouts, and upgrade restrictions still apply.
    ///
    /// An outstanding `Expect: 100-continue` is answered before the final head
    /// when no request body bytes are buffered, even for an empty response.
    pub fn respond_duplex(
        &mut self,
        exchange: ExchangeId,
        response: Response<'_>,
    ) -> Result<(), CommandError> {
        self.core.respond(exchange, response, ResponseMode::Duplex)
    }
    pub fn inform(
        &mut self,
        exchange: ExchangeId,
        response: InformationalResponse<'_>,
    ) -> Result<(), CommandError> {
        self.core.inform(exchange, response)
    }
    pub fn accept_upgrade(
        &mut self,
        exchange: ExchangeId,
        response: UpgradeResponse<'_>,
    ) -> Result<(), CommandError> {
        self.core.respond(
            exchange,
            Response {
                head: response,
                body: BodyLength::Empty,
            },
            ResponseMode::Upgrade,
        )
    }
}

impl<B: Buffer, W: AsRef<[u8]>> Client<B, W> {
    pub fn next<P: ClientPorts<B, W>>(&mut self, ports: &mut P) -> Option<P::Output> {
        let output = self.core.next(ports, |ports, id, head| match head {
            ParsedHead::Response(head, informational) => ports.response(id, head, informational),
            ParsedHead::Request(..) => unreachable!(),
        });
        self.core.assert_invariants();
        output
    }
    pub fn request(&mut self, request: Request<'_>) -> Result<ExchangeId, CommandError> {
        self.core.request(request)
    }
}

#[cfg(test)]
#[path = "connection_tests.rs"]
mod tests;
