use std::{cell::Cell, rc::Rc};

use http::Response;
use kimojio::{SenderOneshot, SenderUnbounded, oneshot};
use kimojio_fsm_http2::{H2HeaderField, StreamId};

use crate::{
    Error,
    driver::{Event, RequestControl, ResponseState},
    metadata,
};

#[derive(Default)]
pub(crate) struct InformationState {
    sequence: Cell<u64>,
    pending: Cell<Option<u64>>,
    cancelled: Cell<Option<u64>>,
}

impl InformationState {
    pub(crate) fn is_pending(&self) -> bool {
        self.pending.get().is_some()
    }
}

/// Optional per-request server control for informational response heads.
///
/// Obtain this handle from a server request's `IncomingBody::informational_sender`.
/// One command can await metadata admission at a time. Clones share that bound.
#[derive(Clone)]
pub struct InformationalSender {
    stream: StreamId,
    control: Rc<RequestControl>,
    state: Rc<InformationState>,
    events: SenderUnbounded<Event>,
}

impl InformationalSender {
    pub(crate) fn new(
        stream: StreamId,
        control: Rc<RequestControl>,
        events: SenderUnbounded<Event>,
    ) -> Self {
        let state = control.informational.get_or_init(Default::default).clone();
        Self {
            stream,
            control,
            state,
            events,
        }
    }

    /// Admits a 1xx head, excluding 101, before the final response.
    ///
    /// Success means that the core accepted and encoded metadata. It does not
    /// mean that the transport wrote it or that the peer received it.
    /// A concurrent pending command returns `Error::Limit`.
    /// Dropping this future before admission prevents that head from committing.
    pub async fn send(&self, response: Response<()>) -> Result<(), Error> {
        if self.control.retired.get() {
            return Err(Error::Closed);
        }
        if self.control.response_state.get() != ResponseState::Awaiting {
            return Err(Error::Command(
                kimojio_fsm_http2::CommandError::InvalidState,
            ));
        }
        if self.state.is_pending() {
            return Err(Error::Limit);
        }
        let fields = metadata::informational(response)?;
        if metadata::storage(&fields) > self.control.metadata_limit {
            return Err(Error::Limit);
        }
        let generation = self
            .state
            .sequence
            .get()
            .checked_add(1)
            .ok_or(Error::Limit)?;
        self.state.sequence.set(generation);
        self.state.pending.set(Some(generation));
        self.state.cancelled.set(None);
        let (accepted, receive) = oneshot();
        self.events
            .send(Event::Informational(PendingInformation {
                stream: self.stream,
                fields,
                generation,
                state: self.state.clone(),
                accepted: Some(accepted),
            }))
            .map_err(|_| Error::Closed)?;
        let mut guard = CancelInformation {
            stream: self.stream,
            generation,
            state: self.state.clone(),
            events: self.events.clone(),
            armed: true,
        };
        let result = receive.recv().await.map_err(|_| Error::Closed)?;
        guard.armed = false;
        result
    }
}

struct CancelInformation {
    stream: StreamId,
    generation: u64,
    state: Rc<InformationState>,
    events: SenderUnbounded<Event>,
    armed: bool,
}

impl Drop for CancelInformation {
    fn drop(&mut self) {
        if self.armed && self.state.pending.get() == Some(self.generation) {
            self.state.cancelled.set(Some(self.generation));
            let _ = self
                .events
                .send(Event::CancelInformation(self.stream, self.generation));
        }
    }
}

pub(crate) struct PendingInformation {
    pub(crate) stream: StreamId,
    pub(crate) fields: Vec<H2HeaderField>,
    pub(crate) generation: u64,
    state: Rc<InformationState>,
    accepted: Option<SenderOneshot<Result<(), Error>>>,
}

impl PendingInformation {
    pub(crate) fn cancelled(&self) -> bool {
        self.state.cancelled.get() == Some(self.generation)
    }

    pub(crate) fn complete(mut self, result: Result<(), Error>) {
        self.release();
        if let Some(accepted) = self.accepted.take() {
            let _ = accepted.send(result);
        }
    }

    fn release(&self) {
        if self.state.pending.get() == Some(self.generation) {
            self.state.pending.set(None);
        }
    }
}

impl Drop for PendingInformation {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_request_control_has_no_informational_allocation() {
        let control = RequestControl::new(None, 1024);
        assert!(control.informational.get().is_none());
    }
}
