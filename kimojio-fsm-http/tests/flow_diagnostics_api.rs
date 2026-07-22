use kimojio_fsm_http::{H2FairStreamScheduler, H2FlowDiagnostics, H2FlowStall};

#[test]
fn downstream_can_record_and_inspect_non_exhaustive_flow_diagnostics() {
    let mut flow = H2FlowDiagnostics::default();
    flow.record_stall(H2FlowStall::ConnectionWindow);
    flow.record_window_update(1, 1024);

    let snapshot = flow.snapshot(0, H2FairStreamScheduler::default().diagnostics());

    assert_eq!(snapshot.connection_window_stalls, 1);
    assert_eq!(snapshot.stream_window_updates, 1);
    assert_eq!(snapshot.stream_window_update_bytes, 1024);
}
