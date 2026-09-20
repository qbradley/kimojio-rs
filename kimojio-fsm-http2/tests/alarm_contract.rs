#[path = "../examples/support/mod.rs"]
mod support;

use kimojio_fsm_http2::*;
use std::time::Duration;
use support::*;

#[test]
fn superseded_alarm_cannot_advance_time_before_or_after_cancellation_dispatch() {
    for replacement in [None, Some(50), Some(200)] {
        for now in [0, 1_000_000] {
            for dispatch_first in [false, true] {
                let mut pair = Pair::new(Config::default());
                let id = pair.client.request(&request(b"GET"), true).unwrap();
                pair.pump(32_768);
                pair.client
                    .set_deadline(id, Some(Duration::from_secs(100)))
                    .unwrap();
                pair.client.next(&mut pair.client_ports);
                let original = pair.client_ports.alarms.pop().unwrap();
                pair.client
                    .set_deadline(id, replacement.map(Duration::from_secs))
                    .unwrap();
                if dispatch_first {
                    pair.client.next(&mut pair.client_ports);
                }
                pair.client
                    .complete_wake(original.complete(Duration::from_secs(now)))
                    .unwrap();
                pair.client.advance_time(Duration::from_secs(1)).unwrap();
                pair.client.next(&mut pair.client_ports);
                assert!(pair.client_ports.ends.is_empty());
                assert!(pair.client_ports.write.is_none());
                assert!(pair.client_ports.retired.is_empty());
                assert_eq!(pair.client_ports.cancels.len(), usize::from(dispatch_first));
                let deadlines: Vec<_> = pair
                    .client_ports
                    .alarms
                    .iter()
                    .map(WakeOp::deadline)
                    .collect();
                assert_eq!(
                    deadlines,
                    replacement
                        .map(Duration::from_secs)
                        .into_iter()
                        .collect::<Vec<_>>()
                );
                for cancel in std::mem::take(&mut pair.client_ports.cancels) {
                    pair.client.complete_cancel(cancel.complete()).unwrap();
                }
                if let Some(seconds) = replacement {
                    let alarm = pair.client_ports.alarms.pop().unwrap();
                    pair.client
                        .complete_wake(alarm.complete(Duration::from_secs(seconds)))
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
            }
        }
    }
}

#[test]
fn shared_deadline_keeps_the_original_alarm_live_after_one_stream_moves() {
    let mut pair = Pair::new(Config::default());
    let first = pair.client.request(&request(b"GET"), true).unwrap();
    let second = pair.client.request(&request(b"GET"), true).unwrap();
    pair.pump(32_768);
    pair.client
        .set_deadline(first, Some(Duration::from_secs(100)))
        .unwrap();
    pair.client
        .set_deadline(second, Some(Duration::from_secs(100)))
        .unwrap();
    pair.client.next(&mut pair.client_ports);
    let original = pair.client_ports.alarms.pop().unwrap();
    pair.client
        .set_deadline(first, Some(Duration::from_secs(200)))
        .unwrap();
    pair.client
        .complete_wake(original.complete(Duration::from_secs(100)))
        .unwrap();
    pair.client.next(&mut pair.client_ports);
    assert_eq!(
        pair.client_ports.ends,
        [ReceiveEnd {
            stream: second,
            outcome: StreamOutcome::Deadline
        }]
    );
    assert!(pair.client_ports.cancels.is_empty());
    assert_eq!(pair.client_ports.alarms.len(), 1);
    assert_eq!(
        pair.client_ports.alarms[0].deadline(),
        Duration::from_secs(200)
    );
    assert_eq!(
        pair.client.advance_time(Duration::from_secs(99)),
        Err(CommandError::TimeReversed)
    );
    assert!(pair.client_ports.closed.is_empty());
}
