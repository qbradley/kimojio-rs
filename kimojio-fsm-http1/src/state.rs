use crate::codec::Framing;
use crate::{Deadline, OperationId, OperationKind, Tick};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MethodSemantics {
    Ordinary,
    Head,
    Connect,
}

impl MethodSemantics {
    pub(crate) fn from_method(method: &str) -> Self {
        match method {
            "HEAD" => Self::Head,
            "CONNECT" => Self::Connect,
            _ => Self::Ordinary,
        }
    }
}

#[derive(Debug)]
pub(crate) enum ReceiveStorage<B> {
    Available(B),
    // Temporarily owned by the synchronous metadata parser, never by a port.
    Parsing,
    Reading,
    Leased(OperationId),
    Transferred,
}

impl<B> ReceiveStorage<B> {
    pub(crate) fn buffer(&self) -> Option<&B> {
        match self {
            Self::Available(buffer) => Some(buffer),
            _ => None,
        }
    }

    pub(crate) fn lease(&self) -> Option<OperationId> {
        match self {
            Self::Leased(id) => Some(*id),
            _ => None,
        }
    }

    fn take(&mut self, next: Self) -> B {
        assert!(matches!(self, Self::Available(_)));
        let Self::Available(buffer) = std::mem::replace(self, next) else {
            unreachable!()
        };
        buffer
    }

    pub(crate) fn parse(&mut self) -> B {
        self.take(Self::Parsing)
    }

    pub(crate) fn parsed(&mut self, buffer: B) {
        assert!(matches!(self, Self::Parsing));
        *self = Self::Available(buffer);
    }

    pub(crate) fn read(&mut self) -> B {
        self.take(Self::Reading)
    }

    pub(crate) fn deliver(&mut self, id: OperationId) -> B {
        self.take(Self::Leased(id))
    }

    pub(crate) fn handoff(&mut self) -> B {
        self.take(Self::Transferred)
    }

    pub(crate) fn complete_read(&mut self, buffer: B) {
        assert!(matches!(self, Self::Reading));
        *self = Self::Available(buffer);
    }

    pub(crate) fn release(&mut self, buffer: B) {
        assert!(matches!(self, Self::Leased(_)));
        *self = Self::Available(buffer);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Notification {
    Pending,
    Delivered,
}

impl Notification {
    pub(crate) fn take(&mut self) -> bool {
        if *self == Self::Pending {
            *self = Self::Delivered;
            true
        } else {
            false
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Admission {
    Accepting,
    Draining,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ContinueGate {
    Open,
    Waiting,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Upgrade {
    Handshake,
    Ready,
    // A client must report the received upgrade head before reporting cancellation.
    Revoked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Closing {
    Settling,
    AwaitingClose(OperationId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Lifecycle {
    Http(Admission),
    Upgrade(Upgrade),
    ErrorResponse,
    Closing(Closing),
    Closed(Notification),
    HandedOff,
}

impl Lifecycle {
    pub(crate) fn accepts_exchange_commands(self) -> bool {
        matches!(self, Self::Http(_) | Self::Upgrade(_))
    }

    pub(crate) fn is_draining(self) -> bool {
        matches!(
            self,
            Self::Http(Admission::Draining) | Self::Upgrade(Upgrade::Revoked)
        )
    }

    pub(crate) fn is_closing(self) -> bool {
        matches!(self, Self::Closing(_))
    }

    pub(crate) fn is_terminal(self) -> bool {
        matches!(self, Self::Closed(_) | Self::HandedOff)
    }

    pub(crate) fn is_upgrade(self) -> bool {
        matches!(self, Self::Upgrade(_))
    }

    pub(crate) fn begin_upgrade(&mut self) {
        *self = match *self {
            Self::Http(Admission::Accepting) => Self::Upgrade(Upgrade::Handshake),
            Self::Http(Admission::Draining) => Self::Upgrade(Upgrade::Revoked),
            _ => unreachable!("upgrade requires HTTP authority"),
        };
    }

    pub(crate) fn begin_closing(&mut self) {
        if !self.is_closing() && !self.is_terminal() {
            *self = Self::Closing(Closing::Settling);
        }
    }

    pub(crate) fn close_operation(self) -> Option<OperationId> {
        match self {
            Self::Closing(Closing::AwaitingClose(id)) => Some(id),
            _ => None,
        }
    }

    pub(crate) fn issue_close(&mut self, id: OperationId) {
        assert_eq!(*self, Self::Closing(Closing::Settling));
        *self = Self::Closing(Closing::AwaitingClose(id));
    }

    pub(crate) fn complete_close(&mut self) {
        assert!(self.close_operation().is_some());
        *self = Self::Closed(Notification::Pending);
    }

    pub(crate) fn notify_upgrade(&mut self) -> bool {
        match self {
            Self::Upgrade(Upgrade::Handshake) => {
                *self = Self::Upgrade(Upgrade::Ready);
                true
            }
            Self::Upgrade(Upgrade::Ready) => false,
            _ => unreachable!("only a live upgrade can become ready"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IoState {
    Idle,
    NeedsReadiness,
    InFlight(OperationId),
    CancelRequested(OperationId),
}

impl IoState {
    pub(crate) fn operation(self) -> Option<OperationId> {
        match self {
            Self::InFlight(id) | Self::CancelRequested(id) => Some(id),
            Self::Idle | Self::NeedsReadiness => None,
        }
    }

    #[inline]
    pub(crate) fn issue(&mut self, id: OperationId) {
        assert!(match self {
            Self::Idle => matches!(id.kind(), OperationKind::Read | OperationKind::Write),
            Self::NeedsReadiness =>
                matches!(id.kind(), OperationKind::Readable | OperationKind::Writable),
            _ => false,
        });
        *self = Self::InFlight(id);
    }

    pub(crate) fn complete(&mut self) {
        assert!(self.operation().is_some());
        *self = Self::Idle;
    }

    pub(crate) fn wait_for_readiness(&mut self) {
        assert_eq!(*self, Self::Idle);
        *self = Self::NeedsReadiness;
    }

    pub(crate) fn cancel(&mut self) -> Option<OperationId> {
        if let Self::InFlight(id) = *self {
            *self = Self::CancelRequested(id);
            Some(id)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SourceEnd {
    Complete,
    EarlyResponse,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Producer {
    Ready,
    Requested,
    Ended(SourceEnd),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Transmit {
    Idle,
    Writing {
        framing: Framing,
        producer: Producer,
    },
    Settled {
        framing: Framing,
        end: SourceEnd,
    },
}

impl Transmit {
    pub(crate) fn begin(framing: Framing) -> Self {
        Self::Writing {
            framing,
            producer: if framing == Framing::Empty {
                Producer::Ended(SourceEnd::Complete)
            } else {
                Producer::Ready
            },
        }
    }

    pub(crate) fn framing(self) -> Framing {
        match self {
            Self::Idle => Framing::Empty,
            Self::Writing { framing, .. } | Self::Settled { framing, .. } => framing,
        }
    }

    pub(crate) fn started(self) -> bool {
        !matches!(self, Self::Idle)
    }

    pub(crate) fn settled(self) -> bool {
        matches!(self, Self::Settled { .. })
    }

    pub(crate) fn source_finished(self) -> bool {
        matches!(
            self,
            Self::Writing {
                producer: Producer::Ended(_),
                ..
            } | Self::Settled { .. }
        )
    }

    pub(crate) fn stopped(self) -> bool {
        matches!(
            self,
            Self::Writing {
                producer: Producer::Ended(SourceEnd::EarlyResponse),
                ..
            } | Self::Settled {
                end: SourceEnd::EarlyResponse,
                ..
            }
        )
    }

    pub(crate) fn accepts_data(self) -> bool {
        matches!(
            self,
            Self::Writing {
                producer: Producer::Ready | Producer::Requested,
                ..
            }
        )
    }

    pub(crate) fn can_request_data(self) -> bool {
        matches!(
            self,
            Self::Writing {
                producer: Producer::Ready,
                ..
            }
        )
    }

    pub(crate) fn request_data(&mut self) {
        let Self::Writing { producer, .. } = self else {
            unreachable!("only a writing transmitter requests data");
        };
        assert_eq!(*producer, Producer::Ready);
        *producer = Producer::Requested;
    }

    pub(crate) fn accept_data(&mut self, bytes: usize, end: bool) {
        let Self::Writing { framing, producer } = self else {
            unreachable!("data requires a writing transmitter");
        };
        assert!(!matches!(producer, Producer::Ended(_)));
        let exhausted = if let Framing::Fixed(left) = framing {
            *left -= bytes as u64;
            *left == 0
        } else {
            false
        };
        *producer = if end || exhausted {
            Producer::Ended(SourceEnd::Complete)
        } else {
            Producer::Ready
        };
    }

    pub(crate) fn finish_source(&mut self) {
        let Self::Writing { producer, .. } = self else {
            unreachable!("source completion requires a writing transmitter");
        };
        *producer = Producer::Ended(SourceEnd::Complete);
    }

    pub(crate) fn stop_upload(&mut self) {
        let Self::Writing { producer, .. } = self else {
            unreachable!("only an unsettled upload can stop");
        };
        *producer = Producer::Ended(SourceEnd::EarlyResponse);
    }

    pub(crate) fn settle(&mut self) {
        *self = match *self {
            Self::Writing {
                framing,
                producer: Producer::Ended(end),
            } => Self::Settled { framing, end },
            Self::Settled { .. } => return,
            _ => unreachable!("transmission cannot settle before its source ends"),
        };
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TimerPhase {
    Head,
    Body,
    Idle,
    Continue,
    Upload,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ArmedDeadline {
    pub(crate) deadline: Deadline,
    pub(crate) kind: TimerPhase,
}

#[derive(Debug)]
pub(crate) struct Timers {
    pub(crate) armed: Option<ArmedDeadline>,
    pub(crate) notification: Notification,
    pub(crate) phase: Option<(TimerPhase, Tick)>,
    pub(crate) continue_at: Option<Tick>,
    pub(crate) upload_at: Option<Tick>,
}

impl Timers {
    pub(crate) fn new() -> Self {
        Self {
            armed: None,
            notification: Notification::Delivered,
            phase: None,
            continue_at: None,
            upload_at: None,
        }
    }

    pub(crate) fn deadline(&self) -> Option<Deadline> {
        self.armed.map(|armed| armed.deadline)
    }

    pub(crate) fn kind(&self) -> Option<TimerPhase> {
        self.armed.map(|armed| armed.kind)
    }

    pub(crate) fn clear(&mut self) {
        if self.armed.take().is_some() {
            self.notification = Notification::Pending;
        }
        self.phase = None;
        self.continue_at = None;
        self.upload_at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ConnectionId;
    use std::{cell::Cell, rc::Rc};

    fn operation(kind: OperationKind, sequence: u64) -> OperationId {
        OperationId {
            connection: ConnectionId {
                slot: 2,
                generation: 3,
            },
            sequence,
            kind,
        }
    }

    #[test]
    fn closing_preserves_outstanding_close_and_emits_one_terminal_notification() {
        let starts = [
            Lifecycle::Http(Admission::Accepting),
            Lifecycle::Http(Admission::Draining),
            Lifecycle::Upgrade(Upgrade::Handshake),
            Lifecycle::Upgrade(Upgrade::Ready),
            Lifecycle::Upgrade(Upgrade::Revoked),
            Lifecycle::ErrorResponse,
        ];
        for mut state in starts {
            state.begin_closing();
            assert_eq!(state, Lifecycle::Closing(Closing::Settling));
            assert!(!state.accepts_exchange_commands());
            let id = operation(OperationKind::Close, 9);
            state.issue_close(id);
            state.begin_closing();
            assert_eq!(state.close_operation(), Some(id));
            state.complete_close();
            state.begin_closing();
            let Lifecycle::Closed(mut notice) = state else {
                panic!()
            };
            assert!(notice.take());
            assert!(!notice.take());
            assert!(state.is_terminal());
            assert_eq!(state.close_operation(), None);
        }
    }

    #[test]
    fn upgrade_and_handoff_have_no_close_authority_after_transfer() {
        let mut state = Lifecycle::Http(Admission::Accepting);
        state.begin_upgrade();
        assert_eq!(state, Lifecycle::Upgrade(Upgrade::Handshake));
        assert!(state.notify_upgrade());
        assert!(!state.notify_upgrade());
        assert_eq!(state, Lifecycle::Upgrade(Upgrade::Ready));
        state = Lifecycle::HandedOff;
        state.begin_closing();
        assert_eq!(state, Lifecycle::HandedOff);
        assert!(!state.accepts_exchange_commands());
        assert_eq!(state.close_operation(), None);

        let mut draining = Lifecycle::Http(Admission::Draining);
        draining.begin_upgrade();
        assert_eq!(draining, Lifecycle::Upgrade(Upgrade::Revoked));
        draining.begin_closing();
        assert!(!draining.is_upgrade());
    }

    #[test]
    fn cancellation_retains_the_original_operation_until_completion() {
        for (io, readiness) in [
            (OperationKind::Read, OperationKind::Readable),
            (OperationKind::Write, OperationKind::Writable),
        ] {
            let mut state = IoState::Idle;
            assert_eq!(state.cancel(), None);
            let id = operation(io, 1);
            state.issue(id);
            assert_eq!(state.cancel(), Some(id));
            assert_eq!(state.cancel(), None);
            assert_eq!(state.operation(), Some(id));
            state.complete();
            assert_eq!(state, IoState::Idle);
            state.wait_for_readiness();
            assert_eq!(state.cancel(), None);
            let id = operation(readiness, 2);
            state.issue(id);
            assert_eq!(state.cancel(), Some(id));
            assert_eq!(state.operation(), Some(id));
            state.complete();
            state.wait_for_readiness();
            state.issue(operation(readiness, 3));
            state.complete();
            state.issue(operation(io, 4));
            assert_eq!(state.operation(), Some(operation(io, 4)));
        }
    }

    #[test]
    fn source_completion_does_not_settle_transport() {
        for framing in [
            Framing::Empty,
            Framing::Fixed(3),
            Framing::Chunked,
            Framing::Eof,
        ] {
            let mut tx = Transmit::begin(framing);
            assert!(tx.started());
            assert!(!tx.settled());
            if framing != Framing::Empty {
                tx.request_data();
                assert!(!tx.can_request_data());
                assert!(tx.accepts_data());
                tx.accept_data(1, false);
                assert!(tx.can_request_data());
                assert!(!tx.source_finished());
                tx.request_data();
                tx.accept_data(2, true);
            }
            assert!(tx.source_finished());
            assert!(!tx.settled());
            assert!(!tx.stopped());
            tx.settle();
            assert!(tx.settled());
            assert!(tx.source_finished());
            tx.settle();
        }
    }

    #[test]
    fn an_early_response_can_stop_requested_or_already_ended_production() {
        for producer in [
            Producer::Ready,
            Producer::Requested,
            Producer::Ended(SourceEnd::Complete),
        ] {
            let mut tx = Transmit::Writing {
                framing: Framing::Chunked,
                producer,
            };
            tx.stop_upload();
            assert!(tx.source_finished());
            assert!(tx.stopped());
            assert!(!tx.settled());
            assert!(!tx.accepts_data());
            tx.settle();
            assert!(tx.stopped());
            assert_eq!(tx.framing(), Framing::Chunked);
        }
    }

    #[test]
    fn receive_storage_has_one_owner_and_does_not_clone_payloads() {
        struct Owned {
            bytes: Vec<u8>,
            drops: Rc<Cell<usize>>,
        }
        impl Drop for Owned {
            fn drop(&mut self) {
                self.drops.set(self.drops.get() + 1);
            }
        }
        let drops = Rc::new(Cell::new(0));
        let buffer = Owned {
            bytes: vec![1, 2, 3],
            drops: drops.clone(),
        };
        let address = buffer.bytes.as_ptr();
        let mut storage = ReceiveStorage::Available(buffer);
        let buffer = storage.parse();
        assert!(matches!(storage, ReceiveStorage::Parsing));
        assert!(storage.buffer().is_none() && storage.lease().is_none());
        assert_eq!(buffer.bytes.as_ptr(), address);
        assert_eq!(drops.get(), 0);
        storage.parsed(buffer);
        let buffer = storage.read();
        assert!(matches!(storage, ReceiveStorage::Reading));
        assert!(storage.buffer().is_none());
        storage.complete_read(buffer);
        let id = operation(OperationKind::Body, 4);
        let buffer = storage.deliver(id);
        assert_eq!(storage.lease(), Some(id));
        assert!(storage.buffer().is_none());
        storage.release(buffer);
        let buffer = storage.handoff();
        assert!(matches!(storage, ReceiveStorage::Transferred));
        assert_eq!(buffer.bytes.as_ptr(), address);
        drop(storage);
        assert_eq!(drops.get(), 0);
        drop(buffer);
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn clearing_timers_retains_a_pending_cancellation_notification() {
        let mut timers = Timers::new();
        assert!(!timers.notification.take());
        timers.clear();
        assert!(!timers.notification.take());
        timers.armed = Some(ArmedDeadline {
            deadline: Deadline {
                connection: ConnectionId {
                    slot: 2,
                    generation: 3,
                },
                sequence: 4,
                at: Tick(8),
            },
            kind: TimerPhase::Body,
        });
        timers.phase = Some((TimerPhase::Body, Tick(8)));
        timers.continue_at = Some(Tick(9));
        timers.upload_at = Some(Tick(10));
        timers.clear();
        timers.clear();
        assert!(timers.armed.is_none());
        assert!(timers.phase.is_none());
        assert!(timers.continue_at.is_none());
        assert!(timers.upload_at.is_none());
        assert!(timers.notification.take());
        assert!(!timers.notification.take());
    }
}
