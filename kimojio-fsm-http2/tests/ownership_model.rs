//! A bounded contract model. The oracle tracks obligations, not engine states.
#[path = "../examples/support/mod.rs"]
mod support;

use kimojio_fsm_http2::*;
use std::time::Duration;
use support::{MemoryPorts, Pair, request, response};

#[test]
fn completed_response_survives_no_error_reset_in_the_same_read_batch() {
    fn frame(kind: u8, flags: u8, stream: StreamId, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0, 0, payload.len() as u8, kind, flags];
        bytes.extend(stream.get().to_be_bytes());
        bytes.extend(payload);
        bytes
    }
    for ending in 0..3 {
        for mask in [0, u16::MAX, 0x5555, 0xaaaa] {
            for release_first in [false, true] {
                for written in [10, 109] {
                    let mut pair = Pair::new(Config::default());
                    let id = pair.client.request(&request(b"POST"), false).unwrap();
                    let sibling = pair.client.request(&request(b"GET"), true).unwrap();
                    pair.pump(32_768);
                    let upload = vec![7; 100];
                    let pointer = upload.as_ptr();
                    pair.client
                        .send(
                            pair.client_ports.permits.pop_front().unwrap(),
                            upload,
                            false,
                        )
                        .unwrap();
                    pair.client.next(&mut pair.client_ports);
                    let write = pair.client_ports.write.take().unwrap();
                    let mut ports = Selective {
                        ports: pair.client_ports,
                        mask,
                    };
                    let mut encoder = H2HeaderBlockEncoder::new();
                    let mut fields = response(b"200");
                    fields.push(H2HeaderField::new(
                        b"content-length",
                        if ending == 0 { b"0" } else { b"3" },
                    ));
                    let mut batch = frame(
                        1,
                        if ending == 0 { 5 } else { 4 },
                        id,
                        &encoder.try_encode_fields(&fields).unwrap(),
                    );
                    if ending != 0 {
                        batch.extend(frame(0, u8::from(ending == 1), id, b"abc"));
                    }
                    if ending == 2 {
                        batch.extend(frame(
                            1,
                            5,
                            id,
                            &encoder
                                .try_encode_fields(&[H2HeaderField::new(b"x-end", b"yes")])
                                .unwrap(),
                        ));
                    }
                    batch.extend(frame(3, 0, id, &0u32.to_be_bytes()));
                    batch.extend(frame(
                        1,
                        5,
                        sibling,
                        &encoder.try_encode_fields(&response(b"200")).unwrap(),
                    ));
                    let mut read = ports.ports.read.take().unwrap();
                    read.buffer_mut()[..batch.len()].copy_from_slice(&batch);
                    pair.client
                        .complete_read(read.complete(ReadOutcome::Read(batch.len())))
                        .unwrap();
                    drive(&mut pair.client, &mut ports);
                    assert_eq!(ports.ports.heads[0].0, id);
                    assert_eq!(ports.ports.heads[0].1, HeadKind::Response(200));
                    assert_eq!(ports.ports.heads[0].2, fields);
                    assert_eq!(ports.ports.heads.len(), if ending == 2 { 3 } else { 2 });
                    if ending == 2 {
                        assert_eq!(ports.ports.heads[1].1, HeadKind::Trailers);
                        assert_eq!(
                            ports.ports.heads[1].2,
                            [H2HeaderField::new(b"x-end", b"yes")]
                        );
                    }
                    assert_eq!(
                        ports.ports.ends,
                        [
                            ReceiveEnd {
                                stream: id,
                                outcome: StreamOutcome::Complete
                            },
                            ReceiveEnd {
                                stream: sibling,
                                outcome: StreamOutcome::Complete
                            },
                        ]
                    );
                    assert_eq!(
                        ports
                            .ports
                            .stopped
                            .iter()
                            .filter(|(stream, _)| *stream == id)
                            .copied()
                            .collect::<Vec<_>>(),
                        [(id, SendStop::Reset(0))]
                    );
                    assert_eq!(
                        ports.ports.retired,
                        [StreamResult {
                            stream: sibling,
                            outcome: StreamOutcome::Complete
                        }]
                    );
                    assert_eq!(ports.ports.bodies.len(), usize::from(ending != 0));
                    if ending != 0 {
                        assert_eq!(ports.ports.bodies[0].bytes(), b"abc");
                    }
                    if release_first {
                        while let Some(body) = ports.ports.bodies.pop_front() {
                            pair.client.release_body(body.release()).unwrap();
                        }
                    }
                    let mut wire: Vec<_> = write
                        .slices()
                        .iter()
                        .flat_map(|part| part.iter().copied())
                        .take(written)
                        .collect();
                    pair.client
                        .complete_write(write.complete(WriteOutcome::Written(written)))
                        .unwrap();
                    flush_writes(&mut pair.client, &mut ports, &mut wire);
                    while let Some(body) = ports.ports.bodies.pop_front() {
                        assert_eq!(body.bytes(), b"abc");
                        pair.client.release_body(body.release()).unwrap();
                    }
                    flush_writes(&mut pair.client, &mut ports, &mut wire);
                    let mut expected = frame(0, 0, id, &[7; 100]);
                    if ending != 0 {
                        expected.extend([0, 0, 4, 8, 0, 0, 0, 0, 0, 0, 0, 0, 3]);
                    }
                    assert_eq!(wire, expected);
                    assert_eq!(ports.ports.sent.len(), 1);
                    assert_eq!(ports.ports.sent[0].buffer.as_ptr(), pointer);
                    assert_eq!(ports.ports.sent[0].accepted, 100);
                    assert_eq!(ports.ports.sent[0].result, Ok(()));
                    assert_eq!(
                        ports.ports.retired,
                        [
                            StreamResult {
                                stream: sibling,
                                outcome: StreamOutcome::Complete
                            },
                            StreamResult {
                                stream: id,
                                outcome: StreamOutcome::Reset(0)
                            },
                        ]
                    );
                    assert!(ports.ports.closed.is_empty());
                }
            }
        }
    }
}

#[test]
fn hard_abort_joins_every_original_and_cancellation_ack_order() {
    for mask in [0, u16::MAX, 0x5555, 0xaaaa] {
        for receipt in 0..4 {
            for order in permutations::<6>() {
                let mut pair = Pair::new(Config::default());
                let id = pair.client.request(&request(b"POST"), false).unwrap();
                pair.pump(32_768);
                pair.client
                    .set_deadline(id, Some(Duration::from_secs(5)))
                    .unwrap();
                let buffer = vec![9; 100];
                let pointer = buffer.as_ptr();
                pair.client
                    .send(
                        pair.client_ports.permits.pop_front().unwrap(),
                        buffer,
                        false,
                    )
                    .unwrap();
                pair.client.next(&mut pair.client_ports);
                let mut read = pair.client_ports.read.take();
                let mut write = pair.client_ports.write.take();
                let mut alarm = pair.client_ports.alarms.pop();
                assert!(read.is_some() && write.is_some() && alarm.is_some());
                let mut ports = Selective {
                    ports: pair.client_ports,
                    mask,
                };
                pair.client.abort();
                drive(&mut pair.client, &mut ports);
                assert_eq!(ports.ports.cancels.len(), 3);
                let mut cancel_read = None;
                let mut cancel_write = None;
                let mut cancel_alarm = None;
                for cancel in ports.ports.cancels.drain(..) {
                    if cancel.original() == read.as_ref().unwrap().token() {
                        cancel_read = Some(cancel);
                    } else if cancel.original() == write.as_ref().unwrap().token() {
                        cancel_write = Some(cancel);
                    } else {
                        assert_eq!(cancel.original(), alarm.as_ref().unwrap().token());
                        cancel_alarm = Some(cancel);
                    }
                }
                let mut settled = [false; 6];
                for action in order {
                    match action {
                        0 => pair
                            .client
                            .complete_read(
                                read.take()
                                    .unwrap()
                                    .complete(ReadOutcome::Failed(IoFailure::Cancelled)),
                            )
                            .unwrap(),
                        1 => {
                            let outcome = match receipt {
                                0 => WriteOutcome::Failed {
                                    progress: Progress::Exact(0),
                                    error: IoFailure::Cancelled,
                                },
                                1 => WriteOutcome::Written(10),
                                2 => WriteOutcome::Written(109),
                                3 => WriteOutcome::Failed {
                                    progress: Progress::AtLeast(12),
                                    error: IoFailure::Failed,
                                },
                                _ => unreachable!(),
                            };
                            pair.client
                                .complete_write(write.take().unwrap().complete(outcome))
                                .unwrap();
                        }
                        2 => {
                            pair.client
                                .complete_wake(alarm.take().unwrap().failed(IoFailure::Cancelled))
                                .unwrap();
                            pair.client.advance_time(Duration::ZERO).unwrap();
                        }
                        3 => pair
                            .client
                            .complete_cancel(cancel_read.take().unwrap().complete())
                            .unwrap(),
                        4 => pair
                            .client
                            .complete_cancel(cancel_write.take().unwrap().complete())
                            .unwrap(),
                        5 => pair
                            .client
                            .complete_cancel(cancel_alarm.take().unwrap().complete())
                            .unwrap(),
                        _ => unreachable!(),
                    }
                    settled[action] = true;
                    drive(&mut pair.client, &mut ports);
                    assert_eq!(
                        ports.ports.close.is_some(),
                        settled.iter().all(|value| *value),
                        "mask={mask}, receipt={receipt}, order={order:?}, action={action}"
                    );
                    assert!(ports.ports.closed.is_empty());
                    assert!(
                        ports.ports.write.is_none(),
                        "no continuation after hard abort"
                    );
                    assert!(ports.ports.cancels.is_empty());
                }
                assert_eq!(ports.ports.sent.len(), 1);
                let sent = &ports.ports.sent[0];
                assert_eq!(sent.stream, id);
                assert_eq!(sent.buffer.as_ptr(), pointer);
                assert_eq!(sent.buffer, [9; 100]);
                assert_eq!(sent.accepted, [0, 1, 100, 3][receipt]);
                assert_eq!(sent.exact, receipt != 3);
                assert_eq!(sent.result.is_ok(), receipt == 2);
                assert_eq!(
                    ports.ports.retired,
                    [StreamResult {
                        stream: id,
                        outcome: StreamOutcome::ConnectionFailed
                    }]
                );
                pair.client
                    .complete_close(ports.ports.close.take().unwrap().complete(Ok(())))
                    .unwrap();
                drive(&mut pair.client, &mut ports);
                assert_eq!(ports.ports.closed, [ConnectionResult::Aborted]);
                let count = ports.ports.sequence.len();
                pair.client.abort_with_cause(ConnectionResult::IoFailed);
                drive(&mut pair.client, &mut ports);
                assert_eq!(ports.ports.sequence.len(), count);
            }
        }
    }
}

struct Selective {
    ports: MemoryPorts,
    mask: u16,
}

#[test]
fn blocked_output_coalesces_released_credit_without_exhausting_control_items() {
    for capacity in [4, Config::default().max_outbound_items] {
        let mut pair = Pair::new(Config {
            max_outbound_items: capacity,
            ..Config::default()
        });
        let first = pair.client.request(&request(b"GET"), true).unwrap();
        let second = pair.client.request(&request(b"GET"), true).unwrap();
        pair.pump(32_768);
        pair.client.request(&request(b"GET"), true).unwrap();
        pair.client.next(&mut pair.client_ports);
        let original = pair.client_ports.write.take().unwrap();
        let mut ports = Selective {
            ports: pair.client_ports,
            mask: 1 << 3,
        };
        let mut bytes = Vec::new();
        for id in [first, second] {
            bytes.extend([0, 0, 1, 1, 4]);
            bytes.extend(id.get().to_be_bytes());
            bytes.push(0x88);
        }
        for index in 0..600 {
            bytes.extend([0, 0, 1, 0, 0]);
            bytes.extend(
                if index % 3 == 0 { first } else { second }
                    .get()
                    .to_be_bytes(),
            );
            bytes.push(7);
        }
        let mut read = ports.ports.read.take().unwrap();
        read.buffer_mut()[..bytes.len()].copy_from_slice(&bytes);
        pair.client
            .complete_read(read.complete(ReadOutcome::Read(bytes.len())))
            .unwrap();
        let mut received = 0;
        for _ in 0..610 {
            pair.client.next(&mut ports);
            while let Some(body) = ports.ports.bodies.pop_front() {
                assert_eq!(body.bytes(), &[7]);
                pair.client.release_body(body.release()).unwrap();
                received += 1;
            }
            if received == 600 || !ports.ports.ends.is_empty() {
                break;
            }
        }
        assert_eq!(
            received, 600,
            "ordinary released DATA must not consume one queued control item per refund"
        );
        assert!(ports.ports.ends.is_empty());
        assert!(ports.ports.retired.is_empty());
        assert!(ports.ports.write.is_none());
        let count = original.remaining();
        pair.client
            .complete_write(original.complete(WriteOutcome::Written(count)))
            .unwrap();
        let mut wire = Vec::new();
        flush_writes(&mut pair.client, &mut ports, &mut wire);
        let mut expected = Vec::new();
        for (id, amount) in [(0, 600u32), (first.get(), 200), (second.get(), 400)] {
            expected.extend([0, 0, 4, 8, 0]);
            expected.extend(id.to_be_bytes());
            expected.extend(amount.to_be_bytes());
        }
        assert_eq!(wire, expected);
        assert!(ports.ports.closed.is_empty());
    }
}

#[test]
fn repeated_eight_stream_megabyte_duplex_preserves_bounded_credit_progress() {
    static PAYLOAD: [u8; 32_768] = [0x5a; 32_768];
    const BODY: usize = 1024 * 1024;
    fn settle(
        connection: &mut Connection,
        ports: &mut Selective,
        ids: &[StreamId],
        sent: &mut [usize; 8],
        received: &mut [usize; 8],
        returned: &mut [usize; 8],
        pool: &mut Vec<Vec<u8>>,
    ) {
        for cancel in std::mem::take(&mut ports.ports.cancels) {
            let index = ports
                .ports
                .alarms
                .iter()
                .position(|alarm| alarm.token() == cancel.original())
                .expect("only completed-handshake alarms need cancellation");
            connection
                .complete_wake(
                    ports
                        .ports
                        .alarms
                        .remove(index)
                        .failed(IoFailure::Cancelled),
                )
                .unwrap();
            connection.complete_cancel(cancel.complete()).unwrap();
        }
        for receipt in ports.ports.sent.drain(..) {
            assert_eq!(receipt.result, Ok(()));
            assert!(receipt.exact);
            assert_eq!(receipt.accepted, receipt.buffer.len());
            returned[ids.iter().position(|id| *id == receipt.stream).unwrap()] += receipt.accepted;
            pool.push(receipt.buffer);
        }
        while let Some(body) = ports.ports.bodies.pop_front() {
            assert_eq!(body.bytes(), &PAYLOAD[..body.bytes().len()]);
            received[ids.iter().position(|id| *id == body.stream()).unwrap()] += body.bytes().len();
            connection.release_body(body.release()).unwrap();
        }
        while let Some(permit) = ports.ports.permits.pop_back() {
            let index = ids.iter().position(|id| *id == permit.stream()).unwrap();
            let count = (BODY - sent[index])
                .min(PAYLOAD.len())
                .min(permit.max_bytes());
            assert_ne!(count, 0);
            let mut buffer = pool.pop().unwrap_or_else(|| PAYLOAD.to_vec());
            buffer.resize(count, 0x5a);
            sent[index] += count;
            connection
                .send(permit, buffer, sent[index] == BODY)
                .unwrap();
        }
    }
    fn transfer(
        from: &mut Connection,
        outgoing: &mut Selective,
        to: &mut Connection,
        incoming: &mut Selective,
    ) -> bool {
        if outgoing.ports.write.is_none() || incoming.ports.read.is_none() {
            return false;
        }
        let write = outgoing.ports.write.take().unwrap();
        let mut read = incoming.ports.read.take().unwrap();
        let length = write.remaining().min(read.buffer_mut().len()).min(65_536);
        let mut offset = 0;
        for slice in write.slices() {
            let count = slice.len().min(length - offset);
            read.buffer_mut()[offset..offset + count].copy_from_slice(&slice[..count]);
            offset += count;
        }
        assert_eq!(offset, length);
        from.complete_write(write.complete(WriteOutcome::Written(length)))
            .unwrap();
        to.complete_read(read.complete(ReadOutcome::Read(length)))
            .unwrap();
        true
    }
    let config = Config {
        http: HttpLimits::new().set_max_active_streams(128),
        ..Config::default()
    };
    let mut client = Client::new(config.clone(), Duration::ZERO).unwrap();
    let mut server = Server::new(config, Duration::ZERO).unwrap();
    let mut cp = Selective {
        ports: MemoryPorts::default(),
        mask: 1 << 3,
    };
    let mut sp = Selective {
        ports: MemoryPorts::default(),
        mask: 1 << 3,
    };
    let mut client_pool = Vec::new();
    let mut server_pool = Vec::new();
    for batch in 0..10 {
        let ids: Vec<_> = (0..8)
            .map(|_| client.request(&request(b"POST"), false).unwrap())
            .collect();
        let mut client_sent = [0; 8];
        let mut server_sent = [0; 8];
        let mut client_received = [0; 8];
        let mut server_received = [0; 8];
        let mut client_returned = [0; 8];
        let mut server_returned = [0; 8];
        let mut responded = 0;
        let mut completed = false;
        let mut overlapped = false;
        for turn in 0..1_000_000 {
            let before = cp.ports.sequence.len() + sp.ports.sequence.len();
            client.next(&mut cp);
            server.next(&mut sp);
            settle(
                &mut client,
                &mut cp,
                &ids,
                &mut client_sent,
                &mut client_received,
                &mut client_returned,
                &mut client_pool,
            );
            settle(
                &mut server,
                &mut sp,
                &ids,
                &mut server_sent,
                &mut server_received,
                &mut server_returned,
                &mut server_pool,
            );
            overlapped |= server_returned.iter().any(|bytes| *bytes != 0)
                && client_returned.iter().sum::<usize>() < 8 * BODY;
            for index in (responded..sp.ports.heads.len()).rev() {
                server
                    .respond(sp.ports.heads[index].0, &response(b"200"), false)
                    .unwrap();
            }
            responded = sp.ports.heads.len();
            let forward = transfer(&mut client, &mut cp, &mut server, &mut sp);
            let reverse = transfer(&mut server, &mut sp, &mut client, &mut cp);
            if cp.ports.retired.len() == 8 && sp.ports.retired.len() == 8 {
                completed = true;
                break;
            }
            assert!(
                forward || reverse || cp.ports.sequence.len() + sp.ports.sequence.len() != before,
                "duplex stalled at batch {batch}, turn {turn}"
            );
        }
        assert!(completed);
        assert!(
            overlapped,
            "response DATA must progress before all uploads finish"
        );
        assert_eq!(client_sent, [BODY; 8]);
        assert_eq!(server_sent, [BODY; 8]);
        assert_eq!(client_received, [BODY; 8]);
        assert_eq!(server_received, [BODY; 8]);
        assert_eq!(client_returned, [BODY; 8]);
        assert_eq!(server_returned, [BODY; 8]);
        for ports in [&mut cp.ports, &mut sp.ports] {
            assert_eq!(ports.heads.len(), 8);
            assert_eq!(ports.ends.len(), 8);
            assert!(
                ports
                    .ends
                    .iter()
                    .all(|end| end.outcome == StreamOutcome::Complete)
            );
            assert!(
                ports
                    .retired
                    .iter()
                    .all(|result| result.outcome == StreamOutcome::Complete)
            );
            assert!(ports.closed.is_empty());
            ports.heads.clear();
            ports.ends.clear();
            ports.retired.clear();
            ports.stopped.clear();
            ports.sequence.clear();
        }
    }
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
