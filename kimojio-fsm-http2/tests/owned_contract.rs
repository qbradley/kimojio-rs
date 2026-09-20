#[path = "../examples/support/mod.rs"]
mod support;

use kimojio_fsm_http2::*;
use std::{collections::VecDeque, time::Duration};
use support::*;

#[test]
fn fragmented_duplex_roundtrip_retirement_waits_for_release() {
    for fragment in [1, 7, 9, 16_384, 32_768] {
        let mut pair = Pair::new(Config::default());
        let id = pair.client.request(&request(b"GET"), true).unwrap();
        pair.pump(fragment);
        pair.server.respond(id, &response(b"200"), false).unwrap();
        pair.pump(fragment);
        let permit = pair.server_ports.permits.pop_front().unwrap();
        pair.server.send(permit, b"hello".to_vec(), true).unwrap();
        pair.pump(fragment);
        assert_eq!(pair.client_ports.bodies.len(), 1);
        assert_eq!(pair.client_ports.bodies[0].bytes(), b"hello");
        assert!(pair.client_ports.retired.is_empty());
        assert_eq!(
            pair.client_ports.ends,
            [ReceiveEnd {
                stream: id,
                outcome: StreamOutcome::Complete
            }]
        );
        pair.release_all();
        pair.pump(fragment);
        assert_eq!(
            pair.client_ports.retired,
            [StreamResult {
                stream: id,
                outcome: StreamOutcome::Complete
            }]
        );
        assert_eq!(pair.server_ports.retired, pair.client_ports.retired);
        assert!(pair.client_ports.closed.is_empty());
    }
}

#[test]
fn initial_permit_and_retained_capacity_rejection_preserve_buffer() {
    let config = Config {
        max_send_buffer_capacity: 1024,
        ..Config::default()
    };
    let mut pair = Pair::new(config);
    pair.client.request(&request(b"POST"), false).unwrap();
    pair.pump(32_768);
    let permit = pair
        .client_ports
        .permits
        .pop_front()
        .expect("initial source demand");
    let mut buffer = Vec::with_capacity(2048);
    buffer.push(1);
    let pointer = buffer.as_ptr();
    let rejected = pair.client.send(permit, buffer, false).unwrap_err();
    assert_eq!(rejected.error, CommandError::Capacity);
    assert_eq!(rejected.value.1.as_ptr(), pointer);
    assert_eq!(rejected.value.1.capacity(), 2048);
    pair.client.send(rejected.value.0, vec![1], true).unwrap();
    pair.pump(32_768);
    assert_eq!(pair.server_ports.bodies[0].bytes(), &[1]);
}

#[test]
fn early_response_arrives_with_original_upload_write_outstanding() {
    let mut pair = Pair::new(Config::default());
    let id = pair.client.request(&request(b"POST"), false).unwrap();
    pair.pump(32_768);
    let permit = pair.client_ports.permits.pop_front().unwrap();
    pair.client.send(permit, vec![1; 1024], true).unwrap();
    pair.client.next(&mut pair.client_ports);
    let original = pair.client_ports.write.take().unwrap();
    pair.server.respond(id, &response(b"413"), true).unwrap();
    let mut discarded = VecDeque::new();
    for _ in 0..20 {
        step(
            &mut pair.server,
            &mut pair.server_ports,
            &mut pair.to_server,
            &mut pair.to_client,
            32_768,
        );
        step(
            &mut pair.client,
            &mut pair.client_ports,
            &mut pair.to_client,
            &mut discarded,
            32_768,
        );
    }
    assert_eq!(pair.client_ports.heads[0].1, HeadKind::Response(413));
    assert!(pair.client_ports.retired.is_empty());
    let bytes: Vec<_> = original
        .slices()
        .iter()
        .flat_map(|part| part.iter().copied())
        .collect();
    let n = bytes.len();
    pair.to_server.extend(bytes);
    pair.client
        .complete_write(original.complete(WriteOutcome::Written(n)))
        .unwrap();
    pair.pump(32_768);
    assert_eq!(pair.client_ports.sent.len(), 1);
    assert_eq!(pair.client_ports.retired.len(), 1);
}

#[test]
fn wrong_owner_read_and_write_return_original_operations() {
    let mut first = Client::<Vec<u8>>::new(Config::default(), Duration::ZERO).unwrap();
    let mut other = Client::<Vec<u8>>::new(Config::default(), Duration::ZERO).unwrap();
    let mut ports = MemoryPorts::default();
    first.next(&mut ports);
    let write = ports.write.take().unwrap();
    let length = write.remaining();
    let rejected = other
        .complete_write(write.complete(WriteOutcome::Written(length)))
        .unwrap_err();
    first.complete_write(rejected.value).unwrap();
    let read = ports.read.take().unwrap();
    let rejected = other
        .complete_read(read.complete(ReadOutcome::Eof))
        .unwrap_err();
    first.complete_read(rejected.value).unwrap();
}

#[test]
fn informational_final_and_trailers_have_exact_semantic_order() {
    let mut pair = Pair::new(Config::default());
    let id = pair.client.request(&request(b"GET"), true).unwrap();
    pair.pump(32_768);
    pair.server.respond(id, &response(b"103"), false).unwrap();
    pair.server.respond(id, &response(b"200"), false).unwrap();
    pair.pump(32_768);
    let permit = pair.server_ports.permits.pop_front().unwrap();
    pair.server.send(permit, b"hello".to_vec(), false).unwrap();
    pair.pump(32_768);
    pair.server
        .trailers(id, &[H2HeaderField::new(b"x-end", b"yes")])
        .unwrap();
    pair.pump(32_768);
    assert_eq!(
        pair.client_ports
            .heads
            .iter()
            .map(|head| head.1)
            .collect::<Vec<_>>(),
        [
            HeadKind::Informational(103),
            HeadKind::Response(200),
            HeadKind::Trailers
        ]
    );
    assert_eq!(
        pair.client_ports
            .sequence
            .iter()
            .filter(|event| matches!(**event, "headers" | "body" | "ended"))
            .copied()
            .collect::<Vec<_>>(),
        ["headers", "headers", "body", "headers", "ended"]
    );
}

#[test]
fn sustained_credit_exceeds_actual_connection_window() {
    let config = Config {
        stream_receive_window: 1024,
        connection_receive_window: 65_535,
        max_send_buffer_bytes: 1024,
        ..Config::default()
    };
    let mut pair = Pair::new(config);
    let id = pair.client.request(&request(b"POST"), false).unwrap();
    pair.pump(32_768);
    let mut received = 0;
    for index in 0..200 {
        let permit = pair
            .client_ports
            .permits
            .pop_front()
            .expect("capacity notification");
        pair.client
            .send(permit, vec![42; 1024], index == 199)
            .unwrap();
        pair.pump(32_768);
        while let Some(body) = pair.server_ports.bodies.pop_front() {
            received += body.bytes().len();
            pair.server.release_body(body.release()).unwrap();
        }
        pair.pump(32_768);
    }
    assert_eq!(received, 204_800);
    pair.server.respond(id, &response(b"204"), true).unwrap();
    pair.pump(32_768);
    assert_eq!(pair.client_ports.retired.len(), 1);
}

#[test]
fn classic_connect_uses_only_method_and_authority() {
    let mut pair = Pair::new(Config::default());
    let id = pair
        .client
        .request(
            &[
                H2HeaderField::new(b":method", b"CONNECT"),
                H2HeaderField::new(b":authority", b"example.test:443"),
            ],
            false,
        )
        .unwrap();
    pair.pump(3);
    assert_eq!(pair.server_ports.heads[0].1, HeadKind::Request);
    pair.server.respond(id, &response(b"200"), false).unwrap();
    pair.pump(3);
    pair.client
        .send(
            pair.client_ports.permits.pop_front().unwrap(),
            b"up".to_vec(),
            true,
        )
        .unwrap();
    pair.server
        .send(
            pair.server_ports.permits.pop_front().unwrap(),
            b"down".to_vec(),
            true,
        )
        .unwrap();
    pair.pump(3);
    assert_eq!(pair.client_ports.bodies[0].bytes(), b"down");
    assert_eq!(pair.server_ports.bodies[0].bytes(), b"up");
    pair.release_all();
    pair.pump(3);
    assert_eq!(pair.client_ports.retired.len(), 1);
    assert_eq!(pair.server_ports.retired.len(), 1);
}

fn frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![
        (payload.len() >> 16) as u8,
        (payload.len() >> 8) as u8,
        payload.len() as u8,
        kind,
        flags,
    ];
    bytes.extend_from_slice(&stream.to_be_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

fn frames(mut bytes: &[u8]) -> Vec<(u8, u8, u32, Vec<u8>)> {
    let mut result = Vec::new();
    while !bytes.is_empty() {
        assert!(bytes.len() >= 9);
        let len = ((bytes[0] as usize) << 16) | ((bytes[1] as usize) << 8) | bytes[2] as usize;
        assert!(bytes.len() >= len + 9);
        result.push((
            bytes[3],
            bytes[4],
            u32::from_be_bytes(bytes[5..9].try_into().unwrap()),
            bytes[9..9 + len].to_vec(),
        ));
        bytes = &bytes[9 + len..];
    }
    result
}

fn pump_one(connection: &mut Connection, ports: &mut MemoryPorts, input: &[u8]) -> Vec<u8> {
    let mut input = input.iter().copied().collect();
    let mut output = VecDeque::new();
    for _ in 0..1000 {
        if !step(connection, ports, &mut input, &mut output, 32_768) {
            assert!(input.is_empty());
            return output.into();
        }
    }
    panic!("single endpoint did not become idle");
}

#[test]
fn padding_and_discard_have_exact_connection_credit() {
    let mut pair = Pair::new(Config::default());
    let id = pair.client.request(&request(b"POST"), false).unwrap();
    pair.pump(32_768);
    let output = pump_one(
        &mut pair.server,
        &mut pair.server_ports,
        &frame(0, 8, id.get(), &[3, b'a', 0, 0, 0]),
    );
    assert_eq!(
        frames(&output),
        [
            (8, 0, 0, 4u32.to_be_bytes().to_vec()),
            (8, 0, id.get(), 4u32.to_be_bytes().to_vec()),
        ]
    );
    let body = pair.server_ports.bodies.pop_front().unwrap();
    assert_eq!(body.bytes(), b"a");
    pair.server.release_body(body.release()).unwrap();
    let output = pump_one(&mut pair.server, &mut pair.server_ports, &[]);
    assert_eq!(
        frames(&output),
        [
            (8, 0, 0, 1u32.to_be_bytes().to_vec()),
            (8, 0, id.get(), 1u32.to_be_bytes().to_vec()),
        ]
    );
    pair.server.reset(id, H2ErrorCode::Cancel).unwrap();
    let reset = pump_one(&mut pair.server, &mut pair.server_ports, &[]);
    assert_eq!(
        frames(&reset),
        [(3, 0, id.get(), 8u32.to_be_bytes().to_vec())]
    );
    let output = pump_one(
        &mut pair.server,
        &mut pair.server_ports,
        &frame(0, 8, id.get(), &[3, b'b', 0, 0, 0]),
    );
    assert_eq!(frames(&output), [(8, 0, 0, 5u32.to_be_bytes().to_vec())]);
    assert!(pair.server_ports.bodies.is_empty());
}

#[test]
fn each_partial_write_cut_preserves_frame_and_original_buffer_after_reset() {
    for cut in 1..13 {
        let mut pair = Pair::new(Config::default());
        let id = pair.client.request(&request(b"POST"), false).unwrap();
        pair.pump(32_768);
        let buffer = b"body".to_vec();
        let pointer = buffer.as_ptr();
        pair.client
            .send(pair.client_ports.permits.pop_front().unwrap(), buffer, true)
            .unwrap();
        pair.client.next(&mut pair.client_ports);
        let original = pair.client_ports.write.take().unwrap();
        let expected: Vec<u8> = original
            .slices()
            .iter()
            .flat_map(|s| s.iter().copied())
            .collect();
        let mut wire = expected[..cut].to_vec();
        pair.client
            .complete_write(original.complete(WriteOutcome::Written(cut)))
            .unwrap();
        pair.client.reset(id, H2ErrorCode::Cancel).unwrap();
        wire.extend(pump_one(&mut pair.client, &mut pair.client_ports, &[]));
        let mut complete = expected;
        complete.extend(frame(3, 0, id.get(), &8u32.to_be_bytes()));
        assert_eq!(wire, complete, "cut {cut}");
        assert_eq!(pair.client_ports.sent.len(), 1);
        let returned = &pair.client_ports.sent[0];
        assert_eq!(returned.buffer.as_ptr(), pointer);
        assert_eq!(returned.accepted, 4);
        assert!(returned.exact);
        assert_eq!(returned.result, Ok(()));
        assert_eq!(
            pair.client_ports.retired,
            [StreamResult {
                stream: id,
                outcome: StreamOutcome::Reset(8)
            }]
        );
    }
}

#[test]
fn release_and_transport_completion_orders_have_same_retirement_join() {
    for release_first in [false, true] {
        for reset in [false, true] {
            let mut pair = Pair::new(Config::default());
            let id = pair.client.request(&request(b"POST"), false).unwrap();
            pair.pump(32_768);
            pair.client
                .send(
                    pair.client_ports.permits.pop_front().unwrap(),
                    vec![7],
                    true,
                )
                .unwrap();
            pair.pump(32_768);
            let lease = pair.server_ports.bodies.pop_front().unwrap();
            pair.server.respond(id, &response(b"200"), true).unwrap();
            pair.server.next(&mut pair.server_ports);
            let original = pair.server_ports.write.take().unwrap();
            let length = original.remaining();
            if reset {
                pair.server.reset(id, H2ErrorCode::Cancel).unwrap();
            }
            if release_first {
                pair.server.release_body(lease.release()).unwrap();
                pair.server.next(&mut pair.server_ports);
                assert!(pair.server_ports.retired.is_empty());
                pair.server
                    .complete_write(original.complete(WriteOutcome::Written(length)))
                    .unwrap();
            } else {
                pair.server
                    .complete_write(original.complete(WriteOutcome::Written(length)))
                    .unwrap();
                pair.server.next(&mut pair.server_ports);
                assert!(pair.server_ports.retired.is_empty());
                pair.server.release_body(lease.release()).unwrap();
            }
            // The callbacks above all continue. Batched completions never revive a retired record.
            pump_one(&mut pair.server, &mut pair.server_ports, &[]);
            assert_eq!(pair.server_ports.retired.len(), 1);
            assert_eq!(pair.server_ports.ends.len(), 1);
            assert_eq!(pair.server_ports.stopped.len(), 1);
            assert!(pair.server_ports.closed.is_empty());
        }
    }
}

#[test]
fn revoked_permit_does_not_consume_capacity_of_a_sibling() {
    let mut pair = Pair::new(Config {
        max_send_capacity: 1024,
        max_send_buffer_capacity: 1024,
        ..Config::default()
    });
    let first = pair.client.request(&request(b"POST"), false).unwrap();
    let second = pair.client.request(&request(b"POST"), false).unwrap();
    pair.pump(32_768);
    assert_eq!(pair.client_ports.permits.len(), 1);
    let revoked = pair.client_ports.permits.pop_front().unwrap();
    assert_eq!(revoked.stream(), first);
    pair.client.reset(first, H2ErrorCode::Cancel).unwrap();
    pair.pump(32_768);
    assert_eq!(pair.client_ports.permits.len(), 1);
    assert_eq!(pair.client_ports.permits[0].stream(), second);
    let buffer = vec![1];
    let pointer = buffer.as_ptr();
    let rejected = pair.client.send(revoked, buffer, true).unwrap_err();
    assert_eq!(rejected.error, CommandError::InvalidState);
    assert_eq!(rejected.value.1.as_ptr(), pointer);
}

#[test]
fn negative_stream_window_after_settings_recovers_without_polling_source() {
    let mut pair = Pair::new(Config::default());
    let id = pair.client.request(&request(b"POST"), false).unwrap();
    pair.pump(32_768);
    pair.client
        .send(
            pair.client_ports.permits.pop_front().unwrap(),
            vec![1; 2000],
            false,
        )
        .unwrap();
    pair.pump(32_768);
    let permit = pair.client_ports.permits.pop_front().unwrap();
    // Initial window decreases to 1024 after 2000 unconsumed DATA octets.
    let settings = frame(4, 0, 0, &[0, 4, 0, 0, 4, 0]);
    let ack = pump_one(&mut pair.client, &mut pair.client_ports, &settings);
    assert_eq!(frames(&ack), [(4, 1, 0, vec![])]);
    pair.client.send(permit, vec![2; 100], true).unwrap();
    assert!(pump_one(&mut pair.client, &mut pair.client_ports, &[]).is_empty());
    let update = frame(8, 0, id.get(), &2000u32.to_be_bytes());
    let output = pump_one(&mut pair.client, &mut pair.client_ports, &update);
    assert_eq!(frames(&output), [(0, 1, id.get(), vec![2; 100])]);
}

#[test]
fn empty_data_counts_as_an_item_and_reset_preserves_siblings() {
    let mut pair = Pair::new(Config {
        max_stream_fragments: 2,
        ..Config::default()
    });
    let first = pair.client.request(&request(b"POST"), false).unwrap();
    let second = pair.client.request(&request(b"POST"), false).unwrap();
    pair.pump(32_768);
    let mut bytes = frame(0, 0, first.get(), &[]);
    bytes.extend(frame(0, 0, first.get(), &[]));
    bytes.extend(frame(0, 0, first.get(), &[]));
    bytes.extend(frame(0, 1, second.get(), b"sibling"));
    let output = pump_one(&mut pair.server, &mut pair.server_ports, &bytes);
    assert_eq!(
        frames(&output),
        [(3, 0, first.get(), 11u32.to_be_bytes().to_vec())]
    );
    assert_eq!(pair.server_ports.bodies.len(), 3);
    assert_eq!(pair.server_ports.bodies[2].stream(), second);
    assert_eq!(pair.server_ports.bodies[2].bytes(), b"sibling");
    assert!(pair.server_ports.closed.is_empty());
}

#[test]
fn deadline_is_stream_local_and_old_wake_cannot_fire_replacement() {
    let mut pair = Pair::new(Config::default());
    let first = pair.client.request(&request(b"POST"), false).unwrap();
    let second = pair.client.request(&request(b"POST"), false).unwrap();
    pair.pump(32_768);
    pair.client
        .set_deadline(first, Some(Duration::from_secs(5)))
        .unwrap();
    pair.client.next(&mut pair.client_ports);
    let alarm = pair.client_ports.alarms.pop().unwrap();
    assert_eq!(alarm.deadline(), Duration::from_secs(5));
    pair.client
        .set_deadline(first, Some(Duration::from_secs(10)))
        .unwrap();
    pair.client.next(&mut pair.client_ports);
    let cancel = pair.client_ports.cancels.pop().unwrap();
    pair.client.complete_cancel(cancel.complete()).unwrap();
    pair.client
        .complete_wake(alarm.complete(Duration::from_secs(100)))
        .unwrap();
    assert!(pair.client_ports.retired.is_empty());
    pair.client.advance_time(Duration::from_secs(10)).unwrap();
    let output = pump_one(&mut pair.client, &mut pair.client_ports, &[]);
    assert_eq!(
        frames(&output),
        [(3, 0, first.get(), 8u32.to_be_bytes().to_vec())]
    );
    assert_eq!(
        pair.client_ports.retired,
        [StreamResult {
            stream: first,
            outcome: StreamOutcome::Deadline
        }]
    );
    assert!(
        pair.client_ports
            .stopped
            .iter()
            .all(|(stream, _)| *stream != second)
    );
}

#[test]
fn push_before_ack_preserves_hpack_and_after_ack_is_connection_error() {
    for fragmented in [false, true] {
        let mut client = Client::new(Config::default(), Duration::ZERO).unwrap();
        let id = client.request(&request(b"GET"), true).unwrap();
        let mut ports = MemoryPorts::default();
        let initial = pump_one(&mut client, &mut ports, &frame(4, 0, 0, &[]));
        let initial_frames = frames(&initial[24..]);
        let settings = &initial_frames[0];
        assert_eq!((settings.0, settings.1, settings.2), (4, 0, 0));
        assert!(settings.3.as_chunks::<6>().0.contains(&[0, 2, 0, 0, 0, 0]));
        let mut encoder = H2HeaderBlockEncoder::new();
        let mut promised = request(b"GET");
        promised.push(H2HeaderField::new(b"x-shared", b"push-value"));
        let block = encoder.try_encode_fields(&promised).unwrap();
        let mut payload = 2u32.to_be_bytes().to_vec();
        let input = if fragmented {
            payload.extend_from_slice(&block[..3]);
            let mut bytes = frame(5, 0, id.get(), &payload);
            bytes.extend(frame(9, 4, id.get(), &block[3..]));
            bytes
        } else {
            payload.extend_from_slice(&block);
            frame(5, 4, id.get(), &payload)
        };
        let output = pump_one(&mut client, &mut ports, &input);
        assert_eq!(frames(&output), [(3, 0, 2, 8u32.to_be_bytes().to_vec())]);
        let fields = vec![
            H2HeaderField::new(b":status", b"200"),
            H2HeaderField::new(b"x-shared", b"second-value"),
        ];
        let rejected_response = encoder.try_encode_fields(&fields).unwrap();
        assert!(pump_one(&mut client, &mut ports, &frame(1, 5, 2, &rejected_response)).is_empty());
        let response = encoder.try_encode_fields(&fields).unwrap();
        assert!(response.len() < rejected_response.len());
        pump_one(&mut client, &mut ports, &frame(1, 4, id.get(), &response));
        assert_eq!(ports.heads.len(), 1);
        assert_eq!(ports.heads[0].2[1].value, b"second-value");
        pump_one(&mut client, &mut ports, &frame(4, 1, 0, &[]));
        let mut payload = 4u32.to_be_bytes().to_vec();
        payload.extend_from_slice(&block);
        let output = pump_one(&mut client, &mut ports, &frame(5, 4, id.get(), &payload));
        assert_eq!(
            frames(&output),
            [(7, 0, 0, [0u32.to_be_bytes(), 1u32.to_be_bytes()].concat())]
        );
        assert!(
            matches!(ports.closed.as_slice(), [ConnectionResult::Protocol(error)] if error.code == H2ErrorCode::ProtocolError)
        );
    }
}

#[test]
fn graceful_shutdown_sends_two_goaways_and_preserves_processed_request() {
    let mut pair = Pair::new(Config::default());
    let id = pair.client.request(&request(b"GET"), true).unwrap();
    pair.pump(32_768);
    pair.server.shutdown().unwrap();
    let output = pump_one(&mut pair.server, &mut pair.server_ports, &[]);
    assert_eq!(
        frames(&output),
        [
            (
                7,
                0,
                0,
                [0x7fff_ffffu32.to_be_bytes(), 0u32.to_be_bytes()].concat()
            ),
            (6, 0, 0, b"kimojio!".to_vec()),
        ]
    );
    let output = pump_one(
        &mut pair.server,
        &mut pair.server_ports,
        &frame(6, 1, 0, b"kimojio!"),
    );
    assert_eq!(
        frames(&output),
        [(
            7,
            0,
            0,
            [id.get().to_be_bytes(), 0u32.to_be_bytes()].concat()
        )]
    );
    assert!(pair.server_ports.closed.is_empty());
    pair.server.respond(id, &response(b"204"), true).unwrap();
    pump_one(&mut pair.server, &mut pair.server_ports, &[]);
    assert_eq!(
        pair.server_ports.retired,
        [StreamResult {
            stream: id,
            outcome: StreamOutcome::Complete
        }]
    );
    assert_eq!(
        pair.server_ports.closed,
        [ConnectionResult::Graceful],
        "sequence {:?}, alarms {}, read {}, write {}",
        pair.server_ports.sequence,
        pair.server_ports.alarms.len(),
        pair.server_ports.read.is_some(),
        pair.server_ports.write.is_some()
    );
}

#[test]
fn goaway_excludes_only_locally_initiated_streams_above_boundary() {
    let mut pair = Pair::new(Config::default());
    let first = pair.client.request(&request(b"GET"), true).unwrap();
    let second = pair.client.request(&request(b"POST"), false).unwrap();
    pair.pump(32_768);
    let goaway = frame(
        7,
        0,
        0,
        &[first.get().to_be_bytes(), 0u32.to_be_bytes()].concat(),
    );
    pump_one(&mut pair.client, &mut pair.client_ports, &goaway);
    assert_eq!(
        pair.client_ports.retired,
        [StreamResult {
            stream: second,
            outcome: StreamOutcome::Unprocessed
        }]
    );
    assert!(pair.client.request(&request(b"GET"), true).is_err());
    // A client's GOAWAY last-stream zero does not reject the server's responses.
    pump_one(
        &mut pair.server,
        &mut pair.server_ports,
        &frame(7, 0, 0, &[0; 8]),
    );
    assert!(pair.server_ports.retired.is_empty());
    pair.server.respond(first, &response(b"204"), true).unwrap();
    pump_one(&mut pair.server, &mut pair.server_ports, &[]);
    assert_eq!(
        pair.server_ports.retired,
        [StreamResult {
            stream: first,
            outcome: StreamOutcome::Complete
        }]
    );
}

#[test]
fn unknown_write_progress_keeps_lower_bound_and_never_replays() {
    let mut pair = Pair::new(Config::default());
    let id = pair.client.request(&request(b"POST"), false).unwrap();
    pair.pump(32_768);
    let buffer = vec![42; 20];
    let pointer = buffer.as_ptr();
    pair.client
        .send(pair.client_ports.permits.pop_front().unwrap(), buffer, true)
        .unwrap();
    pair.client.next(&mut pair.client_ports);
    let original = pair.client_ports.write.take().unwrap();
    pair.client
        .complete_write(original.complete(WriteOutcome::Failed {
            progress: Progress::AtLeast(12),
            error: IoFailure::Failed,
        }))
        .unwrap();
    let output = pump_one(&mut pair.client, &mut pair.client_ports, &[]);
    assert!(output.is_empty());
    assert_eq!(pair.client_ports.sent.len(), 1);
    assert_eq!(pair.client_ports.sent[0].accepted, 3);
    assert!(!pair.client_ports.sent[0].exact);
    assert_eq!(pair.client_ports.sent[0].buffer.as_ptr(), pointer);
    assert_eq!(
        pair.client_ports.retired,
        [StreamResult {
            stream: id,
            outcome: StreamOutcome::ConnectionFailed
        }]
    );
    assert_eq!(pair.client_ports.closed, [ConnectionResult::IoFailed]);
}

#[test]
fn cancellation_ack_does_not_settle_original_read_or_alarm() {
    let mut client = Client::<Vec<u8>>::new(Config::default(), Duration::ZERO).unwrap();
    let mut ports = MemoryPorts::default();
    pump_one(&mut client, &mut ports, &[]);
    let original_read = ports.read.take().unwrap();
    let original_alarm = ports.alarms.pop().unwrap();
    client.advance_time(Duration::from_secs(10)).unwrap();
    client.next(&mut ports);
    for cancel in ports.cancels.drain(..) {
        client.complete_cancel(cancel.complete()).unwrap();
    }
    let write = ports.write.take().unwrap();
    let len = write.remaining();
    client
        .complete_write(write.complete(WriteOutcome::Written(len)))
        .unwrap();
    client.next(&mut ports);
    assert!(ports.close.is_none());
    client
        .complete_read(original_read.complete(ReadOutcome::Failed(IoFailure::Cancelled)))
        .unwrap();
    client.next(&mut ports);
    assert!(ports.close.is_none());
    client
        .complete_wake(original_alarm.complete(Duration::from_secs(10)))
        .unwrap();
    client.next(&mut ports);
    assert!(ports.close.is_some());
    client
        .complete_close(ports.close.take().unwrap().complete(Err(IoFailure::Failed)))
        .unwrap();
    client.next(&mut ports);
    assert!(
        matches!(ports.closed.as_slice(), [ConnectionResult::Protocol(error)] if error.code == H2ErrorCode::SettingsTimeout)
    );
}
