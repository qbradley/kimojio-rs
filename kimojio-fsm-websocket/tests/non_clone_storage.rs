use kimojio_fsm_websocket::*;
use std::{cell::Cell, ops::Range, rc::Rc};

// Deliberately no Clone, AsMut, or shared ownership of the actual bytes.
#[derive(Debug)]
struct Payload {
    bytes: Box<[u8]>,
    drops: Rc<Cell<usize>>,
}

impl AsRef<[u8]> for Payload {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for Payload {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}

struct Probe {
    address: usize,
    length: usize,
    drops: Rc<Cell<usize>>,
}

impl Probe {
    fn new(bytes: &[u8]) -> (Self, Payload) {
        let drops = Rc::new(Cell::new(0));
        let payload = Payload {
            bytes: bytes.into(),
            drops: drops.clone(),
        };
        (
            Self {
                address: payload.as_ref().as_ptr() as usize,
                length: bytes.len(),
                drops,
            },
            payload,
        )
    }

    fn retained(&self) {
        assert_eq!(
            self.drops.get(),
            0,
            "payload dropped before caller ownership"
        );
    }

    fn returned(&self, payload: &Payload) {
        self.retained();
        assert_eq!(payload.as_ref().as_ptr() as usize, self.address);
        assert_eq!(payload.as_ref().len(), self.length);
        assert!(Rc::ptr_eq(&payload.drops, &self.drops));
    }

    fn drop_once(self, payload: Payload) {
        self.returned(&payload);
        drop(payload);
        assert_eq!(self.drops.get(), 1);
        assert_eq!(
            Rc::strong_count(&self.drops),
            1,
            "retained payload owner leaked"
        );
    }
}

type Machine = Server<Vec<u8>, Payload>;

#[derive(Default)]
struct Capture {
    read: Option<ReadOp<Vec<u8>>>,
    write: Option<WriteOp<Payload>>,
    close: Option<CloseOp>,
    cancels: Vec<CancelOp>,
    receipt: Option<MessageSent<Payload>>,
    receipt_count: usize,
    closed: Option<ConnectionResult>,
    trace: Vec<&'static str>,
}

impl Ports<Vec<u8>, Payload> for Capture {
    type Output = ();

    fn read(&mut self, op: ReadOp<Vec<u8>>) -> Option<()> {
        assert!(self.read.replace(op).is_none());
        None
    }

    fn write(&mut self, op: WriteOp<Payload>) -> Option<()> {
        assert!(self.write.replace(op).is_none());
        None
    }

    fn readiness(&mut self, _: ReadinessOp) -> Option<()> {
        panic!("unexpected readiness");
    }

    fn cancel(&mut self, op: CancelOp) -> Option<()> {
        assert!(self.cancels.iter().all(|old| old.target != op.target));
        self.cancels.push(op);
        None
    }

    fn close(&mut self, op: CloseOp) -> Option<()> {
        assert!(self.read.is_none() && self.write.is_none());
        assert_eq!(self.receipt_count, 1);
        assert!(self.close.replace(op).is_none());
        self.trace.push("close issued");
        None
    }

    fn message_started(&mut self, _: MessageInfo) -> Option<()> {
        panic!("unexpected incoming message");
    }

    fn chunk(&mut self, _: ChunkOp<Vec<u8>>) -> Option<()> {
        panic!("unexpected incoming chunk");
    }

    fn message_finished(&mut self, _: MessageInfo) -> Option<()> {
        panic!("unexpected incoming completion");
    }

    fn message_sent(&mut self, receipt: MessageSent<Payload>) -> Option<()> {
        assert_eq!(receipt.buffer.drops.get(), 0);
        assert!(self.closed.is_none());
        assert!(self.trace.contains(&"write settled"));
        assert!(self.receipt.replace(receipt).is_none());
        self.receipt_count += 1;
        assert_eq!(self.receipt_count, 1, "duplicate payload return");
        self.trace.push("payload returned");
        None
    }

    fn peer_closed(&mut self, reason: CloseReason) -> Option<()> {
        assert_eq!(reason.code(), Some(1000));
        self.trace.push("peer closed");
        None
    }

    fn deadline_changed(&mut self, _: Option<Deadline>) -> Option<()> {
        None
    }

    fn closed(&mut self, result: ConnectionResult) -> Option<()> {
        assert!(self.read.is_none() && self.write.is_none() && self.close.is_none());
        assert_eq!(self.receipt_count, 1);
        assert!(self.trace.contains(&"read settled"));
        assert!(self.trace.contains(&"close settled"));
        assert!(self.closed.replace(result).is_none());
        self.trace.push("closed");
        None
    }
}

fn machine(config: Config) -> Machine {
    Server::new(
        ConnectionId {
            slot: 1,
            generation: 2,
        },
        config,
        vec![0; 128],
        Tick(0),
    )
    .unwrap()
}

fn drive(machine: &mut Machine, ports: &mut Capture, probe: &Probe) {
    assert!(machine.next(ports).is_none());
    probe.retained();
}

fn io_error(kind: IoErrorKind) -> IoError {
    IoError { kind, code: None }
}

fn settle_read(machine: &mut Machine, ports: &mut Capture, result: IoResult<usize>) {
    let op = ports.read.take().unwrap();
    machine.complete_read(op.complete(result)).unwrap();
    ports.trace.push("read settled");
}

fn settle_write(machine: &mut Machine, ports: &mut Capture, result: IoResult<usize>) {
    let op = ports.write.take().unwrap();
    machine.complete_write(op.complete(result)).unwrap();
    ports.trace.push("write settled");
}

fn finish_close(machine: &mut Machine, ports: &mut Capture, probe: &Probe) {
    drive(machine, ports, probe);
    let close = ports
        .close
        .take()
        .expect("close after original operations settle");
    assert!(ports.closed.is_none());
    machine.complete_close(close.complete(Ok(()))).unwrap();
    ports.trace.push("close settled");
    drive(machine, ports, probe);
    assert!(ports.closed.is_some());
    let returned = ports
        .trace
        .iter()
        .position(|entry| *entry == "payload returned")
        .unwrap();
    let closed = ports
        .trace
        .iter()
        .position(|entry| *entry == "closed")
        .unwrap();
    assert!(returned < closed);
    for _ in 0..3 {
        drive(machine, ports, probe);
    }
    assert_eq!(ports.receipt_count, 1);
}

fn assert_slice_identity(op: &WriteOp<Payload>, probe: &Probe) {
    let payload = op.slices()[1];
    let address = payload.as_ptr() as usize;
    assert!(address >= probe.address);
    assert!(address + payload.len() <= probe.address + probe.length);
}

#[test]
fn non_clone_fragmented_short_writes_return_original_before_closed() {
    let (probe, payload) = Probe::new(b"abcdefgh");
    let mut machine = machine(Config {
        outgoing_frame_bytes: 3,
        ..Config::default()
    });
    let id = machine
        .send_message(SendMessage {
            kind: MessageKind::Binary,
            buffer: payload,
            range: 0..8,
        })
        .unwrap();
    let mut ports = Capture::default();
    let mut wire = Vec::new();
    for _ in 0..32 {
        drive(&mut machine, &mut ports, &probe);
        if ports.receipt.is_some() {
            break;
        }
        let write = ports.write.as_ref().unwrap();
        assert_slice_identity(write, &probe);
        let byte = write
            .slices()
            .into_iter()
            .find(|slice| !slice.is_empty())
            .unwrap()[0];
        wire.push(byte);
        settle_write(&mut machine, &mut ports, Ok(1));
        probe.retained();
    }
    assert_eq!(wire, b"\x02\x03abc\x00\x03def\x80\x02gh");
    let receipt = ports.receipt.take().unwrap();
    probe.returned(&receipt.buffer);
    assert_eq!(receipt.id, id);
    assert_eq!(receipt.accepted, 8);
    assert_eq!(receipt.acceptance, Acceptance::Exact);
    assert_eq!(receipt.result, Ok(()));

    machine.abort(Failure::Cancelled);
    drive(&mut machine, &mut ports, &probe);
    assert_eq!(ports.cancels.len(), 1);
    assert_eq!(ports.cancels[0].target, ports.read.as_ref().unwrap().id());
    settle_read(
        &mut machine,
        &mut ports,
        Err(io_error(IoErrorKind::Cancelled)),
    );
    finish_close(&mut machine, &mut ports, &probe);
    drop(machine);
    drop(ports);
    probe.drop_once(receipt.buffer);
}

#[test]
fn non_clone_rejected_commands_return_original_without_drop() {
    let cases: &[(&[u8], MessageKind, Range<usize>, RejectReason)] = &[
        (
            b"abc",
            MessageKind::Binary,
            0..4,
            RejectReason::InvalidRange,
        ),
        (&[0xff], MessageKind::Text, 0..1, RejectReason::InvalidState),
        (b"abcd", MessageKind::Binary, 0..4, RejectReason::Limit),
    ];
    for (bytes, kind, range, reason) in cases {
        let (probe, payload) = Probe::new(bytes);
        let mut machine = machine(Config {
            max_message_bytes: 3,
            ..Config::default()
        });
        let rejected = machine
            .send_message(SendMessage {
                kind: *kind,
                buffer: payload,
                range: range.clone(),
            })
            .unwrap_err();
        assert_eq!(rejected.reason, *reason);
        assert_eq!(rejected.value.kind, *kind);
        assert_eq!(rejected.value.range, *range);
        probe.returned(&rejected.value.buffer);
        assert!(machine.can_send());
        drop(machine);
        probe.drop_once(rejected.value.buffer);
    }
}

#[test]
fn non_clone_abort_waits_for_original_completions_and_returns_one_receipt() {
    for write_first in [false, true] {
        for result in [
            Ok(1),
            Ok(3),
            Err(io_error(IoErrorKind::Cancelled)),
            Err(io_error(IoErrorKind::UnknownProgress)),
            Err(io_error(IoErrorKind::CancelledUnknownProgress)),
        ] {
            let (probe, payload) = Probe::new(b"abcd");
            let mut machine = machine(Config::default());
            machine
                .send_message(SendMessage {
                    kind: MessageKind::Binary,
                    buffer: payload,
                    range: 0..4,
                })
                .unwrap();
            let mut ports = Capture::default();
            drive(&mut machine, &mut ports, &probe);
            settle_write(&mut machine, &mut ports, Ok(3));
            drive(&mut machine, &mut ports, &probe);
            assert_eq!(ports.write.as_ref().unwrap().slices().concat(), b"bcd");
            assert_slice_identity(ports.write.as_ref().unwrap(), &probe);

            let (rejected_probe, rejected_payload) = Probe::new(b"busy");
            let rejected = machine
                .send_message(SendMessage {
                    kind: MessageKind::Binary,
                    buffer: rejected_payload,
                    range: 0..4,
                })
                .unwrap_err();
            assert_eq!(rejected.reason, RejectReason::NoCapacity);
            rejected_probe.drop_once(rejected.value.buffer);
            probe.retained();

            machine.abort(Failure::Cancelled);
            drive(&mut machine, &mut ports, &probe);
            assert_eq!(ports.cancels.len(), 2);
            assert!(
                ports
                    .cancels
                    .iter()
                    .any(|cancel| cancel.target == ports.read.as_ref().unwrap().id())
            );
            assert!(
                ports
                    .cancels
                    .iter()
                    .any(|cancel| cancel.target == ports.write.as_ref().unwrap().id())
            );
            assert!(ports.receipt.is_none() && ports.close.is_none());
            for _ in 0..3 {
                drive(&mut machine, &mut ports, &probe);
            }
            for first in [true, false] {
                if first == write_first {
                    settle_write(&mut machine, &mut ports, result);
                } else {
                    settle_read(
                        &mut machine,
                        &mut ports,
                        Err(io_error(IoErrorKind::Cancelled)),
                    );
                }
                drive(&mut machine, &mut ports, &probe);
                if first {
                    assert!(ports.close.is_none() && ports.closed.is_none());
                    assert_eq!(ports.receipt.is_some(), write_first);
                }
            }
            finish_close(&mut machine, &mut ports, &probe);
            assert_eq!(ports.closed.unwrap().result, Err(Failure::Cancelled));
            let receipt = ports.receipt.take().unwrap();
            assert_eq!(receipt.result, Err(Failure::Cancelled));
            assert_eq!(receipt.accepted, 1 + result.unwrap_or(0));
            let uncertain = result.is_err_and(|error| {
                matches!(
                    error.kind,
                    IoErrorKind::UnknownProgress | IoErrorKind::CancelledUnknownProgress
                )
            });
            assert_eq!(
                receipt.acceptance,
                if uncertain {
                    Acceptance::LowerBound
                } else {
                    Acceptance::Exact
                }
            );
            probe.returned(&receipt.buffer);
            drop(machine);
            drop(ports);
            probe.drop_once(receipt.buffer);
        }
    }
}

#[test]
fn non_clone_pending_write_failures_return_storage_before_closed() {
    for source_failure in [false, true] {
        let (probe, payload) = Probe::new(b"abcdef");
        let mut machine = machine(Config {
            outgoing_frame_bytes: 3,
            ..Config::default()
        });
        machine
            .send_message(SendMessage {
                kind: MessageKind::Binary,
                buffer: payload,
                range: 0..6,
            })
            .unwrap();
        let mut ports = Capture::default();
        drive(&mut machine, &mut ports, &probe);
        let failure = if source_failure {
            machine.fail_source();
            Failure::Application
        } else {
            let reset = io_error(IoErrorKind::Reset);
            settle_read(&mut machine, &mut ports, Err(reset));
            Failure::Transport(reset)
        };
        drive(&mut machine, &mut ports, &probe);
        assert!(ports.receipt.is_none() && ports.close.is_none());
        assert_slice_identity(ports.write.as_ref().unwrap(), &probe);
        if source_failure {
            assert_eq!(ports.cancels.len(), 1);
            assert_eq!(ports.cancels[0].target, ports.read.as_ref().unwrap().id());
            settle_read(
                &mut machine,
                &mut ports,
                Err(io_error(IoErrorKind::Cancelled)),
            );
            drive(&mut machine, &mut ports, &probe);
            assert!(ports.receipt.is_none() && ports.close.is_none());
            settle_write(&mut machine, &mut ports, Ok(5));
        } else {
            assert_eq!(ports.cancels.len(), 1);
            assert_eq!(ports.cancels[0].target, ports.write.as_ref().unwrap().id());
            settle_write(
                &mut machine,
                &mut ports,
                Err(io_error(IoErrorKind::Cancelled)),
            );
        }

        drive(&mut machine, &mut ports, &probe);
        let receipt = ports.receipt.take().unwrap();
        probe.returned(&receipt.buffer);
        assert_eq!(receipt.result, Err(failure));
        assert_eq!(receipt.accepted, if source_failure { 3 } else { 0 });
        if source_failure {
            let write = ports.write.as_ref().unwrap();
            assert_eq!(write.slices().concat(), [0x88, 2, 3, 0xf3]);
            settle_write(&mut machine, &mut ports, Ok(4));
        }
        finish_close(&mut machine, &mut ports, &probe);
        assert_eq!(ports.closed.unwrap().result, Err(failure));
        drop(machine);
        drop(ports);
        probe.drop_once(receipt.buffer);
    }
}

#[test]
fn non_clone_closing_during_partial_frame_returns_payload_once() {
    let (probe, payload) = Probe::new(b"abcdef");
    let mut machine = machine(Config {
        outgoing_frame_bytes: 3,
        ..Config::default()
    });
    machine
        .send_message(SendMessage {
            kind: MessageKind::Binary,
            buffer: payload,
            range: 0..6,
        })
        .unwrap();
    let mut ports = Capture::default();
    drive(&mut machine, &mut ports, &probe);
    settle_write(&mut machine, &mut ports, Ok(1));
    drive(&mut machine, &mut ports, &probe);
    machine.close(CloseReason::new(1000, "").unwrap()).unwrap();

    let peer_close = [0x88, 0x82, 0, 0, 0, 0, 3, 0xe8];
    ports.read.as_mut().unwrap().bytes_mut()[..peer_close.len()].copy_from_slice(&peer_close);
    settle_read(&mut machine, &mut ports, Ok(peer_close.len()));
    drive(&mut machine, &mut ports, &probe);
    assert!(ports.receipt.is_none() && ports.close.is_none());
    assert_eq!(ports.write.as_ref().unwrap().slices().concat(), b"\x03abc");
    assert_slice_identity(ports.write.as_ref().unwrap(), &probe);
    settle_write(&mut machine, &mut ports, Ok(4));
    drive(&mut machine, &mut ports, &probe);
    let receipt = ports.receipt.take().unwrap();
    probe.returned(&receipt.buffer);
    assert_eq!(receipt.accepted, 3);
    assert_eq!(receipt.result, Err(Failure::Closing));
    assert_eq!(
        ports.write.as_ref().unwrap().slices().concat(),
        [0x88, 2, 3, 0xe8]
    );
    settle_write(&mut machine, &mut ports, Ok(4));
    finish_close(&mut machine, &mut ports, &probe);
    assert!(ports.closed.unwrap().clean);
    drop(machine);
    drop(ports);
    probe.drop_once(receipt.buffer);
}
