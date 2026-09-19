use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Notification {
    Pending,
    Delivered,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum LocalClose {
    Pending(CloseReason),
    Sent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TransportClose {
    Settling,
    Awaiting(OperationId),
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Lifecycle {
    Open,
    Closing(LocalClose),
    Terminating {
        sent: bool,
        transport: TransportClose,
    },
    Closed {
        sent: bool,
        notification: Notification,
    },
}

impl Lifecycle {
    pub(super) fn open(self) -> bool {
        matches!(self, Self::Open)
    }

    pub(super) fn terminating(self) -> bool {
        matches!(self, Self::Terminating { .. } | Self::Closed { .. })
    }

    pub(super) fn sent(self) -> bool {
        match self {
            Self::Closing(LocalClose::Sent) => true,
            Self::Terminating { sent, .. } | Self::Closed { sent, .. } => sent,
            _ => false,
        }
    }

    pub(super) fn terminate(&mut self) {
        if !self.terminating() {
            *self = Self::Terminating {
                sent: self.sent(),
                transport: TransportClose::Settling,
            };
        }
    }

    pub(super) fn close_operation(self) -> Option<OperationId> {
        match self {
            Self::Terminating {
                transport: TransportClose::Awaiting(id),
                ..
            } => Some(id),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) enum PeerClose {
    Absent,
    Pending(CloseReason),
    Reported(CloseReason),
}

impl PeerClose {
    pub(super) fn reason(self) -> Option<CloseReason> {
        match self {
            Self::Absent => None,
            Self::Pending(reason) | Self::Reported(reason) => Some(reason),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum IoState {
    Idle,
    NeedsReadiness,
    InFlight(OperationId),
    CancelRequested(OperationId),
}

impl IoState {
    pub(super) fn operation(self) -> Option<OperationId> {
        match self {
            Self::InFlight(id) | Self::CancelRequested(id) => Some(id),
            _ => None,
        }
    }

    pub(super) fn raw(self) -> Option<OperationId> {
        self.operation()
            .filter(|id| matches!(id.kind(), OperationKind::Read | OperationKind::Write))
    }

    pub(super) fn readiness(self) -> Option<OperationId> {
        self.operation()
            .filter(|id| matches!(id.kind(), OperationKind::Readable | OperationKind::Writable))
    }

    pub(super) fn issue(&mut self, id: OperationId) {
        assert!(match self {
            Self::Idle => matches!(id.kind(), OperationKind::Read | OperationKind::Write),
            Self::NeedsReadiness =>
                matches!(id.kind(), OperationKind::Readable | OperationKind::Writable),
            _ => false,
        });
        *self = Self::InFlight(id);
    }

    pub(super) fn complete(&mut self) -> bool {
        assert!(self.operation().is_some());
        let cancelled = matches!(self, Self::CancelRequested(_));
        *self = Self::Idle;
        cancelled
    }

    pub(super) fn cancel(&mut self) -> OperationId {
        let Self::InFlight(id) = *self else {
            unreachable!("only an original in-flight operation can be cancelled");
        };
        *self = Self::CancelRequested(id);
        id
    }
}

#[cfg_attr(test, derive(Debug))]
pub(super) enum ReceiveStorage<B> {
    Available(B),
    Reading,
    Leased(OperationId),
}

impl<B> ReceiveStorage<B> {
    pub(super) fn buffer(&self) -> Option<&B> {
        match self {
            Self::Available(buffer) => Some(buffer),
            _ => None,
        }
    }
    pub(super) fn buffer_mut(&mut self) -> Option<&mut B> {
        match self {
            Self::Available(buffer) => Some(buffer),
            _ => None,
        }
    }
    pub(super) fn lease(&self) -> Option<OperationId> {
        match self {
            Self::Leased(id) => Some(*id),
            _ => None,
        }
    }
    pub(super) fn take(&mut self, next: Self) -> B {
        assert!(matches!(self, Self::Available(_)));
        let Self::Available(buffer) = std::mem::replace(self, next) else {
            unreachable!()
        };
        buffer
    }
}

#[cfg_attr(test, derive(Debug))]
pub(super) enum Transmit<W> {
    Idle,
    Pending(Outgoing<W>),
    Framed,
    Receipt(MessageSent<W>),
}

impl<W> Transmit<W> {
    pub(super) fn pending(&self) -> bool {
        matches!(self, Self::Pending(_))
    }
    pub(super) fn take_pending(&mut self) -> Outgoing<W> {
        assert!(self.pending());
        let Self::Pending(outgoing) = std::mem::replace(self, Self::Framed) else {
            unreachable!()
        };
        outgoing
    }
    pub(super) fn take_receipt(&mut self) -> MessageSent<W> {
        assert!(matches!(self, Self::Receipt(_)));
        let Self::Receipt(receipt) = std::mem::replace(self, Self::Idle) else {
            unreachable!()
        };
        receipt
    }
}

#[cfg_attr(test, derive(Debug))]
pub(super) enum IncomingState {
    Idle,
    Active {
        message: Incoming,
        notification: Notification,
    },
    Finished(MessageInfo),
}

impl IncomingState {
    pub(super) fn active(&self) -> Option<&Incoming> {
        match self {
            Self::Active { message, .. } => Some(message),
            _ => None,
        }
    }
    pub(super) fn active_mut(&mut self) -> Option<&mut Incoming> {
        match self {
            Self::Active { message, .. } => Some(message),
            _ => None,
        }
    }
    pub(super) fn finish(&mut self) -> Incoming {
        let Self::Active { message, .. } = std::mem::replace(self, Self::Idle) else {
            unreachable!("only an active message can finish");
        };
        message
    }
}

#[derive(Default)]
#[cfg_attr(test, derive(Debug))]
pub(super) struct HeaderState {
    pub bytes: [u8; 14],
    pub len: usize,
}

#[cfg_attr(test, derive(Debug))]
pub(super) enum Receive {
    Header(HeaderState),
    Payload(Frame),
}

#[cfg_attr(test, derive(Debug))]
pub(super) struct Timers {
    pub idle: Option<Tick>,
    pub frame: Option<Tick>,
    pub message: Option<Tick>,
    pub write: Option<Tick>,
    pub close: Option<Tick>,
    pub armed: Option<Deadline>,
    pub notification: Notification,
    pub sequence: u64,
}
