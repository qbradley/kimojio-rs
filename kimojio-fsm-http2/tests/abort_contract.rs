#[path = "../examples/support/mod.rs"]
mod support;

use kimojio_fsm_http2::*;
use std::{collections::VecDeque, time::Duration};
use support::*;

fn settle(connection: &mut Connection, ports: &mut MemoryPorts) {
    let mut output = VecDeque::new();
    for _ in 0..128 {
        if !step(connection, ports, &mut VecDeque::new(), &mut output, 32_768) {
            assert!(output.is_empty(), "hard abort must not flush unsent output");
            return;
        }
    }
    panic!("hard-abort settlement stalled");
}

#[test]
fn abort_after_partial_receipt_returns_the_engine_owned_remainder() {
    let mut pair = Pair::new(Config::default());
    pair.client.request(&request(b"POST"), false).unwrap();
    pair.pump(32_768);
    let buffer = vec![1; 100];
    let pointer = buffer.as_ptr();
    pair.client
        .send(pair.client_ports.permits.pop_front().unwrap(), buffer, true)
        .unwrap();
    pair.client.next(&mut pair.client_ports);
    let write = pair.client_ports.write.take().unwrap();
    pair.client
        .complete_write(write.complete(WriteOutcome::Written(10)))
        .unwrap();
    pair.client.abort();
    settle(&mut pair.client, &mut pair.client_ports);
    assert_eq!(pair.client_ports.sent.len(), 1);
    let sent = &pair.client_ports.sent[0];
    assert_eq!(sent.buffer.as_ptr(), pointer);
    assert_eq!(sent.buffer.len(), 100);
    assert_eq!(sent.accepted, 1);
    assert!(sent.exact);
    assert_eq!(sent.result, Err(SendStop::ConnectionFailed));
    assert_eq!(pair.client_ports.closed, [ConnectionResult::Aborted]);
}

#[test]
fn active_alarm_failure_is_recoverable_and_closes_without_advancing_time() {
    for error in [IoFailure::Failed, IoFailure::Cancelled] {
        let mut pair = Pair::new(Config::default());
        let id = pair.client.request(&request(b"GET"), true).unwrap();
        pair.pump(32_768);
        pair.client
            .set_deadline(id, Some(Duration::from_secs(5)))
            .unwrap();
        pair.client.next(&mut pair.client_ports);
        let alarm = pair.client_ports.alarms.pop().unwrap();
        let token = alarm.token().clone();
        let completion = alarm.failed(error);
        assert_eq!(completion.token(), &token);
        assert_eq!(completion.outcome(), WakeOutcome::Failed(error));
        let rejected = pair.server.complete_wake(completion).unwrap_err();
        assert_eq!(rejected.error, CommandError::InvalidCompletion);
        let (alarm, outcome) = rejected.value.into_parts();
        assert_eq!(alarm.token(), &token);
        assert_eq!(outcome, WakeOutcome::Failed(error));
        pair.client.complete_wake(alarm.failed(error)).unwrap();
        pair.client.advance_time(Duration::ZERO).unwrap();
        assert!(
            pair.client_ports.read.is_some(),
            "the original read still belongs to its executor"
        );
        settle(&mut pair.client, &mut pair.client_ports);
        assert_eq!(pair.client_ports.closed, [ConnectionResult::IoFailed]);
        assert_eq!(
            pair.client_ports.retired,
            [StreamResult {
                stream: id,
                outcome: StreamOutcome::ConnectionFailed
            }]
        );
    }
}

#[test]
fn late_successful_read_and_alarm_failure_cannot_replace_abort_or_deliver_headers() {
    let mut pair = Pair::new(Config::default());
    let id = pair.client.request(&request(b"GET"), true).unwrap();
    pair.pump(32_768);
    pair.client
        .set_deadline(id, Some(Duration::from_secs(5)))
        .unwrap();
    pair.client.next(&mut pair.client_ports);
    let mut read = pair.client_ports.read.take().unwrap();
    let alarm = pair.client_ports.alarms.pop().unwrap();
    pair.client.abort();
    pair.client.next(&mut pair.client_ports);
    assert_eq!(pair.client_ports.cancels.len(), 2);
    let response = [0, 0, 1, 1, 5, 0, 0, 0, 1, 0x88];
    read.buffer_mut()[..response.len()].copy_from_slice(&response);
    pair.client
        .complete_read(read.complete(ReadOutcome::Read(response.len())))
        .unwrap();
    pair.client
        .complete_wake(alarm.failed(IoFailure::Failed))
        .unwrap();
    pair.client.advance_time(Duration::ZERO).unwrap();
    settle(&mut pair.client, &mut pair.client_ports);
    assert!(pair.client_ports.heads.is_empty());
    assert_eq!(pair.client_ports.closed, [ConnectionResult::Aborted]);
    assert_eq!(
        pair.client_ports.ends,
        [ReceiveEnd {
            stream: id,
            outcome: StreamOutcome::ConnectionFailed
        }]
    );
}

#[test]
fn obsolete_expected_cancellation_is_not_an_alarm_failure_default() {
    for error in [IoFailure::Cancelled, IoFailure::Failed] {
        let mut pair = Pair::new(Config::default());
        let id = pair.client.request(&request(b"GET"), true).unwrap();
        pair.pump(32_768);
        pair.client
            .set_deadline(id, Some(Duration::from_secs(5)))
            .unwrap();
        pair.client.next(&mut pair.client_ports);
        let old = pair.client_ports.alarms.pop().unwrap();
        pair.client
            .set_deadline(id, Some(Duration::from_secs(10)))
            .unwrap();
        pair.client.next(&mut pair.client_ports);
        assert_eq!(pair.client_ports.cancels.len(), 1);
        pair.client.complete_wake(old.failed(error)).unwrap();
        pair.client.advance_time(Duration::ZERO).unwrap();
        if error == IoFailure::Cancelled {
            pair.client.next(&mut pair.client_ports);
            assert!(pair.client_ports.ends.is_empty());
            assert!(pair.client_ports.closed.is_empty());
            assert_eq!(pair.client_ports.alarms.len(), 1);
            assert_eq!(
                pair.client_ports.alarms[0].deadline(),
                Duration::from_secs(10)
            );
            let cancel = pair.client_ports.cancels.pop().unwrap();
            pair.client.complete_cancel(cancel.complete()).unwrap();
            pair.client.abort();
            settle(&mut pair.client, &mut pair.client_ports);
            assert_eq!(pair.client_ports.closed, [ConnectionResult::Aborted]);
        } else {
            settle(&mut pair.client, &mut pair.client_ports);
            assert_eq!(pair.client_ports.closed, [ConnectionResult::IoFailed]);
        }
    }
}

#[test]
fn hard_abort_preserves_the_first_non_graceful_cause_through_close_failure() {
    for cause in [
        ConnectionResult::Aborted,
        ConnectionResult::IoFailed,
        ConnectionResult::PeerClosed,
        ConnectionResult::ResourceExhausted,
        ConnectionResult::Protocol(H2ProtocolError::connection(
            H2ErrorCode::ProtocolError,
            "primary",
        )),
    ] {
        let mut client = Client::new(Config::default(), Duration::ZERO).unwrap();
        let mut ports = MemoryPorts::default();
        client.abort_with_cause(cause);
        client.abort();
        client.next(&mut ports);
        assert!(ports.read.is_none());
        assert!(ports.write.is_none());
        assert!(ports.alarms.is_empty());
        assert!(ports.cancels.is_empty());
        let close = ports.close.take().unwrap();
        client.abort_with_cause(ConnectionResult::IoFailed);
        client
            .complete_close(close.complete(Err(IoFailure::Failed)))
            .unwrap();
        client.next(&mut ports);
        assert_eq!(ports.closed, [cause]);
        let count = ports.sequence.len();
        client.abort_with_cause(ConnectionResult::ResourceExhausted);
        client.next(&mut ports);
        assert_eq!(ports.sequence.len(), count);
    }
}

#[test]
fn hard_abort_can_escalate_graceful_close_before_its_original_completion() {
    let mut pair = Pair::new(Config::default());
    pair.pump(32_768);
    pair.client.shutdown().unwrap();
    step(
        &mut pair.client,
        &mut pair.client_ports,
        &mut VecDeque::new(),
        &mut VecDeque::new(),
        32_768,
    );
    pair.client.next(&mut pair.client_ports);
    let close = pair.client_ports.close.take().unwrap();
    pair.client.abort();
    pair.client.complete_close(close.complete(Ok(()))).unwrap();
    pair.client.next(&mut pair.client_ports);
    assert_eq!(pair.client_ports.closed, [ConnectionResult::Aborted]);
}
