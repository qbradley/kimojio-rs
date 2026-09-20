#[path = "../examples/support/mod.rs"]
mod support;

use kimojio_fsm_http2::*;
use std::time::Duration;
use support::*;

#[test]
fn rejected_wake_recovers_original_alarm_and_early_completion_rearms_it() {
    let mut pair = Pair::new(Config::default());
    let id = pair.client.request(&request(b"GET"), true).unwrap();
    pair.pump(32_768);
    pair.client
        .set_deadline(id, Some(Duration::from_secs(5)))
        .unwrap();
    pair.client.next(&mut pair.client_ports);
    let alarm = pair.client_ports.alarms.pop().unwrap();
    let token = alarm.token().clone();
    let rejected = pair
        .server
        .complete_wake(alarm.complete(Duration::from_secs(4)))
        .unwrap_err();
    assert_eq!(rejected.error, CommandError::InvalidCompletion);
    let (alarm, time) = rejected.value.into_parts();
    assert_eq!(alarm.token(), &token);
    assert_eq!(alarm.deadline(), Duration::from_secs(5));
    assert_eq!(time, Duration::from_secs(4));
    pair.client.complete_wake(alarm.complete(time)).unwrap();
    pair.client.next(&mut pair.client_ports);
    let replacement = pair.client_ports.alarms.pop().unwrap();
    assert_ne!(replacement.token(), &token);
    assert_eq!(replacement.deadline(), Duration::from_secs(5));
    assert!(pair.client_ports.ends.is_empty());
    pair.client
        .complete_wake(replacement.complete(Duration::from_secs(5)))
        .unwrap();
    pair.client.next(&mut pair.client_ports);
    assert_eq!(
        pair.client_ports.ends,
        [ReceiveEnd {
            stream: id,
            outcome: StreamOutcome::Deadline
        }]
    );
}

#[test]
fn rejected_release_recovers_the_original_body_page() {
    let mut pair = Pair::new(Config::default());
    let id = pair.client.request(&request(b"GET"), true).unwrap();
    pair.pump(32_768);
    pair.server.respond(id, &response(b"200"), false).unwrap();
    pair.pump(32_768);
    pair.server
        .send(
            pair.server_ports.permits.pop_front().unwrap(),
            vec![1, 2, 3],
            true,
        )
        .unwrap();
    pair.pump(32_768);
    let body = pair.client_ports.bodies.pop_front().unwrap();
    let pointer = body.bytes().as_ptr();
    let rejected = pair.server.release_body(body.release()).unwrap_err();
    assert_eq!(rejected.error, CommandError::InvalidCompletion);
    let body = rejected.value.into_op();
    assert_eq!(body.stream(), id);
    assert_eq!(body.bytes(), [1, 2, 3]);
    assert_eq!(body.bytes().as_ptr(), pointer);
    pair.client.release_body(body.release()).unwrap();
    pair.pump(32_768);
    assert_eq!(
        pair.client_ports.retired,
        [StreamResult {
            stream: id,
            outcome: StreamOutcome::Complete
        }]
    );
}

#[test]
fn rejected_cancel_and_close_preserve_the_original_settlement_operations() {
    let mut pair = Pair::new(Config::default());
    pair.pump(32_768);
    pair.client.shutdown().unwrap();
    pair.client.next(&mut pair.client_ports);
    let write = pair.client_ports.write.take().unwrap();
    let count = write.remaining();
    pair.client
        .complete_write(write.complete(WriteOutcome::Written(count)))
        .unwrap();
    pair.client.next(&mut pair.client_ports);
    let cancel = pair.client_ports.cancels.pop().unwrap();
    let original = cancel.original().clone();
    let rejected = pair.server.complete_cancel(cancel.complete()).unwrap_err();
    assert_eq!(rejected.error, CommandError::InvalidCompletion);
    let cancel = rejected.value.into_op();
    assert_eq!(cancel.original(), &original);
    pair.client.complete_cancel(cancel.complete()).unwrap();
    let read = pair.client_ports.read.take().unwrap();
    assert_eq!(read.token(), &original);
    pair.client
        .complete_read(read.complete(ReadOutcome::Failed(IoFailure::Cancelled)))
        .unwrap();
    pair.client.next(&mut pair.client_ports);
    let close = pair.client_ports.close.take().unwrap();
    let rejected = pair
        .server
        .complete_close(close.complete(Ok(())))
        .unwrap_err();
    assert_eq!(rejected.error, CommandError::InvalidCompletion);
    let (close, result) = rejected.value.into_parts();
    assert_eq!(result, Ok(()));
    pair.client.complete_close(close.complete(result)).unwrap();
    pair.client.next(&mut pair.client_ports);
    assert_eq!(pair.client_ports.closed, [ConnectionResult::Graceful]);
}
