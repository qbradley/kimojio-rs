use crate::codec::{self, Framing};
use crate::operations::WriteStorage;
use crate::*;
use std::io::Write;

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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TimerPhase {
    Head,
    Body,
    Idle,
    Continue,
    Upload,
}

#[derive(Debug)]
struct Exchange {
    id: ExchangeId,
    version: Version,
    head_method: bool,
    connect_method: bool,
    persistent: bool,
    upgrade: Option<Vec<u8>>,
    expect: bool,
    incoming_notified: bool,
}

#[derive(Debug)]
struct Core<B, W> {
    id: ConnectionId,
    config: Config,
    server: bool,
    sequence: u64,
    exchanges: u64,
    now: Tick,
    deadline: Option<Deadline>,
    deadline_dirty: bool,
    deadline_kind: Option<TimerPhase>,
    phase_timer: Option<(TimerPhase, Tick)>,
    continue_at: Option<Tick>,
    upload_at: Option<Tick>,
    input: Option<B>,
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
    tx: Framing,
    tx_started: bool,
    tx_done: bool,
    tx_end: bool,
    demand_issued: bool,
    source_notified: bool,
    wait_continue: bool,
    stop_upload: bool,
    output: Option<WriteOp<W>>,
    pending_body: Option<(BodyId, SendBody<W>)>,
    body_result: Option<BodySent<W>>,
    read_live: Option<OperationId>,
    write_live: Option<OperationId>,
    body_live: Option<OperationId>,
    close_live: Option<OperationId>,
    read_blocked: bool,
    write_blocked: bool,
    cancel_read_sent: bool,
    cancel_write_sent: bool,
    close_after: bool,
    graceful: bool,
    failure: Option<Failure>,
    closing: bool,
    closed: bool,
    closed_notified: bool,
    upgrade_pending: bool,
    upgrade_notified: bool,
    handed_off: bool,
}

/// A single-exchange HTTP/1 server with independently owned I/O operations.
#[derive(Debug)]
pub struct Server<B: Buffer, W: AsRef<[u8]> = B> {
    core: Core<B, W>,
}

/// A single-exchange HTTP/1 client. Requests are never automatically replayed.
#[derive(Debug)]
pub struct Client<B: Buffer, W: AsRef<[u8]> = B> {
    core: Core<B, W>,
}

enum ParsedHead<'a> {
    Request(RequestHead<'a>),
    Response(ResponseHead<'a>, bool),
}

impl<B: Buffer, W: AsRef<[u8]>> Core<B, W> {
    fn new(
        id: ConnectionId,
        config: Config,
        mut input: B,
        now: Tick,
        server: bool,
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
            server,
            sequence: 0,
            exchanges: 0,
            now,
            deadline: None,
            deadline_dirty: false,
            deadline_kind: None,
            phase_timer: None,
            continue_at: None,
            upload_at: None,
            input: Some(input),
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
            tx: Framing::Empty,
            tx_started: false,
            tx_done: false,
            tx_end: false,
            demand_issued: false,
            source_notified: false,
            wait_continue: false,
            stop_upload: false,
            output: None,
            pending_body: None,
            body_result: None,
            read_live: None,
            write_live: None,
            body_live: None,
            close_live: None,
            read_blocked: false,
            write_blocked: false,
            cancel_read_sent: false,
            cancel_write_sent: false,
            close_after: false,
            graceful: false,
            failure: None,
            closing: false,
            closed: false,
            closed_notified: false,
            upgrade_pending: false,
            upgrade_notified: false,
            handed_off: false,
        };
        core.set_deadline(
            if server {
                TimerPhase::Head
            } else {
                TimerPhase::Idle
            },
            if server {
                core.config.head_timeout_ns
            } else {
                core.config.idle_timeout_ns
            },
        )?;
        Ok(core)
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
        self.update_deadline(phase_timer, self.continue_at)
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
        if let Some(at) = self.upload_at
            && earliest.is_none_or(|(_, prior)| at < prior)
        {
            earliest = Some((TimerPhase::Upload, at));
        }
        if earliest != self.deadline_kind.zip(self.deadline.map(|d| d.at)) {
            let deadline = match earliest {
                Some((_, at)) => Some(Deadline {
                    connection: self.id,
                    sequence: self.sequence()?,
                    at,
                }),
                None => None,
            };
            self.deadline = deadline;
            self.deadline_kind = earliest.map(|(kind, _)| kind);
            self.deadline_dirty = true;
        }
        self.phase_timer = phase_timer;
        self.continue_at = continue_at;
        Ok(())
    }

    fn progress(&mut self) {
        if !self.closing && !self.stop_upload && self.upload_at.is_some() {
            self.arm_upload();
        }
        if !self.closing
            && self
                .phase_timer
                .is_some_and(|(kind, _)| kind == TimerPhase::Body)
            && self
                .set_deadline(TimerPhase::Body, self.config.body_timeout_ns)
                .is_err()
        {
            self.fail(Failure::SequenceExhausted);
        }
    }

    fn clear_deadlines(&mut self) {
        self.deadline_dirty |= self.deadline.is_some();
        self.deadline = None;
        self.deadline_kind = None;
        self.phase_timer = None;
        self.continue_at = None;
        self.upload_at = None;
    }

    fn arm_upload(&mut self) {
        self.upload_at = self
            .config
            .body_timeout_ns
            .and_then(|duration| self.now.0.checked_add(duration))
            .map(Tick);
        if self.config.body_timeout_ns.is_some() && self.upload_at.is_none()
            || self
                .update_deadline(self.phase_timer, self.continue_at)
                .is_err()
        {
            self.fail(Failure::SequenceExhausted);
        }
    }

    fn check_exchange(&self, exchange: ExchangeId) -> Result<(), CommandError> {
        if self.exchange.as_ref().map(|e| e.id) != Some(exchange) {
            Err(CommandError::StaleExchange)
        } else if self.closing || self.closed || self.handed_off || self.failure.is_some() {
            Err(CommandError::InvalidState)
        } else {
            Ok(())
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
        self.upgrade_notified = false;
        self.upgrade_pending = false;
        self.credit = 0;
        self.rx = Rx::Paused;
        self.wait_continue = false;
        self.clear_deadlines();
        if first_failure
            && self.server
            && !self.tx_started
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
            if let Ok((bytes, _)) =
                codec::encode_response(response, false, true, false, &self.config)
                && self
                    .check_outgoing_metadata(bytes.len(), header_count(&bytes))
                    .is_ok()
            {
                self.outgoing_metadata_bytes += bytes.len();
                self.outgoing_metadata_fields += header_count(&bytes);
                self.tx_started = true;
                self.tx = Framing::Empty;
                self.tx_end = true;
                self.tx_done = false;
                self.close_after = true;
                if let Some(op) = self.output.as_mut() {
                    let WriteStorage::Head(queued) = &mut op.storage else {
                        unreachable!()
                    };
                    queued.reserve_exact(bytes.len());
                    queued.extend_from_slice(&bytes);
                } else {
                    self.queue_head(bytes);
                }
                if self
                    .set_deadline(TimerPhase::Body, self.config.body_timeout_ns)
                    .is_ok()
                {
                    return;
                }
            }
        }
        self.closing = true;
        self.clear_deadlines();
    }

    fn queue_head(&mut self, bytes: Vec<u8>) {
        // A queued operation receives its external identity only at issuance.
        self.output = Some(WriteOp {
            id: OperationId {
                connection: self.id,
                sequence: 0,
                kind: OperationKind::Write,
            },
            storage: WriteStorage::Head(bytes),
            cursor: 0,
        });
    }

    fn request(&mut self, request: Request<'_>) -> Result<ExchangeId, CommandError> {
        if self.exchange.is_some()
            || self.start < self.end
            || self.closing
            || self.closed
            || self.graceful
            || self.handed_off
        {
            return Err(CommandError::InvalidState);
        }
        let (head, framing) = codec::encode_request(request, &self.config)?;
        self.check_outgoing_metadata(
            head.len()
                .saturating_add(if framing == Framing::Chunked { 5 } else { 0 }),
            header_count(&head),
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
            head_method: request.head.method == "HEAD",
            connect_method: request.head.method == "CONNECT",
            persistent: codec::persistent(request.head.version, request.head.headers),
            upgrade,
            expect: request.expect_continue,
            incoming_notified: false,
        });
        self.exchanges += 1;
        self.tx = framing;
        self.tx_started = true;
        self.tx_done = false;
        self.tx_end = framing == Framing::Empty;
        self.wait_continue = request.expect_continue
            && request.head.version == Version::Http11
            && framing != Framing::Empty;
        self.outgoing_connection_fields = connection_fields;
        self.outgoing_metadata_bytes = head.len();
        self.outgoing_metadata_fields = header_count(&head);
        self.queue_head(head);
        Ok(id)
    }

    fn respond(
        &mut self,
        exchange: ExchangeId,
        mut response: Response<'_>,
        upgrade: bool,
    ) -> Result<(), CommandError> {
        self.check_exchange(exchange)?;
        let current = self.exchange.as_ref().unwrap();
        if self.tx_started
            || (!upgrade && response.head.status < 200)
            || (upgrade && self.rx != Rx::Done)
            || (upgrade && self.graceful)
        {
            return Err(CommandError::InvalidState);
        }
        response.head.version = current.version;
        let tunnel = current.connect_method && (200..300).contains(&response.head.status);
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
            || early
            || self.graceful
            || self.exchanges >= self.config.max_requests;
        let (final_bytes, framing) = codec::encode_response(
            response,
            current.head_method,
            close && !upgrade,
            tunnel,
            &self.config,
        )?;
        let fields = header_count(&final_bytes);
        let continue_first = current.expect
            && self.credit != 0
            && self.start == self.end
            && !self.eof
            && !matches!(self.rx, Rx::Done | Rx::Paused)
            && (200..300).contains(&response.head.status)
            && framing != Framing::Empty;
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
            .0;
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
        self.upgrade_pending = upgrade;
        self.tx_started = true;
        self.tx = framing;
        self.tx_end = framing == Framing::Empty;
        self.outgoing_connection_fields = connection_fields;
        if continue_first {
            self.exchange.as_mut().unwrap().expect = false;
            self.outgoing_informational += 1;
        }
        self.outgoing_metadata_bytes += bytes.len();
        self.outgoing_metadata_fields += fields;
        if let Some(op) = self.output.as_mut() {
            let WriteStorage::Head(queued) = &mut op.storage else {
                unreachable!()
            };
            queued.reserve_exact(bytes.len());
            queued.extend_from_slice(&bytes);
        } else {
            self.queue_head(bytes);
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
        if self.tx_started
            || self.output.is_some()
            || self.write_live.is_some()
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
        let (bytes, _) = codec::encode_response(
            Response {
                head,
                body: BodyLength::Empty,
            },
            false,
            false,
            false,
            &self.config,
        )?;
        let fields = header_count(&bytes);
        self.check_outgoing_metadata(bytes.len(), fields)?;
        if head.status == 100 {
            self.exchange.as_mut().unwrap().expect = false;
        }
        self.outgoing_metadata_bytes += bytes.len();
        self.outgoing_metadata_fields += fields;
        self.outgoing_informational += 1;
        self.queue_head(bytes);
        Ok(())
    }

    fn send_body(&mut self, command: SendBody<W>) -> Result<BodyId, Rejected<SendBody<W>>> {
        let reason = if self.check_exchange(command.exchange).is_err()
            || !self.tx_started
            || self.tx_done
            || self.tx_end
            || self.stop_upload
        {
            Some(RejectReason::InvalidState)
        } else if self.pending_body.is_some()
            || self.body_result.is_some()
            || self.output.is_some()
            || self.write_live.is_some()
            || self.wait_continue
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
            match self.tx {
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
        let chunk_metadata = if self.tx == Framing::Chunked {
            command.range.len().max(1).ilog(16) as usize + 5
        } else {
            0
        };
        let reason = reason.or_else(|| {
            (self.tx == Framing::Chunked
                && (chunk_metadata - 2 > self.config.max_chunk_line_bytes
                    || self
                        .outgoing_chunk_metadata_bytes
                        .saturating_add(chunk_metadata)
                        .saturating_add(3)
                        > self.config.max_chunk_metadata_bytes))
                .then_some(RejectReason::Limit)
        });
        let reason = reason.or_else(|| {
            (self.tx == Framing::Chunked
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
        if let Framing::Fixed(left) = self.tx {
            let remaining = left - command.range.len() as u64;
            self.tx = Framing::Fixed(remaining);
            self.tx_end = remaining == 0;
        }
        self.tx_end |= command.end;
        self.outgoing_bytes += command.range.len() as u64;
        self.outgoing_chunk_metadata_bytes += chunk_metadata;
        if self.tx == Framing::Chunked && command.end {
            self.outgoing_chunk_metadata_bytes += 3;
        }
        self.pending_body = Some((id, command));
        self.demand_issued = false;
        Ok(id)
    }

    fn finish_body(
        &mut self,
        exchange: ExchangeId,
        trailers: Headers<'_>,
    ) -> Result<(), CommandError> {
        self.check_exchange(exchange)?;
        if !self.tx_started
            || self.tx_done
            || self.tx_end
            || self.pending_body.is_some()
            || self.output.is_some()
            || self.write_live.is_some()
            || self.body_result.is_some()
        {
            return Err(CommandError::InvalidState);
        }
        if matches!(self.tx, Framing::Fixed(n) if n != 0) {
            return Err(CommandError::InvalidFraming);
        }
        if !trailers.is_empty() && self.tx != Framing::Chunked {
            return Err(CommandError::InvalidFraming);
        }
        let bytes = if self.tx == Framing::Chunked {
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
        if self.tx == Framing::Chunked {
            self.outgoing_chunk_metadata_bytes += 3;
        }
        self.tx_end = true;
        self.demand_issued = false;
        if bytes.is_empty() {
            self.tx_done = true;
        } else {
            self.queue_head(bytes);
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
            .validate(completion.op.id, self.read_live)
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
        self.read_live = None;
        self.cancel_read_sent = false;
        let ReadCompletion { op, result } = completion;
        self.input = Some(op.buffer);
        match result {
            Ok(n) => {
                self.start = op.range.start;
                self.end = op.range.start + n;
                self.eof |= n == 0;
                if n != 0 {
                    if self.server
                        && self.rx == Rx::Head
                        && self.exchange.is_none()
                        && self
                            .phase_timer
                            .is_some_and(|(kind, _)| kind == TimerPhase::Idle)
                        && self
                            .set_deadline(TimerPhase::Head, self.config.head_timeout_ns)
                            .is_err()
                    {
                        self.fail(Failure::SequenceExhausted);
                    } else {
                        self.progress();
                    }
                }
            }
            Err(error) if self.closing || self.rx == Rx::Paused => {
                let _ = error;
            }
            Err(IoError {
                kind: IoErrorKind::WouldBlock,
                ..
            }) => self.read_blocked = true,
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
            .validate(completion.op.id, self.write_live)
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
        self.write_live = None;
        self.cancel_write_sent = false;
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
        match result {
            Ok(0) => self.fail(Failure::WriteZero),
            Ok(n) => {
                op.cursor += n;
                self.progress();
            }
            Err(IoError {
                kind: IoErrorKind::WouldBlock,
                ..
            }) if !self.closing => self.write_blocked = true,
            Err(IoError {
                kind: IoErrorKind::Interrupted,
                ..
            }) if !self.closing => {}
            Err(IoError {
                kind: IoErrorKind::Cancelled | IoErrorKind::CancelledUnknownProgress,
                ..
            }) if self.stop_upload => {}
            Err(error) => self.fail(Failure::Transport(error)),
        }
        if self.closing || self.stop_upload || op.remaining() == 0 {
            self.settle_write(op, acceptance);
        } else {
            self.output = Some(op);
        }
        Ok(())
    }

    fn settle_write(&mut self, op: WriteOp<W>, acceptance: Acceptance) {
        match op.storage {
            WriteStorage::Head(_) => {
                if self.tx_started && self.tx_end {
                    self.tx_done = true;
                }
                if !self.server && !self.tx_end && !self.stop_upload && !self.closing {
                    self.arm_upload();
                }
                if !self.server && self.wait_continue && !self.closing && !self.stop_upload {
                    let at = self
                        .config
                        .continue_timeout_ns
                        .and_then(|duration| self.now.0.checked_add(duration))
                        .map(Tick);
                    if self.config.continue_timeout_ns.is_some() && at.is_none()
                        || self.update_deadline(self.phase_timer, at).is_err()
                    {
                        self.fail(Failure::SequenceExhausted);
                    }
                }
            }
            WriteStorage::Body {
                command,
                body_id,
                prefix_len,
                chunked,
                ..
            } => {
                let accepted = op
                    .cursor
                    .saturating_sub(prefix_len)
                    .min(command.range.len());
                self.body_result = Some(BodySent {
                    exchange: command.exchange,
                    id: body_id,
                    buffer: command.buffer,
                    accepted,
                    acceptance,
                    result: self
                        .failure
                        .or(self.stop_upload.then_some(Failure::EarlyResponse))
                        .map_or(Ok(()), Err),
                });
                if self.tx_end && !self.closing && !self.stop_upload {
                    if chunked {
                        self.outgoing_metadata_bytes += 5;
                        self.queue_head(b"0\r\n\r\n".to_vec());
                    } else {
                        self.tx_done = true;
                    }
                }
            }
        }
        if self.tx_done
            && self.upload_at.take().is_some()
            && self
                .update_deadline(self.phase_timer, self.continue_at)
                .is_err()
        {
            self.fail(Failure::SequenceExhausted);
        }
    }

    fn complete_readiness(
        &mut self,
        completion: ReadinessCompletion,
    ) -> Result<(), Rejected<ReadinessCompletion>> {
        let live = match completion.op.direction {
            Direction::Read => self.read_live,
            Direction::Write => self.write_live,
        };
        if let Err(reason) = self.validate(completion.op.id, live) {
            return Err(Rejected {
                reason,
                value: completion,
            });
        }
        match completion.op.direction {
            Direction::Read => {
                self.read_live = None;
                self.read_blocked = false;
                self.cancel_read_sent = false;
            }
            Direction::Write => {
                self.write_live = None;
                self.write_blocked = false;
                self.cancel_write_sent = false;
            }
        }
        if let Err(error) = completion.result {
            if error.kind == IoErrorKind::Interrupted && !self.closing {
                match completion.op.direction {
                    Direction::Read => self.read_blocked = true,
                    Direction::Write => self.write_blocked = true,
                }
            } else if !self.closing
                && self.rx != Rx::Paused
                && !(self.stop_upload
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
            .validate(completion.op.id, self.body_live)
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
        self.body_live = None;
        self.input = Some(completion.op.buffer);
        self.start = completion.op.range.start + completion.consumed;
        self.end = completion.op.buffered_end;
        if completion.consumed == 0 {
            self.credit = 0;
        }
        self.received += completion.consumed as u64;
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
        if let Err(reason) = self.validate(completion.op.id, self.close_live) {
            return Err(Rejected {
                reason,
                value: completion,
            });
        }
        self.close_live = None;
        if let Err(error) = completion.result {
            self.failure.get_or_insert(Failure::Transport(error));
        }
        self.closed = true;
        Ok(())
    }

    fn next<P: Ports<B, W>>(
        &mut self,
        ports: &mut P,
        head_callback: fn(&mut P, ExchangeId, ParsedHead<'_>) -> Option<P::Output>,
    ) -> Option<P::Output> {
        loop {
            if self.handed_off {
                return None;
            }
            let response_head_ready = !self.server && self.rx == Rx::Head && self.start < self.end;
            if self.deadline_dirty {
                self.deadline_dirty = false;
                if let Some(output) = ports.deadline_changed(self.deadline) {
                    return Some(output);
                }
                continue;
            }
            if let Some(result) = self.body_result.take() {
                if let Some(output) = ports.body_sent(result) {
                    return Some(output);
                }
                continue;
            }
            if self.closed {
                if !self.closed_notified {
                    self.closed_notified = true;
                    return ports.closed(self.failure.map_or(Ok(()), Err));
                }
                return None;
            }
            if self.graceful && self.upgrade_pending {
                self.fail(Failure::Cancelled);
                continue;
            }
            if self.server
                && self.tx_done
                && !matches!(self.rx, Rx::Done | Rx::Paused)
                && !self.upgrade_pending
                && !self.closing
            {
                self.rx = Rx::Paused;
                self.credit = 0;
                self.close_after = true;
            }
            if !self.server
                && self.rx == Rx::Done
                && !self.tx_done
                && self.tx != Framing::Empty
                && !self.upgrade_pending
                && !self.closing
            {
                self.stop_upload = true;
                self.tx_end = true;
                self.wait_continue = false;
                self.close_after = true;
                self.upload_at = None;
                if self.update_deadline(self.phase_timer, None).is_err() {
                    self.fail(Failure::SequenceExhausted);
                }
            }
            if !self.source_notified
                && (self.tx_end || self.stop_upload || self.failure.is_some())
                && let Some(exchange) = self.exchange.as_ref()
            {
                let id = exchange.id;
                self.source_notified = true;
                if let Some(output) = ports.source_finished(id) {
                    return Some(output);
                }
                continue;
            }
            if self.stop_upload && !self.closing {
                if self.write_live.is_some() && !self.cancel_write_sent {
                    self.cancel_write_sent = true;
                    if let Some(output) = ports.cancel(CancelOp {
                        target: self.write_live.unwrap(),
                    }) {
                        return Some(output);
                    }
                    continue;
                }
                if let Some(op) = self.output.take() {
                    self.settle_write(op, Acceptance::Exact);
                    continue;
                }
                if let Some((id, command)) = self.pending_body.take() {
                    self.body_result = Some(BodySent {
                        exchange: command.exchange,
                        id,
                        buffer: command.buffer,
                        accepted: 0,
                        acceptance: Acceptance::Exact,
                        result: Err(Failure::EarlyResponse),
                    });
                    continue;
                }
                if self.write_live.is_none() {
                    self.tx_done = true;
                }
            }
            if (self.closing || self.rx == Rx::Paused)
                && self.read_live.is_some()
                && !self.cancel_read_sent
            {
                self.cancel_read_sent = true;
                if let Some(output) = ports.cancel(CancelOp {
                    target: self.read_live.unwrap(),
                }) {
                    return Some(output);
                }
                continue;
            }
            if self.closing {
                self.clear_deadlines();
                if self.write_live.is_some() && !self.cancel_write_sent {
                    self.cancel_write_sent = true;
                    if let Some(output) = ports.cancel(CancelOp {
                        target: self.write_live.unwrap(),
                    }) {
                        return Some(output);
                    }
                    continue;
                }
                if let Some(op) = self.output.take() {
                    self.settle_write(op, Acceptance::Exact);
                    continue;
                }
                if let Some((id, command)) = self.pending_body.take() {
                    self.body_result = Some(BodySent {
                        exchange: command.exchange,
                        id,
                        buffer: command.buffer,
                        accepted: 0,
                        acceptance: Acceptance::Exact,
                        result: Err(self.failure.unwrap_or(Failure::Cancelled)),
                    });
                    continue;
                }
                if let Some(exchange) = self.exchange.take() {
                    if let Some(output) = ports.exchange_finished(ExchangeFinished {
                        exchange: exchange.id,
                        result: self.failure.map_or(Ok(()), Err),
                        reusable: false,
                    }) {
                        return Some(output);
                    }
                    continue;
                }
                if self.read_live.is_none()
                    && self.write_live.is_none()
                    && self.body_live.is_none()
                    && self.close_live.is_none()
                {
                    // This identity is reserved for exhaustion cleanup.
                    let id = OperationId {
                        connection: self.id,
                        sequence: u64::MAX,
                        kind: OperationKind::Close,
                    };
                    self.close_live = Some(id);
                    if let Some(output) = ports.close(CloseOp { id }) {
                        return Some(output);
                    }
                }
                return None;
            }
            if self.server
                && !self.tx_started
                && self.credit != 0
                && self.start == self.end
                && !self.eof
                && !matches!(self.rx, Rx::Done | Rx::Paused)
                && self.write_live.is_none()
                && self.output.is_none()
                && let Some(exchange) = self.exchange.as_ref().filter(|exchange| exchange.expect)
            {
                let id = exchange.id;
                if self
                    .inform(id, ResponseHead::new(100, "Continue", &[]))
                    .is_err()
                {
                    self.fail(Failure::Limit);
                }
                continue;
            }
            if self.write_live.is_none() && self.output.is_some() && !response_head_ready {
                if self.write_blocked {
                    let Some(id) = self.operation(OperationKind::Writable) else {
                        continue;
                    };
                    self.write_live = Some(id);
                    if let Some(output) = ports.readiness(ReadinessOp {
                        id,
                        direction: Direction::Write,
                    }) {
                        return Some(output);
                    }
                } else {
                    let Some(id) = self.operation(OperationKind::Write) else {
                        continue;
                    };
                    let mut op = self.output.take().unwrap();
                    op.id = id;
                    self.write_live = Some(id);
                    if let Some(output) = ports.write(op) {
                        return Some(output);
                    }
                }
                continue;
            }
            if self.write_live.is_none()
                && self.output.is_none()
                && !response_head_ready
                && let Some((body_id, command)) = self.pending_body.take()
            {
                let mut prefix = [0; 24];
                let chunked = self.tx == Framing::Chunked;
                let prefix_len = if chunked {
                    let mut target = &mut prefix[..];
                    write!(target, "{:x}\r\n", command.range.len()).unwrap();
                    24 - target.len()
                } else {
                    0
                };
                self.output = Some(WriteOp {
                    id: OperationId {
                        connection: self.id,
                        sequence: 0,
                        kind: OperationKind::Write,
                    },
                    storage: WriteStorage::Body {
                        command,
                        body_id,
                        prefix,
                        prefix_len,
                        chunked,
                    },
                    cursor: 0,
                });
                continue;
            }
            if self.failure.is_some()
                && self.tx_done
                && self.write_live.is_none()
                && self.output.is_none()
            {
                self.closing = true;
                self.clear_deadlines();
                continue;
            }
            if self.rx == Rx::Done
                && let Some(exchange) = self.exchange.as_mut()
                && !exchange.incoming_notified
            {
                exchange.incoming_notified = true;
                if let Some(output) = ports.incoming_finished(exchange.id) {
                    return Some(output);
                }
                continue;
            }
            if self.upgrade_pending
                && self.tx_done
                && self.read_live.is_none()
                && self.write_live.is_none()
                && self.body_live.is_none()
            {
                if !self.upgrade_notified {
                    self.clear_deadlines();
                    self.upgrade_notified = true;
                    return ports.upgrade_ready(self.exchange.as_ref().unwrap().id);
                }
                return None;
            }
            if self.tx_done
                && matches!(self.rx, Rx::Done | Rx::Paused)
                && self.read_live.is_none()
                && self.write_live.is_none()
                && self.body_live.is_none()
                && let Some(exchange) = self.exchange.take()
            {
                let reusable = !self.close_after
                    && (self.server || self.start == self.end)
                    && !self.graceful
                    && !self.eof
                    && exchange.persistent
                    && self.exchanges < self.config.max_requests;
                self.tx_started = false;
                self.tx_done = false;
                self.tx_end = false;
                self.demand_issued = false;
                self.source_notified = false;
                self.credit = 0;
                self.received = 0;
                self.no_content = false;
                self.outgoing_bytes = 0;
                self.metadata_bytes = 0;
                self.metadata_fields = 0;
                self.chunk_metadata_bytes = 0;
                self.informational = 0;
                self.outgoing_metadata_bytes = 0;
                self.outgoing_metadata_fields = 0;
                self.outgoing_informational = 0;
                self.outgoing_chunk_metadata_bytes = 0;
                self.stop_upload = false;
                self.incoming_connection_fields.clear();
                self.outgoing_connection_fields.clear();
                if reusable {
                    self.rx = Rx::Head;
                    if self
                        .set_deadline(TimerPhase::Idle, self.config.idle_timeout_ns)
                        .is_err()
                    {
                        self.fail(Failure::SequenceExhausted);
                    }
                } else {
                    self.closing = true;
                    self.clear_deadlines();
                }
                if let Some(output) = ports.exchange_finished(ExchangeFinished {
                    exchange: exchange.id,
                    result: Ok(()),
                    reusable,
                }) {
                    return Some(output);
                }
                continue;
            }
            if self.tx_started
                && !self.tx_end
                && !self.wait_continue
                && !self.demand_issued
                && self.pending_body.is_none()
                && self.output.is_none()
                && self.write_live.is_none()
                && !response_head_ready
            {
                self.demand_issued = true;
                let mut max = match self.tx {
                    Framing::Fixed(left) => usize::try_from(left)
                        .unwrap_or(usize::MAX)
                        .min(self.config.max_buffer_bytes),
                    _ => self.config.max_buffer_bytes,
                };
                max = max.min(
                    usize::try_from(self.config.max_body_bytes - self.outgoing_bytes)
                        .unwrap_or(usize::MAX),
                );
                if self.tx == Framing::Chunked {
                    let digits = self.config.max_chunk_line_bytes.saturating_sub(2).min(
                        self.config
                            .max_chunk_metadata_bytes
                            .saturating_sub(self.outgoing_chunk_metadata_bytes)
                            .saturating_sub(7),
                    );
                    let chunk_max = if digits >= (usize::BITS / 4) as usize {
                        usize::MAX
                    } else {
                        (1usize << (digits * 4)) - 1
                    };
                    max = max.min(chunk_max);
                }
                if let Some(output) = ports.send_ready(self.exchange.as_ref().unwrap().id, max) {
                    return Some(output);
                }
                continue;
            }
            if self.body_live.is_none()
                && self.start < self.end
                && self.input.is_some()
                && (self.server || self.exchange.is_some())
            {
                match self.rx {
                    Rx::Eof if self.no_content => {
                        self.fail(Failure::Protocol);
                        continue;
                    }
                    Rx::Head | Rx::Size | Rx::ChunkCrlf | Rx::Trailers => {
                        if self.rx == Rx::Head
                            && self.server
                            && self.head.is_empty()
                            && self
                                .phase_timer
                                .is_some_and(|(kind, _)| kind == TimerPhase::Idle)
                            && self
                                .set_deadline(TimerPhase::Head, self.config.head_timeout_ns)
                                .is_err()
                        {
                            self.fail(Failure::SequenceExhausted);
                            continue;
                        }
                        let byte = self.input.as_ref().unwrap().as_ref()[self.start];
                        self.start += 1;
                        if (byte == b'\n' && self.head.last() != Some(&b'\r'))
                            || (self.head.last() == Some(&b'\r') && byte != b'\n')
                        {
                            self.fail(Failure::Protocol);
                            continue;
                        }
                        let limit = if matches!(self.rx, Rx::Size | Rx::ChunkCrlf) {
                            self.config.max_chunk_line_bytes
                        } else {
                            self.config.max_head_bytes
                        };
                        let retained = if matches!(self.rx, Rx::Head | Rx::Trailers) {
                            self.metadata_bytes
                        } else {
                            0
                        };
                        if self.head.len().saturating_add(retained) >= limit {
                            self.fail(Failure::Limit);
                            continue;
                        }
                        if self.head.len() == self.head.capacity() {
                            let capacity = self.head.len().saturating_mul(2).max(32).min(limit);
                            self.head.reserve_exact(capacity - self.head.len());
                        }
                        self.head.push(byte);
                        let complete = match self.rx {
                            Rx::Head => self.head.ends_with(b"\r\n\r\n"),
                            Rx::Trailers => {
                                self.head == b"\r\n" || self.head.ends_with(b"\r\n\r\n")
                            }
                            _ => self.head.ends_with(b"\r\n"),
                        };
                        if complete
                            && let Some(output) = self.process_metadata(ports, head_callback)
                        {
                            return Some(output);
                        }
                        continue;
                    }
                    Rx::Fixed(_) | Rx::Chunk(_) | Rx::Eof if self.credit != 0 => {
                        let left = match self.rx {
                            Rx::Fixed(left) | Rx::Chunk(left) => left,
                            _ => u64::MAX,
                        };
                        let count = (self.end - self.start)
                            .min(self.credit)
                            .min(usize::try_from(left).unwrap_or(usize::MAX));
                        if self
                            .received
                            .checked_add(count as u64)
                            .is_none_or(|n| n > self.config.max_body_bytes)
                        {
                            self.fail(Failure::Limit);
                            continue;
                        }
                        let Some(id) = self.operation(OperationKind::Body) else {
                            continue;
                        };
                        self.body_live = Some(id);
                        self.credit -= count;
                        let op = BodyOp {
                            id,
                            exchange: self.exchange.as_ref().unwrap().id,
                            buffer: self.input.take().unwrap(),
                            range: self.start..self.start + count,
                            buffered_end: self.end,
                        };
                        if let Some(output) = ports.body(op) {
                            return Some(output);
                        }
                        continue;
                    }
                    _ => {}
                }
            }
            let needs_input = matches!(self.rx, Rx::Head | Rx::Size | Rx::ChunkCrlf | Rx::Trailers)
                || (matches!(self.rx, Rx::Fixed(_) | Rx::Chunk(_) | Rx::Eof)
                    && (self.credit != 0 || self.no_content));
            if self.eof && self.start == self.end && self.body_live.is_none() {
                match self.rx {
                    Rx::Eof => {
                        self.rx = Rx::Done;
                        self.close_after = true;
                        continue;
                    }
                    Rx::Head if self.head.is_empty() && self.exchange.is_none() => {
                        self.closing = true;
                        continue;
                    }
                    Rx::Done | Rx::Paused => {}
                    _ => {
                        self.fail(Failure::UnexpectedEof);
                        continue;
                    }
                }
            }
            if needs_input
                && !self.eof
                && self.start == self.end
                && self.read_live.is_none()
                && self.body_live.is_none()
                && (self.server || self.exchange.is_some())
            {
                self.start = 0;
                self.end = 0;
                if self.read_blocked {
                    let Some(id) = self.operation(OperationKind::Readable) else {
                        continue;
                    };
                    self.read_live = Some(id);
                    if let Some(output) = ports.readiness(ReadinessOp {
                        id,
                        direction: Direction::Read,
                    }) {
                        return Some(output);
                    }
                } else {
                    let Some(id) = self.operation(OperationKind::Read) else {
                        continue;
                    };
                    let buffer = self.input.take().unwrap();
                    let len = buffer.as_ref().len();
                    self.read_live = Some(id);
                    if let Some(output) = ports.read(ReadOp {
                        id,
                        buffer,
                        range: 0..len,
                    }) {
                        return Some(output);
                    }
                }
                continue;
            }
            return None;
        }
    }

    fn process_metadata<P: Ports<B, W>>(
        &mut self,
        ports: &mut P,
        callback: fn(&mut P, ExchangeId, ParsedHead<'_>) -> Option<P::Output>,
    ) -> Option<P::Output> {
        let mut bytes = std::mem::take(&mut self.head);
        let mut headers = [httparse::EMPTY_HEADER; 128];
        let accounting = if matches!(self.rx, Rx::Head | Rx::Trailers) {
            self.metadata_bytes = self.metadata_bytes.saturating_add(bytes.len());
            self.metadata_bytes <= self.config.max_head_bytes
        } else {
            self.chunk_metadata_bytes = self.chunk_metadata_bytes.saturating_add(bytes.len());
            self.chunk_metadata_bytes <= self.config.max_chunk_metadata_bytes
        };
        let result = if !accounting {
            Err(Failure::Limit)
        } else if !codec::strict_lines(&bytes) {
            Err(Failure::Protocol)
        } else {
            self.metadata(
                &bytes,
                &mut headers[..self.config.max_headers],
                ports,
                callback,
            )
        };
        bytes.clear();
        self.head = bytes;
        match result {
            Ok(output) => output,
            Err(error) => {
                self.fail(error);
                None
            }
        }
    }

    fn metadata<'a, P: Ports<B, W>>(
        &mut self,
        bytes: &'a [u8],
        headers: &'a mut [Header<'a>],
        ports: &mut P,
        callback: fn(&mut P, ExchangeId, ParsedHead<'_>) -> Option<P::Output>,
    ) -> Result<Option<P::Output>, Failure> {
        match self.rx {
            Rx::Size => {
                let line = &bytes[..bytes.len() - 2];
                let size = codec::chunk_size(line)?;
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
                let httparse::Status::Complete((count, trailers)) =
                    httparse::parse_headers(bytes, headers).map_err(parse_error)?
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
                Ok(ports.trailers(self.exchange.as_ref().unwrap().id, trailers))
            }
            Rx::Head if self.server => {
                let mut request = httparse::Request::new(headers);
                let httparse::Status::Complete(count) =
                    request.parse(bytes).map_err(parse_error)?
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
                    head_method: method == "HEAD",
                    connect_method: method == "CONNECT",
                    persistent: codec::persistent(version, request.headers),
                    upgrade,
                    expect,
                    incoming_notified: false,
                });
                self.exchanges += 1;
                self.set_rx(framing)?;
                Ok(callback(
                    ports,
                    id,
                    ParsedHead::Request(RequestHead {
                        method,
                        target: request.path.ok_or(Failure::Protocol)?,
                        version,
                        headers: request.headers,
                    }),
                ))
            }
            Rx::Head => {
                let mut response = httparse::Response::new(headers);
                let httparse::Status::Complete(count) =
                    response.parse(bytes).map_err(parse_error)?
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
                let tunnel = exchange.connect_method && (200..300).contains(&status);
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
                        self.wait_continue = false;
                        self.update_deadline(self.phase_timer, None)
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
                            || !self.tx_end
                        {
                            return Err(Failure::Protocol);
                        }
                        self.upgrade_pending = true;
                        self.rx = Rx::Done;
                    } else if tunnel {
                        if !self.tx_end {
                            return Err(Failure::Protocol);
                        }
                        self.upgrade_pending = true;
                        self.rx = Rx::Done;
                    } else {
                        let framing = if exchange.head_method || status == 204 || status == 304 {
                            Framing::Empty
                        } else {
                            framing
                        };
                        self.set_rx(framing)?;
                    }
                    if !self.tx_done
                        && self.tx != Framing::Empty
                        && !self.upgrade_pending
                        && (status >= 300 || self.wait_continue || self.rx == Rx::Done)
                    {
                        self.wait_continue = false;
                        self.close_after = true;
                        self.tx_end = true;
                        self.stop_upload = true;
                        self.upload_at = None;
                    }
                    self.update_deadline(self.phase_timer, None)
                        .map_err(|_| Failure::SequenceExhausted)?;
                }
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

fn header_count(bytes: &[u8]) -> usize {
    bytes
        .windows(2)
        .filter(|pair| *pair == b"\r\n")
        .count()
        .saturating_sub(2)
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
            core: Core::new(id, config, receive_buffer, now, true)?,
        })
    }
    pub fn connection_id(&self) -> ConnectionId {
        self.core.id
    }
    /// Bytes already received but not yet consumed by HTTP or a body consumer.
    ///
    /// This view is empty while the receive buffer belongs to an operation.
    pub fn buffered_input(&self) -> &[u8] {
        self.core.input.as_ref().map_or(&[], |buffer| {
            &buffer.as_ref()[self.core.start..self.core.end]
        })
    }
    pub fn send_body(&mut self, command: SendBody<W>) -> Result<BodyId, Rejected<SendBody<W>>> {
        self.core.send_body(command)
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
        if self.core.closed || self.core.handed_off {
            return;
        }
        self.core.graceful = true;
        if mode == ShutdownMode::Abort || self.core.upgrade_pending {
            self.core.fail(Failure::Cancelled);
        } else if self.core.exchange.is_none() {
            self.core.closing = true;
            self.core.clear_deadlines();
        }
    }
    pub fn observe_time(&mut self, now: Tick) -> Result<(), CommandError> {
        if now < self.core.now {
            return Err(CommandError::TimeRegression);
        }
        self.core.now = now;
        Ok(())
    }
    pub fn expire(&mut self, deadline: Deadline, now: Tick) -> Result<(), CommandError> {
        if self.core.deadline != Some(deadline) {
            return Err(CommandError::StaleDeadline);
        }
        if now < deadline.at {
            return Err(CommandError::EarlyDeadline);
        }
        if self.core.deadline_kind == Some(TimerPhase::Continue) && self.core.phase_timer.is_some()
        {
            self.core
                .sequence
                .checked_add(1)
                .filter(|n| *n < u64::MAX)
                .ok_or(CommandError::SequenceExhausted)?;
        }
        self.observe_time(now)?;
        if self.core.deadline_kind == Some(TimerPhase::Continue) {
            self.core.wait_continue = false;
            self.core.update_deadline(self.core.phase_timer, None)?;
        } else {
            self.core.fail(Failure::Timeout);
        }
        Ok(())
    }
    pub fn take_upgrade(&mut self) -> Result<Handoff<B>, CommandError> {
        if !self.core.upgrade_notified
            || !self.core.upgrade_pending
            || self.core.handed_off
            || self.core.input.is_none()
            || self.core.closing
            || self.core.closed
            || self.core.graceful
            || self.core.failure.is_some()
            || !self.core.tx_done
            || self.core.rx != Rx::Done
            || self.core.read_live.is_some()
            || self.core.write_live.is_some()
            || self.core.body_live.is_some()
            || self.core.close_live.is_some()
            || self.core.output.is_some()
            || self.core.pending_body.is_some()
            || self.core.body_result.is_some()
        {
            return Err(CommandError::NotReady);
        }
        self.core.handed_off = true;
        self.core.upgrade_notified = false;
        self.core.upgrade_pending = false;
        Ok(Handoff {
            connection: self.core.id,
            buffered: BufferedInput {
                buffer: self.core.input.take().unwrap(),
                range: self.core.start..self.core.end,
            },
        })
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
            core: Core::new(id, config, receive_buffer, now, false)?,
        })
    }
    pub fn connection_id(&self) -> ConnectionId {
        self.core.id
    }
    /// Bytes already received but not yet consumed by HTTP or a body consumer.
    ///
    /// This view is empty while the receive buffer belongs to an operation.
    pub fn buffered_input(&self) -> &[u8] {
        self.core.input.as_ref().map_or(&[], |buffer| {
            &buffer.as_ref()[self.core.start..self.core.end]
        })
    }
    pub fn send_body(&mut self, command: SendBody<W>) -> Result<BodyId, Rejected<SendBody<W>>> {
        self.core.send_body(command)
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
        if self.core.closed || self.core.handed_off {
            return;
        }
        self.core.graceful = true;
        if mode == ShutdownMode::Abort || self.core.upgrade_pending {
            self.core.fail(Failure::Cancelled);
        } else if self.core.exchange.is_none() {
            self.core.closing = true;
            self.core.clear_deadlines();
        }
    }
    pub fn observe_time(&mut self, now: Tick) -> Result<(), CommandError> {
        if now < self.core.now {
            return Err(CommandError::TimeRegression);
        }
        self.core.now = now;
        Ok(())
    }
    pub fn expire(&mut self, deadline: Deadline, now: Tick) -> Result<(), CommandError> {
        if self.core.deadline != Some(deadline) {
            return Err(CommandError::StaleDeadline);
        }
        if now < deadline.at {
            return Err(CommandError::EarlyDeadline);
        }
        if self.core.deadline_kind == Some(TimerPhase::Continue) && self.core.phase_timer.is_some()
        {
            self.core
                .sequence
                .checked_add(1)
                .filter(|n| *n < u64::MAX)
                .ok_or(CommandError::SequenceExhausted)?;
        }
        self.observe_time(now)?;
        if self.core.deadline_kind == Some(TimerPhase::Continue) {
            self.core.wait_continue = false;
            self.core.update_deadline(self.core.phase_timer, None)?;
        } else {
            self.core.fail(Failure::Timeout);
        }
        Ok(())
    }
    pub fn take_upgrade(&mut self) -> Result<Handoff<B>, CommandError> {
        if !self.core.upgrade_notified
            || !self.core.upgrade_pending
            || self.core.handed_off
            || self.core.input.is_none()
            || self.core.closing
            || self.core.closed
            || self.core.graceful
            || self.core.failure.is_some()
            || !self.core.tx_done
            || self.core.rx != Rx::Done
            || self.core.read_live.is_some()
            || self.core.write_live.is_some()
            || self.core.body_live.is_some()
            || self.core.close_live.is_some()
            || self.core.output.is_some()
            || self.core.pending_body.is_some()
            || self.core.body_result.is_some()
        {
            return Err(CommandError::NotReady);
        }
        self.core.handed_off = true;
        self.core.upgrade_notified = false;
        self.core.upgrade_pending = false;
        Ok(Handoff {
            connection: self.core.id,
            buffered: BufferedInput {
                buffer: self.core.input.take().unwrap(),
                range: self.core.start..self.core.end,
            },
        })
    }
}

impl<B: Buffer, W: AsRef<[u8]>> Server<B, W> {
    pub fn next<P: ServerPorts<B, W>>(&mut self, ports: &mut P) -> Option<P::Output> {
        self.core.next(ports, |ports, id, head| match head {
            ParsedHead::Request(head) => ports.request(id, head),
            ParsedHead::Response(..) => unreachable!(),
        })
    }
    pub fn respond(
        &mut self,
        exchange: ExchangeId,
        response: Response<'_>,
    ) -> Result<(), CommandError> {
        self.core.respond(exchange, response, false)
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
            true,
        )
    }
}

impl<B: Buffer, W: AsRef<[u8]>> Client<B, W> {
    pub fn next<P: ClientPorts<B, W>>(&mut self, ports: &mut P) -> Option<P::Output> {
        self.core.next(ports, |ports, id, head| match head {
            ParsedHead::Response(head, informational) => ports.response(id, head, informational),
            ParsedHead::Request(..) => unreachable!(),
        })
    }
    pub fn request(&mut self, request: Request<'_>) -> Result<ExchangeId, CommandError> {
        self.core.request(request)
    }
}

#[cfg(test)]
#[path = "connection_tests.rs"]
mod tests;
