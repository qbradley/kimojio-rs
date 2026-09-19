use super::*;
use std::collections::HashSet;

type Machine = Server<Vec<u8>, Vec<u8>>;

#[derive(Clone, Copy, Debug)]
enum Drive {
    Steps,
    Continue,
    Yield,
    Mixed,
}

struct Probe {
    mode: Drive,
    read: Option<ReadOp<Vec<u8>>>,
    write: Option<WriteOp<Vec<u8>>>,
    readable: Option<ReadinessOp>,
    writable: Option<ReadinessOp>,
    chunk: Option<ChunkOp<Vec<u8>>>,
    close: Option<CloseOp>,
    receipt: Option<MessageSent<Vec<u8>>>,
    closed: Option<ConnectionResult>,
    deadline: Option<Deadline>,
    cancelled: HashSet<OperationId>,
    issued: HashSet<OperationId>,
    trace: Vec<String>,
    wire: Vec<u8>,
    expect_receipt: bool,
}

impl Probe {
    fn new(mode: Drive) -> Self {
        Self {
            mode,
            read: None,
            write: None,
            readable: None,
            writable: None,
            chunk: None,
            close: None,
            receipt: None,
            closed: None,
            deadline: None,
            cancelled: HashSet::new(),
            issued: HashSet::new(),
            trace: Vec::new(),
            wire: Vec::new(),
            expect_receipt: false,
        }
    }
    fn record(&mut self, event: String) -> Option<()> {
        self.trace.push(event);
        assert!(self.trace.len() < 256);
        match self.mode {
            Drive::Steps | Drive::Continue => None,
            Drive::Yield => Some(()),
            Drive::Mixed => self.trace.len().is_multiple_of(3).then_some(()),
        }
    }
    fn own(&mut self, id: OperationId) {
        assert!(self.issued.insert(id), "duplicate issuance: {id:?}");
    }
    fn outstanding(&self) -> bool {
        self.read.is_some()
            || self.readable.is_some()
            || self.chunk.is_some()
            || self.write.is_some()
            || self.writable.is_some()
    }
}

impl Ports<Vec<u8>> for Probe {
    type Output = ();
    fn read(&mut self, op: ReadOp<Vec<u8>>) -> Option<()> {
        assert!(self.read.is_none() && self.readable.is_none() && self.chunk.is_none());
        self.own(op.id());
        self.read = Some(op);
        self.record("read".into())
    }
    fn write(&mut self, op: WriteOp<Vec<u8>>) -> Option<()> {
        assert!(self.write.is_none() && self.writable.is_none());
        self.own(op.id());
        let event = format!("write {:?}", op.slices());
        self.write = Some(op);
        self.record(event)
    }
    fn readiness(&mut self, op: ReadinessOp) -> Option<()> {
        self.own(op.id());
        match op.direction {
            Direction::Read => {
                assert!(self.read.is_none() && self.readable.is_none() && self.chunk.is_none());
                self.readable = Some(op);
            }
            Direction::Write => {
                assert!(self.write.is_none() && self.writable.is_none());
                self.writable = Some(op);
            }
        }
        self.record(format!("ready {:?}", op.direction))
    }
    fn cancel(&mut self, op: CancelOp) -> Option<()> {
        assert!(self.cancelled.insert(op.target), "duplicate cancellation");
        assert!(
            [
                self.read.as_ref().map(ReadOp::id),
                self.write.as_ref().map(WriteOp::id),
                self.readable.map(ReadinessOp::id),
                self.writable.map(ReadinessOp::id),
            ]
            .contains(&Some(op.target)),
            "cancellation lost the original owner"
        );
        self.record(format!("cancel {:?}", op.target))
    }
    fn close(&mut self, op: CloseOp) -> Option<()> {
        assert!(
            !self.outstanding(),
            "close preceded original operation settlement"
        );
        assert_eq!(self.receipt.is_some(), self.expect_receipt);
        assert!(self.close.is_none() && self.closed.is_none());
        self.own(op.id());
        self.close = Some(op);
        self.record("close".into())
    }
    fn message_started(&mut self, info: MessageInfo) -> Option<()> {
        self.record(format!("start {:?}", info))
    }
    fn chunk(&mut self, op: ChunkOp<Vec<u8>>) -> Option<()> {
        assert!(self.read.is_none() && self.readable.is_none() && self.chunk.is_none());
        self.own(op.id());
        let event = format!("chunk {:?}", op.bytes());
        self.chunk = Some(op);
        self.record(event)
    }
    fn message_finished(&mut self, info: MessageInfo) -> Option<()> {
        self.record(format!("finish {:?}", info))
    }
    fn message_sent(&mut self, receipt: MessageSent<Vec<u8>>) -> Option<()> {
        assert!(self.receipt.is_none(), "duplicate receipt");
        let event = format!(
            "receipt {} {:?} {:?}",
            receipt.accepted, receipt.acceptance, receipt.result
        );
        self.receipt = Some(receipt);
        self.record(event)
    }
    fn peer_closed(&mut self, reason: CloseReason) -> Option<()> {
        self.record(format!("peer {:?}", reason))
    }
    fn deadline_changed(&mut self, deadline: Option<Deadline>) -> Option<()> {
        self.deadline = deadline;
        self.record(format!("deadline {:?}", deadline))
    }
    fn closed(&mut self, result: ConnectionResult) -> Option<()> {
        assert!(self.closed.is_none());
        self.closed = Some(result);
        self.record(format!("closed {:?}", result))
    }
}

fn machine() -> Machine {
    Server::new(
        ConnectionId {
            slot: 1,
            generation: 1,
        },
        Config {
            outgoing_frame_bytes: 2,
            ..Config::default()
        },
        vec![0; 32],
        Tick(0),
    )
    .unwrap()
}

fn drive(machine: &mut Machine, probe: &mut Probe) {
    if matches!(probe.mode, Drive::Steps) {
        let mut visited = HashSet::new();
        while let Some(step) = machine.next_transition() {
            let before = format!("{machine:?}");
            assert_eq!(machine.next_transition(), Some(step));
            assert_eq!(format!("{machine:?}"), before, "selection mutated state");
            assert!(
                visited.insert(before),
                "internal transition cycle: {step:?}"
            );
            assert!(
                visited.len() < 128,
                "fixture exceeded its internal work bound"
            );
            machine.advance(step, probe);
            machine.assert_invariants();
        }
    } else {
        while machine.next(probe).is_some() {}
    }
    machine.assert_invariants();
    assert_eq!(machine.next_transition(), None, "false quiescence");
}

fn receive(machine: &mut Machine, probe: &mut Probe, bytes: &[u8]) {
    let mut op = probe.read.take().unwrap();
    op.bytes_mut()[..bytes.len()].copy_from_slice(bytes);
    machine.complete_read(op.complete(Ok(bytes.len()))).unwrap();
}

fn error(kind: IoErrorKind) -> IoError {
    IoError { kind, code: None }
}

fn write(machine: &mut Machine, probe: &mut Probe, result: IoResult<usize>) {
    let op = probe.write.take().unwrap();
    if let Ok(n) = result {
        probe.wire.extend_from_slice(&op.slices().concat()[..n]);
    }
    machine.complete_write(op.complete(result)).unwrap();
}

#[derive(Clone, Copy, Debug)]
enum InputOwner {
    Read,
    Readiness,
    Chunk,
}
#[derive(Clone, Copy, Debug)]
enum OutputOwner {
    Kernel,
    Queued,
    Partial,
    Readiness,
    PartialReadiness,
}

fn abort_case(
    input: InputOwner,
    output: OutputOwner,
    timeout: bool,
    write_first: bool,
    completion: IoResult<usize>,
    mode: Drive,
) -> Vec<String> {
    let mut machine = machine();
    let mut probe = Probe::new(mode);
    drive(&mut machine, &mut probe);
    match input {
        InputOwner::Read => {}
        InputOwner::Readiness => {
            let op = probe.read.take().unwrap();
            machine
                .complete_read(op.complete(Err(error(IoErrorKind::WouldBlock))))
                .unwrap();
        }
        InputOwner::Chunk => receive(
            &mut machine,
            &mut probe,
            &[0x82, 0x82, 1, 2, 3, 4, b'a' ^ 1, b'b' ^ 2],
        ),
    }
    drive(&mut machine, &mut probe);
    let payload = vec![1, 2, 3, 4];
    let pointer = payload.as_ptr();
    probe.expect_receipt = true;
    machine
        .send_message(SendMessage {
            kind: MessageKind::Binary,
            buffer: payload,
            range: 0..4,
        })
        .unwrap();
    drive(&mut machine, &mut probe);
    match output {
        OutputOwner::Kernel => {}
        OutputOwner::Queued => write(
            &mut machine,
            &mut probe,
            Err(error(IoErrorKind::Interrupted)),
        ),
        OutputOwner::Partial => write(&mut machine, &mut probe, Ok(3)),
        OutputOwner::Readiness => {
            write(
                &mut machine,
                &mut probe,
                Err(error(IoErrorKind::WouldBlock)),
            );
            drive(&mut machine, &mut probe);
        }
        OutputOwner::PartialReadiness => {
            write(&mut machine, &mut probe, Ok(3));
            drive(&mut machine, &mut probe);
            write(
                &mut machine,
                &mut probe,
                Err(error(IoErrorKind::WouldBlock)),
            );
            drive(&mut machine, &mut probe);
        }
    }
    let failure = if timeout {
        let deadline = probe.deadline.unwrap();
        machine.expire(deadline, deadline.at).unwrap();
        Failure::Timeout
    } else {
        machine.abort(Failure::Cancelled);
        Failure::Cancelled
    };
    drive(&mut machine, &mut probe);
    assert!(machine.is_closing() && !machine.can_send());
    assert!(probe.close.is_none());

    // The oracle uses original resource ownership and exact write counts only.
    let expected_accepted = match output {
        OutputOwner::Kernel => completion.unwrap_or(0).saturating_sub(2),
        OutputOwner::Partial | OutputOwner::PartialReadiness => 1,
        _ => 0,
    };
    let lower_bound = matches!(output, OutputOwner::Kernel)
        && completion.is_err_and(|e| {
            matches!(
                e.kind,
                IoErrorKind::UnknownProgress | IoErrorKind::CancelledUnknownProgress
            )
        });
    assert_eq!(
        probe.receipt.is_some(),
        !matches!(output, OutputOwner::Kernel)
    );
    let after_abort = probe.trace.len();
    for settle_write in [write_first, !write_first] {
        if settle_write {
            if probe.write.is_some() {
                write(&mut machine, &mut probe, completion);
            } else if let Some(ready) = probe.writable.take() {
                assert!(probe.cancelled.contains(&ready.id()));
                machine
                    .complete_readiness(ready.complete(Err(error(IoErrorKind::Cancelled))))
                    .unwrap();
            }
        } else if let Some(read) = probe.read.take() {
            assert!(probe.cancelled.contains(&read.id()));
            machine
                .complete_read(read.complete(Err(error(IoErrorKind::Cancelled))))
                .unwrap();
        } else if let Some(ready) = probe.readable.take() {
            assert!(probe.cancelled.contains(&ready.id()));
            machine
                .complete_readiness(ready.complete(Err(error(IoErrorKind::Cancelled))))
                .unwrap();
        } else {
            machine
                .release_chunk(probe.chunk.take().unwrap().release())
                .unwrap();
        }
        drive(&mut machine, &mut probe);
        assert_eq!(probe.close.is_some(), !probe.outstanding());
    }
    let receipt = probe.receipt.as_ref().unwrap();
    assert_eq!(receipt.buffer.as_ptr(), pointer);
    assert_eq!(receipt.buffer, [1, 2, 3, 4]);
    assert_eq!(receipt.result, Err(failure));
    assert_eq!(receipt.accepted, expected_accepted);
    assert_eq!(
        receipt.acceptance,
        if lower_bound {
            Acceptance::LowerBound
        } else {
            Acceptance::Exact
        }
    );
    let close = probe.close.take().unwrap();
    for _ in 0..3 {
        machine.abort(failure);
        drive(&mut machine, &mut probe);
        assert!(
            probe.close.is_none(),
            "original close identity was replaced"
        );
    }
    machine.complete_close(close.complete(Ok(()))).unwrap();
    drive(&mut machine, &mut probe);
    let closed = probe.closed.unwrap();
    assert_eq!(closed.result, Err(failure));
    assert!(!closed.clean && closed.peer_close.is_none());
    assert!(probe.deadline.is_none());
    assert!(
        probe.trace[after_abort..]
            .iter()
            .all(|event| !event.starts_with("write ")
                && event != "read"
                && !event.starts_with("ready ")
                && !event.starts_with("finish ")
                && !event.starts_with("chunk "))
    );
    probe.trace
}

#[test]
fn bounded_termination_model_preserves_ownership_in_every_completion_order() {
    let mut schedules = 0;
    for input in [InputOwner::Read, InputOwner::Readiness, InputOwner::Chunk] {
        for output in [
            OutputOwner::Kernel,
            OutputOwner::Queued,
            OutputOwner::Partial,
            OutputOwner::Readiness,
            OutputOwner::PartialReadiness,
        ] {
            let completions = if matches!(output, OutputOwner::Kernel) {
                vec![
                    Ok(1),
                    Ok(4),
                    Err(error(IoErrorKind::Cancelled)),
                    Err(error(IoErrorKind::UnknownProgress)),
                    Err(error(IoErrorKind::CancelledUnknownProgress)),
                ]
            } else {
                vec![Err(error(IoErrorKind::Cancelled))]
            };
            for completion in completions {
                for timeout in [false, true] {
                    for write_first in [false, true] {
                        let reference = abort_case(
                            input,
                            output,
                            timeout,
                            write_first,
                            completion,
                            Drive::Steps,
                        );
                        for mode in [Drive::Continue, Drive::Yield, Drive::Mixed] {
                            assert_eq!(
                                abort_case(input, output, timeout, write_first, completion, mode),
                                reference
                            );
                        }
                        schedules += 1;
                    }
                }
            }
        }
    }
    assert_eq!(schedules, 108);
}

#[test]
fn close_handshake_waits_for_both_wire_directions_in_either_order() {
    for peer_first in [false, true] {
        let mut reference = None;
        for mode in [Drive::Steps, Drive::Continue, Drive::Yield, Drive::Mixed] {
            let mut machine = machine();
            let mut probe = Probe::new(mode);
            drive(&mut machine, &mut probe);
            machine.close(CloseReason::empty()).unwrap();
            drive(&mut machine, &mut probe);
            for peer in [peer_first, !peer_first] {
                if peer {
                    receive(&mut machine, &mut probe, &[0x88, 0x80, 1, 2, 3, 4]);
                } else {
                    write(&mut machine, &mut probe, Ok(2));
                }
                drive(&mut machine, &mut probe);
                assert_eq!(probe.close.is_some(), !probe.outstanding());
            }
            let close = probe.close.take().unwrap();
            machine.complete_close(close.complete(Ok(()))).unwrap();
            drive(&mut machine, &mut probe);
            assert_eq!(probe.wire, [0x88, 0]);
            assert!(probe.closed.unwrap().clean);
            assert_eq!(
                probe
                    .trace
                    .iter()
                    .filter(|e| e.starts_with("peer "))
                    .count(),
                1
            );
            assert_eq!(
                probe
                    .trace
                    .iter()
                    .filter(|e| e.starts_with("closed "))
                    .count(),
                1
            );
            if let Some(reference) = &reference {
                assert_eq!(&probe.trace, reference);
            } else {
                reference = Some(probe.trace);
            }
        }
    }
}

#[test]
fn peer_close_settles_a_cancelled_partial_pong_without_new_output() {
    for mode in [Drive::Steps, Drive::Continue, Drive::Yield, Drive::Mixed] {
        let mut machine = machine();
        let mut probe = Probe::new(mode);
        drive(&mut machine, &mut probe);
        machine.close(CloseReason::empty()).unwrap();
        drive(&mut machine, &mut probe);
        write(&mut machine, &mut probe, Ok(2));
        drive(&mut machine, &mut probe);
        receive(
            &mut machine,
            &mut probe,
            &[0x89, 0x81, 1, 2, 3, 4, b'?' ^ 1],
        );
        drive(&mut machine, &mut probe);
        let pong_id = probe.write.as_ref().unwrap().id();
        receive(&mut machine, &mut probe, &[0x88, 0x80, 1, 2, 3, 4]);
        drive(&mut machine, &mut probe);
        assert!(probe.cancelled.contains(&pong_id));
        assert!(probe.close.is_none());
        write(&mut machine, &mut probe, Ok(1));
        drive(&mut machine, &mut probe);
        assert!(machine.output.is_none());
        assert!(probe.write.is_none());
        let close = probe.close.take().unwrap();
        machine.complete_close(close.complete(Ok(()))).unwrap();
        drive(&mut machine, &mut probe);
        assert!(probe.closed.unwrap().clean);
        assert_eq!(probe.wire, [0x88, 0, 0x8a]);
    }
}

#[test]
fn exhaustion_retains_cleanup_authority_and_does_not_spin() {
    for timer in [false, true] {
        let mut machine = machine();
        if timer {
            machine.timers.sequence = u64::MAX;
        } else {
            machine.sequence = u64::MAX - 1;
        }
        let mut probe = Probe::new(Drive::Steps);
        drive(&mut machine, &mut probe);
        assert_eq!(machine.failure, Some(Failure::SequenceExhausted));
        let close = probe.close.take().unwrap();
        assert_eq!(close.id().sequence(), u64::MAX);
        machine.complete_close(close.complete(Ok(()))).unwrap();
        drive(&mut machine, &mut probe);
        assert_eq!(
            probe.closed.unwrap().result,
            Err(Failure::SequenceExhausted)
        );
    }
}
