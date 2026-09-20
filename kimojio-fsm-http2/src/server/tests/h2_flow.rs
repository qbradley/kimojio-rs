//! HTTP/2 window, scheduler, and backpressure tests.

use std::time::{Duration, Instant};

use crate::server::*;

#[test]
fn flow_control_window_tracks_signed_credit_and_debt() {
    let mut window = H2FlowControlWindow::new(100).unwrap();

    window.consume(40).unwrap();
    assert_eq!(window.available(), 60);
    window.adjust(-120).unwrap();
    assert_eq!(window.available(), -60);
    window.increase(30).unwrap();
    assert_eq!(window.available(), -30);
    assert_eq!(window.consume(1), Err(ServerError::FlowControlViolation));
}

#[test]
fn flow_receive_window_batches_refunds_and_extra_credit() {
    let mut window = H2ReceiveWindow::new(8);

    window.receive_data(4).unwrap();
    assert_eq!(window.consume_data(1).unwrap(), None);
    assert_eq!(window.consume_data(1).unwrap(), Some(2));
    assert_eq!(window.available(), 6);

    window.receive_data(6).unwrap();
    assert_eq!(window.grant_extra_credit_for_frame(10, 6).unwrap(), Some(4));
    assert_eq!(window.available(), 4);
    assert_eq!(window.consume_data(10).unwrap(), Some(6));
}

#[test]
fn flow_receive_window_rollback_does_not_create_future_credit_debt() {
    let mut window = H2ReceiveWindow::new(10);

    for _ in 0..4 {
        window.receive_data(5).unwrap();
        assert_eq!(window.rollback_received_data(5).unwrap(), Some(5));
        assert_eq!(window.available(), 10);
        assert_eq!(window.pending_update(), 0);
    }

    window.receive_data(10).unwrap();
    assert_eq!(window.consume_data(10).unwrap(), Some(10));
    assert_eq!(window.available(), 10);
}

#[test]
fn flow_receive_window_rollback_removes_adaptive_sample_and_blockage() {
    let start = Instant::now();
    let mut sample_window =
        H2ReceiveWindow::with_adaptive_growth(100, 400, Duration::from_millis(100), start).unwrap();

    sample_window
        .receive_data_at(100, start + Duration::from_millis(10))
        .unwrap();
    sample_window
        .consume_data_at(100, start + Duration::from_millis(10))
        .unwrap();
    sample_window
        .receive_data_at(50, start + Duration::from_millis(20))
        .unwrap();
    sample_window.rollback_received_data(50).unwrap();
    sample_window
        .receive_data_at(100, start + Duration::from_millis(90))
        .unwrap();
    assert_eq!(
        sample_window
            .consume_data_at(100, start + Duration::from_millis(90))
            .unwrap(),
        Some(150)
    );

    let diagnostics = sample_window.diagnostics();
    assert_eq!(diagnostics.current_window_bytes, 150);
    assert_eq!(diagnostics.estimated_bdp_bytes, 125);
    assert_eq!(diagnostics.growth_events, 1);
    assert_eq!(diagnostics.growth_bytes, 50);
    assert_eq!(diagnostics.blocked_time, Duration::from_millis(10));

    let mut blocked_window =
        H2ReceiveWindow::with_adaptive_growth(100, 400, Duration::from_millis(100), start).unwrap();
    blocked_window
        .receive_data_at(100, start + Duration::from_millis(10))
        .unwrap();
    blocked_window
        .consume_data_at(100, start + Duration::from_millis(10))
        .unwrap();
    blocked_window
        .receive_data_at(100, start + Duration::from_millis(20))
        .unwrap();
    blocked_window.rollback_received_data(100).unwrap();
    blocked_window
        .receive_data_at(1, start + Duration::from_millis(90))
        .unwrap();

    let diagnostics = blocked_window.diagnostics();
    assert_eq!(diagnostics.blocked_time, Duration::from_millis(10));
    assert_eq!(diagnostics.last_blocked_time, Duration::from_millis(10));
}

#[test]
fn flow_receive_window_wholly_rolled_back_first_sample_reanchors_epoch() {
    let start = Instant::now();
    let mut window =
        H2ReceiveWindow::with_adaptive_growth(100, 400, Duration::from_millis(100), start).unwrap();

    window
        .receive_data_at(100, start + Duration::from_millis(10))
        .unwrap();
    window.rollback_received_data(100).unwrap();
    window
        .receive_data_at(100, start + Duration::from_millis(90))
        .unwrap();
    window
        .consume_data_at(100, start + Duration::from_millis(150))
        .unwrap();

    let diagnostics = window.diagnostics();
    assert_eq!(diagnostics.estimated_bdp_bytes, 166);
    assert_eq!(diagnostics.received_bytes, 200);
    assert_eq!(diagnostics.consumed_bytes, 100);
}

#[test]
fn flow_receive_window_zero_amount_preserves_pending_first_sample_reanchor() {
    let start = Instant::now();
    let mut window =
        H2ReceiveWindow::with_adaptive_growth(100, 400, Duration::from_millis(100), start).unwrap();

    window
        .receive_data_at(100, start + Duration::from_millis(10))
        .unwrap();
    window.rollback_received_data(100).unwrap();
    window
        .receive_data_at(0, start + Duration::from_millis(50))
        .unwrap();
    window
        .receive_data_at(100, start + Duration::from_millis(90))
        .unwrap();
    window
        .consume_data_at(100, start + Duration::from_millis(150))
        .unwrap();

    let diagnostics = window.diagnostics();
    assert_eq!(diagnostics.estimated_bdp_bytes, 166);
    assert_eq!(diagnostics.received_bytes, 200);
    assert_eq!(diagnostics.consumed_bytes, 100);
    assert_eq!(diagnostics.growth_events, 0);
    assert_eq!(diagnostics.growth_bytes, 0);
}

#[test]
fn flow_receive_window_adapts_after_sustained_fast_turnovers() {
    let start = Instant::now();
    let mut window =
        H2ReceiveWindow::with_adaptive_growth(100, 400, Duration::from_millis(100), start).unwrap();

    window
        .receive_data_at(100, start + Duration::from_millis(10))
        .unwrap();
    assert_eq!(
        window
            .consume_data_at(100, start + Duration::from_millis(10))
            .unwrap(),
        Some(100)
    );
    window
        .receive_data_at(100, start + Duration::from_millis(20))
        .unwrap();
    assert_eq!(
        window
            .consume_data_at(100, start + Duration::from_millis(20))
            .unwrap(),
        Some(200)
    );

    assert_eq!(window.limit(), 200);
    assert_eq!(window.available(), 200);
    assert_eq!(
        window.diagnostics(),
        H2ReceiveWindowDiagnostics {
            current_window_bytes: 200,
            max_window_bytes: 400,
            received_bytes: 200,
            consumed_bytes: 200,
            blocked_time: Duration::from_millis(10),
            last_blocked_time: Duration::from_millis(10),
            rtt_proxy: Duration::from_millis(100),
            estimated_bdp_bytes: 1_000,
            growth_events: 1,
            growth_bytes: 100,
        }
    );
}

#[test]
fn flow_receive_window_starts_first_sample_with_first_data() {
    let start = Instant::now();
    let mut window =
        H2ReceiveWindow::with_adaptive_growth(100, 400, Duration::from_millis(100), start).unwrap();

    window
        .receive_data_at(100, start + Duration::from_millis(50))
        .unwrap();
    window
        .consume_data_at(100, start + Duration::from_millis(150))
        .unwrap();
    window
        .receive_data_at(100, start + Duration::from_millis(160))
        .unwrap();
    window
        .consume_data_at(100, start + Duration::from_millis(250))
        .unwrap();

    assert_eq!(window.limit(), 150);
    assert_eq!(window.diagnostics().growth_events, 1);
}

#[test]
fn flow_receive_window_counts_only_consumed_turnovers() {
    let start = Instant::now();
    let mut window =
        H2ReceiveWindow::with_adaptive_growth(100, 400, Duration::from_millis(100), start).unwrap();
    window.grant_extra_credit(100).unwrap();

    window
        .receive_data_at(200, start + Duration::from_millis(10))
        .unwrap();
    assert_eq!(
        window
            .consume_data_at(100, start + Duration::from_millis(10))
            .unwrap(),
        None
    );

    assert_eq!(window.limit(), 100);
    assert_eq!(window.diagnostics().growth_events, 0);
}

#[test]
fn flow_receive_window_does_not_grow_slow_or_above_cap() {
    let start = Instant::now();
    let mut slow =
        H2ReceiveWindow::with_adaptive_growth(100, 400, Duration::from_millis(100), start).unwrap();
    for elapsed_ms in [10, 160, 310] {
        let now = start + Duration::from_millis(elapsed_ms);
        slow.receive_data_at(100, now).unwrap();
        slow.consume_data_at(100, now).unwrap();
    }
    assert_eq!(slow.limit(), 100);
    assert_eq!(slow.diagnostics().growth_events, 0);

    let mut capped =
        H2ReceiveWindow::with_adaptive_growth(100, 150, Duration::from_millis(100), start).unwrap();
    for elapsed_ms in [10, 20, 30, 40] {
        let now = start + Duration::from_millis(elapsed_ms);
        let amount = capped.limit();
        capped.receive_data_at(amount, now).unwrap();
        capped.consume_data_at(amount, now).unwrap();
    }
    assert_eq!(capped.limit(), 150);
    assert_eq!(capped.diagnostics().growth_bytes, 50);
}

#[test]
fn flow_receive_window_rejects_invalid_adaptive_policy() {
    let now = Instant::now();
    assert_eq!(
        H2ReceiveWindow::with_adaptive_growth(100, 99, Duration::from_millis(1), now),
        Err(ServerError::InvalidFrame)
    );
    assert_eq!(
        H2ReceiveWindow::with_adaptive_growth(100, 200, Duration::ZERO, now),
        Err(ServerError::InvalidFrame)
    );
}

#[test]
fn flow_fair_scheduler_rotates_ready_streams_and_skips_blocked() {
    let mut scheduler = H2FairStreamScheduler::default();
    scheduler.register(1);
    scheduler.register(3);
    scheduler.register(5);

    assert_eq!(scheduler.next_ready(|stream_id| stream_id != 1), Some(3));
    assert_eq!(scheduler.next_ready(|stream_id| stream_id != 1), Some(5));
    scheduler.mark_drained(5);
    assert_eq!(scheduler.next_ready(|stream_id| stream_id != 1), Some(3));
    scheduler.remove(3);
    assert_eq!(scheduler.next_ready(|stream_id| stream_id != 1), None);

    let diagnostics = scheduler.diagnostics();
    assert_eq!(diagnostics.observed_streams, 3);
    assert_eq!(diagnostics.tracked_streams, 2);
    assert_eq!(diagnostics.queued_streams, 1);
    assert_eq!(diagnostics.selection_attempts, 4);
    assert_eq!(diagnostics.selections, 3);
    assert_eq!(diagnostics.no_ready_attempts, 1);
    assert_eq!(diagnostics.skipped_streams, 3);
    assert_eq!(diagnostics.max_scan_depth, 2);
    assert_eq!(diagnostics.min_stream_selections, 0);
    assert_eq!(diagnostics.max_stream_selections, 2);
    assert_eq!(
        diagnostics.stream_selection_buckets,
        [1, 1, 1, 0, 0, 0, 0, 0]
    );
    assert_eq!(
        scheduler.pending_capacity_streams(|stream_id| stream_id == 1),
        1
    );
}

#[test]
fn flow_fair_scheduler_bounds_extreme_initial_capacity() {
    let mut scheduler = H2FairStreamScheduler::with_capacity(usize::MAX);

    scheduler.register(1);

    assert_eq!(scheduler.next_ready(|_| true), Some(1));
}

#[test]
fn flow_fair_scheduler_uses_fx_hash() {
    fn assert_fx_hasher(_: &rustc_hash::FxBuildHasher) {}

    let scheduler = H2FairStreamScheduler::default();

    assert_fx_hasher(scheduler.stream_slots.hasher());
}

#[test]
fn flow_fair_scheduler_requeue_does_not_duplicate_stale_turns() {
    let mut scheduler = H2FairStreamScheduler::default();
    scheduler.register(1);
    scheduler.register(3);

    assert_eq!(scheduler.next_ready(|_| true), Some(1));
    scheduler.mark_drained(1);
    scheduler.register(1);

    assert_eq!(scheduler.next_ready(|_| true), Some(3));
    assert_eq!(scheduler.next_ready(|_| true), Some(1));
    assert_eq!(scheduler.next_ready(|_| true), Some(3));
    assert_eq!(scheduler.diagnostics().stream_selection_buckets[2], 2);
}

#[test]
fn flow_fair_scheduler_churn_storage_remains_bound_to_live_state() {
    const CYCLES: u32 = 1_000_000;

    let mut scheduler = H2FairStreamScheduler::default();
    for index in 0..CYCLES {
        let stream_id = index.saturating_mul(2).saturating_add(1);
        scheduler.register(stream_id);
        assert_eq!(scheduler.stream_slots.len(), 1);
        assert_eq!(scheduler.slots.len(), 1);
        assert_eq!(scheduler.queued_streams, 1);
        assert_eq!(
            scheduler.slot_stream_id(scheduler.ready_head),
            Some(stream_id)
        );
        assert_eq!(
            scheduler.slot_stream_id(scheduler.ready_tail),
            Some(stream_id)
        );

        scheduler.record_immediate_dispatch();
        scheduler.mark_drained(stream_id);
        assert_eq!(scheduler.stream_slots.len(), 1);
        assert_eq!(scheduler.slots.len(), 1);
        assert_eq!(scheduler.queued_streams, 0);
        assert_eq!(scheduler.ready_head, None);
        assert_eq!(scheduler.ready_tail, None);

        scheduler.remove(stream_id);
        assert!(scheduler.stream_slots.is_empty());
        assert_eq!(scheduler.slots.len(), 1);
        assert_eq!(scheduler.queued_streams, 0);
        assert_eq!(scheduler.ready_head, None);
        assert_eq!(scheduler.ready_tail, None);
    }

    let useful_stream_id = CYCLES.saturating_mul(2).saturating_add(1);
    scheduler.register(useful_stream_id);
    assert_eq!(scheduler.next_ready(|_| true), Some(useful_stream_id));

    let diagnostics = scheduler.diagnostics();
    assert_eq!(diagnostics.observed_streams, CYCLES as usize + 1);
    assert_eq!(diagnostics.tracked_streams, 1);
    assert_eq!(diagnostics.queued_streams, 1);
    assert_eq!(diagnostics.selection_attempts, 1);
    assert_eq!(diagnostics.selections, 1);
    assert_eq!(diagnostics.immediate_dispatches, CYCLES as u64);
    assert_eq!(diagnostics.skipped_streams, 0);
    assert_eq!(diagnostics.max_scan_depth, 1);
    assert_eq!(
        diagnostics.stream_selection_buckets,
        [CYCLES as usize, 1, 0, 0, 0, 0, 0, 0]
    );
}

#[test]
fn flow_fair_scheduler_slot_storage_tracks_peak_live_streams() {
    const DEFAULT_STREAM_CAP: usize = 100;

    let mut scheduler = H2FairStreamScheduler::default();
    for index in 0..DEFAULT_STREAM_CAP as u32 {
        scheduler.register(index.saturating_mul(2).saturating_add(1));
    }
    assert_eq!(scheduler.stream_slots.len(), DEFAULT_STREAM_CAP);
    assert_eq!(scheduler.slots.len(), DEFAULT_STREAM_CAP);
    assert_eq!(scheduler.queued_streams, DEFAULT_STREAM_CAP);
    let stream_slot_capacity = scheduler.stream_slots.capacity();
    let slot_capacity = scheduler.slots.capacity();

    for index in 0..DEFAULT_STREAM_CAP as u32 {
        scheduler.remove(index.saturating_mul(2).saturating_add(1));
    }
    assert!(scheduler.stream_slots.is_empty());
    assert_eq!(scheduler.slots.len(), DEFAULT_STREAM_CAP);
    assert_eq!(scheduler.queued_streams, 0);

    for index in 0..100_000_u32 {
        let stream_id = 1_001_u32.saturating_add(index.saturating_mul(2));
        scheduler.register(stream_id);
        scheduler.remove(stream_id);
        assert_eq!(scheduler.slots.len(), DEFAULT_STREAM_CAP);
    }
    assert_eq!(scheduler.stream_slots.capacity(), stream_slot_capacity);
    assert_eq!(scheduler.slots.capacity(), slot_capacity);
}

#[test]
fn flow_fair_scheduler_histogram_uses_every_inclusive_boundary() {
    let cases = [
        (1, 0_u64),
        (3, 1),
        (5, 3),
        (7, 7),
        (9, 15),
        (11, 31),
        (13, 63),
        (15, 64),
    ];
    let mut scheduler = H2FairStreamScheduler::default();

    for (stream_id, selections) in cases {
        scheduler.register(stream_id);
        for _ in 0..selections {
            assert_eq!(scheduler.next_ready(|id| id == stream_id), Some(stream_id));
        }
        scheduler.remove(stream_id);
    }

    let diagnostics = scheduler.diagnostics();
    assert_eq!(
        diagnostics.stream_selection_buckets,
        [1, 1, 1, 1, 1, 1, 1, 1]
    );
    assert_eq!(diagnostics.min_stream_selections, 0);
    assert_eq!(diagnostics.max_stream_selections, 64);
}

#[test]
fn flow_fair_scheduler_equality_ignores_diagnostic_history() {
    let mut left = H2FairStreamScheduler::default();
    let mut right = H2FairStreamScheduler::default();
    left.register(1);
    right.register(1);

    assert_eq!(left.next_ready(|_| true), Some(1));
    assert_eq!(right.next_ready(|_| false), None);
    left.record_immediate_dispatch();
    assert_eq!(left, right);

    right.mark_drained(1);
    assert_ne!(left, right);
}

#[test]
fn flow_fair_scheduler_retains_immediate_selection_distribution() {
    let mut scheduler = H2FairStreamScheduler::default();
    scheduler.register(1);
    scheduler.record_immediate_dispatch();
    scheduler.mark_drained(1);

    let active = scheduler.diagnostics();
    assert_eq!(active.observed_streams, 1);
    assert_eq!(active.queued_streams, 0);
    assert_eq!(active.selections, 0);
    assert_eq!(active.immediate_dispatches, 1);
    assert_eq!(active.stream_selection_buckets, [1, 0, 0, 0, 0, 0, 0, 0]);

    scheduler.remove(1);
    let retired = scheduler.diagnostics();
    assert_eq!(retired.observed_streams, 1);
    assert_eq!(retired.tracked_streams, 0);
    assert_eq!(retired.min_stream_selections, 0);
    assert_eq!(retired.max_stream_selections, 0);
    assert_eq!(retired.stream_selection_buckets, [1, 0, 0, 0, 0, 0, 0, 0]);
}

#[test]
fn flow_diagnostics_snapshot_classifies_updates_and_saturates() {
    let mut flow = H2FlowDiagnostics::default();
    flow.record_stream_window_stall();
    flow.record_connection_window_stall();
    flow.record_window_update(1, 7);
    flow.record_window_update(0, 11);

    let snapshot = flow.snapshot(2, H2FairStreamSchedulerDiagnostics::default());
    assert_eq!(snapshot.stream_window_stalls, 1);
    assert_eq!(snapshot.connection_window_stalls, 1);
    assert_eq!(snapshot.stream_window_updates, 1);
    assert_eq!(snapshot.connection_window_updates, 1);
    assert_eq!(snapshot.stream_window_update_bytes, 7);
    assert_eq!(snapshot.connection_window_update_bytes, 11);
    assert_eq!(snapshot.pending_capacity_streams, 2);

    flow.stream_window_stalls = u64::MAX;
    flow.connection_window_stalls = u64::MAX;
    flow.stream_window_updates = u64::MAX;
    flow.connection_window_updates = u64::MAX;
    flow.stream_window_update_bytes = u64::MAX;
    flow.connection_window_update_bytes = u64::MAX;
    flow.record_stream_window_stall();
    flow.record_connection_window_stall();
    flow.record_window_update(1, 1);
    flow.record_window_update(0, 1);

    assert_eq!(
        flow,
        H2FlowDiagnostics {
            stream_window_stalls: u64::MAX,
            connection_window_stalls: u64::MAX,
            stream_window_updates: u64::MAX,
            connection_window_updates: u64::MAX,
            stream_window_update_bytes: u64::MAX,
            connection_window_update_bytes: u64::MAX,
        }
    );
}

#[test]
fn flow_backpressure_state_reports_queue_limits() {
    let limits = H2Limits {
        max_queued_data_bytes: 4,
        max_queued_control_frames: 1,
        ..H2Limits::default()
    };

    let state = H2BackpressureState::new(2, 5, 2, limits);

    assert_eq!(state.pending_streams, 2);
    assert!(state.data_over_limit);
    assert!(state.control_over_limit);
    assert!(!state.should_read_more());
}
