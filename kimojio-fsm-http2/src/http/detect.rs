use std::{rc::Rc, time::Duration};

use super::{Protocol, ServerPorts, probe::Probe};
use crate::{
    CancelCompletion, CancelOp, CloseCompletion, CloseOp, CommandError, IoFailure, ReadCompletion,
    ReadOp, ReadOutcome, Rejected, SendBuffer, Token, WakeCompletion, WakeOp, WakeOutcome,
    api::{Owner, Page},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetectionFailure {
    EndOfInput,
    TruncatedInput,
    Transport,
    Timeout,
    Aborted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DetectionClosed {
    pub failure: DetectionFailure,
    pub close_result: Result<(), IoFailure>,
}

enum Cancellation {
    Unrequested,
    Issued(Token),
    Acknowledged,
}

struct Slot {
    original: Option<Token>,
    cancellation: Cancellation,
}

impl Slot {
    fn new() -> Self {
        Self {
            original: None,
            cancellation: Cancellation::Unrequested,
        }
    }

    fn needs_cancel(&self) -> bool {
        self.original.is_some() && matches!(self.cancellation, Cancellation::Unrequested)
    }

    fn settled(&self) -> bool {
        self.original.is_none() && !matches!(self.cancellation, Cancellation::Issued(_))
    }

    fn cancel_completed(&mut self, token: &Token) -> bool {
        if matches!(&self.cancellation, Cancellation::Issued(expected) if expected == token) {
            self.cancellation = Cancellation::Acknowledged;
            true
        } else {
            false
        }
    }
}

enum Phase {
    Reading,
    Selected(Protocol),
    Closing(DetectionFailure),
    ClosePending {
        token: Token,
        failure: DetectionFailure,
    },
    NotifyClosed(DetectionClosed),
    Closed,
}

pub(super) enum Step<O> {
    Output(O),
    Selected(Protocol),
    Blocked,
}

pub(super) struct Detector {
    owner: Rc<Owner>,
    sequence: u64,
    now: Duration,
    deadline: Duration,
    phase: Phase,
    probe: Probe,
    read: Slot,
    alarm: Slot,
}

impl Detector {
    pub(super) fn new(now: Duration, timeout: Duration) -> Result<Self, CommandError> {
        let deadline = now.checked_add(timeout).ok_or(CommandError::Capacity)?;
        if timeout.is_zero() {
            return Err(CommandError::InvalidState);
        }
        Ok(Self {
            owner: Rc::new(Owner),
            sequence: 0,
            now,
            deadline,
            phase: Phase::Reading,
            probe: Probe::new(),
            read: Slot::new(),
            alarm: Slot::new(),
        })
    }

    fn token(&mut self) -> Token {
        // Each successful read adds a byte to a 24-byte probe. Other reads end it.
        self.sequence = self
            .sequence
            .checked_add(1)
            .expect("bounded probe operations");
        Token {
            owner: self.owner.clone(),
            sequence: self.sequence,
        }
    }

    pub(super) fn prefix(&self) -> &[u8] {
        self.probe.bytes()
    }

    pub(super) fn now(&self) -> Duration {
        self.now
    }

    pub(super) fn advance_time(&mut self, now: Duration) -> Result<(), CommandError> {
        if now < self.now {
            return Err(CommandError::TimeReversed);
        }
        self.now = now;
        if matches!(self.phase, Phase::Reading) && now >= self.deadline {
            self.phase = Phase::Closing(DetectionFailure::Timeout);
        }
        Ok(())
    }

    pub(super) fn abort(&mut self) {
        if matches!(self.phase, Phase::Reading | Phase::Selected(_)) {
            self.phase = Phase::Closing(DetectionFailure::Aborted);
        }
    }

    pub(super) fn complete_read(
        &mut self,
        completion: ReadCompletion,
    ) -> Result<(), Rejected<ReadCompletion>> {
        let count_valid = match completion.outcome {
            ReadOutcome::Read(count) => count > 0 && count <= completion.op.page.bytes.len(),
            _ => true,
        };
        if self.read.original.as_ref() != Some(&completion.op.token) || !count_valid {
            return Err(Rejected {
                error: CommandError::InvalidCompletion,
                value: completion,
            });
        }
        self.read.original = None;
        if !matches!(self.phase, Phase::Reading) {
            return Ok(());
        }
        match completion.outcome {
            ReadOutcome::Read(count) => {
                if let Some(protocol) = self
                    .probe
                    .accept(&completion.op.page.bytes[..count])
                    .expect("read range is bounded by remaining probe storage")
                {
                    self.phase = Phase::Selected(protocol);
                }
            }
            ReadOutcome::Eof => {
                self.phase = Phase::Closing(if self.probe.bytes().is_empty() {
                    DetectionFailure::EndOfInput
                } else {
                    DetectionFailure::TruncatedInput
                });
            }
            ReadOutcome::Failed(_) => self.phase = Phase::Closing(DetectionFailure::Transport),
        }
        Ok(())
    }

    pub(super) fn complete_wake(
        &mut self,
        completion: WakeCompletion,
    ) -> Result<(), Rejected<WakeCompletion>> {
        let current = matches!(self.phase, Phase::Reading);
        if self.alarm.original.as_ref() != Some(completion.token())
            || current
                && matches!(completion.outcome(), WakeOutcome::Fired(now)
                    if now < self.deadline || now < self.now)
        {
            return Err(Rejected {
                error: CommandError::InvalidCompletion,
                value: completion,
            });
        }
        self.alarm.original = None;
        if current {
            match completion.outcome() {
                WakeOutcome::Fired(now) => self.advance_time(now).expect("validated time"),
                WakeOutcome::Failed(_) => {
                    self.phase = Phase::Closing(DetectionFailure::Transport);
                }
            }
        }
        Ok(())
    }

    pub(super) fn complete_cancel(
        &mut self,
        completion: CancelCompletion,
    ) -> Result<(), Rejected<CancelCompletion>> {
        if self.read.cancel_completed(&completion.op.token)
            || self.alarm.cancel_completed(&completion.op.token)
        {
            Ok(())
        } else {
            Err(Rejected {
                error: CommandError::InvalidCompletion,
                value: completion,
            })
        }
    }

    pub(super) fn complete_close(
        &mut self,
        completion: CloseCompletion,
    ) -> Result<(), Rejected<CloseCompletion>> {
        if let Phase::ClosePending { token, failure } = &self.phase
            && *token == completion.op.token
        {
            self.phase = Phase::NotifyClosed(DetectionClosed {
                failure: *failure,
                close_result: completion.result,
            });
            Ok(())
        } else {
            Err(Rejected {
                error: CommandError::InvalidCompletion,
                value: completion,
            })
        }
    }

    pub(super) fn next<B: SendBuffer, P: ServerPorts<B>>(
        &mut self,
        ports: &mut P,
    ) -> Step<<P as crate::Ports<B>>::Output> {
        loop {
            let output = match &self.phase {
                Phase::Reading if self.alarm.original.is_none() => {
                    let token = self.token();
                    self.alarm.original = Some(token.clone());
                    crate::Ports::wake(
                        ports,
                        WakeOp {
                            token,
                            deadline: self.deadline,
                        },
                    )
                }
                Phase::Reading if self.read.original.is_none() => {
                    let token = self.token();
                    self.read.original = Some(token.clone());
                    crate::Ports::read(
                        ports,
                        ReadOp {
                            token,
                            page: Rc::new(Page {
                                bytes: vec![0; self.probe.remaining()].into_boxed_slice(),
                            }),
                        },
                    )
                }
                Phase::Selected(_) | Phase::Closing(_) if self.read.needs_cancel() => {
                    let token = self.token();
                    self.read.cancellation = Cancellation::Issued(token.clone());
                    crate::Ports::cancel(
                        ports,
                        CancelOp {
                            token,
                            original: self.read.original.clone().expect("pending read"),
                        },
                    )
                }
                Phase::Selected(_) | Phase::Closing(_) if self.alarm.needs_cancel() => {
                    let token = self.token();
                    self.alarm.cancellation = Cancellation::Issued(token.clone());
                    crate::Ports::cancel(
                        ports,
                        CancelOp {
                            token,
                            original: self.alarm.original.clone().expect("pending alarm"),
                        },
                    )
                }
                Phase::Selected(protocol) if self.read.settled() && self.alarm.settled() => {
                    return Step::Selected(*protocol);
                }
                Phase::Closing(failure) if self.read.settled() && self.alarm.settled() => {
                    let failure = *failure;
                    let token = self.token();
                    self.phase = Phase::ClosePending {
                        token: token.clone(),
                        failure,
                    };
                    crate::Ports::close(ports, CloseOp { token })
                }
                Phase::NotifyClosed(result) => {
                    let result = *result;
                    self.phase = Phase::Closed;
                    ports.detection_closed(result)
                }
                _ => return Step::Blocked,
            };
            if let Some(output) = output {
                return Step::Output(output);
            }
        }
    }
}
