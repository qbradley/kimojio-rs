use crate::operations::{Outgoing, WriteStorage};
use crate::*;
use kimojio_fsm_http1::Handoff;

#[derive(Default)]
struct Utf8 {
    remaining: u8,
    low: u8,
    high: u8,
}
impl Utf8 {
    fn feed(&mut self, bytes: &[u8]) -> bool {
        for &byte in bytes {
            if self.remaining != 0 {
                if byte < self.low || byte > self.high {
                    return false;
                }
                self.remaining -= 1;
                self.low = 0x80;
                self.high = 0xbf;
            } else {
                let (remaining, low, high) = match byte {
                    0..=0x7f => (0, 0x80, 0xbf),
                    0xc2..=0xdf => (1, 0x80, 0xbf),
                    0xe0 => (2, 0xa0, 0xbf),
                    0xe1..=0xec | 0xee..=0xef => (2, 0x80, 0xbf),
                    0xed => (2, 0x80, 0x9f),
                    0xf0 => (3, 0x90, 0xbf),
                    0xf1..=0xf3 => (3, 0x80, 0xbf),
                    0xf4 => (3, 0x80, 0x8f),
                    _ => return false,
                };
                self.remaining = remaining;
                self.low = low;
                self.high = high;
            }
        }
        true
    }
}

struct Incoming {
    info: MessageInfo,
    utf8: Utf8,
    fragments: u64,
}
struct Frame {
    opcode: u8,
    fin: bool,
    remaining: u64,
    offset: u64,
    mask: [u8; 4],
    control: [u8; 125],
    control_len: usize,
}

/// Server framing after a validated HTTP upgrade.
///
/// One receive buffer, one accepted outgoing message and fixed control slots
/// bound storage. The core allocates no message payloads and never clones `W`.
pub struct Server<B, W = B> {
    id: ConnectionId,
    config: Config,
    sequence: u64,
    now: Tick,
    input: Option<B>,
    start: usize,
    end: usize,
    read_live: Option<OperationId>,
    write_live: Option<OperationId>,
    chunk_live: Option<OperationId>,
    read_wait: Option<OperationId>,
    write_wait: Option<OperationId>,
    read_blocked: bool,
    write_blocked: bool,
    read_cancelled: bool,
    write_cancelled: bool,
    read_wait_cancelled: bool,
    write_wait_cancelled: bool,
    output: Option<WriteOp<W>>,
    outgoing: Option<Outgoing<W>>,
    tx_active: bool,
    receipt: Option<MessageSent<W>>,
    header: [u8; 14],
    header_len: usize,
    frame: Option<Frame>,
    incoming: Option<Incoming>,
    started: Option<MessageInfo>,
    finished: Option<MessageInfo>,
    pong: Option<([u8; 125], usize)>,
    close_reason: Option<CloseReason>,
    close_sent: bool,
    peer_close: Option<CloseReason>,
    peer_notice: bool,
    failure: Option<Failure>,
    hard_abort: bool,
    close_live: Option<OperationId>,
    transport_closed: bool,
    closed_notified: bool,
    idle_at: Option<Tick>,
    frame_at: Option<Tick>,
    message_at: Option<Tick>,
    write_at: Option<Tick>,
    close_at: Option<Tick>,
    deadline: Option<Deadline>,
    deadline_dirty: bool,
    timer_sequence: u64,
}

fn after(now: Tick, duration: Option<u64>) -> Option<Tick> {
    duration.map(|duration| Tick(now.0.saturating_add(duration)))
}

impl<B: Buffer, W: AsRef<[u8]>> Server<B, W> {
    /// Starts framing on an already upgraded stream without buffered bytes.
    pub fn new(
        id: ConnectionId,
        config: Config,
        buffer: B,
        now: Tick,
    ) -> Result<Self, Rejected<B>> {
        if !Self::valid_config(&config, buffer.as_ref().len()) {
            return Err(Rejected {
                reason: RejectReason::Limit,
                value: buffer,
            });
        }
        Ok(Self::initialize(id, config, buffer, 0, 0, now))
    }

    /// Consumes exactly the unread range returned by HTTP's `take_upgrade`.
    pub fn from_handoff(
        handoff: Handoff<B>,
        config: Config,
        now: Tick,
    ) -> Result<Self, Rejected<UpgradeInput<B>>> {
        let id = handoff.connection;
        let (buffer, range) = handoff.buffered.into_parts();
        if !Self::valid_config(&config, buffer.as_ref().len()) {
            return Err(Rejected {
                reason: RejectReason::Limit,
                value: UpgradeInput {
                    connection: id,
                    buffer,
                    range,
                },
            });
        }
        Ok(Self::initialize(
            id,
            config,
            buffer,
            range.start,
            range.end,
            now,
        ))
    }

    fn valid_config(c: &Config, buffer_len: usize) -> bool {
        buffer_len > 0
            && buffer_len <= c.max_buffer_bytes
            && c.outgoing_frame_bytes > 0
            && c.max_fragments > 0
            && c.outgoing_frame_bytes as u64 <= c.max_frame_bytes
            && [
                c.idle_timeout_ns,
                c.frame_timeout_ns,
                c.message_timeout_ns,
                c.write_timeout_ns,
                c.close_timeout_ns,
            ]
            .into_iter()
            .flatten()
            .all(|n| n > 0)
    }
    fn initialize(
        id: ConnectionId,
        config: Config,
        buffer: B,
        start: usize,
        end: usize,
        now: Tick,
    ) -> Self {
        let idle_at = after(now, config.idle_timeout_ns);
        Self {
            id,
            config,
            sequence: 0,
            now,
            input: Some(buffer),
            start,
            end,
            read_live: None,
            write_live: None,
            chunk_live: None,
            read_wait: None,
            write_wait: None,
            read_blocked: false,
            write_blocked: false,
            read_cancelled: false,
            write_cancelled: false,
            read_wait_cancelled: false,
            write_wait_cancelled: false,
            output: None,
            outgoing: None,
            tx_active: false,
            receipt: None,
            header: [0; 14],
            header_len: 0,
            frame: None,
            incoming: None,
            started: None,
            finished: None,
            pong: None,
            close_reason: None,
            close_sent: false,
            peer_close: None,
            peer_notice: false,
            failure: None,
            hard_abort: false,
            close_live: None,
            transport_closed: false,
            closed_notified: false,
            idle_at,
            frame_at: None,
            message_at: None,
            write_at: None,
            close_at: None,
            deadline: None,
            deadline_dirty: false,
            timer_sequence: 0,
        }
    }
    pub fn connection(&self) -> ConnectionId {
        self.id
    }
    pub fn can_send(&self) -> bool {
        !self.tx_active
            && self.receipt.is_none()
            && self.close_reason.is_none()
            && !self.hard_abort
            && !self.transport_closed
    }
    pub fn is_closing(&self) -> bool {
        self.close_reason.is_some() || self.hard_abort
    }

    fn sequence(&mut self) -> Result<u64, CommandError> {
        if self.sequence >= u64::MAX - 1 {
            return Err(CommandError::SequenceExhausted);
        }
        self.sequence += 1;
        Ok(self.sequence)
    }
    fn operation(&mut self, kind: OperationKind) -> Option<OperationId> {
        match self.sequence() {
            Ok(sequence) => Some(OperationId {
                connection: self.id,
                sequence,
                kind,
            }),
            Err(_) => {
                self.abort(Failure::SequenceExhausted);
                None
            }
        }
    }
    pub fn send_message(
        &mut self,
        command: SendMessage<W>,
    ) -> Result<MessageId, Rejected<SendMessage<W>>> {
        let reason = if !self.can_send() {
            Some(RejectReason::NoCapacity)
        } else if command.range.start > command.range.end
            || command.range.end > command.buffer.as_ref().len()
        {
            Some(RejectReason::InvalidRange)
        } else if command.buffer.as_ref().len() > self.config.max_buffer_bytes
            || command.range.len() as u64 > self.config.max_message_bytes
        {
            Some(RejectReason::Limit)
        } else if command.kind == MessageKind::Text
            && std::str::from_utf8(&command.buffer.as_ref()[command.range.clone()]).is_err()
        {
            Some(RejectReason::InvalidState)
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(Rejected {
                reason,
                value: command,
            });
        }
        let Ok(sequence) = self.sequence() else {
            self.abort(Failure::SequenceExhausted);
            return Err(Rejected {
                reason: RejectReason::Limit,
                value: command,
            });
        };
        let id = MessageId {
            connection: self.id,
            sequence,
        };
        self.tx_active = true;
        self.outgoing = Some(Outgoing {
            id,
            offset: command.range.start,
            command,
            accepted: 0,
            acceptance: Acceptance::Exact,
        });
        self.write_at = after(self.now, self.config.write_timeout_ns);
        Ok(id)
    }
    pub fn close(&mut self, reason: CloseReason) -> Result<(), CommandError> {
        if reason.code() == Some(1010) {
            return Err(CommandError::InvalidClose);
        }
        if self.is_closing() || self.transport_closed {
            return Err(CommandError::InvalidState);
        }
        self.begin_close(reason);
        Ok(())
    }
    fn begin_close(&mut self, reason: CloseReason) {
        if self.close_reason.is_none() {
            self.close_reason = Some(reason);
            self.close_at = after(self.now, self.config.close_timeout_ns);
        }
        self.started = None;
        self.finished = None;
        self.incoming = None;
        self.message_at = None;
        if self.peer_close.is_some() || self.failure.is_some() {
            self.pong = None;
        }
    }
    /// Reports application/source failure. No outgoing producer streaming exists.
    pub fn fail_source(&mut self) {
        self.fail(Failure::Application);
    }
    pub fn abort(&mut self, failure: Failure) {
        self.failure.get_or_insert(failure);
        self.hard_abort = true;
        self.started = None;
        self.finished = None;
        self.incoming = None;
        self.pong = None;
    }
    fn fail(&mut self, failure: Failure) {
        self.failure.get_or_insert(failure);
        let code = match failure {
            Failure::InvalidUtf8 => 1007,
            Failure::Limit => 1009,
            Failure::Application => 1011,
            _ => 1002,
        };
        self.begin_close(CloseReason::new(code, "").unwrap());
    }
    fn terminating(&self) -> bool {
        self.hard_abort
            || (self.close_sent && (self.peer_close.is_some() || self.failure.is_some()))
    }
    fn reject<T>(
        &self,
        id: OperationId,
        expected: Option<OperationId>,
        value: T,
    ) -> Result<T, Rejected<T>> {
        let reason = if id.connection != self.id {
            Some(RejectReason::WrongConnection)
        } else if expected != Some(id) {
            Some(RejectReason::Stale)
        } else {
            None
        };
        match reason {
            Some(reason) => Err(Rejected { reason, value }),
            None => Ok(value),
        }
    }

    pub fn complete_read(
        &mut self,
        completion: ReadCompletion<B>,
    ) -> Result<(), Rejected<ReadCompletion<B>>> {
        let c = self.reject(completion.op.id, self.read_live, completion)?;
        if c.result.is_ok_and(|n| n > c.op.range.len()) {
            return Err(Rejected {
                reason: RejectReason::InvalidCount,
                value: c,
            });
        }
        self.read_live = None;
        self.read_cancelled = false;
        self.input = Some(c.op.buffer);
        match c.result {
            Ok(n) if n > 0 => {
                self.start = c.op.range.start;
                self.end = c.op.range.start + n;
                self.idle_at = after(self.now, self.config.idle_timeout_ns);
            }
            _ if self.terminating() || self.failure.is_some() => {}
            Ok(_) => self.abort(Failure::UnexpectedEof),
            Err(IoError {
                kind: IoErrorKind::WouldBlock,
                ..
            }) => self.read_blocked = true,
            Err(IoError {
                kind: IoErrorKind::Interrupted,
                ..
            }) => {}
            Err(error) => self.abort(Failure::Transport(error)),
        }
        Ok(())
    }
    pub fn release_chunk(
        &mut self,
        completion: ChunkCompletion<B>,
    ) -> Result<(), Rejected<ChunkCompletion<B>>> {
        let c = self.reject(completion.op.id, self.chunk_live, completion)?;
        self.chunk_live = None;
        self.input = Some(c.op.buffer);
        Ok(())
    }
    #[allow(clippy::result_large_err)] // Rejection returns the original inline operation without allocation.
    pub fn complete_write(
        &mut self,
        completion: WriteCompletion<W>,
    ) -> Result<(), Rejected<WriteCompletion<W>>> {
        let c = self.reject(completion.op.id, self.write_live, completion)?;
        if c.result.is_ok_and(|n| n > c.op.remaining()) {
            return Err(Rejected {
                reason: RejectReason::InvalidCount,
                value: c,
            });
        }
        self.write_live = None;
        self.write_cancelled = false;
        let mut op = c.op;
        match c.result {
            Ok(n) if n > 0 => {
                let before = op.cursor.saturating_sub(op.header_len);
                op.cursor += n;
                let accepted = op.cursor.saturating_sub(op.header_len) - before;
                if let WriteStorage::Data { outgoing, .. } = &mut op.storage {
                    outgoing.accepted += accepted;
                }
                self.write_at = after(self.now, self.config.write_timeout_ns);
                if op.remaining() == 0 {
                    match op.storage {
                        WriteStorage::Data {
                            mut outgoing,
                            range,
                            last,
                        } => {
                            outgoing.offset = range.end;
                            if last {
                                let result = if self.is_closing() {
                                    Err(self.failure.unwrap_or(Failure::Closing))
                                } else {
                                    Ok(())
                                };
                                self.finish_outgoing(outgoing, result);
                            } else {
                                self.outgoing = Some(outgoing);
                            }
                        }
                        WriteStorage::Control { close: true, .. } => self.close_sent = true,
                        _ => {}
                    }
                } else {
                    self.output = Some(op);
                }
            }
            Ok(_) => {
                self.output = Some(op);
                self.abort(Failure::WriteZero);
            }
            Err(IoError {
                kind: IoErrorKind::WouldBlock,
                ..
            }) if !self.terminating() => {
                self.output = Some(op);
                self.write_blocked = true;
            }
            Err(IoError {
                kind: IoErrorKind::Interrupted,
                ..
            }) if !self.terminating() => self.output = Some(op),
            Err(error) => {
                if matches!(
                    error.kind,
                    IoErrorKind::UnknownProgress | IoErrorKind::CancelledUnknownProgress
                ) && let WriteStorage::Data { outgoing, .. } = &mut op.storage
                {
                    outgoing.acceptance = Acceptance::LowerBound;
                }
                self.output = Some(op);
                if !self.terminating() {
                    self.abort(Failure::Transport(error));
                }
            }
        }
        Ok(())
    }
    fn finish_outgoing(&mut self, outgoing: Outgoing<W>, result: Result<(), Failure>) {
        self.tx_active = false;
        self.receipt = Some(MessageSent {
            id: outgoing.id,
            buffer: outgoing.command.buffer,
            accepted: outgoing.accepted,
            acceptance: outgoing.acceptance,
            result,
        });
    }
    pub fn complete_readiness(
        &mut self,
        completion: ReadinessCompletion,
    ) -> Result<(), Rejected<ReadinessCompletion>> {
        let expected = match completion.op.direction {
            Direction::Read => self.read_wait,
            Direction::Write => self.write_wait,
        };
        let c = self.reject(completion.op.id, expected, completion)?;
        let cancelled = match c.op.direction {
            Direction::Read => self.read_wait_cancelled,
            Direction::Write => self.write_wait_cancelled,
        };
        match c.op.direction {
            Direction::Read => {
                self.read_wait = None;
                self.read_blocked = false;
                self.read_wait_cancelled = false;
            }
            Direction::Write => {
                self.write_wait = None;
                self.write_blocked = false;
                self.write_wait_cancelled = false;
            }
        }
        if let Err(error) = c.result
            && !self.terminating()
            && !cancelled
        {
            self.abort(Failure::Transport(error));
        }
        Ok(())
    }
    pub fn complete_close(
        &mut self,
        completion: CloseCompletion,
    ) -> Result<(), Rejected<CloseCompletion>> {
        let c = self.reject(completion.op.id, self.close_live, completion)?;
        self.close_live = None;
        self.transport_closed = true;
        if let Err(error) = c.result {
            self.failure.get_or_insert(Failure::Transport(error));
        }
        Ok(())
    }
    pub fn observe_time(&mut self, now: Tick) -> Result<(), CommandError> {
        if now < self.now {
            return Err(CommandError::TimeRegression);
        }
        self.now = now;
        Ok(())
    }
    pub fn expire(&mut self, deadline: Deadline, now: Tick) -> Result<(), CommandError> {
        self.refresh_deadline();
        if self.deadline != Some(deadline) {
            return Err(CommandError::StaleDeadline);
        }
        if now < self.now {
            return Err(CommandError::TimeRegression);
        }
        if now < deadline.at {
            return Err(CommandError::EarlyDeadline);
        }
        self.now = now;
        self.abort(Failure::Timeout);
        Ok(())
    }
    fn refresh_deadline(&mut self) {
        let at = if self.terminating() || self.transport_closed {
            None
        } else if self.is_closing() {
            [self.close_at, self.write_at].into_iter().flatten().min()
        } else {
            [self.idle_at, self.frame_at, self.message_at, self.write_at]
                .into_iter()
                .flatten()
                .min()
        };
        if self.deadline.map(|d| d.at) != at {
            if let Some(sequence) = self.timer_sequence.checked_add(1) {
                self.timer_sequence = sequence;
                self.deadline = at.map(|at| Deadline {
                    connection: self.id,
                    sequence,
                    at,
                });
            } else {
                self.abort(Failure::SequenceExhausted);
                self.deadline = None;
            }
            self.deadline_dirty = true;
        }
    }

    /// Drains finite buffered work. Scheduling budgets belong to the caller.
    pub fn next<P: Ports<B, W>>(&mut self, ports: &mut P) -> Option<P::Output> {
        loop {
            if self.write_live.is_none() && self.output.is_none() && self.outgoing.is_none() {
                self.write_at = None;
            }
            self.refresh_deadline();
            if self.deadline_dirty {
                self.deadline_dirty = false;
                if let Some(output) = ports.deadline_changed(self.deadline) {
                    return Some(output);
                }
                continue;
            }
            if let Some(receipt) = self.receipt.take() {
                if let Some(output) = ports.message_sent(receipt) {
                    return Some(output);
                }
                continue;
            }
            if self.peer_notice {
                self.peer_notice = false;
                if let Some(output) = ports.peer_closed(self.peer_close.unwrap()) {
                    return Some(output);
                }
                continue;
            }
            if self.is_closing() {
                let discard_output = self.output.as_ref().is_some_and(|op| {
                    self.hard_abort
                        || (op.cursor == 0
                            && match op.storage {
                                WriteStorage::Data { .. } => true,
                                WriteStorage::Control { close, .. } => {
                                    !close && (self.peer_close.is_some() || self.failure.is_some())
                                }
                            })
                });
                if discard_output && let Some(op) = self.output.take() {
                    if let WriteStorage::Data { outgoing, .. } = op.storage {
                        self.finish_outgoing(
                            outgoing,
                            Err(self.failure.unwrap_or(Failure::Closing)),
                        );
                    }
                    continue;
                }
                if let Some(outgoing) = self.outgoing.take() {
                    self.finish_outgoing(outgoing, Err(self.failure.unwrap_or(Failure::Closing)));
                    continue;
                }
            }
            if let Some(cancel) = self.next_cancel() {
                if let Some(output) = ports.cancel(cancel) {
                    return Some(output);
                }
                continue;
            }
            if self.terminating() {
                if self.read_live.is_some()
                    || self.write_live.is_some()
                    || self.chunk_live.is_some()
                    || self.read_wait.is_some()
                    || self.write_wait.is_some()
                {
                    return None;
                }
                if self.transport_closed {
                    if !self.closed_notified {
                        self.closed_notified = true;
                        return ports.closed(ConnectionResult {
                            result: self.failure.map_or(Ok(()), Err),
                            peer_close: self.peer_close,
                            clean: self.failure.is_none()
                                && self.peer_close.is_some()
                                && self.close_sent,
                        });
                    }
                    return None;
                }
                if self.close_live.is_none() {
                    let id = OperationId {
                        connection: self.id,
                        sequence: u64::MAX,
                        kind: OperationKind::Close,
                    };
                    self.close_live = Some(id);
                    if let Some(output) = ports.close(CloseOp { id }) {
                        return Some(output);
                    }
                    continue;
                }
                return None;
            }
            if self.write_live.is_none() && self.write_wait.is_none() {
                if self.write_blocked {
                    let Some(id) = self.operation(OperationKind::Writable) else {
                        continue;
                    };
                    self.write_wait = Some(id);
                    if let Some(output) = ports.readiness(ReadinessOp {
                        id,
                        direction: Direction::Write,
                    }) {
                        return Some(output);
                    }
                    continue;
                }
                self.prepare_output();
                if let Some(mut op) = self.output.take() {
                    let Some(id) = self.operation(OperationKind::Write) else {
                        self.output = Some(op);
                        continue;
                    };
                    op.id = id;
                    self.write_live = Some(id);
                    self.write_at.get_or_insert_with(|| {
                        after(self.now, self.config.write_timeout_ns).unwrap_or(Tick(u64::MAX))
                    });
                    if self.config.write_timeout_ns.is_none() {
                        self.write_at = None;
                    }
                    if let Some(output) = ports.write(op) {
                        return Some(output);
                    }
                    continue;
                }
            }
            if let Some(info) = self.started.take() {
                if let Some(output) = ports.message_started(info) {
                    return Some(output);
                }
                continue;
            }
            if let Some(info) = self.finished.take() {
                if let Some(output) = ports.message_finished(info) {
                    return Some(output);
                }
                continue;
            }
            if self.failure.is_some() || self.peer_close.is_some() {
                return None;
            }
            if self.chunk_live.is_none() && self.input.is_some() {
                if let Some(frame) = &self.frame
                    && frame.remaining == 0
                {
                    self.finish_frame();
                    continue;
                }
                if self.start < self.end {
                    if self.frame.is_none() {
                        self.parse_header();
                        continue;
                    }
                    if let Some(chunk) = self.parse_payload()
                        && let Some(output) = ports.chunk(chunk)
                    {
                        return Some(output);
                    }
                    continue;
                }
                if self.read_live.is_none() && self.read_wait.is_none() {
                    if self.read_blocked {
                        let Some(id) = self.operation(OperationKind::Readable) else {
                            continue;
                        };
                        self.read_wait = Some(id);
                        if let Some(output) = ports.readiness(ReadinessOp {
                            id,
                            direction: Direction::Read,
                        }) {
                            return Some(output);
                        }
                        continue;
                    }
                    let Some(id) = self.operation(OperationKind::Read) else {
                        continue;
                    };
                    let buffer = self.input.take().unwrap();
                    let len = buffer.as_ref().len();
                    self.start = 0;
                    self.end = 0;
                    self.read_live = Some(id);
                    if let Some(output) = ports.read(ReadOp {
                        id,
                        buffer,
                        range: 0..len,
                    }) {
                        return Some(output);
                    }
                    continue;
                }
            }
            return None;
        }
    }
    fn next_cancel(&mut self) -> Option<CancelOp> {
        if self.terminating() || self.failure.is_some() || self.peer_close.is_some() {
            for (id, cancelled) in [
                (self.read_live, &mut self.read_cancelled),
                (self.read_wait, &mut self.read_wait_cancelled),
            ] {
                if let Some(target) = id
                    && !*cancelled
                {
                    *cancelled = true;
                    return Some(CancelOp { target });
                }
            }
        }
        if self.terminating() {
            for (id, cancelled) in [
                (self.write_live, &mut self.write_cancelled),
                (self.write_wait, &mut self.write_wait_cancelled),
            ] {
                if let Some(target) = id
                    && !*cancelled
                {
                    *cancelled = true;
                    return Some(CancelOp { target });
                }
            }
        }
        None
    }
    fn prepare_output(&mut self) {
        if self.output.is_some() {
            return;
        }
        if let Some(reason) = self.close_reason
            && !self.close_sent
        {
            self.control_output(8, reason.bytes, usize::from(reason.len), true);
        } else if let Some((bytes, len)) = self.pong.take() {
            self.control_output(10, bytes, len, false);
        } else if let Some(outgoing) = self.outgoing.take() {
            let start = outgoing.offset;
            let end = (start.saturating_add(self.config.outgoing_frame_bytes))
                .min(outgoing.command.range.end);
            let last = end == outgoing.command.range.end;
            let opcode = if start != outgoing.command.range.start {
                0
            } else if outgoing.command.kind == MessageKind::Text {
                1
            } else {
                2
            };
            let (header, header_len) = encode_header(opcode, last, end - start);
            self.output = Some(WriteOp {
                id: OperationId {
                    connection: self.id,
                    sequence: 0,
                    kind: OperationKind::Write,
                },
                header,
                header_len,
                cursor: 0,
                storage: WriteStorage::Data {
                    outgoing,
                    range: start..end,
                    last,
                },
            });
        }
    }
    fn control_output(&mut self, opcode: u8, bytes: [u8; 125], len: usize, close: bool) {
        let (header, header_len) = encode_header(opcode, true, len);
        self.output = Some(WriteOp {
            id: OperationId {
                connection: self.id,
                sequence: 0,
                kind: OperationKind::Write,
            },
            header,
            header_len,
            cursor: 0,
            storage: WriteStorage::Control { bytes, len, close },
        });
    }
    fn parse_header(&mut self) {
        if self.header_len == 0 {
            self.frame_at = after(self.now, self.config.frame_timeout_ns);
        }
        while self.start < self.end {
            self.header[self.header_len] = self.input.as_ref().unwrap().as_ref()[self.start];
            self.header_len += 1;
            self.start += 1;
            if self.header_len < 2 {
                continue;
            }
            let first = self.header[0];
            let marker = self.header[1] & 127;
            let opcode = first & 15;
            if first & 0x70 != 0
                || self.header[1] & 128 == 0
                || !matches!(opcode, 0 | 1 | 2 | 8 | 9 | 10)
                || (opcode >= 8 && (first & 128 == 0 || marker > 125))
            {
                self.fail(Failure::Protocol);
                return;
            }
            let extra = match marker {
                126 => 2,
                127 => 8,
                _ => 0,
            };
            if self.header_len != 2 + extra + 4 {
                continue;
            }
            let fin = first & 128 != 0;
            let opcode = first & 15;
            let length = match marker {
                126 => u64::from(u16::from_be_bytes([self.header[2], self.header[3]])),
                127 => u64::from_be_bytes(self.header[2..10].try_into().unwrap()),
                value => u64::from(value),
            };
            if first & 0x70 != 0
                || self.header[1] & 128 == 0
                || !matches!(opcode, 0 | 1 | 2 | 8 | 9 | 10)
                || (marker == 126 && length < 126)
                || (marker == 127 && length < 65536)
                || length > i64::MAX as u64
                || (opcode >= 8 && (!fin || length > 125))
            {
                self.fail(Failure::Protocol);
                return;
            }
            if opcode < 8 && length > self.config.max_frame_bytes {
                self.fail(Failure::Limit);
                return;
            }
            if opcode < 8 && !self.is_closing() {
                if opcode == 0 {
                    let Some(incoming) = &mut self.incoming else {
                        self.fail(Failure::Protocol);
                        return;
                    };
                    let Some(fragments) = incoming.fragments.checked_add(1) else {
                        self.fail(Failure::Limit);
                        return;
                    };
                    incoming.fragments = fragments;
                    let Some(total) = incoming.info.length.checked_add(length) else {
                        self.fail(Failure::Limit);
                        return;
                    };
                    incoming.info.length = total;
                } else {
                    if self.incoming.is_some() {
                        self.fail(Failure::Protocol);
                        return;
                    }
                    let Ok(sequence) = self.sequence() else {
                        self.abort(Failure::SequenceExhausted);
                        return;
                    };
                    let info = MessageInfo {
                        id: MessageId {
                            connection: self.id,
                            sequence,
                        },
                        kind: if opcode == 1 {
                            MessageKind::Text
                        } else {
                            MessageKind::Binary
                        },
                        length,
                    };
                    self.started = Some(MessageInfo { length: 0, ..info });
                    self.incoming = Some(Incoming {
                        info,
                        utf8: Utf8::default(),
                        fragments: 1,
                    });
                    self.message_at = after(self.now, self.config.message_timeout_ns);
                }
                let incoming = self.incoming.as_ref().unwrap();
                if incoming.info.length > self.config.max_message_bytes
                    || incoming.fragments > self.config.max_fragments
                {
                    self.fail(Failure::Limit);
                    return;
                }
            }
            self.frame = Some(Frame {
                opcode,
                fin,
                remaining: length,
                offset: 0,
                mask: self.header[2 + extra..6 + extra].try_into().unwrap(),
                control: [0; 125],
                control_len: 0,
            });
            self.header_len = 0;
            return;
        }
    }
    fn parse_payload(&mut self) -> Option<ChunkOp<B>> {
        let closing = self.is_closing();
        let frame = self.frame.as_mut().unwrap();
        let count = frame.remaining.min((self.end - self.start) as u64) as usize;
        let range = self.start..self.start + count;
        let bytes = &mut self.input.as_mut().unwrap().as_mut()[range.clone()];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte ^= frame.mask[((frame.offset + index as u64) & 3) as usize];
        }
        frame.offset += count as u64;
        frame.remaining -= count as u64;
        self.start += count;
        if frame.opcode >= 8 {
            frame.control[frame.control_len..frame.control_len + count].copy_from_slice(bytes);
            frame.control_len += count;
            return None;
        }
        if closing {
            return None;
        }
        let incoming = self.incoming.as_mut().unwrap();
        if incoming.info.kind == MessageKind::Text && !incoming.utf8.feed(bytes) {
            self.fail(Failure::InvalidUtf8);
            return None;
        }
        let message = incoming.info.id;
        let id = self.operation(OperationKind::Chunk)?;
        self.chunk_live = Some(id);
        Some(ChunkOp {
            id,
            message,
            buffer: self.input.take().unwrap(),
            range,
        })
    }
    fn finish_frame(&mut self) {
        let frame = self.frame.take().unwrap();
        self.frame_at = None;
        match frame.opcode {
            0..=2 if frame.fin && !self.is_closing() => {
                let incoming = self.incoming.take().unwrap();
                self.message_at = None;
                if incoming.info.kind == MessageKind::Text && incoming.utf8.remaining != 0 {
                    self.fail(Failure::InvalidUtf8);
                } else {
                    self.finished = Some(incoming.info);
                }
            }
            8 => {
                if frame.control_len == 1 {
                    self.fail(Failure::Protocol);
                    return;
                }
                let reason = CloseReason {
                    bytes: frame.control,
                    len: frame.control_len as u8,
                };
                if reason
                    .code()
                    .is_some_and(|code| !crate::types::valid_close_code(code))
                {
                    self.fail(Failure::Protocol);
                    return;
                }
                if frame.control_len >= 2 && std::str::from_utf8(&reason.payload()[2..]).is_err() {
                    self.fail(Failure::InvalidUtf8);
                    return;
                }
                if self.peer_close.is_none() {
                    self.peer_close = Some(reason);
                    self.peer_notice = true;
                }
                let reply = if reason.code() == Some(1010) {
                    CloseReason::new(1000, "").unwrap()
                } else {
                    reason
                };
                self.begin_close(reply);
            }
            9 if self.peer_close.is_none() => self.pong = Some((frame.control, frame.control_len)),
            _ => {}
        }
    }
}

fn encode_header(opcode: u8, fin: bool, length: usize) -> ([u8; 10], usize) {
    let mut header = [0; 10];
    header[0] = opcode | if fin { 128 } else { 0 };
    let size = if length < 126 {
        header[1] = length as u8;
        2
    } else if length <= u16::MAX as usize {
        header[1] = 126;
        header[2..4].copy_from_slice(&(length as u16).to_be_bytes());
        4
    } else {
        header[1] = 127;
        header[2..10].copy_from_slice(&(length as u64).to_be_bytes());
        10
    };
    (header, size)
}
