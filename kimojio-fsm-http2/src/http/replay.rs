use kimojio_fsm_http1 as h1;

use crate::SendBuffer;

pub(super) struct Prefix {
    bytes: [u8; 24],
    len: usize,
    cursor: usize,
}

impl Prefix {
    pub(super) fn new(bytes: &[u8]) -> Self {
        let mut prefix = Self {
            bytes: [0; 24],
            len: bytes.len(),
            cursor: 0,
        };
        prefix.bytes[..bytes.len()].copy_from_slice(bytes);
        prefix
    }

    pub(super) fn is_empty(&self) -> bool {
        self.cursor == self.len
    }

    pub(super) fn copy_to(&mut self, output: &mut [u8]) -> usize {
        let count = output.len().min(self.len - self.cursor);
        assert!(count > 0, "live prefix and nonempty child read");
        output[..count].copy_from_slice(&self.bytes[self.cursor..self.cursor + count]);
        self.cursor += count;
        count
    }
}

pub(super) enum Step<O> {
    Output(O),
    Read1(h1::ReadOp<Vec<u8>>),
    Read2(crate::ReadOp),
}

pub(super) struct Ports<'a, P> {
    pub(super) outer: &'a mut P,
}

impl<B: SendBuffer, P: h1::Ports<Vec<u8>, B>> h1::Ports<Vec<u8>, B> for Ports<'_, P> {
    type Output = Step<P::Output>;

    fn read(&mut self, op: h1::ReadOp<Vec<u8>>) -> Option<Self::Output> {
        Some(Step::Read1(op))
    }
    fn write(&mut self, op: h1::WriteOp<B>) -> Option<Self::Output> {
        self.outer.write(op).map(Step::Output)
    }
    fn readiness(&mut self, op: h1::ReadinessOp) -> Option<Self::Output> {
        self.outer.readiness(op).map(Step::Output)
    }
    fn cancel(&mut self, op: h1::CancelOp) -> Option<Self::Output> {
        self.outer.cancel(op).map(Step::Output)
    }
    fn close(&mut self, op: h1::CloseOp) -> Option<Self::Output> {
        self.outer.close(op).map(Step::Output)
    }
    fn body(&mut self, op: h1::BodyOp<Vec<u8>>) -> Option<Self::Output> {
        self.outer.body(op).map(Step::Output)
    }
    fn trailers(&mut self, id: h1::ExchangeId, fields: h1::Headers<'_>) -> Option<Self::Output> {
        self.outer.trailers(id, fields).map(Step::Output)
    }
    fn incoming_finished(&mut self, id: h1::ExchangeId) -> Option<Self::Output> {
        self.outer.incoming_finished(id).map(Step::Output)
    }
    fn send_ready(&mut self, id: h1::ExchangeId, capacity: usize) -> Option<Self::Output> {
        self.outer.send_ready(id, capacity).map(Step::Output)
    }
    fn source_finished(&mut self, id: h1::ExchangeId) -> Option<Self::Output> {
        self.outer.source_finished(id).map(Step::Output)
    }
    fn body_sent(&mut self, result: h1::BodySent<B>) -> Option<Self::Output> {
        self.outer.body_sent(result).map(Step::Output)
    }
    fn exchange_finished(&mut self, result: h1::ExchangeFinished) -> Option<Self::Output> {
        self.outer.exchange_finished(result).map(Step::Output)
    }
    fn deadline_changed(&mut self, deadline: Option<h1::Deadline>) -> Option<Self::Output> {
        self.outer.deadline_changed(deadline).map(Step::Output)
    }
    fn upgrade_ready(&mut self, id: h1::ExchangeId) -> Option<Self::Output> {
        self.outer.upgrade_ready(id).map(Step::Output)
    }
    fn closed(&mut self, result: h1::ConnectionResult) -> Option<Self::Output> {
        self.outer.closed(result).map(Step::Output)
    }
}

impl<B: SendBuffer, P: h1::ServerPorts<Vec<u8>, B>> h1::ServerPorts<Vec<u8>, B> for Ports<'_, P> {
    fn request(&mut self, id: h1::ExchangeId, head: h1::RequestHead<'_>) -> Option<Self::Output> {
        self.outer.request(id, head).map(Step::Output)
    }
}

impl<B: SendBuffer, P: crate::Ports<B>> crate::Ports<B> for Ports<'_, P> {
    type Output = Step<P::Output>;

    fn read(&mut self, op: crate::ReadOp) -> Option<Self::Output> {
        Some(Step::Read2(op))
    }
    fn write(&mut self, op: crate::WriteOp<B>) -> Option<Self::Output> {
        self.outer.write(op).map(Step::Output)
    }
    fn headers(&mut self, head: crate::Head<'_>) -> Option<Self::Output> {
        self.outer.headers(head).map(Step::Output)
    }
    fn body(&mut self, op: crate::BodyOp) -> Option<Self::Output> {
        self.outer.body(op).map(Step::Output)
    }
    fn send_ready(&mut self, permit: crate::SendPermit) -> Option<Self::Output> {
        self.outer.send_ready(permit).map(Step::Output)
    }
    fn send_stopped(
        &mut self,
        id: crate::StreamId,
        reason: crate::SendStop,
    ) -> Option<Self::Output> {
        self.outer.send_stopped(id, reason).map(Step::Output)
    }
    fn sent(&mut self, result: crate::Sent<B>) -> Option<Self::Output> {
        self.outer.sent(result).map(Step::Output)
    }
    fn ended(&mut self, end: crate::ReceiveEnd) -> Option<Self::Output> {
        self.outer.ended(end).map(Step::Output)
    }
    fn retired(&mut self, result: crate::StreamResult) -> Option<Self::Output> {
        self.outer.retired(result).map(Step::Output)
    }
    fn cancel(&mut self, op: crate::CancelOp) -> Option<Self::Output> {
        self.outer.cancel(op).map(Step::Output)
    }
    fn wake(&mut self, op: crate::WakeOp) -> Option<Self::Output> {
        self.outer.wake(op).map(Step::Output)
    }
    fn close(&mut self, op: crate::CloseOp) -> Option<Self::Output> {
        self.outer.close(op).map(Step::Output)
    }
    fn closed(&mut self, result: crate::ConnectionResult) -> Option<Self::Output> {
        self.outer.closed(result).map(Step::Output)
    }
    fn reschedule(&mut self) -> Option<Self::Output> {
        self.outer.reschedule().map(Step::Output)
    }
}
