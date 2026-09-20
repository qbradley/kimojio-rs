use super::*;
#[path = "../examples/support/mod.rs"]
mod support;
use support::{MemoryPorts, Pair, request, response, step};

#[test]
fn unexpected_planner_failure_resets_all_stream_layers_before_retirement() {
    let mut pair = Pair::new(Config::default());
    let id = pair.client.request(&request(b"POST"), false).unwrap();
    let sibling = pair.client.request(&request(b"GET"), true).unwrap();
    pair.pump(32_768);
    let buffer = vec![7; 2];
    let pointer = buffer.as_ptr();
    pair.client
        .send(
            pair.client_ports.permits.pop_front().unwrap(),
            buffer,
            false,
        )
        .unwrap();
    let Protocol::Client(role) = &mut pair.client.0.protocol else {
        unreachable!()
    };
    // Inject an inconsistent private limit to exercise the defensive planner path.
    role.send_mut(id.0).unwrap().sent.limit = Some(1);
    pair.client.next(&mut pair.client_ports);
    let reset = pair.client_ports.write.take().unwrap();
    let wire: Vec<_> = reset
        .slices()
        .iter()
        .flat_map(|part| part.iter().copied())
        .collect();
    assert_eq!(wire, [0, 0, 4, 3, 0, 0, 0, 0, 1, 0, 0, 0, 2]);
    assert!(pair.client_ports.retired.is_empty());
    assert_eq!(pair.client_ports.sent.len(), 1);
    assert_eq!(pair.client_ports.sent[0].buffer.as_ptr(), pointer);
    assert_eq!(pair.client_ports.sent[0].accepted, 0);
    assert_eq!(pair.client_ports.sent[0].result, Err(SendStop::Reset(2)));
    pair.client
        .complete_write(reset.complete(WriteOutcome::Written(wire.len())))
        .unwrap();
    pair.client.next(&mut pair.client_ports);
    assert_eq!(
        pair.client_ports.retired,
        [StreamResult {
            stream: id,
            outcome: StreamOutcome::Reset(2)
        }]
    );
    let Protocol::Client(role) = &pair.client.0.protocol else {
        unreachable!()
    };
    assert!(!role.endpoint.streams.contains_key(&id.0));
    assert!(role.is_reset_tolerant(id.0));

    let mut encoder = crate::H2HeaderBlockEncoder::new();
    let fields = [
        H2HeaderField::new(b":status", b"200"),
        H2HeaderField::new(b"x-shared", b"late"),
    ];
    let late = encoder.try_encode_fields(&fields).unwrap();
    let next = encoder.try_encode_fields(&fields).unwrap();
    assert_eq!(next, [0x88, 0xbe]);
    for (stream, block) in [(id, late), (sibling, next)] {
        let mut frame = vec![0, 0, block.len() as u8, 1, 5];
        frame.extend(stream.0.to_be_bytes());
        frame.extend(block);
        pair.to_client.extend(frame);
    }
    let mut output = VecDeque::new();
    for _ in 0..32 {
        if !step(
            &mut pair.client,
            &mut pair.client_ports,
            &mut pair.to_client,
            &mut output,
            32_768,
        ) {
            break;
        }
    }
    assert!(pair.to_client.is_empty());
    assert!(output.is_empty());
    assert_eq!(pair.client_ports.heads.len(), 1);
    assert_eq!(pair.client_ports.heads[0].0, sibling);
    assert_eq!(
        pair.client_ports.retired,
        [
            StreamResult {
                stream: id,
                outcome: StreamOutcome::Reset(2)
            },
            StreamResult {
                stream: sibling,
                outcome: StreamOutcome::Complete
            },
        ]
    );
    assert!(pair.client_ports.closed.is_empty());
}

#[test]
fn stream_identifier_exhaustion_does_not_wrap_or_reuse() {
    let mut client = Client::<Vec<u8>>::new(
        Config {
            http: HttpLimits::new().set_max_active_streams(1),
            ..Config::default()
        },
        Duration::ZERO,
    )
    .unwrap();
    let Protocol::Client(role) = &mut client.0.protocol else {
        unreachable!()
    };
    role.next_stream_id = 0x7fff_ffff;
    role.endpoint.settings.max_concurrent_streams = 0;
    assert_eq!(
        client.request(&request(b"GET"), true),
        Err(CommandError::Blocked)
    );
    let Protocol::Client(role) = &mut client.0.protocol else {
        unreachable!()
    };
    role.endpoint.settings.max_concurrent_streams = 1;
    let last = client.request(&request(b"GET"), true).unwrap();
    assert_eq!(last.get(), 0x7fff_ffff);
    let pending = client.controls.len();
    assert_eq!(
        client.request(&request(b"GET"), true),
        Err(CommandError::SequenceExhausted),
        "terminal exhaustion precedes the full local stream limit"
    );
    assert_eq!(client.controls.len(), pending);
    let Protocol::Client(role) = &client.0.protocol else {
        unreachable!()
    };
    assert_eq!(role.next_stream_id, 0x8000_0001);
    let mut ports = MemoryPorts::default();
    client.next(&mut ports);
    assert_eq!(ports.admissions, 1);
    for _ in 0..32 {
        assert_eq!(
            client.request(&request(b"GET"), true),
            Err(CommandError::SequenceExhausted)
        );
        client.next(&mut ports);
    }
    assert_eq!(ports.admissions, 1);
}

#[test]
fn alarm_replacement_bounds_originals_and_delayed_cancel_acknowledgments() {
    fn originals(connection: &mut Connection, ports: &mut MemoryPorts, alarms: Vec<WakeOp>) {
        for alarm in alarms {
            connection
                .complete_wake(alarm.complete(Duration::from_secs(1_000_000)))
                .unwrap();
        }
        let read = ports.read.take().unwrap();
        connection
            .complete_read(read.complete(ReadOutcome::Failed(IoFailure::Cancelled)))
            .unwrap();
        connection.next(ports);
    }

    for capacity in [4, Config::default().max_outbound_items] {
        for hold_originals in [false, true] {
            for acknowledgments_first in [false, true] {
                let mut pair = Pair::new(Config {
                    max_outbound_items: capacity,
                    ..Config::default()
                });
                let id = pair.client.request(&request(b"GET"), true).unwrap();
                pair.pump(32_768);
                pair.client.request(&request(b"GET"), true).unwrap();
                pair.client
                    .set_deadline(id, Some(Duration::from_secs(100)))
                    .unwrap();
                pair.client.next(&mut pair.client_ports);
                let original_write = pair.client_ports.write.take().unwrap();
                let mut retained = Vec::new();
                let mut replacements = 0;
                for index in 0..4096 {
                    let alarm = pair.client_ports.alarms.pop().unwrap();
                    pair.client
                        .set_deadline(
                            id,
                            Some(Duration::from_secs(if index % 2 == 0 { 200 } else { 100 })),
                        )
                        .unwrap();
                    pair.client.next(&mut pair.client_ports);
                    assert!(
                        pair.client.wake_tokens.len() + pair.client.cancellations.len()
                            <= capacity + 3
                    );
                    if hold_originals {
                        retained.push(alarm);
                    } else {
                        pair.client
                            .complete_wake(alarm.complete(Duration::ZERO))
                            .unwrap();
                    }
                    replacements += 1;
                    if matches!(
                        pair.client.life,
                        Life::Settling(ConnectionResult::ResourceExhausted)
                    ) {
                        break;
                    }
                }
                assert!(
                    replacements < capacity,
                    "replacement debt must remain bounded"
                );
                assert!(matches!(
                    pair.client.life,
                    Life::Settling(ConnectionResult::ResourceExhausted)
                ));
                assert!(pair.client_ports.alarms.is_empty());
                assert_eq!(pair.client_ports.cancels.len(), replacements + 1);
                assert_eq!(pair.client.cancellations.len(), replacements + 1);
                assert_eq!(pair.client.wake_tokens.len(), retained.len());
                assert!(pair.client_ports.close.is_none());
                let bytes = original_write.remaining();
                pair.client
                    .complete_write(original_write.complete(WriteOutcome::Written(bytes)))
                    .unwrap();
                pair.client.next(&mut pair.client_ports);
                let goaway = pair.client_ports.write.take().unwrap();
                let wire: Vec<_> = goaway
                    .slices()
                    .iter()
                    .flat_map(|part| part.iter().copied())
                    .collect();
                assert_eq!(wire, [0, 0, 8, 7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 11]);
                pair.client
                    .complete_write(goaway.complete(WriteOutcome::Written(wire.len())))
                    .unwrap();
                pair.client.next(&mut pair.client_ports);
                assert!(pair.client_ports.close.is_none());

                let cancellations = std::mem::take(&mut pair.client_ports.cancels);
                if acknowledgments_first {
                    for cancel in cancellations {
                        pair.client.complete_cancel(cancel.complete()).unwrap();
                        pair.client.next(&mut pair.client_ports);
                        assert!(pair.client_ports.close.is_none());
                    }
                    originals(&mut pair.client, &mut pair.client_ports, retained);
                } else {
                    originals(&mut pair.client, &mut pair.client_ports, retained);
                    assert!(pair.client_ports.close.is_none());
                    let total = cancellations.len();
                    for (index, cancel) in cancellations.into_iter().enumerate() {
                        pair.client.complete_cancel(cancel.complete()).unwrap();
                        pair.client.next(&mut pair.client_ports);
                        assert_eq!(pair.client_ports.close.is_some(), index + 1 == total);
                    }
                }
                assert!(pair.client.wake_tokens.is_empty());
                assert!(pair.client.cancellations.is_empty());
                let close = pair
                    .client_ports
                    .close
                    .take()
                    .expect("all original operations and acknowledgments settled");
                pair.client.complete_close(close.complete(Ok(()))).unwrap();
                pair.client.next(&mut pair.client_ports);
                assert_eq!(
                    pair.client_ports.closed,
                    [ConnectionResult::ResourceExhausted]
                );
            }
        }
    }
}

#[test]
fn operation_identifier_exhaustion_uses_reserved_settlement_identities() {
    let mut client = Client::<Vec<u8>>::new(Config::default(), Duration::ZERO).unwrap();
    client.request(&request(b"GET"), true).unwrap();
    client.sequence = u64::MAX - 8;
    let mut ports = MemoryPorts::default();
    let mut input = VecDeque::new();
    let mut output = VecDeque::new();
    for _ in 0..20 {
        if !step(&mut client, &mut ports, &mut input, &mut output, 32_768) {
            break;
        }
    }
    assert_eq!(ports.closed, [ConnectionResult::ResourceExhausted]);
    assert_eq!(ports.retired.len(), 1);
    assert!(output.is_empty());
    assert!(client.sequence > u64::MAX - 8);
    assert!(client.sequence < u64::MAX);
    assert!(ports.read.is_none());
}

#[test]
fn alarm_bound_reserves_three_cancellations_for_forced_shutdown() {
    let capacity = 4;
    let mut pair = Pair::new(Config {
        max_outbound_items: capacity,
        ..Config::default()
    });
    let id = pair.client.request(&request(b"GET"), true).unwrap();
    pair.pump(32_768);
    pair.client.request(&request(b"GET"), true).unwrap();
    pair.client
        .set_deadline(id, Some(Duration::from_secs(100)))
        .unwrap();
    pair.client.next(&mut pair.client_ports);
    let write = pair.client_ports.write.take().unwrap();
    let old = pair.client_ports.alarms.pop().unwrap();
    pair.client
        .set_deadline(id, Some(Duration::from_secs(200)))
        .unwrap();
    pair.client.next(&mut pair.client_ports);
    pair.client
        .complete_wake(old.complete(Duration::ZERO))
        .unwrap();
    let old = pair.client_ports.alarms.pop().unwrap();
    pair.client
        .set_deadline(id, Some(Duration::from_secs(100)))
        .unwrap();
    pair.client.next(&mut pair.client_ports);
    let current = pair.client_ports.alarms.pop().unwrap();
    assert_eq!(
        pair.client.wake_tokens.len() + pair.client.cancellations.len(),
        capacity
    );

    pair.client.shutdown().unwrap();
    pair.client.advance_time(Duration::from_secs(30)).unwrap();
    pair.client.next(&mut pair.client_ports);
    assert_eq!(
        pair.client.wake_tokens.len() + pair.client.cancellations.len(),
        capacity + 3
    );
    assert_eq!(pair.client_ports.cancels.len(), 5);
    assert!(pair.client_ports.write.is_none());
    assert!(pair.client_ports.close.is_none());
    for alarm in [old, current] {
        pair.client
            .complete_wake(alarm.complete(Duration::from_secs(1_000_000)))
            .unwrap();
    }
    let read = pair.client_ports.read.take().unwrap();
    pair.client
        .complete_read(read.complete(ReadOutcome::Failed(IoFailure::Cancelled)))
        .unwrap();
    pair.client
        .complete_write(write.complete(WriteOutcome::Failed {
            progress: Progress::Exact(0),
            error: IoFailure::Cancelled,
        }))
        .unwrap();
    pair.client.next(&mut pair.client_ports);
    assert!(pair.client_ports.close.is_none());
    let cancellations = std::mem::take(&mut pair.client_ports.cancels);
    for (index, cancel) in cancellations.into_iter().enumerate() {
        pair.client.complete_cancel(cancel.complete()).unwrap();
        pair.client.next(&mut pair.client_ports);
        assert_eq!(pair.client_ports.close.is_some(), index == 4);
    }
    let close = pair.client_ports.close.take().unwrap();
    pair.client.complete_close(close.complete(Ok(()))).unwrap();
    pair.client.next(&mut pair.client_ports);
    assert_eq!(pair.client_ports.closed, [ConnectionResult::Graceful]);
    assert!(pair.client.wake_tokens.is_empty());
    assert!(pair.client.cancellations.is_empty());
}

#[test]
fn retired_sources_do_not_accumulate_behind_one_held_permit() {
    let mut pair = Pair::new(Config {
        max_send_capacity: 1024,
        max_send_buffer_capacity: 1024,
        http: HttpLimits::new().set_max_active_streams(4),
        ..Config::default()
    });
    let held = pair.client.request(&request(b"GET"), true).unwrap();
    pair.pump(32_768);
    pair.server.respond(held, &response(b"200"), false).unwrap();
    pair.pump(32_768);
    assert_eq!(pair.server_ports.permits.len(), 1);
    for index in 1..=512 {
        pair.client
            .advance_time(Duration::from_secs(index))
            .unwrap();
        pair.server
            .advance_time(Duration::from_secs(index))
            .unwrap();
        let id = pair.client.request(&request(b"GET"), true).unwrap();
        pair.pump(32_768);
        pair.server.respond(id, &response(b"200"), false).unwrap();
        pair.pump(32_768);
        assert_eq!(pair.server.demand.len(), 1);
        pair.client.reset(id, H2ErrorCode::Cancel).unwrap();
        pair.pump(32_768);
        assert!(pair.server.demand.is_empty());
        assert!(pair.server.ready.is_empty());
        assert!(pair.server.blocked.is_empty());
        assert_eq!(pair.server.streams.len(), 1);
        assert_eq!(pair.server_ports.permits.len(), 1);
    }
}

#[test]
fn a_retained_small_fragment_does_not_monopolize_receive_storage() {
    let mut pair = Pair::new(Config {
        connection_receive_window: 65_535,
        max_receive_capacity: 4 * PAGE_SIZE,
        ..Config::default()
    });
    let held = pair.client.request(&request(b"POST"), false).unwrap();
    let sibling = pair.client.request(&request(b"POST"), false).unwrap();
    pair.pump(32_768);
    let permit = pair.client_ports.permits.pop_front().unwrap();
    assert_eq!(permit.stream(), held);
    pair.client.send(permit, vec![1], false).unwrap();
    pair.pump(32_768);
    let body = pair.server_ports.bodies.pop_front().unwrap();
    let retained_address = body.bytes().as_ptr();
    for _ in 0..128 {
        let index = pair
            .client_ports
            .permits
            .iter()
            .position(|permit| permit.stream() == sibling)
            .unwrap();
        let permit = pair.client_ports.permits.remove(index).unwrap();
        pair.client.send(permit, vec![2; 1024], false).unwrap();
        pair.pump(32_768);
        let chunk = pair.server_ports.bodies.pop_front().unwrap();
        assert_eq!(chunk.stream(), sibling);
        assert_eq!(chunk.bytes(), &[2; 1024]);
        pair.server.release_body(chunk.release()).unwrap();
        pair.pump(32_768);
        assert_eq!(body.bytes().as_ptr(), retained_address);
        assert_eq!(body.bytes(), &[1]);
        assert!(pair.server.receive_capacity <= 3 * PAGE_SIZE);
        assert!(pair.server_ports.closed.is_empty());
    }
    pair.server.release_body(body.release()).unwrap();
}

#[test]
fn small_held_pages_hit_capacity_bound_not_only_payload_bound() {
    let mut pair = Pair::new(Config {
        connection_receive_window: 65_535,
        max_receive_capacity: 4 * PAGE_SIZE,
        max_stream_receive_capacity: 8 * PAGE_SIZE,
        ..Config::default()
    });
    pair.client.request(&request(b"POST"), false).unwrap();
    pair.pump(32_768);
    for _ in 0..4 {
        pair.client
            .send(
                pair.client_ports.permits.pop_front().unwrap(),
                vec![1],
                false,
            )
            .unwrap();
        pair.pump(32_768);
    }
    assert_eq!(pair.server_ports.bodies.len(), 4);
    assert_eq!(pair.server.receive_capacity, 4 * PAGE_SIZE);
    assert_eq!(
        pair.server_ports.closed,
        [ConnectionResult::ResourceExhausted]
    );
    assert!(pair.server_ports.retired.is_empty());
    pair.release_all();
    pair.pump(32_768);
    assert_eq!(pair.server_ports.retired.len(), 1);
    assert_eq!(pair.server.pool.len(), 4);
}
