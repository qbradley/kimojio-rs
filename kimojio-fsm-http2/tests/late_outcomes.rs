#[path = "../examples/support/mod.rs"]
mod support;

use kimojio_fsm_http2::*;
use std::{collections::VecDeque, time::Duration};
use support::*;

#[test]
fn frames_after_receive_end_reset_only_that_stream_and_preserve_hpack() {
    for headers in [false, true] {
        let mut pair = Pair::new(Config::default());
        let first = pair.client.request(&request(b"POST"), false).unwrap();
        let sibling = pair.client.request(&request(b"GET"), true).unwrap();
        pair.pump(32_768);
        pair.server.respond(first, &response(b"200"), true).unwrap();
        pair.pump(32_768);
        let mut encoder = H2HeaderBlockEncoder::new();
        let fields = [
            H2HeaderField::new(b":status", b"200"),
            H2HeaderField::new(b"x-shared", b"late"),
        ];
        let packet = |kind, id: u32, payload: &[u8]| {
            let mut wire = vec![0, 0, payload.len() as u8, kind, 5];
            wire.extend(id.to_be_bytes());
            wire.extend(payload);
            wire
        };
        let invalid = if headers {
            packet(1, first.get(), &encoder.try_encode_fields(&fields).unwrap())
        } else {
            packet(0, first.get(), &[1])
        };
        read(&mut pair.client, &mut pair.client_ports, &invalid);
        let mut expected = Vec::new();
        if !headers {
            expected.extend([0, 0, 4, 8, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        }
        expected.extend([0, 0, 4, 3, 0, 0, 0, 0, 1, 0, 0, 0, 5]);
        assert_eq!(settle(&mut pair.client, &mut pair.client_ports), expected);
        let block = encoder.try_encode_fields(&fields).unwrap();
        if headers {
            assert_eq!(block, [0x88, 0xbe]);
        }
        read(
            &mut pair.client,
            &mut pair.client_ports,
            &packet(1, sibling.get(), &block),
        );
        assert!(settle(&mut pair.client, &mut pair.client_ports).is_empty());
        assert_eq!(
            pair.client_ports.retired,
            [
                StreamResult {
                    stream: first,
                    outcome: StreamOutcome::Reset(5)
                },
                StreamResult {
                    stream: sibling,
                    outcome: StreamOutcome::Complete
                },
            ]
        );
        assert!(pair.client_ports.closed.is_empty());
    }
}

#[test]
fn response_end_closes_only_receive_while_upload_continues() {
    for ending in 0..3 {
        let mut pair = Pair::new(Config::default());
        let id = pair.client.request(&request(b"POST"), false).unwrap();
        pair.pump(32_768);
        pair.server
            .respond(id, &response(b"200"), ending == 0)
            .unwrap();
        pair.pump(32_768);
        if ending == 1 {
            pair.server
                .send(
                    pair.server_ports.permits.pop_front().unwrap(),
                    vec![1],
                    true,
                )
                .unwrap();
        } else if ending == 2 {
            pair.server
                .trailers_ref(id, &[H2RawHeaderRef::new(b"x-end", b"yes")])
                .unwrap();
        }
        pair.pump(32_768);
        assert_eq!(
            pair.client_ports.ends,
            [ReceiveEnd {
                stream: id,
                outcome: StreamOutcome::Complete
            }]
        );
        for end in [false, true] {
            pair.client
                .send(pair.client_ports.permits.pop_front().unwrap(), vec![7], end)
                .unwrap();
            pair.client.next(&mut pair.client_ports);
            let write = pair
                .client_ports
                .write
                .take()
                .expect("receive end preserves the send half");
            let wire: Vec<_> = write
                .slices()
                .iter()
                .flat_map(|s| s.iter().copied())
                .collect();
            assert_eq!(wire, [0, 0, 1, 0, u8::from(end), 0, 0, 0, 1, 7]);
            pair.to_server.extend(&wire);
            pair.client
                .complete_write(write.complete(WriteOutcome::Written(wire.len())))
                .unwrap();
            pair.pump(32_768);
        }
        assert_eq!(
            pair.server_ports
                .bodies
                .iter()
                .map(|body| body.bytes()[0])
                .collect::<Vec<_>>(),
            [7, 7]
        );
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
    }
}

fn settle(connection: &mut Connection, ports: &mut MemoryPorts) -> Vec<u8> {
    let mut output = VecDeque::new();
    for _ in 0..1024 {
        if !step(connection, ports, &mut VecDeque::new(), &mut output, 32_768) {
            return output.into();
        }
    }
    panic!("settlement did not become idle");
}

fn read(connection: &mut Connection, ports: &mut MemoryPorts, bytes: &[u8]) {
    let mut op = ports.read.take().unwrap();
    op.buffer_mut()[..bytes.len()].copy_from_slice(bytes);
    connection
        .complete_read(op.complete(ReadOutcome::Read(bytes.len())))
        .unwrap();
    connection.next(ports);
}

#[test]
fn completed_and_reset_outcomes_survive_late_connection_failure_and_release() {
    for reset in [false, true] {
        for release_first in [false, true] {
            let mut pair = Pair::new(Config::default());
            let id = pair.client.request(&request(b"GET"), true).unwrap();
            pair.pump(32_768);
            pair.server.respond(id, &response(b"200"), false).unwrap();
            pair.pump(32_768);
            pair.server
                .send(
                    pair.server_ports.permits.pop_front().unwrap(),
                    vec![1],
                    true,
                )
                .unwrap();
            pair.pump(32_768);
            let body = pair.client_ports.bodies.pop_front().unwrap();
            if reset {
                pair.client.reset(id, H2ErrorCode::Cancel).unwrap();
                assert_eq!(
                    pair.client.reset(id, H2ErrorCode::Cancel),
                    Err(CommandError::InvalidState)
                );
                assert_eq!(
                    settle(&mut pair.client, &mut pair.client_ports),
                    [0, 0, 4, 3, 0, 0, 0, 0, 1, 0, 0, 0, 8]
                );
            }
            if release_first {
                pair.client.release_body(body.release()).unwrap();
            } else {
                pair.client_ports.bodies.push_back(body);
            }
            let op = pair.client_ports.read.take().unwrap();
            pair.client
                .complete_read(op.complete(ReadOutcome::Failed(IoFailure::Failed)))
                .unwrap();
            if let Some(body) = pair.client_ports.bodies.pop_front() {
                pair.client.release_body(body.release()).unwrap();
            }
            assert!(settle(&mut pair.client, &mut pair.client_ports).is_empty());
            assert_eq!(
                pair.client_ports.retired,
                [StreamResult {
                    stream: id,
                    outcome: if reset {
                        StreamOutcome::Reset(8)
                    } else {
                        StreamOutcome::Complete
                    }
                }]
            );
            assert_eq!(
                pair.client_ports.ends,
                [ReceiveEnd {
                    stream: id,
                    outcome: StreamOutcome::Complete
                }]
            );
            assert_eq!(pair.client_ports.closed, [ConnectionResult::IoFailed]);
        }
    }
}

#[test]
fn original_final_write_receipt_decides_success_after_unrelated_read_failure() {
    for (written, accepted, full) in [(5, 0, false), (10, 1, false), (109, 100, true)] {
        for deliver_notices in [false, true] {
            for (read_outcome, closed) in [
                (
                    ReadOutcome::Failed(IoFailure::Failed),
                    ConnectionResult::IoFailed,
                ),
                (ReadOutcome::Eof, ConnectionResult::PeerClosed),
            ] {
                let mut pair = Pair::new(Config::default());
                let id = pair.client.request(&request(b"POST"), false).unwrap();
                pair.pump(32_768);
                let buffer = vec![7; 100];
                let pointer = buffer.as_ptr();
                pair.client
                    .send(pair.client_ports.permits.pop_front().unwrap(), buffer, true)
                    .unwrap();
                pair.client.next(&mut pair.client_ports);
                let write = pair.client_ports.write.take().unwrap();
                pair.server.respond(id, &response(b"200"), true).unwrap();
                pair.pump(32_768);
                let bytes: Vec<_> = write
                    .slices()
                    .iter()
                    .flat_map(|s| s.iter().copied())
                    .collect();
                let mut expected = vec![0, 0, 100, 0, 1, 0, 0, 0, 1];
                expected.extend([7; 100]);
                assert_eq!(bytes, expected);
                let read = pair.client_ports.read.take().unwrap();
                pair.client
                    .complete_read(read.complete(read_outcome))
                    .unwrap();
                if deliver_notices {
                    pair.client.next(&mut pair.client_ports);
                }
                assert!(pair.client_ports.retired.is_empty());
                pair.client
                    .complete_write(write.complete(WriteOutcome::Written(written)))
                    .unwrap();
                assert!(settle(&mut pair.client, &mut pair.client_ports).is_empty());
                assert_eq!(pair.client_ports.sent.len(), 1);
                let receipt = &pair.client_ports.sent[0];
                assert_eq!(receipt.buffer.as_ptr(), pointer);
                assert_eq!(receipt.accepted, accepted);
                assert_eq!(receipt.result.is_ok(), full);
                assert_eq!(
                    pair.client_ports.retired,
                    [StreamResult {
                        stream: id,
                        outcome: if full {
                            StreamOutcome::Complete
                        } else {
                            StreamOutcome::ConnectionFailed
                        }
                    }]
                );
                assert_eq!(pair.client_ports.closed, [closed]);
            }
        }
    }
}

#[test]
fn goaway_never_makes_an_observed_response_retryable() {
    for complete in [false, true] {
        let mut pair = Pair::new(Config::default());
        let id = pair.client.request(&request(b"GET"), true).unwrap();
        pair.pump(32_768);
        pair.server.respond(id, &response(b"200"), false).unwrap();
        pair.pump(32_768);
        pair.server
            .send(
                pair.server_ports.permits.pop_front().unwrap(),
                vec![1],
                complete,
            )
            .unwrap();
        pair.pump(32_768);
        read(
            &mut pair.client,
            &mut pair.client_ports,
            &[0, 0, 8, 7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        );
        let body = pair.client_ports.bodies.pop_front().unwrap();
        pair.client.release_body(body.release()).unwrap();
        settle(&mut pair.client, &mut pair.client_ports);
        assert_eq!(
            pair.client_ports.retired,
            [StreamResult {
                stream: id,
                outcome: if complete {
                    StreamOutcome::Complete
                } else {
                    StreamOutcome::ConnectionFailed
                }
            }]
        );
    }
}

#[test]
fn queued_final_headers_do_not_report_success_when_the_transport_fails() {
    let mut pair = Pair::new(Config::default());
    let first = pair.client.request(&request(b"HEAD"), true).unwrap();
    let second = pair.client.request(&request(b"HEAD"), true).unwrap();
    pair.pump(32_768);
    pair.server.respond(first, &response(b"200"), true).unwrap();
    pair.server.next(&mut pair.server_ports);
    let original = pair.server_ports.write.take().unwrap();
    pair.server
        .respond(second, &response(b"200"), true)
        .unwrap();
    pair.server.next(&mut pair.server_ports);
    let read = pair.server_ports.read.take().unwrap();
    pair.server
        .complete_read(read.complete(ReadOutcome::Failed(IoFailure::Failed)))
        .unwrap();
    let count = original.remaining();
    pair.server
        .complete_write(original.complete(WriteOutcome::Written(count)))
        .unwrap();
    assert!(settle(&mut pair.server, &mut pair.server_ports).is_empty());
    assert_eq!(
        pair.server_ports.retired,
        [
            StreamResult {
                stream: second,
                outcome: StreamOutcome::ConnectionFailed
            },
            StreamResult {
                stream: first,
                outcome: StreamOutcome::Complete
            },
        ]
    );
}

#[test]
fn shutdown_deadline_cancels_blocked_original_output_but_waits_for_its_receipt() {
    let mut pair = Pair::new(Config::default());
    pair.pump(32_768);
    pair.server.shutdown().unwrap();
    pair.server.next(&mut pair.server_ports);
    let original = pair.server_ports.write.take().unwrap();
    pair.server.advance_time(Duration::from_secs(30)).unwrap();
    pair.server.next(&mut pair.server_ports);
    assert!(
        pair.server_ports
            .cancels
            .iter()
            .any(|op| op.original() == original.token())
    );
    assert!(settle(&mut pair.server, &mut pair.server_ports).is_empty());
    assert!(pair.server_ports.closed.is_empty());
    pair.server
        .complete_write(original.complete(WriteOutcome::Failed {
            error: IoFailure::Cancelled,
            progress: Progress::Exact(0),
        }))
        .unwrap();
    assert!(settle(&mut pair.server, &mut pair.server_ports).is_empty());
    assert_eq!(pair.server_ports.closed, [ConnectionResult::Graceful]);
}
