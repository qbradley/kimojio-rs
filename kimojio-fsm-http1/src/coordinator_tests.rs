use super::*;
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Input {
    Read,
    Readiness,
    Lease,
    Returned,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Output {
    Write,
    Readiness,
    Returned,
}

#[derive(Clone, Copy, Debug)]
enum Stimulus {
    Receive,
    ReadReady,
    Release,
    Write,
    ShortWrite,
    BlockWrite,
    InterruptWrite,
    ResetWrite,
    WriteReady,
    Abort,
    Timeout,
}

// This model knows ownership and the early-response contract, not Core's states
// or transition selector. The request and response each contain two body bytes.
#[derive(Clone, Copy, Debug)]
struct Model {
    input: Input,
    output: Output,
    accepted: usize,
    failure: Option<Failure>,
    perturbed: bool,
    incoming_finished: bool,
    receipt: Option<(usize, Result<(), Failure>)>,
}

impl Model {
    fn new(input: Input) -> Self {
        Self {
            input,
            output: Output::Write,
            accepted: 0,
            failure: None,
            perturbed: false,
            incoming_finished: false,
            receipt: None,
        }
    }

    fn stopped(self) -> bool {
        self.failure.is_some() || self.output == Output::Returned
    }

    fn complete(self) -> bool {
        self.input == Input::Returned && self.output == Output::Returned
    }

    fn choices(self) -> Vec<Stimulus> {
        let mut choices = Vec::new();
        match self.input {
            Input::Read => choices.push(Stimulus::Receive),
            Input::Readiness => choices.push(Stimulus::ReadReady),
            Input::Lease => choices.push(Stimulus::Release),
            Input::Returned => {}
        }
        match self.output {
            Output::Write => {
                choices.push(Stimulus::Write);
                if !self.perturbed && self.failure.is_none() {
                    choices.extend([
                        Stimulus::ShortWrite,
                        Stimulus::BlockWrite,
                        Stimulus::InterruptWrite,
                        Stimulus::ResetWrite,
                    ]);
                }
            }
            Output::Readiness => choices.push(Stimulus::WriteReady),
            Output::Returned => {}
        }
        if self.failure.is_none() && !self.complete() {
            choices.extend([Stimulus::Abort, Stimulus::Timeout]);
        }
        choices
    }

    fn apply(&mut self, stimulus: Stimulus) {
        match stimulus {
            Stimulus::Receive => {
                self.input = if self.stopped() {
                    Input::Returned
                } else {
                    Input::Lease
                };
            }
            Stimulus::ReadReady => {
                self.input = if self.stopped() {
                    Input::Returned
                } else {
                    Input::Read
                };
            }
            Stimulus::Release => {
                self.incoming_finished = !self.stopped();
                self.input = Input::Returned;
            }
            Stimulus::Write => {
                self.accepted = 2;
                self.output = Output::Returned;
                self.receipt = Some((self.accepted, self.failure.map_or(Ok(()), Err)));
            }
            Stimulus::ShortWrite => {
                self.accepted = 1;
                self.perturbed = true;
            }
            Stimulus::BlockWrite => {
                self.output = Output::Readiness;
                self.perturbed = true;
            }
            Stimulus::InterruptWrite => self.perturbed = true,
            Stimulus::ResetWrite => {
                self.failure = Some(Failure::Transport(io_error(IoErrorKind::Reset)));
                self.output = Output::Returned;
                self.perturbed = true;
                self.receipt = Some((self.accepted, self.failure.map_or(Ok(()), Err)));
            }
            Stimulus::WriteReady => {
                self.output = if self.failure.is_some() {
                    Output::Returned
                } else {
                    Output::Write
                };
            }
            Stimulus::Abort => {
                self.failure = Some(Failure::Cancelled);
                // WouldBlock already returned the write storage to the machine.
                // Cancelling readiness retains the readiness ID, not that storage.
            }
            Stimulus::Timeout => self.failure = Some(Failure::Timeout),
        }
        if self.failure.is_some() && self.output == Output::Readiness {
            self.receipt
                .get_or_insert((self.accepted, self.failure.map_or(Ok(()), Err)));
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Drive {
    Steps,
    Continue,
    Yield,
    Mixed,
}

struct Observer {
    drive: Drive,
    trace: Vec<String>,
    logs: Vec<(ConnectionId, Tick, LogEvent)>,
    read: Option<ReadOp<Vec<u8>>>,
    read_ready: Option<ReadinessOp>,
    write: Option<WriteOp<Vec<u8>>>,
    write_ready: Option<ReadinessOp>,
    body: Option<BodyOp<Vec<u8>>>,
    close: Option<CloseOp>,
    deadline: Option<Deadline>,
    exchange: Option<ExchangeId>,
    issued: HashSet<OperationId>,
    cancelled: HashSet<OperationId>,
    sources: usize,
    incoming: usize,
    receipts: Vec<(usize, Result<(), Failure>)>,
    expected_receipts: usize,
    finished: usize,
    closed: Vec<ConnectionResult>,
}

impl Observer {
    fn new(drive: Drive) -> Self {
        Self {
            drive,
            trace: Vec::new(),
            logs: Vec::new(),
            read: None,
            read_ready: None,
            write: None,
            write_ready: None,
            body: None,
            close: None,
            deadline: None,
            exchange: None,
            issued: HashSet::new(),
            cancelled: HashSet::new(),
            sources: 0,
            incoming: 0,
            receipts: Vec::new(),
            expected_receipts: 1,
            finished: 0,
            closed: Vec::new(),
        }
    }

    fn record(&mut self, value: String) -> Option<()> {
        self.trace.push(value);
        assert!(self.trace.len() < 256, "unbounded callback sequence");
        match self.drive {
            Drive::Steps | Drive::Continue => None,
            Drive::Yield => Some(()),
            Drive::Mixed => self.trace.len().is_multiple_of(3).then_some(()),
        }
    }

    fn issue(&mut self, id: OperationId) {
        assert!(self.issued.insert(id), "operation issued twice: {id:?}");
    }
}

impl Ports<Vec<u8>> for Observer {
    type Output = ();
    fn log(&mut self, connection: ConnectionId, now: Tick, event: LogEvent) {
        self.logs.push((connection, now, event));
    }
    fn read(&mut self, op: ReadOp<Vec<u8>>) -> Option<()> {
        assert!(self.read.is_none() && self.read_ready.is_none() && self.body.is_none());
        self.issue(op.id());
        let output = self.record(format!("read {:?}", op.id()));
        self.read = Some(op);
        output
    }
    fn write(&mut self, op: WriteOp<Vec<u8>>) -> Option<()> {
        assert!(self.write.is_none() && self.write_ready.is_none());
        self.issue(op.id());
        let output = self.record(format!("write {:?} {:?}", op.id(), op.slices()));
        self.write = Some(op);
        output
    }
    fn readiness(&mut self, op: ReadinessOp) -> Option<()> {
        self.issue(op.id());
        let output = self.record(format!("readiness {:?}", op));
        match op.direction {
            Direction::Read => {
                assert!(self.read.is_none() && self.read_ready.is_none());
                self.read_ready = Some(op);
            }
            Direction::Write => {
                assert!(self.write.is_none() && self.write_ready.is_none());
                self.write_ready = Some(op);
            }
        }
        output
    }
    fn cancel(&mut self, op: CancelOp) -> Option<()> {
        assert!(self.cancelled.insert(op.target), "duplicate cancellation");
        assert!(
            [
                self.read.as_ref().map(ReadOp::id),
                self.write.as_ref().map(WriteOp::id),
                self.read_ready.map(ReadinessOp::id),
                self.write_ready.map(ReadinessOp::id),
            ]
            .contains(&Some(op.target)),
            "cancellation must refer to a retained original operation"
        );
        self.record(format!("cancel {:?}", op.target))
    }
    fn close(&mut self, op: CloseOp) -> Option<()> {
        assert!(self.read.is_none() && self.read_ready.is_none());
        assert!(self.write.is_none() && self.write_ready.is_none() && self.body.is_none());
        assert!(self.close.is_none());
        assert_eq!(self.sources, 1);
        assert_eq!(self.receipts.len(), self.expected_receipts);
        assert_eq!(self.finished, 1);
        self.issue(op.id());
        self.close = Some(op);
        self.record("close".into())
    }
    fn body(&mut self, op: BodyOp<Vec<u8>>) -> Option<()> {
        assert!(self.read.is_none() && self.read_ready.is_none() && self.body.is_none());
        self.issue(op.id());
        assert_eq!(op.bytes(), b"ab");
        self.body = Some(op);
        self.record("body".into())
    }
    fn trailers(&mut self, _: ExchangeId, _: Headers<'_>) -> Option<()> {
        panic!("the model uses fixed framing")
    }
    fn incoming_finished(&mut self, _: ExchangeId) -> Option<()> {
        self.incoming += 1;
        assert_eq!(self.incoming, 1);
        self.record("incoming".into())
    }
    fn send_ready(&mut self, _: ExchangeId, capacity: usize) -> Option<()> {
        assert_eq!(capacity, 2);
        self.record("demand".into())
    }
    fn source_finished(&mut self, _: ExchangeId) -> Option<()> {
        self.sources += 1;
        assert_eq!(self.sources, 1);
        self.record("source".into())
    }
    fn body_sent(&mut self, result: BodySent<Vec<u8>>) -> Option<()> {
        assert_eq!(result.buffer, b"xy");
        assert_eq!(result.acceptance, Acceptance::Exact);
        self.receipts.push((result.accepted, result.result));
        assert_eq!(self.receipts.len(), 1);
        self.record(format!("receipt {} {:?}", result.accepted, result.result))
    }
    fn exchange_finished(&mut self, result: ExchangeFinished) -> Option<()> {
        self.finished += 1;
        assert_eq!(self.finished, 1);
        assert!(!result.reusable);
        self.record(format!("finished {:?}", result.result))
    }
    fn deadline_changed(&mut self, deadline: Option<Deadline>) -> Option<()> {
        self.deadline = deadline;
        self.record(format!("deadline {:?}", deadline))
    }
    fn upgrade_ready(&mut self, _: ExchangeId) -> Option<()> {
        self.record("upgrade".into())
    }
    fn closed(&mut self, result: ConnectionResult) -> Option<()> {
        self.closed.push(result);
        assert_eq!(self.closed.len(), 1);
        self.record(format!("closed {:?}", result))
    }
}

fn head(observer: &mut Observer, id: ExchangeId, parsed: ParsedHead<'_>) -> Option<()> {
    observer.exchange = Some(id);
    observer.record(match parsed {
        ParsedHead::Request(_) => "request".into(),
        ParsedHead::Response(head, info) => format!("response {} {info}", head.status),
    })
}

fn drive<const SERVER: bool>(core: &mut Core<Vec<u8>, Vec<u8>, SERVER>, observer: &mut Observer) {
    match observer.drive {
        Drive::Steps => {
            let mut visited = HashSet::new();
            while let Some(transition) = core.next_transition() {
                let before = format!("{core:?}");
                assert_eq!(core.next_transition(), Some(transition));
                assert_eq!(before, format!("{core:?}"), "selection changed state");
                assert!(
                    visited.insert(before),
                    "internal transition cycle: {transition:?}"
                );
                assert!(
                    visited.len() <= 512,
                    "internal progress exceeded the fixture bound"
                );
                if transition == Transition::PrepareBody {
                    // Keep the unfused path as the reference for the ownership
                    // model and its continue/yield/mixed callback trace checks.
                    core.prepare_body();
                    assert_eq!(core.next_transition(), Some(Transition::Write));
                } else {
                    core.advance(transition, observer, head);
                }
                core.assert_invariants();
            }
        }
        _ => while core.next(observer, head).is_some() {},
    }
    assert_eq!(core.next_transition(), None, "false quiescence");
    core.assert_invariants();
}

fn io_error(kind: IoErrorKind) -> IoError {
    IoError { kind, code: None }
}

fn receive<const SERVER: bool>(
    core: &mut Core<Vec<u8>, Vec<u8>, SERVER>,
    observer: &mut Observer,
    bytes: &[u8],
) {
    let mut op = observer.read.take().unwrap();
    op.bytes_mut()[..bytes.len()].copy_from_slice(bytes);
    core.complete_read(op.complete(Ok(bytes.len()))).unwrap();
}

fn fixture(input: Input, mode: Drive) -> (Core<Vec<u8>, Vec<u8>, true>, Observer) {
    let config = Config {
        head_timeout_ns: None,
        idle_timeout_ns: None,
        continue_timeout_ns: None,
        body_timeout_ns: Some(100),
        ..Config::default()
    };
    let mut core = Core::new(
        ConnectionId {
            slot: 1,
            generation: 1,
        },
        config,
        vec![0; 128],
        Tick(0),
    )
    .unwrap();
    let mut observer = Observer::new(mode);
    drive(&mut core, &mut observer);
    receive(
        &mut core,
        &mut observer,
        b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 2\r\n\r\n",
    );
    drive(&mut core, &mut observer);
    let exchange = observer.exchange.unwrap();
    core.grant_body_credit(exchange, 2).unwrap();
    drive(&mut core, &mut observer);
    match input {
        Input::Lease => receive(&mut core, &mut observer, b"ab"),
        Input::Readiness => {
            let op = observer.read.take().unwrap();
            core.complete_read(op.complete(Err(io_error(IoErrorKind::WouldBlock))))
                .unwrap();
        }
        Input::Read => {}
        Input::Returned => unreachable!(),
    }
    drive(&mut core, &mut observer);
    core.respond(
        exchange,
        Response {
            head: ResponseHead::new(200, "OK", &[]),
            body: BodyLength::Known(2),
        },
        ResponseMode::Conservative,
    )
    .unwrap();
    drive(&mut core, &mut observer);
    let op = observer.write.take().unwrap();
    let size = op.remaining();
    core.complete_write(op.complete(Ok(size))).unwrap();
    drive(&mut core, &mut observer);
    core.send_body(SendBody {
        exchange,
        buffer: b"xy".to_vec(),
        range: 0..2,
        end: true,
    })
    .unwrap();
    drive(&mut core, &mut observer);
    (core, observer)
}

fn replay(
    input: Input,
    script: &[Stimulus],
    mode: Drive,
) -> (Vec<String>, Vec<(ConnectionId, Tick, LogEvent)>) {
    let (mut core, mut observer) = fixture(input, mode);
    let mut model = Model::new(input);
    for &stimulus in script {
        model.apply(stimulus);
        match stimulus {
            Stimulus::Receive => receive(&mut core, &mut observer, b"ab"),
            Stimulus::ReadReady => {
                core.complete_readiness(observer.read_ready.take().unwrap().complete(Ok(())))
                    .unwrap();
            }
            Stimulus::Release => {
                core.release_body(observer.body.take().unwrap().release(2))
                    .unwrap();
            }
            Stimulus::Write
            | Stimulus::ShortWrite
            | Stimulus::BlockWrite
            | Stimulus::InterruptWrite
            | Stimulus::ResetWrite => {
                let op = observer.write.take().unwrap();
                let result = match stimulus {
                    Stimulus::Write => Ok(op.remaining()),
                    Stimulus::ShortWrite => Ok(1),
                    Stimulus::BlockWrite => Err(io_error(IoErrorKind::WouldBlock)),
                    Stimulus::InterruptWrite => Err(io_error(IoErrorKind::Interrupted)),
                    Stimulus::ResetWrite => Err(io_error(IoErrorKind::Reset)),
                    _ => unreachable!(),
                };
                core.complete_write(op.complete(result)).unwrap();
            }
            Stimulus::WriteReady => {
                core.complete_readiness(observer.write_ready.take().unwrap().complete(Ok(())))
                    .unwrap();
            }
            Stimulus::Abort => core.shutdown(ShutdownMode::Abort),
            Stimulus::Timeout => {
                let deadline = observer.deadline.unwrap();
                core.expire(deadline, deadline.at).unwrap();
            }
        }
        drive(&mut core, &mut observer);
        let actual_input = if observer.read.is_some() {
            Input::Read
        } else if observer.read_ready.is_some() {
            Input::Readiness
        } else if observer.body.is_some() {
            Input::Lease
        } else {
            Input::Returned
        };
        let actual_output = if observer.write.is_some() {
            Output::Write
        } else if observer.write_ready.is_some() {
            Output::Readiness
        } else {
            Output::Returned
        };
        assert_eq!(actual_input, model.input, "{script:?} at {stimulus:?}");
        assert_eq!(actual_output, model.output, "{script:?} at {stimulus:?}");
        assert_eq!(core.failure, model.failure);
        assert_eq!(observer.close.is_some(), model.complete());
        assert_eq!(observer.sources, 1);
        assert_eq!(observer.incoming, usize::from(model.incoming_finished));
        assert_eq!(
            observer.finished,
            usize::from(model.failure.is_some() || model.complete())
        );
        assert_eq!(
            observer.receipts,
            model.receipt.into_iter().collect::<Vec<_>>()
        );
        if model.stopped() {
            for id in [
                observer.read.as_ref().map(ReadOp::id),
                observer.read_ready.map(ReadinessOp::id),
            ]
            .into_iter()
            .flatten()
            {
                assert!(observer.cancelled.contains(&id));
            }
        }
        if model.failure.is_some() {
            for id in [
                observer.write.as_ref().map(WriteOp::id),
                observer.write_ready.map(ReadinessOp::id),
            ]
            .into_iter()
            .flatten()
            {
                assert!(observer.cancelled.contains(&id));
            }
        }
    }
    assert!(
        model.complete(),
        "exploration must reach terminal ownership: {script:?}"
    );
    core.complete_close(observer.close.take().unwrap().complete(Ok(())))
        .unwrap();
    drive(&mut core, &mut observer);
    assert_eq!(observer.closed, [model.failure.map_or(Ok(()), Err)]);
    assert_eq!(observer.sources, 1);
    assert_eq!(observer.receipts.len(), 1);
    assert_eq!(observer.finished, 1);
    assert!(observer.deadline.is_none());
    (observer.trace, observer.logs)
}

fn explore(input: Input, model: Model, script: &mut Vec<Stimulus>, count: &mut usize) {
    assert!(script.len() <= 10, "model has an unbounded external cycle");
    if model.complete() {
        let expected = replay(input, script, Drive::Steps);
        for mode in [Drive::Continue, Drive::Yield, Drive::Mixed] {
            assert_eq!(
                replay(input, script, mode),
                expected,
                "{input:?}: {script:?}"
            );
        }
        *count += 1;
        return;
    }
    let choices = model.choices();
    assert!(!choices.is_empty(), "model deadlock: {model:?}");
    for stimulus in choices {
        let mut next = model;
        next.apply(stimulus);
        script.push(stimulus);
        explore(input, next, script, count);
        script.pop();
    }
}

#[test]
fn bounded_ownership_model_matches_all_completion_orders_and_callback_yields() {
    let mut count = 0;
    for input in [Input::Read, Input::Readiness, Input::Lease] {
        explore(input, Model::new(input), &mut Vec::new(), &mut count);
    }
    assert_eq!(count, 474, "the bounded exploration changed");
    eprintln!("explored {count} terminal schedules in four drive modes");
}

fn client_fixture(
    mode: Drive,
    method: &str,
    body: BodyLength,
) -> (Core<Vec<u8>, Vec<u8>, false>, Observer) {
    let mut core = Core::new(
        ConnectionId {
            slot: 2,
            generation: 1,
        },
        Config {
            head_timeout_ns: Some(100),
            body_timeout_ns: Some(100),
            idle_timeout_ns: None,
            continue_timeout_ns: None,
            ..Config::default()
        },
        vec![0; 128],
        Tick(0),
    )
    .unwrap();
    core.request(Request {
        head: RequestHead {
            method,
            target: if method == "CONNECT" { "a:443" } else { "/" },
            version: Version::Http11,
            headers: &[Header {
                name: "host",
                value: b"a",
            }],
        },
        body,
        expect_continue: false,
    })
    .unwrap();
    let mut observer = Observer::new(mode);
    drive(&mut core, &mut observer);
    let op = observer.write.take().unwrap();
    let size = op.remaining();
    core.complete_write(op.complete(Ok(size))).unwrap();
    (core, observer)
}

#[test]
fn fused_body_write_matches_unfused_successor_and_sequence_exhaustion() {
    for body in [BodyLength::Known(2), BodyLength::Streaming] {
        for end in [false, true] {
            for exhausted in [false, true] {
                let mut reference = None;
                for mode in [Drive::Steps, Drive::Continue, Drive::Yield, Drive::Mixed] {
                    let (mut core, mut observer) = client_fixture(mode, "POST", body);
                    // Streaming uses the configured capacity, not a fixed remainder.
                    // Admit the body before driving so this test needs no demand.
                    core.send_body(SendBody {
                        exchange: core.exchange.as_ref().unwrap().id,
                        buffer: b"xy".to_vec(),
                        range: 0..2,
                        end,
                    })
                    .unwrap();
                    // Flush notifications using the same callbacks, stopping at
                    // the internal boundary whose successor we are exercising.
                    while core.next_transition() != Some(Transition::PrepareBody) {
                        let step = core.next_transition().expect("body must become writable");
                        core.advance(step, &mut observer, head);
                    }
                    if exhausted {
                        core.sequence = u64::MAX - 1;
                    }
                    // One fused advance must have exactly the same externally
                    // visible behavior and state as the two unfused advances.
                    if matches!(mode, Drive::Steps) {
                        core.prepare_body();
                        assert_eq!(core.next_transition(), Some(Transition::Write));
                        core.advance(Transition::Write, &mut observer, head);
                    } else {
                        let result = core.advance(Transition::PrepareBody, &mut observer, head);
                        if matches!(mode, Drive::Yield) && !exhausted {
                            assert_eq!(result, Some(()));
                        }
                    }
                    core.assert_invariants();
                    assert_eq!(observer.write.is_some(), !exhausted);
                    assert_eq!(
                        core.failure,
                        exhausted.then_some(Failure::SequenceExhausted)
                    );
                    let actual = (format!("{core:?}"), observer.trace, observer.logs);
                    if let Some(expected) = &reference {
                        assert_eq!(&actual, expected);
                    } else {
                        reference = Some(actual);
                    }
                }
            }
        }
    }
}

#[test]
fn buffered_response_preempts_both_queued_upload_and_producer_demand() {
    for queued in [false, true] {
        let mut reference = None;
        for mode in [Drive::Steps, Drive::Continue, Drive::Yield, Drive::Mixed] {
            let (mut core, mut observer) = client_fixture(mode, "POST", BodyLength::Known(2));
            observer.expected_receipts = usize::from(queued);
            if queued {
                core.send_body(SendBody {
                    exchange: core.exchange.as_ref().unwrap().id,
                    buffer: b"xy".to_vec(),
                    range: 0..2,
                    end: true,
                })
                .unwrap();
            }
            receive(
                &mut core,
                &mut observer,
                b"HTTP/1.1 413 Too Large\r\nContent-Length: 0\r\n\r\n",
            );
            drive(&mut core, &mut observer);
            assert!(!observer.trace.iter().any(|entry| entry == "demand"));
            assert_eq!(
                observer
                    .trace
                    .iter()
                    .filter(|entry| entry.starts_with("write "))
                    .count(),
                1
            );
            if queued {
                assert_eq!(observer.receipts, [(0, Err(Failure::EarlyResponse))]);
            }
            assert_eq!(observer.sources, 1);
            assert!(observer.close.is_some());
            if let Some(reference) = &reference {
                assert_eq!(&observer.trace, reference);
            } else {
                reference = Some(observer.trace);
            }
        }
    }
}

#[test]
fn upgrade_callback_none_does_not_hide_pending_deadline_cancellation() {
    let (mut core, mut observer) = client_fixture(Drive::Continue, "CONNECT", BodyLength::Empty);
    assert!(observer.deadline.is_some());
    receive(
        &mut core,
        &mut observer,
        b"HTTP/1.1 200 Connected\r\n\r\nnext-protocol",
    );
    assert!(core.next(&mut observer, head).is_none());
    assert_eq!(core.next_transition(), None);
    assert!(observer.deadline.is_none());
    assert_eq!(
        &observer.trace[observer.trace.len() - 2..],
        ["upgrade", "deadline None"]
    );
    let handoff = core.take_upgrade().unwrap();
    assert_eq!(
        &handoff.buffered.buffer[handoff.buffered.range],
        b"next-protocol"
    );
}

#[test]
fn boundary_policy_preserves_completions_batched_before_drive() {
    let mut reference = None;
    for receive_first in [false, true] {
        let (mut core, mut observer) = fixture(Input::Lease, Drive::Steps);
        let body = observer.body.take().unwrap().release(2);
        let op = observer.write.take().unwrap();
        let count = op.remaining();
        let write = op.complete(Ok(count));
        if receive_first {
            core.release_body(body).unwrap();
            core.complete_write(write).unwrap();
        } else {
            core.complete_write(write).unwrap();
            core.release_body(body).unwrap();
        }
        drive(&mut core, &mut observer);
        assert!(observer.trace.iter().any(|entry| entry == "incoming"));
        assert!(observer.close.is_some());
        if let Some(reference) = &reference {
            assert_eq!(&observer.trace, reference);
        } else {
            reference = Some(observer.trace);
        }
    }
}

#[test]
fn clean_idle_eof_cancels_the_deadline_before_close_issuance() {
    let mut core = Core::<Vec<u8>, Vec<u8>, true>::new(
        ConnectionId {
            slot: 3,
            generation: 1,
        },
        Config {
            head_timeout_ns: Some(100),
            ..Config::default()
        },
        vec![0; 128],
        Tick(0),
    )
    .unwrap();
    let mut observer = Observer::new(Drive::Continue);
    drive(&mut core, &mut observer);
    assert!(observer.deadline.is_some());
    core.complete_read(observer.read.take().unwrap().complete(Ok(0)))
        .unwrap();
    assert_eq!(core.next_transition(), Some(Transition::Eof));
    core.advance(Transition::Eof, &mut observer, head);
    assert_eq!(core.next_transition(), Some(Transition::Deadline));
    core.advance(Transition::Deadline, &mut observer, head);
    assert!(observer.deadline.is_none());
    assert_eq!(core.next_transition(), Some(Transition::Close));
    core.assert_invariants();
}

// Independent byte-at-a-time oracle for the bulk scanner's section boundary.
fn scalar_metadata_span(bytes: &[u8], prefix: &[u8], rx: Rx) -> (usize, MetadataEnd) {
    let mut head = prefix.to_vec();
    for (offset, &byte) in bytes.iter().enumerate() {
        if (byte == b'\n' && head.last() != Some(&b'\r'))
            || (head.last() == Some(&b'\r') && byte != b'\n')
        {
            return (offset, MetadataEnd::Invalid);
        }
        head.push(byte);
        let complete = match rx {
            Rx::Head => head.ends_with(b"\r\n\r\n"),
            Rx::Trailers => head == b"\r\n" || head.ends_with(b"\r\n\r\n"),
            Rx::Size | Rx::ChunkCrlf => head.ends_with(b"\r\n"),
            _ => unreachable!(),
        };
        if complete {
            return (offset + 1, MetadataEnd::Complete);
        }
    }
    (bytes.len(), MetadataEnd::Buffered)
}

fn check_metadata_span(bytes: &[u8], prefix: &[u8], rx: Rx) {
    let actual = metadata_span(bytes, prefix, rx);
    assert_eq!(
        actual,
        scalar_metadata_span(bytes, prefix, rx),
        "rx={rx:?} prefix={prefix:?} bytes={bytes:?}"
    );
    if actual.1 == MetadataEnd::Complete {
        let mut accepted = prefix.to_vec();
        accepted.extend_from_slice(&bytes[..actual.0]);
        // This is the invariant that permits process_metadata to omit its
        // second CRLF scan, including in optimized builds of these tests.
        assert!(codec::strict_lines(&accepted));
    }
}

#[test]
fn bulk_metadata_matches_scalar_for_all_short_crlf_sequences() {
    let prefixes: &[&[u8]] = &[
        b"",
        b"x",
        b"x\r",
        b"x\r\n",
        b"x\r\nx",
        b"x\r\nx\r",
        b"x\r\n\r",
        b"\r",
        b"\r\n",
    ];
    for rx in [Rx::Head, Rx::Trailers, Rx::Size, Rx::ChunkCrlf] {
        for &prefix in prefixes {
            if scalar_metadata_span(prefix, b"", rx).1 != MetadataEnd::Buffered {
                continue;
            }
            for length in 0..=7 {
                for mut pattern in 0..3usize.pow(length) {
                    let bytes: Vec<_> = (0..length)
                        .map(|_| {
                            let byte = b"x\r\n"[pattern % 3];
                            pattern /= 3;
                            byte
                        })
                        .collect();
                    check_metadata_span(&bytes, prefix, rx);
                }
            }
        }
    }
}

#[test]
fn bulk_metadata_matches_scalar_across_long_and_binary_fragments() {
    for length in [
        0, 1, 15, 16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 129, 1024,
    ] {
        for fill in [b'x', 0, 255] {
            for suffix in [b"\r\nY: z\r\n\r\nUNREAD\n".as_slice(), b"\n", b"\rX"] {
                let mut bytes = vec![fill; length];
                bytes.extend_from_slice(suffix);
                for rx in [Rx::Head, Rx::Trailers, Rx::Size, Rx::ChunkCrlf] {
                    for split in 0..=bytes.len() {
                        let (prefix, input) = bytes.split_at(split);
                        if scalar_metadata_span(prefix, b"", rx).1 != MetadataEnd::Buffered {
                            continue;
                        }
                        check_metadata_span(input, prefix, rx);
                    }
                }
            }
        }
    }
}

#[test]
fn bulk_metadata_preserves_limit_error_precedence_and_consumed_prefix() {
    for limit in [16, 20] {
        for retained in [0, 3, usize::MAX] {
            for length in [0, 15, 16, 19, 20] {
                for trailing_cr in [false, true] {
                    for pending in [false, true] {
                        for input in [b"x".as_slice(), b"\n", b"\r\n", b"\rX", b"abc\r\nsuffix"] {
                            let mut prefix = vec![b'A'; length];
                            if trailing_cr && let Some(last) = prefix.last_mut() {
                                *last = b'\r';
                            }
                            let mut expected_head = prefix.clone();
                            let mut expected_failure = None;
                            let mut consumed = 0;
                            for &byte in input {
                                consumed += 1;
                                if (byte == b'\n' && expected_head.last() != Some(&b'\r'))
                                    || (expected_head.last() == Some(&b'\r') && byte != b'\n')
                                {
                                    expected_failure = Some(Failure::Protocol);
                                    break;
                                }
                                if expected_head.len().saturating_add(retained) >= limit {
                                    expected_failure = Some(Failure::Limit);
                                    break;
                                }
                                expected_head.push(byte);
                                assert!(!expected_head.ends_with(b"\r\n\r\n"));
                                if pending {
                                    break;
                                }
                            }
                            let mut core = Core::<Vec<u8>, Vec<u8>, true>::new(
                                ConnectionId {
                                    slot: 4,
                                    generation: 1,
                                },
                                Config {
                                    max_head_bytes: limit,
                                    head_timeout_ns: None,
                                    ..Config::default()
                                },
                                vec![0; 64],
                                Tick(0),
                            )
                            .unwrap();
                            core.head = prefix;
                            core.metadata_bytes = retained;
                            core.start = 3;
                            core.end = 3 + input.len();
                            let ReceiveStorage::Available(buffer) = &mut core.receive else {
                                unreachable!()
                            };
                            buffer[core.start..core.end].copy_from_slice(input);
                            core.timers.notification = if pending {
                                Notification::Pending
                            } else {
                                Notification::Delivered
                            };
                            let mut observer = Observer::new(Drive::Continue);
                            assert!(core.receive_metadata(&mut observer, head).is_none());
                            assert_eq!(core.failure, expected_failure);
                            assert_eq!(core.head, expected_head);
                            assert_eq!(core.start, 3 + consumed);
                            assert!(observer.trace.is_empty());
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn metadata_batches_only_until_a_section_or_deadline_boundary() {
    let request = b"GET / HTTP/1.1\r\nHost: a\r\n\r\n";
    for idle in [false, true] {
        let mut core = Core::<Vec<u8>, Vec<u8>, true>::new(
            ConnectionId {
                slot: 4,
                generation: 1,
            },
            Config {
                head_timeout_ns: Some(100),
                idle_timeout_ns: Some(200),
                ..Config::default()
            },
            vec![0; 128],
            Tick(0),
        )
        .unwrap();
        let mut observer = Observer::new(Drive::Yield);
        drive(&mut core, &mut observer);
        receive(
            &mut core,
            &mut observer,
            &[request.as_slice(), request.as_slice()].concat(),
        );
        if idle {
            core.rx = Rx::AwaitingRequest;
            core.set_deadline(TimerPhase::Idle, Some(200)).unwrap();
            core.advance(Transition::Deadline, &mut observer, head);
        }
        observer.trace.clear();
        assert_eq!(core.next_transition(), Some(Transition::Metadata));
        core.advance(Transition::Metadata, &mut observer, head);
        if idle {
            assert_eq!(core.start, 1);
            assert!(observer.trace.is_empty());
            assert_eq!(core.next_transition(), Some(Transition::Deadline));
            core.advance(Transition::Deadline, &mut observer, head);
            assert_eq!(observer.deadline.unwrap().at, Tick(100));
            assert_eq!(core.next_transition(), Some(Transition::Metadata));
            core.advance(Transition::Metadata, &mut observer, head);
        }
        assert_eq!(core.start, request.len());
        assert_eq!(observer.trace.last().unwrap(), "request");
        assert_eq!(observer.trace.len(), if idle { 2 } else { 1 });
        assert_eq!(core.rx, Rx::Done);
        assert_eq!(core.end - core.start, request.len());
        core.assert_invariants();
    }
}
