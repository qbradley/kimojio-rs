use super::*;

/// Deferred cross-direction policy, recorded at a semantic completion boundary.
/// A role can produce only one of these kinds, so repeated records coalesce.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Boundary {
    None,
    IncomingEnded,
    OutgoingSettled,
}

#[cfg(test)]
#[path = "coordinator_tests.rs"]
mod tests;

/// Control-flow labels only: operations and borrowed metadata are never stored here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Transition {
    Deadline,
    Receipt,
    Closed,
    Coordinate,
    RevokeUpgrade,
    SourceFinished,
    CancelRead,
    CancelWrite,
    DiscardOutput,
    ReturnBody,
    SettleUpload,
    FinishAbortedExchange,
    Close,
    Continue,
    Write,
    PrepareBody,
    BeginClosing,
    IncomingFinished,
    UpgradeReady,
    RetireExchange,
    Demand,
    Metadata,
    Body,
    RejectBody,
    Eof,
    Read,
}

impl<B: Buffer, W: AsRef<[u8]>, const SERVER: bool> Core<B, W, SERVER> {
    pub(super) fn incoming_ended(&mut self) {
        if !SERVER && matches!(self.lifecycle, Lifecycle::Http(_)) {
            self.boundary = Boundary::IncomingEnded;
        }
    }

    pub(super) fn settle_transmit(&mut self) {
        self.tx.settle();
        if SERVER && matches!(self.lifecycle, Lifecycle::Http(_)) {
            self.boundary = Boundary::OutgoingSettled;
        }
    }

    fn coordinate(&mut self) {
        match std::mem::replace(&mut self.boundary, Boundary::None) {
            Boundary::OutgoingSettled => {
                if !self.exchange.as_ref().unwrap().consume_request
                    && !matches!(self.rx, Rx::Done | Rx::Paused)
                {
                    self.rx = Rx::Paused;
                    self.credit = 0;
                    self.close_after = true;
                }
            }
            Boundary::IncomingEnded => {
                if !self.tx.settled() && self.tx.framing() != Framing::Empty {
                    self.stop_upload();
                    if self.update_deadline(self.timers.phase, None).is_err() {
                        self.fail(Failure::SequenceExhausted);
                    }
                }
            }
            Boundary::None => unreachable!("coordination requires a recorded boundary"),
        }
    }

    pub(super) fn stop_upload(&mut self) {
        self.tx.stop_upload();
        self.release_continue();
        self.close_after = true;
        self.timers.upload_at = None;
    }

    /// Selection is pure. Each lifecycle admits only its own transition vocabulary.
    fn next_transition(&self) -> Option<Transition> {
        if self.lifecycle == Lifecycle::HandedOff {
            return None;
        }
        if self.timers.notification == Notification::Pending {
            return Some(Transition::Deadline);
        }
        if self.body_result.is_some() {
            return Some(Transition::Receipt);
        }
        match self.lifecycle {
            Lifecycle::Http(_) => self.http_transition(),
            Lifecycle::ErrorResponse => self.error_transition(),
            Lifecycle::Closing(Closing::Settling) => self.closing_transition(),
            Lifecycle::Closing(Closing::AwaitingClose(_)) => None,
            Lifecycle::Upgrade(Upgrade::Revoked) => Some(Transition::RevokeUpgrade),
            Lifecycle::Upgrade(Upgrade::Handshake) => self.upgrade_transition(),
            Lifecycle::Upgrade(Upgrade::Ready) => None,
            Lifecycle::Closed(Notification::Pending) => Some(Transition::Closed),
            Lifecycle::Closed(Notification::Delivered) | Lifecycle::HandedOff => None,
        }
    }

    fn source_transition(&self) -> Option<Transition> {
        let exchange = self.exchange.as_ref()?;
        (exchange.source_notification == Notification::Pending
            && (self.tx.source_finished() || self.failure.is_some()))
        .then_some(Transition::SourceFinished)
    }

    fn incoming_transition(&self) -> Option<Transition> {
        let exchange = self.exchange.as_ref()?;
        (self.rx == Rx::Done && exchange.incoming_notification == Notification::Pending)
            .then_some(Transition::IncomingFinished)
    }

    fn cancel_read_transition(&self) -> Option<Transition> {
        matches!(self.read, IoState::InFlight(_)).then_some(Transition::CancelRead)
    }

    fn release_output_transition(&self) -> Option<Transition> {
        if matches!(self.write, IoState::InFlight(_)) {
            Some(Transition::CancelWrite)
        } else if self.output.is_some() {
            Some(Transition::DiscardOutput)
        } else if self.pending_body.is_some() {
            Some(Transition::ReturnBody)
        } else {
            None
        }
    }

    fn closing_transition(&self) -> Option<Transition> {
        self.source_transition()
            .or_else(|| self.cancel_read_transition())
            .or_else(|| self.release_output_transition())
            .or_else(|| {
                if self.exchange.is_some() {
                    Some(Transition::FinishAbortedExchange)
                } else {
                    self.io_settled().then_some(Transition::Close)
                }
            })
    }

    fn error_transition(&self) -> Option<Transition> {
        self.source_transition()
            .or_else(|| self.cancel_read_transition())
            .or_else(|| self.transmit_transition())
            .or_else(|| self.tx.settled().then_some(Transition::BeginClosing))
    }

    fn upgrade_transition(&self) -> Option<Transition> {
        self.source_transition()
            .or_else(|| self.transmit_transition())
            .or_else(|| self.incoming_transition())
            .or_else(|| {
                (self.tx.settled() && self.io_settled()).then_some(Transition::UpgradeReady)
            })
    }

    fn http_transition(&self) -> Option<Transition> {
        if self.boundary != Boundary::None {
            return Some(Transition::Coordinate);
        }
        if let Some(step) = self.source_transition() {
            return Some(step);
        }
        if self.tx.stopped() {
            if let Some(step) = self.release_output_transition() {
                return Some(step);
            }
            if self.write.operation().is_none() && !self.tx.settled() {
                return Some(Transition::SettleUpload);
            }
        }
        if self.rx == Rx::Paused
            && let Some(step) = self.cancel_read_transition()
        {
            return Some(step);
        }
        if self.continue_due() {
            return Some(Transition::Continue);
        }

        // A buffered response can revoke upload authority. Resolve it before
        // selecting either a queued upload write or new producer demand.
        if !SERVER && self.rx == Rx::Head && self.start < self.end {
            return self.receive_transition();
        }
        self.transmit_transition()
            .or_else(|| self.incoming_transition())
            .or_else(|| {
                (self.exchange.is_some()
                    && self.tx.settled()
                    && matches!(self.rx, Rx::Done | Rx::Paused)
                    && self.io_settled())
                .then_some(Transition::RetireExchange)
            })
            .or_else(|| self.producer_transition())
            .or_else(|| self.receive_transition())
    }

    fn continue_due(&self) -> bool {
        SERVER
            && !self.tx.started()
            && self.credit != 0
            && self.start == self.end
            && !self.eof
            && !matches!(self.rx, Rx::Done | Rx::Paused)
            && self.write.operation().is_none()
            && self.output.is_none()
            && self
                .exchange
                .as_ref()
                .is_some_and(|exchange| exchange.expect)
    }

    fn transmit_transition(&self) -> Option<Transition> {
        if self.write.operation().is_some() {
            return None;
        }
        if self.output.is_some() {
            Some(Transition::Write)
        } else {
            self.pending_body.as_ref().map(|_| Transition::PrepareBody)
        }
    }

    fn producer_transition(&self) -> Option<Transition> {
        (self.tx.can_request_data()
            && !self.waiting_continue()
            && self.pending_body.is_none()
            && self.output.is_none()
            && self.write.operation().is_none())
        .then_some(Transition::Demand)
    }

    fn receive_transition(&self) -> Option<Transition> {
        self.receive.buffer()?;
        let active = SERVER || self.exchange.is_some();
        if self.start < self.end && active {
            return match self.rx {
                Rx::Eof if self.no_content => Some(Transition::RejectBody),
                Rx::AwaitingRequest | Rx::Head | Rx::Size | Rx::ChunkCrlf | Rx::Trailers => {
                    Some(Transition::Metadata)
                }
                Rx::Fixed(_) | Rx::Chunk(_) | Rx::Eof if self.credit != 0 => Some(Transition::Body),
                _ => None,
            };
        }
        if self.start != self.end {
            return None;
        }
        if self.eof {
            return (!matches!(self.rx, Rx::Done | Rx::Paused)).then_some(Transition::Eof);
        }
        let needs_input = match self.rx {
            Rx::AwaitingRequest | Rx::Head | Rx::Size | Rx::ChunkCrlf | Rx::Trailers => true,
            Rx::Fixed(_) | Rx::Chunk(_) | Rx::Eof => self.credit != 0 || self.no_content,
            Rx::Done | Rx::Paused => false,
        };
        (active && needs_input && self.read.operation().is_none()).then_some(Transition::Read)
    }

    pub(super) fn next<P: Ports<B, W>>(
        &mut self,
        ports: &mut P,
        head_callback: fn(&mut P, ExchangeId, ParsedHead<'_>) -> Option<P::Output>,
    ) -> Option<P::Output> {
        loop {
            self.assert_invariants();
            let transition = self.next_transition()?;
            if let Some(output) = self.advance(transition, ports, head_callback) {
                return Some(output);
            }
        }
    }

    /// None means the selected transition committed without yielding, never blocked.
    fn advance<P: Ports<B, W>>(
        &mut self,
        transition: Transition,
        ports: &mut P,
        head_callback: fn(&mut P, ExchangeId, ParsedHead<'_>) -> Option<P::Output>,
    ) -> Option<P::Output> {
        if let Some(failure) = self.failure
            && !self.failure_logged
        {
            self.failure_logged = true;
            self.log(ports, LogEvent::PrimaryFailure(failure));
        }
        match transition {
            Transition::Deadline => {
                self.timers.notification.take();
                self.log(ports, LogEvent::DeadlineChanged(self.timers.deadline()));
                ports.deadline_changed(self.timers.deadline())
            }
            Transition::Receipt => {
                let result = self.body_result.take().unwrap();
                self.log(
                    ports,
                    LogEvent::BodyReturned {
                        exchange: result.exchange,
                        body: result.id,
                        accepted: result.accepted,
                        acceptance: result.acceptance,
                        result: result.result,
                    },
                );
                ports.body_sent(result)
            }
            Transition::Closed => {
                self.lifecycle = Lifecycle::Closed(Notification::Delivered);
                self.log(ports, LogEvent::Closed(self.failure.map_or(Ok(()), Err)));
                ports.closed(self.failure.map_or(Ok(()), Err))
            }
            Transition::Coordinate => {
                self.coordinate();
                None
            }
            Transition::RevokeUpgrade => {
                self.fail(Failure::Cancelled);
                None
            }
            Transition::SourceFinished => {
                let exchange = self.exchange.as_mut().unwrap();
                exchange.source_notification.take();
                let id = exchange.id;
                self.log(ports, LogEvent::SourceFinished(id));
                ports.source_finished(id)
            }
            Transition::CancelRead => {
                let target = self.read.cancel().unwrap();
                self.count(Metric::Cancellations, 1);
                self.log(ports, LogEvent::CancellationRequested(target));
                ports.cancel(CancelOp { target })
            }
            Transition::CancelWrite => {
                let target = self.write.cancel().unwrap();
                self.count(Metric::Cancellations, 1);
                self.log(ports, LogEvent::CancellationRequested(target));
                ports.cancel(CancelOp { target })
            }
            Transition::DiscardOutput => {
                let op = self.output.take().unwrap();
                self.settle_write(op, Acceptance::Exact);
                None
            }
            Transition::ReturnBody => {
                let (id, command) = self.pending_body.take().unwrap();
                self.body_result = Some(BodySent {
                    exchange: command.exchange,
                    id,
                    buffer: command.buffer,
                    accepted: 0,
                    acceptance: Acceptance::Exact,
                    result: Err(self.failure.unwrap_or(if self.tx.stopped() {
                        Failure::EarlyResponse
                    } else {
                        Failure::Cancelled
                    })),
                });
                None
            }
            Transition::SettleUpload => {
                self.settle_transmit();
                None
            }
            Transition::FinishAbortedExchange => {
                let exchange = self.exchange.take().unwrap();
                let finished = ExchangeFinished {
                    exchange: exchange.id,
                    result: self.failure.map_or(Ok(()), Err),
                    reusable: false,
                };
                self.count(Metric::ExchangesRetired, 1);
                if finished.result.is_err() {
                    self.count(Metric::ExchangesFailed, 1);
                }
                self.log(ports, LogEvent::ExchangeFinished(finished));
                ports.exchange_finished(finished)
            }
            Transition::Close => {
                let id = OperationId {
                    connection: self.id,
                    sequence: u64::MAX,
                    kind: OperationKind::Close,
                };
                self.lifecycle.issue_close(id);
                self.log(ports, LogEvent::OperationIssued(id));
                ports.close(CloseOp { id })
            }
            Transition::Continue => {
                let id = self.exchange.as_ref().unwrap().id;
                if self
                    .inform(id, ResponseHead::new(100, "Continue", &[]))
                    .is_err()
                {
                    self.fail(Failure::Limit);
                }
                None
            }
            Transition::Write => self.issue_write(ports),
            Transition::PrepareBody => {
                self.prepare_body();
                None
            }
            Transition::BeginClosing => {
                self.lifecycle.begin_closing();
                self.boundary = Boundary::None;
                self.clear_deadlines();
                None
            }
            Transition::IncomingFinished => {
                let exchange = self.exchange.as_mut().unwrap();
                exchange.incoming_notification.take();
                let id = exchange.id;
                self.log(ports, LogEvent::IncomingFinished(id));
                ports.incoming_finished(id)
            }
            Transition::UpgradeReady => {
                self.lifecycle.notify_upgrade();
                self.clear_deadlines();
                self.log(
                    ports,
                    LogEvent::UpgradeReady(self.exchange.as_ref().unwrap().id),
                );
                ports.upgrade_ready(self.exchange.as_ref().unwrap().id)
            }
            Transition::RetireExchange => {
                let finished = self.retire_exchange();
                self.count(Metric::ExchangesRetired, 1);
                self.log(ports, LogEvent::ExchangeFinished(finished));
                ports.exchange_finished(finished)
            }
            Transition::Demand => {
                self.tx.request_data();
                self.log(
                    ports,
                    LogEvent::SendReady {
                        exchange: self.exchange.as_ref().unwrap().id,
                        capacity: self.send_capacity(),
                    },
                );
                ports.send_ready(self.exchange.as_ref().unwrap().id, self.send_capacity())
            }
            Transition::Metadata => self.receive_metadata(ports, head_callback),
            Transition::Body => self.deliver_body(ports),
            Transition::RejectBody => {
                self.fail(Failure::Protocol);
                None
            }
            Transition::Eof => {
                self.receive_eof();
                None
            }
            Transition::Read => self.issue_read(ports),
        }
    }

    fn issue_write<P: Ports<B, W>>(&mut self, ports: &mut P) -> Option<P::Output> {
        if self.write == IoState::NeedsReadiness {
            let id = self.operation(OperationKind::Writable)?;
            self.write.issue(id);
            self.log(ports, LogEvent::OperationIssued(id));
            ports.readiness(ReadinessOp {
                id,
                direction: Direction::Write,
            })
        } else {
            let id = self.operation(OperationKind::Write)?;
            let mut op = self.output.take().unwrap();
            op.id = id;
            self.write.issue(id);
            self.log(ports, LogEvent::OperationIssued(id));
            ports.write(op)
        }
    }

    fn issue_read<P: Ports<B, W>>(&mut self, ports: &mut P) -> Option<P::Output> {
        self.start = 0;
        self.end = 0;
        if self.read == IoState::NeedsReadiness {
            let id = self.operation(OperationKind::Readable)?;
            self.read.issue(id);
            self.log(ports, LogEvent::OperationIssued(id));
            ports.readiness(ReadinessOp {
                id,
                direction: Direction::Read,
            })
        } else {
            let id = self.operation(OperationKind::Read)?;
            let buffer = self.receive.read();
            let len = buffer.as_ref().len();
            self.read.issue(id);
            self.log(ports, LogEvent::OperationIssued(id));
            ports.read(ReadOp {
                id,
                buffer,
                range: 0..len,
            })
        }
    }

    fn prepare_body(&mut self) {
        let (body_id, command) = self.pending_body.take().unwrap();
        let mut prefix = [0; 24];
        let chunked = self.tx.framing() == Framing::Chunked;
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
    }

    fn retire_exchange(&mut self) -> ExchangeFinished {
        let exchange = self.exchange.take().unwrap();
        let reusable = !self.close_after
            && (SERVER || self.start == self.end)
            && !self.lifecycle.is_draining()
            && !self.eof
            && exchange.persistent
            && self.exchanges < self.config.max_requests;
        self.tx = Transmit::Idle;
        self.boundary = Boundary::None;
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
        self.incoming_connection_fields.clear();
        self.outgoing_connection_fields.clear();
        if reusable {
            self.rx = if SERVER {
                Rx::AwaitingRequest
            } else {
                Rx::Head
            };
            if self
                .set_deadline(TimerPhase::Idle, self.config.idle_timeout_ns)
                .is_err()
            {
                self.fail(Failure::SequenceExhausted);
            }
        } else {
            self.lifecycle.begin_closing();
            self.clear_deadlines();
        }
        ExchangeFinished {
            exchange: exchange.id,
            result: Ok(()),
            reusable,
        }
    }

    fn send_capacity(&self) -> usize {
        let mut max = match self.tx.framing() {
            Framing::Fixed(left) => usize::try_from(left)
                .unwrap_or(usize::MAX)
                .min(self.config.max_buffer_bytes),
            _ => self.config.max_buffer_bytes,
        };
        max = max.min(
            usize::try_from(self.config.max_body_bytes - self.outgoing_bytes).unwrap_or(usize::MAX),
        );
        if self.tx.framing() == Framing::Chunked {
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
        max
    }

    fn receive_metadata<P: Ports<B, W>>(
        &mut self,
        ports: &mut P,
        head_callback: fn(&mut P, ExchangeId, ParsedHead<'_>) -> Option<P::Output>,
    ) -> Option<P::Output> {
        if self.rx == Rx::AwaitingRequest && self.start_request_head().is_err() {
            self.fail(Failure::SequenceExhausted);
            return None;
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
        let deadline_pending = self.timers.notification == Notification::Pending;
        loop {
            let byte = self.receive.buffer().unwrap().as_ref()[self.start];
            self.start += 1;
            if (byte == b'\n' && self.head.last() != Some(&b'\r'))
                || (self.head.last() == Some(&b'\r') && byte != b'\n')
            {
                self.fail(Failure::Protocol);
                return None;
            }
            if self.head.len().saturating_add(retained) >= limit {
                self.fail(Failure::Limit);
                return None;
            }
            if self.head.len() == self.head.capacity() {
                let capacity = self.head.len().saturating_mul(2).max(32).min(limit);
                self.head.reserve_exact(capacity - self.head.len());
            }
            self.head.push(byte);
            let complete = match self.rx {
                Rx::Head => self.head.ends_with(b"\r\n\r\n"),
                Rx::Trailers => self.head == b"\r\n" || self.head.ends_with(b"\r\n\r\n"),
                _ => self.head.ends_with(b"\r\n"),
            };
            if complete {
                return self.process_metadata(ports, head_callback);
            }
            // Preserve the deadline callback boundary before consuming more bytes.
            if self.start == self.end || deadline_pending {
                return None;
            }
        }
    }

    pub(super) fn start_request_head(&mut self) -> Result<(), CommandError> {
        debug_assert_eq!(self.rx, Rx::AwaitingRequest);
        self.rx = Rx::Head;
        self.set_deadline(TimerPhase::Head, self.config.head_timeout_ns)
    }

    fn deliver_body<P: Ports<B, W>>(&mut self, ports: &mut P) -> Option<P::Output> {
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
            return None;
        }
        let id = self.operation(OperationKind::Body)?;
        self.credit -= count;
        let op = BodyOp {
            id,
            exchange: self.exchange.as_ref().unwrap().id,
            buffer: self.receive.deliver(id),
            range: self.start..self.start + count,
            buffered_end: self.end,
        };
        self.count(Metric::BodyDeliveries, 1);
        self.count(Metric::BodyDelivered, count as u64);
        self.log(
            ports,
            LogEvent::BodyOffered {
                exchange: op.exchange,
                operation: op.id,
                bytes: count,
            },
        );
        ports.body(op)
    }

    fn receive_eof(&mut self) {
        match self.rx {
            Rx::Eof => {
                self.rx = Rx::Done;
                self.close_after = true;
                self.incoming_ended();
            }
            Rx::Head | Rx::AwaitingRequest if self.head.is_empty() && self.exchange.is_none() => {
                self.lifecycle.begin_closing();
                self.clear_deadlines();
            }
            _ => self.fail(Failure::UnexpectedEof),
        }
    }
}
