use crate::operations::{Outgoing, WriteStorage};
use crate::*;
use kimojio_fsm_http1::Handoff;

#[path = "state.rs"]
mod state;
use state::*;

#[path = "coordinator.rs"]
mod coordinator;

#[derive(Default)]
#[cfg_attr(test, derive(Debug))]
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

#[cfg_attr(test, derive(Debug))]
struct Incoming {
    info: MessageInfo,
    utf8: Utf8,
    fragments: u64,
}
#[cfg_attr(test, derive(Debug))]
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
#[cfg_attr(test, derive(Debug))]
pub struct Server<B, W = B> {
    id: ConnectionId,
    config: Config,
    sequence: u64,
    now: Tick,
    receive: ReceiveStorage<B>,
    start: usize,
    end: usize,
    read: IoState,
    write: IoState,
    output: Option<WriteOp<W>>,
    tx: Transmit<W>,
    rx: Receive,
    incoming: IncomingState,
    pong: Option<([u8; 125], usize)>,
    lifecycle: Lifecycle,
    peer: PeerClose,
    failure: Option<Failure>,
    timers: Timers,
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
            receive: ReceiveStorage::Available(buffer),
            start,
            end,
            read: IoState::Idle,
            write: IoState::Idle,
            output: None,
            tx: Transmit::Idle,
            rx: Receive::Header(HeaderState::default()),
            incoming: IncomingState::Idle,
            pong: None,
            lifecycle: Lifecycle::Open,
            peer: PeerClose::Absent,
            failure: None,
            timers: Timers {
                idle: idle_at,
                frame: None,
                message: None,
                write: None,
                close: None,
                armed: None,
                notification: Notification::Delivered,
                sequence: 0,
            },
        }
    }
    pub fn connection(&self) -> ConnectionId {
        self.id
    }
    pub fn can_send(&self) -> bool {
        matches!(self.tx, Transmit::Idle) && self.lifecycle.open()
    }
    pub fn is_closing(&self) -> bool {
        !self.lifecycle.open()
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
        self.tx = Transmit::Pending(Outgoing {
            id,
            offset: command.range.start,
            command,
            accepted: 0,
            acceptance: Acceptance::Exact,
        });
        self.timers.write = after(self.now, self.config.write_timeout_ns);
        self.assert_invariants();
        Ok(id)
    }
    pub fn close(&mut self, reason: CloseReason) -> Result<(), CommandError> {
        if reason.code() == Some(1010) {
            return Err(CommandError::InvalidClose);
        }
        if self.is_closing() {
            return Err(CommandError::InvalidState);
        }
        self.begin_close(reason);
        self.assert_invariants();
        Ok(())
    }
    fn begin_close(&mut self, reason: CloseReason) {
        if self.lifecycle.open() {
            self.lifecycle = Lifecycle::Closing(LocalClose::Pending(reason));
            self.timers.close = after(self.now, self.config.close_timeout_ns);
        }
        self.incoming = IncomingState::Idle;
        self.timers.message = None;
        if self.peer.reason().is_some() || self.failure.is_some() {
            self.pong = None;
        }
        self.settle_handshake();
    }
    /// Reports application/source failure. No outgoing producer streaming exists.
    pub fn fail_source(&mut self) {
        self.fail(Failure::Application);
        self.assert_invariants();
    }
    pub fn abort(&mut self, failure: Failure) {
        self.failure.get_or_insert(failure);
        self.lifecycle.terminate();
        self.incoming = IncomingState::Idle;
        self.pong = None;
        self.assert_invariants();
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
        self.lifecycle.terminating()
    }
    fn settle_handshake(&mut self) {
        if self.lifecycle.sent() && (self.peer.reason().is_some() || self.failure.is_some()) {
            self.lifecycle.terminate();
        }
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
        let c = self.reject(completion.op.id, self.read.raw(), completion)?;
        if c.result.is_ok_and(|n| n > c.op.range.len()) {
            return Err(Rejected {
                reason: RejectReason::InvalidCount,
                value: c,
            });
        }
        self.read.complete();
        assert!(matches!(self.receive, ReceiveStorage::Reading));
        self.receive = ReceiveStorage::Available(c.op.buffer);
        match c.result {
            Ok(n) if n > 0 => {
                self.start = c.op.range.start;
                self.end = c.op.range.start + n;
                self.timers.idle = after(self.now, self.config.idle_timeout_ns);
            }
            _ if self.terminating() || self.failure.is_some() => {}
            Ok(_) => self.abort(Failure::UnexpectedEof),
            Err(IoError {
                kind: IoErrorKind::WouldBlock,
                ..
            }) => self.read = IoState::NeedsReadiness,
            Err(IoError {
                kind: IoErrorKind::Interrupted,
                ..
            }) => {}
            Err(error) => self.abort(Failure::Transport(error)),
        }
        self.assert_invariants();
        Ok(())
    }
    pub fn release_chunk(
        &mut self,
        completion: ChunkCompletion<B>,
    ) -> Result<(), Rejected<ChunkCompletion<B>>> {
        let c = self.reject(completion.op.id, self.receive.lease(), completion)?;
        self.receive = ReceiveStorage::Available(c.op.buffer);
        self.assert_invariants();
        Ok(())
    }
    #[allow(clippy::result_large_err)] // Rejection returns the original inline operation without allocation.
    pub fn complete_write(
        &mut self,
        completion: WriteCompletion<W>,
    ) -> Result<(), Rejected<WriteCompletion<W>>> {
        let c = self.reject(completion.op.id, self.write.raw(), completion)?;
        if c.result.is_ok_and(|n| n > c.op.remaining()) {
            return Err(Rejected {
                reason: RejectReason::InvalidCount,
                value: c,
            });
        }
        self.write.complete();
        let mut op = c.op;
        match c.result {
            Ok(n) if n > 0 => {
                let before = op.cursor.saturating_sub(op.header_len);
                op.cursor += n;
                let accepted = op.cursor.saturating_sub(op.header_len) - before;
                if let WriteStorage::Data { outgoing, .. } = &mut op.storage {
                    outgoing.accepted += accepted;
                }
                self.timers.write = after(self.now, self.config.write_timeout_ns);
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
                                self.tx = Transmit::Pending(outgoing);
                            }
                        }
                        WriteStorage::Control { close: true, .. } => {
                            match &mut self.lifecycle {
                                Lifecycle::Closing(local) => *local = LocalClose::Sent,
                                Lifecycle::Terminating { sent, .. } => *sent = true,
                                _ => unreachable!("a close write requires closing authority"),
                            }
                            self.settle_handshake();
                        }
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
                self.write = IoState::NeedsReadiness;
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
        self.assert_invariants();
        Ok(())
    }
    fn finish_outgoing(&mut self, outgoing: Outgoing<W>, result: Result<(), Failure>) {
        assert!(matches!(self.tx, Transmit::Framed));
        self.tx = Transmit::Receipt(MessageSent {
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
            Direction::Read => self.read.readiness(),
            Direction::Write => self.write.readiness(),
        };
        let c = self.reject(completion.op.id, expected, completion)?;
        let cancelled = match c.op.direction {
            Direction::Read => self.read.complete(),
            Direction::Write => self.write.complete(),
        };
        if let Err(error) = c.result
            && !self.terminating()
            && !cancelled
        {
            self.abort(Failure::Transport(error));
        }
        self.assert_invariants();
        Ok(())
    }
    pub fn complete_close(
        &mut self,
        completion: CloseCompletion,
    ) -> Result<(), Rejected<CloseCompletion>> {
        let c = self.reject(
            completion.op.id,
            self.lifecycle.close_operation(),
            completion,
        )?;
        self.lifecycle = Lifecycle::Closed {
            sent: self.lifecycle.sent(),
            notification: Notification::Pending,
        };
        if let Err(error) = c.result {
            self.failure.get_or_insert(Failure::Transport(error));
        }
        self.assert_invariants();
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
        if self.timers.armed != Some(deadline) {
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
        let at = self.deadline_at();
        if self.timers.armed.map(|d| d.at) != at {
            if let Some(sequence) = self.timers.sequence.checked_add(1) {
                self.timers.sequence = sequence;
                self.timers.armed = at.map(|at| Deadline {
                    connection: self.id,
                    sequence,
                    at,
                });
            } else {
                self.abort(Failure::SequenceExhausted);
                self.timers.armed = None;
            }
            self.timers.notification = Notification::Pending;
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
        if matches!(&self.rx, Receive::Header(HeaderState { len: 0, .. })) {
            self.timers.frame = after(self.now, self.config.frame_timeout_ns);
        }
        while self.start < self.end {
            let Receive::Header(header) = &mut self.rx else {
                unreachable!("header parsing requires header state");
            };
            header.bytes[header.len] = self.receive.buffer().unwrap().as_ref()[self.start];
            header.len += 1;
            self.start += 1;
            if header.len < 2 {
                continue;
            }
            let first = header.bytes[0];
            let marker = header.bytes[1] & 127;
            let opcode = first & 15;
            if first & 0x70 != 0
                || header.bytes[1] & 128 == 0
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
            if header.len != 2 + extra + 4 {
                continue;
            }
            let fin = first & 128 != 0;
            let opcode = first & 15;
            let length = match marker {
                126 => u64::from(u16::from_be_bytes([header.bytes[2], header.bytes[3]])),
                127 => u64::from_be_bytes(header.bytes[2..10].try_into().unwrap()),
                value => u64::from(value),
            };
            if first & 0x70 != 0
                || header.bytes[1] & 128 == 0
                || !matches!(opcode, 0 | 1 | 2 | 8 | 9 | 10)
                || (marker == 126 && length < 126)
                || (marker == 127 && length < 65536)
                || length > i64::MAX as u64
                || (opcode >= 8 && (!fin || length > 125))
            {
                self.fail(Failure::Protocol);
                return;
            }
            let mask = header.bytes[2 + extra..6 + extra].try_into().unwrap();
            if opcode < 8 && length > self.config.max_frame_bytes {
                self.fail(Failure::Limit);
                return;
            }
            if opcode < 8 && !self.is_closing() {
                if opcode == 0 {
                    let Some(incoming) = self.incoming.active_mut() else {
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
                    if !matches!(self.incoming, IncomingState::Idle) {
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
                    self.incoming = IncomingState::Active {
                        message: Incoming {
                            info,
                            utf8: Utf8::default(),
                            fragments: 1,
                        },
                        notification: Notification::Pending,
                    };
                    self.timers.message = after(self.now, self.config.message_timeout_ns);
                }
                let incoming = self.incoming.active().unwrap();
                if incoming.info.length > self.config.max_message_bytes
                    || incoming.fragments > self.config.max_fragments
                {
                    self.fail(Failure::Limit);
                    return;
                }
            }
            self.rx = Receive::Payload(Frame {
                opcode,
                fin,
                remaining: length,
                offset: 0,
                mask,
                control: [0; 125],
                control_len: 0,
            });
            return;
        }
    }
    fn parse_payload(&mut self) -> Option<ChunkOp<B>> {
        let closing = self.is_closing();
        let Receive::Payload(frame) = &mut self.rx else {
            unreachable!("payload parsing requires a complete header");
        };
        let count = frame.remaining.min((self.end - self.start) as u64) as usize;
        let range = self.start..self.start + count;
        let bytes = &mut self.receive.buffer_mut().unwrap().as_mut()[range.clone()];
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
        let incoming = self.incoming.active_mut().unwrap();
        if incoming.info.kind == MessageKind::Text && !incoming.utf8.feed(bytes) {
            self.fail(Failure::InvalidUtf8);
            return None;
        }
        let message = incoming.info.id;
        let id = self.operation(OperationKind::Chunk)?;
        Some(ChunkOp {
            id,
            message,
            buffer: self.receive.take(ReceiveStorage::Leased(id)),
            range,
        })
    }
    fn finish_frame(&mut self) {
        let Receive::Payload(frame) =
            std::mem::replace(&mut self.rx, Receive::Header(HeaderState::default()))
        else {
            unreachable!("only a payload state completes a frame");
        };
        self.timers.frame = None;
        match frame.opcode {
            0..=2 if frame.fin && !self.is_closing() => {
                let incoming = self.incoming.finish();
                self.timers.message = None;
                if incoming.info.kind == MessageKind::Text && incoming.utf8.remaining != 0 {
                    self.fail(Failure::InvalidUtf8);
                } else {
                    self.incoming = IncomingState::Finished(incoming.info);
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
                if self.peer.reason().is_none() {
                    self.peer = PeerClose::Pending(reason);
                }
                let reply = if reason.code() == Some(1010) {
                    CloseReason::new(1000, "").unwrap()
                } else {
                    reason
                };
                self.begin_close(reply);
            }
            9 if self.peer.reason().is_none() => {
                self.pong = Some((frame.control, frame.control_len))
            }
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
