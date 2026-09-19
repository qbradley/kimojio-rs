use super::*;

#[cfg(test)]
#[path = "coordinator_tests.rs"]
mod tests;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Transition {
    RefreshTimers,
    Deadline,
    Receipt,
    PeerNotice,
    DiscardFrame,
    ReturnMessage,
    CancelRead,
    CancelWrite,
    Close,
    Closed,
    WriteReady,
    PrepareClose,
    PreparePong,
    PrepareData,
    Write,
    MessageStarted,
    MessageFinished,
    FinishFrame,
    ParseHeader,
    ParsePayload,
    ReadReady,
    Read,
}

impl<B: Buffer, W: AsRef<[u8]>> Server<B, W> {
    fn write_work(&self) -> bool {
        self.write.raw().is_some() || self.output.is_some() || self.tx.pending()
    }

    pub(super) fn deadline_at(&self) -> Option<Tick> {
        match self.lifecycle {
            Lifecycle::Open => [
                self.timers.idle,
                self.timers.frame,
                self.timers.message,
                self.timers.write,
            ]
            .into_iter()
            .flatten()
            .min(),
            Lifecycle::Closing(_) => [self.timers.close, self.timers.write]
                .into_iter()
                .flatten()
                .min(),
            Lifecycle::Terminating { .. } | Lifecycle::Closed { .. } => None,
        }
    }

    fn next_transition(&self) -> Option<Transition> {
        if (self.timers.write.is_some() && !self.write_work())
            || self.timers.armed.map(|d| d.at) != self.deadline_at()
        {
            return Some(Transition::RefreshTimers);
        }
        if self.timers.notification == Notification::Pending {
            return Some(Transition::Deadline);
        }
        if matches!(self.tx, Transmit::Receipt(_)) {
            return Some(Transition::Receipt);
        }
        if matches!(self.peer, PeerClose::Pending(_)) {
            return Some(Transition::PeerNotice);
        }
        match self.lifecycle {
            Lifecycle::Open => self
                .transmit_transition()
                .or_else(|| self.receive_transition()),
            Lifecycle::Closing(_) => self.closing_transition(),
            Lifecycle::Terminating {
                transport: TransportClose::Settling,
                ..
            } => self.termination_transition(),
            Lifecycle::Terminating {
                transport: TransportClose::Awaiting(_),
                ..
            } => None,
            Lifecycle::Closed {
                notification: Notification::Pending,
                ..
            } => Some(Transition::Closed),
            Lifecycle::Closed {
                notification: Notification::Delivered,
                ..
            } => None,
        }
    }

    fn closing_transition(&self) -> Option<Transition> {
        // Never splice close bytes into a partially accepted frame.
        if self.output.as_ref().is_some_and(|op| {
            op.cursor == 0
                && match op.storage {
                    WriteStorage::Data { .. } => true,
                    WriteStorage::Control { close, .. } => !close && self.input_stopped(),
                }
        }) {
            return Some(Transition::DiscardFrame);
        }
        if self.tx.pending() {
            return Some(Transition::ReturnMessage);
        }
        if self.input_stopped() && matches!(self.read, IoState::InFlight(_)) {
            return Some(Transition::CancelRead);
        }
        self.transmit_transition().or_else(|| {
            if self.input_stopped() {
                None
            } else {
                self.receive_transition()
            }
        })
    }

    fn termination_transition(&self) -> Option<Transition> {
        if self.output.is_some() {
            return Some(Transition::DiscardFrame);
        }
        if self.tx.pending() {
            return Some(Transition::ReturnMessage);
        }
        if matches!(self.read, IoState::InFlight(_)) {
            return Some(Transition::CancelRead);
        }
        if matches!(self.write, IoState::InFlight(_)) {
            return Some(Transition::CancelWrite);
        }
        self.io_settled().then_some(Transition::Close)
    }

    fn input_stopped(&self) -> bool {
        self.failure.is_some() || self.peer.reason().is_some()
    }

    fn transmit_transition(&self) -> Option<Transition> {
        if self.write.operation().is_some() {
            return None;
        }
        if self.write == IoState::NeedsReadiness {
            return Some(Transition::WriteReady);
        }
        if self.output.is_some() {
            return Some(Transition::Write);
        }
        if matches!(self.lifecycle, Lifecycle::Closing(LocalClose::Pending(_))) {
            return Some(Transition::PrepareClose);
        }
        if self.pong.is_some() {
            return Some(Transition::PreparePong);
        }
        (self.lifecycle.open() && self.tx.pending()).then_some(Transition::PrepareData)
    }

    fn receive_transition(&self) -> Option<Transition> {
        match self.incoming {
            IncomingState::Active {
                notification: Notification::Pending,
                ..
            } => {
                return Some(Transition::MessageStarted);
            }
            IncomingState::Finished(_) => return Some(Transition::MessageFinished),
            _ => {}
        }
        self.receive.buffer()?;
        match &self.rx {
            Receive::Payload(frame) if frame.remaining == 0 => Some(Transition::FinishFrame),
            Receive::Header(_) if self.start < self.end => Some(Transition::ParseHeader),
            Receive::Payload(_) if self.start < self.end => Some(Transition::ParsePayload),
            _ if self.read == IoState::NeedsReadiness => Some(Transition::ReadReady),
            _ if self.read == IoState::Idle => Some(Transition::Read),
            _ => None,
        }
    }

    /// Drains finite buffered work. Scheduling budgets belong to the caller.
    pub fn next<P: Ports<B, W>>(&mut self, ports: &mut P) -> Option<P::Output> {
        loop {
            self.assert_invariants();
            let transition = self.next_transition()?;
            let output = self.advance(transition, ports);
            self.assert_invariants();
            if output.is_some() {
                return output;
            }
        }
    }

    fn advance<P: Ports<B, W>>(
        &mut self,
        transition: Transition,
        ports: &mut P,
    ) -> Option<P::Output> {
        match transition {
            Transition::RefreshTimers => {
                if !self.write_work() {
                    self.timers.write = None;
                }
                self.refresh_deadline();
                None
            }
            Transition::Deadline => {
                self.timers.notification = Notification::Delivered;
                ports.deadline_changed(self.timers.armed)
            }
            Transition::Receipt => ports.message_sent(self.tx.take_receipt()),
            Transition::PeerNotice => {
                let PeerClose::Pending(reason) = self.peer else {
                    unreachable!()
                };
                self.peer = PeerClose::Reported(reason);
                ports.peer_closed(reason)
            }
            Transition::DiscardFrame => {
                if let WriteStorage::Data { outgoing, .. } = self.output.take().unwrap().storage {
                    self.finish_outgoing(outgoing, Err(self.failure.unwrap_or(Failure::Closing)));
                }
                None
            }
            Transition::ReturnMessage => {
                let outgoing = self.tx.take_pending();
                self.finish_outgoing(outgoing, Err(self.failure.unwrap_or(Failure::Closing)));
                None
            }
            Transition::CancelRead => ports.cancel(CancelOp {
                target: self.read.cancel(),
            }),
            Transition::CancelWrite => ports.cancel(CancelOp {
                target: self.write.cancel(),
            }),
            Transition::Close => {
                self.read = IoState::Idle;
                self.write = IoState::Idle;
                let id = OperationId {
                    connection: self.id,
                    sequence: u64::MAX,
                    kind: OperationKind::Close,
                };
                self.lifecycle = Lifecycle::Terminating {
                    sent: self.lifecycle.sent(),
                    transport: TransportClose::Awaiting(id),
                };
                ports.close(CloseOp { id })
            }
            Transition::Closed => {
                let sent = self.lifecycle.sent();
                self.lifecycle = Lifecycle::Closed {
                    sent,
                    notification: Notification::Delivered,
                };
                ports.closed(ConnectionResult {
                    result: self.failure.map_or(Ok(()), Err),
                    peer_close: self.peer.reason(),
                    clean: self.failure.is_none() && self.peer.reason().is_some() && sent,
                })
            }
            Transition::WriteReady => {
                let id = self.operation(OperationKind::Writable)?;
                self.write.issue(id);
                ports.readiness(ReadinessOp {
                    id,
                    direction: Direction::Write,
                })
            }
            Transition::PrepareClose => {
                let Lifecycle::Closing(LocalClose::Pending(reason)) = self.lifecycle else {
                    unreachable!()
                };
                self.control_output(8, reason.bytes, usize::from(reason.len), true);
                None
            }
            Transition::PreparePong => {
                let (bytes, len) = self.pong.take().unwrap();
                self.control_output(10, bytes, len, false);
                None
            }
            Transition::PrepareData => {
                self.prepare_data();
                None
            }
            Transition::Write => {
                let id = self.operation(OperationKind::Write)?;
                let mut op = self.output.take().unwrap();
                op.id = id;
                self.write.issue(id);
                if self.timers.write.is_none() {
                    self.timers.write = after(self.now, self.config.write_timeout_ns);
                }
                ports.write(op)
            }
            Transition::MessageStarted => {
                let IncomingState::Active {
                    message,
                    notification,
                } = &mut self.incoming
                else {
                    unreachable!()
                };
                *notification = Notification::Delivered;
                ports.message_started(MessageInfo {
                    length: 0,
                    ..message.info
                })
            }
            Transition::MessageFinished => {
                let IncomingState::Finished(info) =
                    std::mem::replace(&mut self.incoming, IncomingState::Idle)
                else {
                    unreachable!()
                };
                ports.message_finished(info)
            }
            Transition::FinishFrame => {
                self.finish_frame();
                None
            }
            Transition::ParseHeader => {
                self.parse_header();
                None
            }
            Transition::ParsePayload => self.parse_payload().and_then(|chunk| ports.chunk(chunk)),
            Transition::ReadReady => {
                let id = self.operation(OperationKind::Readable)?;
                self.read.issue(id);
                ports.readiness(ReadinessOp {
                    id,
                    direction: Direction::Read,
                })
            }
            Transition::Read => {
                let id = self.operation(OperationKind::Read)?;
                let buffer = self.receive.take(ReceiveStorage::Reading);
                let len = buffer.as_ref().len();
                self.start = 0;
                self.end = 0;
                self.read.issue(id);
                ports.read(ReadOp {
                    id,
                    buffer,
                    range: 0..len,
                })
            }
        }
    }

    fn prepare_data(&mut self) {
        let outgoing = self.tx.take_pending();
        let start = outgoing.offset;
        let end = start
            .saturating_add(self.config.outgoing_frame_bytes)
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

    fn io_settled(&self) -> bool {
        self.read.operation().is_none()
            && self.write.operation().is_none()
            && self.receive.lease().is_none()
    }

    pub(super) fn assert_invariants(&self) {
        debug_assert_eq!(
            matches!(self.receive, ReceiveStorage::Reading),
            self.read.raw().is_some()
        );
        debug_assert!(self.receive.lease().is_none() || self.read.operation().is_none());
        if let Some(buffer) = self.receive.buffer() {
            debug_assert!(self.start <= self.end && self.end <= buffer.as_ref().len());
        }
        let queued_data = self
            .output
            .as_ref()
            .is_some_and(|op| matches!(op.storage, WriteStorage::Data { .. }));
        if matches!(self.tx, Transmit::Framed) {
            debug_assert_ne!(queued_data, self.write.raw().is_some());
        } else {
            debug_assert!(!queued_data);
        }
        if self.is_closing() {
            debug_assert!(matches!(self.incoming, IncomingState::Idle));
        } else {
            debug_assert!(self.failure.is_none() && self.peer.reason().is_none());
        }
        if matches!(self.lifecycle, Lifecycle::Closing(LocalClose::Sent)) {
            debug_assert!(!self.input_stopped());
        }
        if let Receive::Header(header) = &self.rx {
            debug_assert!(header.len < header.bytes.len() || self.failure.is_some());
        }
        if matches!(
            self.lifecycle,
            Lifecycle::Terminating {
                transport: TransportClose::Awaiting(_),
                ..
            } | Lifecycle::Closed { .. }
        ) {
            debug_assert!(self.io_settled());
            debug_assert_eq!(self.read, IoState::Idle);
            debug_assert_eq!(self.write, IoState::Idle);
            debug_assert!(self.output.is_none());
            debug_assert!(matches!(self.tx, Transmit::Idle));
            debug_assert!(self.timers.armed.is_none());
        }
    }
}
