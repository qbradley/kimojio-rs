//! A bounded contract model. The oracle tracks obligations, not engine states.
#[path = "../examples/support/mod.rs"]
mod support;

use kimojio_fsm_http2::*;
use std::time::Duration;
use support::{MemoryPorts, Pair, request, response};

struct Selective {
    ports: MemoryPorts,
    mask: u16,
}

impl Selective {
    fn suspension(&self, bit: u16) -> Option<()> {
        (self.mask & (1 << bit) != 0).then_some(())
    }
}

impl Ports<Vec<u8>> for Selective {
    type Output = ();
    fn read(&mut self, op: ReadOp) -> Option<()> {
        self.ports.read(op);
        self.suspension(0)
    }
    fn write(&mut self, op: WriteOp) -> Option<()> {
        self.ports.write(op);
        self.suspension(1)
    }
    fn headers(&mut self, head: Head<'_>) -> Option<()> {
        self.ports.headers(head);
        self.suspension(2)
    }
    fn body(&mut self, op: BodyOp) -> Option<()> {
        self.ports.body(op);
        self.suspension(3)
    }
    fn send_ready(&mut self, op: SendPermit) -> Option<()> {
        self.ports.send_ready(op);
        self.suspension(4)
    }
    fn send_stopped(&mut self, stream: StreamId, reason: SendStop) -> Option<()> {
        self.ports.send_stopped(stream, reason);
        self.suspension(5)
    }
    fn sent(&mut self, sent: Sent) -> Option<()> {
        self.ports.sent(sent);
        self.suspension(6)
    }
    fn ended(&mut self, end: ReceiveEnd) -> Option<()> {
        self.ports.ended(end);
        self.suspension(7)
    }
    fn retired(&mut self, result: StreamResult) -> Option<()> {
        self.ports.retired(result);
        self.suspension(8)
    }
    fn cancel(&mut self, op: CancelOp) -> Option<()> {
        self.ports.cancel(op);
        self.suspension(9)
    }
    fn wake(&mut self, op: WakeOp) -> Option<()> {
        self.ports.wake(op);
        self.suspension(10)
    }
    fn close(&mut self, op: CloseOp) -> Option<()> {
        self.ports.close(op);
        self.suspension(11)
    }
    fn closed(&mut self, result: ConnectionResult) -> Option<()> {
        self.ports.closed(result);
        self.suspension(12)
    }
    fn reschedule(&mut self) -> Option<()> {
        Some(())
    }
}

fn drive(connection: &mut Connection, ports: &mut Selective) {
    for _ in 0..100 {
        let before = ports.ports.sequence.len();
        connection.next(ports);
        if ports.ports.sequence.len() == before {
            return;
        }
    }
    panic!("callbacks did not reach quiescence");
}

fn flush_writes(connection: &mut Connection, ports: &mut Selective, bytes: &mut Vec<u8>) {
    for _ in 0..100 {
        drive(connection, ports);
        let Some(op) = ports.ports.write.take() else {
            return;
        };
        bytes.extend(op.slices().iter().flat_map(|part| part.iter().copied()));
        let count = op.remaining();
        connection
            .complete_write(op.complete(WriteOutcome::Written(count)))
            .unwrap();
    }
    panic!("output did not reach quiescence");
}

fn permutations<const N: usize>() -> Vec<[usize; N]> {
    fn visit<const N: usize>(prefix: &mut Vec<usize>, output: &mut Vec<[usize; N]>) {
        if prefix.len() == N {
            output.push(prefix.as_slice().try_into().unwrap());
        } else {
            for next in 0..N {
                if !prefix.contains(&next) {
                    prefix.push(next);
                    visit(prefix, output);
                    prefix.pop();
                }
            }
        }
    }
    let mut output = Vec::new();
    visit(&mut Vec::new(), &mut output);
    output
}

#[derive(Default)]
struct StreamOracle {
    original_write_settled: bool,
    input_released: bool,
    reset_accepted: bool,
}
impl StreamOracle {
    fn retired(&self) -> bool {
        self.original_write_settled && self.input_released
    }
}

#[test]
fn all_small_stream_settlement_orders_and_selective_callbacks() {
    // 14 transport cuts × 6 completion/reset orders × 4 callback policies.
    let masks = [0, u16::MAX, (1 << 1) | (1 << 6), (1 << 5) | (1 << 8)];
    let mut explored = 0;
    for cut in 0..=13 {
        for order in permutations::<3>() {
            for mask in masks {
                let mut pair = Pair::new(Config::default());
                let id = pair.client.request(&request(b"POST"), false).unwrap();
                pair.pump(32_768);
                pair.client
                    .send(
                        pair.client_ports.permits.pop_front().unwrap(),
                        vec![1],
                        true,
                    )
                    .unwrap();
                pair.pump(32_768);
                let mut body = pair.server_ports.bodies.pop_front();
                pair.server.respond(id, &response(b"200"), false).unwrap();
                pair.pump(32_768);
                let payload = b"four".to_vec();
                let pointer = payload.as_ptr();
                pair.server
                    .send(
                        pair.server_ports.permits.pop_front().unwrap(),
                        payload,
                        true,
                    )
                    .unwrap();
                let mut ports = Selective {
                    ports: pair.server_ports,
                    mask,
                };
                drive(&mut pair.server, &mut ports);
                let first = ports.ports.write.take().unwrap();
                let expected_data: Vec<u8> = first
                    .slices()
                    .iter()
                    .flat_map(|s| s.iter().copied())
                    .collect();
                assert_eq!(expected_data.len(), 13);
                let mut wire = expected_data[..cut].to_vec();
                let mut original = if cut == 0 {
                    Some(first)
                } else {
                    pair.server
                        .complete_write(first.complete(WriteOutcome::Written(cut)))
                        .unwrap();
                    drive(&mut pair.server, &mut ports);
                    ports.ports.write.take()
                };
                let mut oracle = StreamOracle {
                    original_write_settled: cut == 13,
                    ..StreamOracle::default()
                };
                let mut expected_controls = Vec::new();
                for action in order {
                    match action {
                        0 => {
                            if let Some(op) = original.take() {
                                wire.extend(op.slices().iter().flat_map(|s| s.iter().copied()));
                                let count = op.remaining();
                                pair.server
                                    .complete_write(op.complete(WriteOutcome::Written(count)))
                                    .unwrap();
                            }
                            oracle.original_write_settled = true;
                        }
                        1 => {
                            pair.server
                                .release_body(body.take().unwrap().release())
                                .unwrap();
                            oracle.input_released = true;
                            expected_controls
                                .extend_from_slice(&[0, 0, 4, 8, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
                        }
                        2 => {
                            let accepted = pair.server.reset(id, H2ErrorCode::Cancel).is_ok();
                            assert_eq!(accepted, !oracle.retired());
                            oracle.reset_accepted = accepted;
                            if accepted {
                                expected_controls.extend_from_slice(&[0, 0, 4, 3, 0]);
                                expected_controls.extend_from_slice(&id.get().to_be_bytes());
                                expected_controls.extend_from_slice(&8u32.to_be_bytes());
                            }
                        }
                        _ => unreachable!(),
                    }
                    if oracle.original_write_settled {
                        flush_writes(&mut pair.server, &mut ports, &mut wire);
                    } else {
                        drive(&mut pair.server, &mut ports);
                        assert!(
                            ports.ports.write.is_none(),
                            "must not issue a second shared write"
                        );
                    }
                    assert_eq!(
                        ports.ports.retired.len(),
                        usize::from(oracle.retired()),
                        "cut={cut}, order={order:?}, mask={mask}, action={action}"
                    );
                    assert!(ports.ports.closed.is_empty());
                }
                let mut expected = expected_data;
                expected.extend(expected_controls);
                assert_eq!(wire, expected, "no replay, omitted prefix, or extra frame");
                assert_eq!(ports.ports.sent.len(), 1);
                assert_eq!(ports.ports.sent[0].buffer.as_ptr(), pointer);
                assert_eq!(ports.ports.sent[0].accepted, 4);
                assert!(ports.ports.sent[0].exact);
                assert_eq!(ports.ports.sent[0].result, Ok(()));
                assert_eq!(ports.ports.stopped.len(), 1);
                assert_eq!(ports.ports.ends.len(), 1);
                assert_eq!(
                    ports.ports.retired[0].outcome,
                    if oracle.reset_accepted {
                        StreamOutcome::Reset(8)
                    } else {
                        StreamOutcome::Complete
                    }
                );
                explored += 1;
            }
        }
    }
    assert_eq!(explored, 336);
}

#[test]
fn all_connection_original_and_cancellation_ack_orders() {
    // Five independent obligations, all 120 orders, four callback policies.
    let masks = [0, u16::MAX, (1 << 1) | (1 << 9), (1 << 11) | (1 << 12)];
    let mut explored = 0;
    for order in permutations::<5>() {
        for mask in masks {
            let mut connection = Client::<Vec<u8>>::new(Config::default(), Duration::ZERO).unwrap();
            connection.request(&request(b"GET"), true).unwrap();
            let mut ports = Selective {
                ports: MemoryPorts::default(),
                mask,
            };
            drive(&mut connection, &mut ports);
            let settings = ports.ports.write.take().unwrap();
            let length = settings.remaining();
            connection
                .complete_write(settings.complete(WriteOutcome::Written(length)))
                .unwrap();
            drive(&mut connection, &mut ports);
            let mut read = ports.ports.read.take();
            let mut write = ports.ports.write.take();
            let mut alarm = ports.ports.alarms.pop();
            let original_bytes: Vec<u8> = write
                .as_ref()
                .unwrap()
                .slices()
                .iter()
                .flat_map(|part| part.iter().copied())
                .collect();
            connection.advance_time(Duration::from_secs(10)).unwrap();
            drive(&mut connection, &mut ports);
            assert_eq!(ports.ports.cancels.len(), 2);
            let mut cancel_read = None;
            let mut cancel_alarm = None;
            for cancel in ports.ports.cancels.drain(..) {
                if cancel.original() == read.as_ref().unwrap().token() {
                    cancel_read = Some(cancel);
                } else if cancel.original() == alarm.as_ref().unwrap().token() {
                    cancel_alarm = Some(cancel);
                } else {
                    panic!("unexpected cancellation target");
                }
            }
            let mut settled = [false; 5];
            let mut wire = Vec::new();
            for action in order {
                match action {
                    0 => connection
                        .complete_read(
                            read.take()
                                .unwrap()
                                .complete(ReadOutcome::Failed(IoFailure::Cancelled)),
                        )
                        .unwrap(),
                    1 => {
                        let op = write.take().unwrap();
                        let count = op.remaining();
                        wire.extend(op.slices().iter().flat_map(|part| part.iter().copied()));
                        connection
                            .complete_write(op.complete(WriteOutcome::Written(count)))
                            .unwrap();
                    }
                    2 => connection
                        .complete_wake(alarm.take().unwrap().complete(Duration::from_secs(10)))
                        .unwrap(),
                    3 => connection
                        .complete_cancel(cancel_read.take().unwrap().complete())
                        .unwrap(),
                    4 => connection
                        .complete_cancel(cancel_alarm.take().unwrap().complete())
                        .unwrap(),
                    _ => unreachable!(),
                }
                settled[action] = true;
                if settled[1] {
                    flush_writes(&mut connection, &mut ports, &mut wire);
                } else {
                    drive(&mut connection, &mut ports);
                }
                assert_eq!(
                    ports.ports.close.is_some(),
                    settled.iter().all(|done| *done),
                    "order={order:?}, mask={mask}, action={action}"
                );
                assert!(ports.ports.closed.is_empty());
            }
            let mut expected = original_bytes;
            expected.extend_from_slice(&[0, 0, 8, 7, 0, 0, 0, 0, 0]);
            expected.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 4]);
            assert_eq!(wire, expected);
            connection
                .complete_close(ports.ports.close.take().unwrap().complete(Ok(())))
                .unwrap();
            drive(&mut connection, &mut ports);
            assert!(matches!(ports.ports.closed.as_slice(),
                [ConnectionResult::Protocol(error)] if error.code == H2ErrorCode::SettingsTimeout));
            let notification_count = ports.ports.sequence.len();
            drive(&mut connection, &mut ports);
            assert_eq!(ports.ports.sequence.len(), notification_count);
            explored += 1;
        }
    }
    assert_eq!(explored, 480);
}
