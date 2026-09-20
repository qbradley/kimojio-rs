#[path = "../examples/support/mod.rs"]
mod support;

use kimojio_fsm_http2::*;
use std::{collections::VecDeque, time::Duration};
use support::*;

fn frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let mut result = vec![
        (payload.len() >> 16) as u8,
        (payload.len() >> 8) as u8,
        payload.len() as u8,
        kind,
        flags,
    ];
    result.extend_from_slice(&stream.to_be_bytes());
    result.extend_from_slice(payload);
    result
}

fn frames(mut bytes: &[u8]) -> Vec<(u8, u8, u32, Vec<u8>)> {
    let mut result = Vec::new();
    while !bytes.is_empty() {
        assert!(bytes.len() >= 9);
        let length = ((bytes[0] as usize) << 16) | ((bytes[1] as usize) << 8) | bytes[2] as usize;
        assert!(bytes.len() >= length + 9);
        result.push((
            bytes[3],
            bytes[4],
            u32::from_be_bytes(bytes[5..9].try_into().unwrap()),
            bytes[9..9 + length].to_vec(),
        ));
        bytes = &bytes[9 + length..];
    }
    result
}

fn pump(connection: &mut Connection, ports: &mut MemoryPorts, bytes: &[u8]) -> Vec<u8> {
    let mut incoming = bytes.iter().copied().collect();
    let mut outgoing = VecDeque::new();
    for _ in 0..4096 {
        if !step(connection, ports, &mut incoming, &mut outgoing, 32_768) {
            assert!(incoming.is_empty());
            return outgoing.into();
        }
    }
    panic!("transport failed to become idle");
}

#[test]
fn both_directions_exceed_actual_default_windows_and_eight_megabytes() {
    let total = 9 * 1024 * 1024 + 17;
    let mut pair = Pair::new(Config {
        http: HttpLimits::new().set_max_body_bytes(total),
        ..Config::default()
    });
    let id = pair.client.request(&request(b"POST"), false).unwrap();
    pair.pump(32_768);
    pair.server.respond(id, &response(b"200"), false).unwrap();
    pair.pump(32_768);
    let mut uploaded = 0;
    let mut downloaded = 0;
    for offset in (0..total).step_by(16_384) {
        let count = (total - offset).min(16_384);
        let end = offset + count == total;
        pair.client
            .send(
                pair.client_ports
                    .permits
                    .pop_front()
                    .expect("upload admission"),
                vec![1; count],
                end,
            )
            .unwrap();
        pair.server
            .send(
                pair.server_ports
                    .permits
                    .pop_front()
                    .expect("download admission"),
                vec![2; count],
                end,
            )
            .unwrap();
        pair.pump(32_768);
        while let Some(body) = pair.server_ports.bodies.pop_front() {
            uploaded += body.bytes().len();
            assert!(body.bytes().iter().all(|byte| *byte == 1));
            pair.server.release_body(body.release()).unwrap();
        }
        while let Some(body) = pair.client_ports.bodies.pop_front() {
            downloaded += body.bytes().len();
            assert!(body.bytes().iter().all(|byte| *byte == 2));
            pair.client.release_body(body.release()).unwrap();
        }
        pair.client_ports.sent.clear();
        pair.server_ports.sent.clear();
        pair.pump(32_768);
        assert_eq!(uploaded, offset + count);
        assert_eq!(downloaded, offset + count);
        assert!(pair.client_ports.closed.is_empty());
        assert!(pair.server_ports.closed.is_empty());
    }
    assert_eq!(
        pair.client_ports.retired,
        [StreamResult {
            stream: id,
            outcome: StreamOutcome::Complete
        }]
    );
    assert_eq!(pair.server_ports.retired, pair.client_ports.retired);
}

#[test]
fn sequential_end_stream_fragments_refund_more_than_default_connection_window() {
    let mut pair = Pair::new(Config::default());
    let mut bytes = 0;
    for _ in 0..1030 {
        let id = pair.client.request(&request(b"POST"), false).unwrap();
        pair.pump(32_768);
        pair.client
            .send(
                pair.client_ports.permits.pop_front().unwrap(),
                vec![1; 1024],
                true,
            )
            .unwrap();
        pair.pump(32_768);
        let body = pair
            .server_ports
            .bodies
            .pop_front()
            .expect("END_STREAM must not strand connection credit");
        bytes += body.bytes().len();
        pair.server.release_body(body.release()).unwrap();
        pair.server.respond(id, &response(b"204"), true).unwrap();
        pair.pump(32_768);
        pair.client_ports.sent.clear();
    }
    assert_eq!(bytes, 1030 * 1024);
    assert_eq!(pair.client_ports.retired.len(), 1030);
    assert_eq!(pair.server_ports.retired.len(), 1030);
    assert!(pair.client_ports.closed.is_empty());
}

#[test]
fn concurrent_uploads_are_round_robin_and_ping_preempts_next_data() {
    let mut pair = Pair::new(Config::default());
    let ids: Vec<_> = (0..3)
        .map(|_| pair.client.request(&request(b"POST"), false).unwrap())
        .collect();
    pair.pump(32_768);
    for _ in 0..3 {
        pair.client
            .send(
                pair.client_ports.permits.pop_front().unwrap(),
                vec![1; 49_152],
                true,
            )
            .unwrap();
    }
    pair.client.next(&mut pair.client_ports);
    let original = pair.client_ports.write.take().unwrap();
    let mut wire: Vec<_> = original
        .slices()
        .iter()
        .flat_map(|part| part.iter().copied())
        .collect();
    let ping = frame(6, 0, 0, b"12345678");
    let mut read = pair.client_ports.read.take().unwrap();
    read.buffer_mut()[..ping.len()].copy_from_slice(&ping);
    pair.client
        .complete_read(read.complete(ReadOutcome::Read(ping.len())))
        .unwrap();
    pair.client.next(&mut pair.client_ports);
    assert!(pair.client_ports.write.is_none());
    let count = original.remaining();
    pair.client
        .complete_write(original.complete(WriteOutcome::Written(count)))
        .unwrap();
    wire.extend(pump(&mut pair.client, &mut pair.client_ports, &[]));
    let decoded = frames(&wire);
    assert_eq!(decoded[0], (0, 0, ids[0].get(), vec![1; 16_384]));
    assert_eq!(decoded[1], (6, 1, 0, b"12345678".to_vec()));
    let data: Vec<_> = decoded
        .iter()
        .filter(|(kind, _, _, _)| *kind == 0)
        .map(|(_, flags, id, bytes)| (*flags, *id, bytes.len()))
        .collect();
    assert_eq!(
        data,
        [
            (0, ids[0].get(), 16_384),
            (0, ids[1].get(), 16_384),
            (0, ids[2].get(), 16_384),
            (0, ids[0].get(), 16_384),
            (0, ids[1].get(), 16_384),
            (0, ids[2].get(), 16_384),
            (1, ids[0].get(), 16_384),
            (1, ids[1].get(), 16_384),
            (1, ids[2].get(), 16_384),
        ]
    );
    assert_eq!(decoded.len(), 10);
}

#[test]
fn one_byte_connection_refunds_do_not_starve_siblings() {
    let mut pair = Pair::new(Config {
        connection_receive_window: 65_535,
        ..Config::default()
    });
    let ids: Vec<_> = (0..3)
        .map(|_| pair.client.request(&request(b"POST"), false).unwrap())
        .collect();
    pair.pump(32_768);
    for _ in 0..3 {
        pair.client
            .send(
                pair.client_ports.permits.pop_front().unwrap(),
                vec![1; 49_152],
                true,
            )
            .unwrap();
    }
    let initial = pump(&mut pair.client, &mut pair.client_ports, &[]);
    assert_eq!(
        frames(&initial)
            .iter()
            .map(|frame| frame.3.len())
            .sum::<usize>(),
        65_535
    );
    let mut observed = Vec::new();
    for _ in 0..9 {
        let output = pump(
            &mut pair.client,
            &mut pair.client_ports,
            &frame(8, 0, 0, &1u32.to_be_bytes()),
        );
        let decoded = frames(&output);
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].0, 0);
        assert_eq!(decoded[0].3, [1]);
        observed.push(decoded[0].2);
    }
    assert_eq!(
        observed,
        ids.iter()
            .cycle()
            .take(9)
            .map(|id| id.get())
            .collect::<Vec<_>>()
    );
}

#[test]
fn head_informational_response_does_not_end_stream_or_apply_representation_body_limit() {
    let mut pair = Pair::new(Config {
        http: HttpLimits::new().set_max_body_bytes(1),
        ..Config::default()
    });
    let id = pair.client.request(&request(b"HEAD"), true).unwrap();
    pair.pump(32_768);
    pair.server.respond(id, &response(b"103"), false).unwrap();
    pair.server
        .respond_ref(
            id,
            &[
                H2RawHeaderRef::new(b":status", b"200"),
                H2RawHeaderRef::new(b"content-length", b"1000000000"),
            ],
            false,
        )
        .unwrap();
    let wire = pump(&mut pair.server, &mut pair.server_ports, &[]);
    assert_eq!(
        frames(&wire)
            .iter()
            .map(|f| (f.0, f.1, f.2))
            .collect::<Vec<_>>(),
        [(1, 4, id.get()), (1, 5, id.get())]
    );
    pump(&mut pair.client, &mut pair.client_ports, &wire);
    assert_eq!(
        pair.client_ports
            .heads
            .iter()
            .map(|h| h.1)
            .collect::<Vec<_>>(),
        [HeadKind::Informational(103), HeadKind::Response(200)]
    );
    assert!(pair.client_ports.bodies.is_empty());
    assert!(pair.server_ports.permits.is_empty());
    assert_eq!(pair.client_ports.retired.len(), 1);
}

#[test]
fn content_length_and_trailer_rejection_do_not_mutate_encoder_or_permit() {
    let mut pair = Pair::new(Config {
        http: HttpLimits::new().set_max_body_bytes(4),
        ..Config::default()
    });
    let mut fields = request(b"POST");
    fields.push(H2HeaderField::new(b"content-length", b"3"));
    let id = pair.client.request(&fields, false).unwrap();
    pair.pump(32_768);
    let permit = pair.client_ports.permits.pop_front().unwrap();
    let rejected = pair
        .client
        .send(permit, b"short".to_vec(), true)
        .unwrap_err();
    assert!(matches!(
        rejected.error,
        CommandError::Message(ServerError::BodyTooLarge { .. })
    ));
    let rejected = pair
        .client
        .send(rejected.value.0, b"ab".to_vec(), true)
        .unwrap_err();
    assert_eq!(
        rejected.error,
        CommandError::Message(ServerError::InvalidContentLength)
    );
    pair.client
        .send(rejected.value.0, b"abc".to_vec(), false)
        .unwrap();
    pair.pump(32_768);
    assert!(
        pair.client
            .trailers_ref(id, &[H2RawHeaderRef::new(b"content-length", b"3")])
            .is_err()
    );
    pair.client
        .trailers_ref(id, &[H2RawHeaderRef::new(b"x-end", b"yes")])
        .unwrap();
    pair.pump(32_768);
    assert_eq!(
        pair.server_ports.heads.last().unwrap().1,
        HeadKind::Trailers
    );
    assert_eq!(pair.server_ports.bodies[0].bytes(), b"abc");
    assert!(pair.server_ports.closed.is_empty());
}

#[test]
fn streaming_body_limit_without_content_length_preserves_both_role_buffers() {
    let mut pair = Pair::new(Config {
        http: HttpLimits::new().set_max_body_bytes(1),
        ..Config::default()
    });
    let id = pair.client.request(&request(b"POST"), false).unwrap();
    pair.pump(32_768);
    pair.server.respond(id, &response(b"200"), false).unwrap();
    pair.pump(32_768);
    for (connection, ports) in [
        (&mut *pair.client, &mut pair.client_ports),
        (&mut *pair.server, &mut pair.server_ports),
    ] {
        let buffer = vec![1, 2];
        let pointer = buffer.as_ptr();
        let permit = ports.permits.pop_front().unwrap();
        let rejected = connection.send(permit, buffer, true).unwrap_err();
        assert_eq!(
            rejected.error,
            CommandError::Message(ServerError::BodyTooLarge {
                limit: 1,
                actual: 2
            })
        );
        let (permit, mut buffer) = rejected.value;
        assert_eq!(permit.stream(), id);
        assert_eq!(buffer.as_ptr(), pointer);
        assert_eq!(buffer, [1, 2]);
        buffer.truncate(1);
        connection.send(permit, buffer, true).unwrap();
    }
    pair.pump(32_768);
    assert_eq!(pair.client_ports.bodies[0].bytes(), [1]);
    assert_eq!(pair.server_ports.bodies[0].bytes(), [1]);
    pair.release_all();
    pair.pump(32_768);
    assert_eq!(
        pair.client_ports.retired,
        [StreamResult {
            stream: id,
            outcome: StreamOutcome::Complete
        }]
    );
    assert_eq!(pair.server_ports.retired, pair.client_ports.retired);
    assert!(pair.client_ports.closed.is_empty());
    assert!(pair.server_ports.closed.is_empty());
}

#[test]
fn oversized_decoded_headers_preserve_hpack_for_following_stream() {
    let config = Config {
        http: HttpLimits::new().set_max_headers(2),
        ..Config::default()
    };
    let mut server = Server::new(config, Duration::ZERO).unwrap();
    let mut ports = MemoryPorts::default();
    let mut preface = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_vec();
    preface.extend(frame(4, 0, 0, &[]));
    pump(&mut server, &mut ports, &preface);
    pump(&mut server, &mut ports, &frame(4, 1, 0, &[]));
    let mut encoder = H2HeaderBlockEncoder::new();
    let mut fields = request(b"GET");
    fields.push(H2HeaderField::new(b"x-shared", b"retained"));
    fields.push(H2HeaderField::new(b"x-excess", b"rejected"));
    let rejected = encoder.try_encode_fields(&fields).unwrap();
    let output = pump(&mut server, &mut ports, &frame(1, 5, 1, &rejected));
    assert_eq!(frames(&output), [(3, 0, 1, 1u32.to_be_bytes().to_vec())]);
    assert!(ports.heads.is_empty());
    fields.pop();
    let accepted = encoder.try_encode_fields(&fields).unwrap();
    assert!(accepted.len() < rejected.len());
    assert!(pump(&mut server, &mut ports, &frame(1, 5, 3, &accepted)).is_empty());
    assert_eq!(ports.heads.len(), 1);
    assert_eq!(ports.heads[0].0.get(), 3);
    assert_eq!(ports.heads[0].2.last().unwrap().value, b"retained");
    assert!(ports.closed.is_empty());
}

#[test]
fn continuation_interleaving_and_settings_ack_timeout_keep_connection_scope() {
    let mut pair = Pair::new(Config::default());
    let id = pair.client.request(&request(b"GET"), true).unwrap();
    pair.pump(32_768);
    let mut encoder = H2HeaderBlockEncoder::new();
    let block = encoder.try_encode_fields(&response(b"200")).unwrap();
    assert!(
        pump(
            &mut pair.client,
            &mut pair.client_ports,
            &frame(1, 0, id.get(), &block)
        )
        .is_empty()
    );
    let output = pump(
        &mut pair.client,
        &mut pair.client_ports,
        &frame(6, 0, 0, b"abcdefgh"),
    );
    assert_eq!(
        frames(&output),
        [(7, 0, 0, [0u32.to_be_bytes(), 1u32.to_be_bytes()].concat())]
    );
    assert!(
        matches!(pair.client_ports.closed.as_slice(), [ConnectionResult::Protocol(error)]
        if error.scope == H2ErrorScope::Connection && error.code == H2ErrorCode::ProtocolError)
    );
}

#[test]
fn read_failure_cancels_original_write_without_starting_an_uncancelled_continuation() {
    let mut pair = Pair::new(Config::default());
    pair.client.request(&request(b"POST"), false).unwrap();
    pair.pump(32_768);
    pair.client
        .send(
            pair.client_ports.permits.pop_front().unwrap(),
            vec![1; 100],
            true,
        )
        .unwrap();
    pair.client.next(&mut pair.client_ports);
    let write = pair.client_ports.write.take().unwrap();
    let read = pair.client_ports.read.take().unwrap();
    pair.client
        .complete_read(read.complete(ReadOutcome::Failed(IoFailure::Failed)))
        .unwrap();
    pair.client.next(&mut pair.client_ports);
    let cancel = pair.client_ports.cancels.pop().unwrap();
    assert_eq!(cancel.original(), write.token());
    pair.client.complete_cancel(cancel.complete()).unwrap();
    pair.client.next(&mut pair.client_ports);
    assert!(pair.client_ports.close.is_none());
    pair.client
        .complete_write(write.complete(WriteOutcome::Written(12)))
        .unwrap();
    let output = pump(&mut pair.client, &mut pair.client_ports, &[]);
    assert!(output.is_empty());
    assert_eq!(pair.client_ports.sent.len(), 1);
    assert_eq!(pair.client_ports.sent[0].accepted, 3);
    assert!(pair.client_ports.sent[0].exact);
    assert_eq!(
        pair.client_ports.sent[0].result,
        Err(SendStop::ConnectionFailed)
    );
    assert_eq!(pair.client_ports.closed, [ConnectionResult::IoFailed]);
}
