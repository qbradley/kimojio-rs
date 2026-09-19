#[path = "../benches/support/mod.rs"]
mod support;

use support::{IO_BYTES, Scenario, client, server};

#[test]
fn all_timed_workloads_validate_bytes_and_reuse_the_connection() {
    for scenario in [
        Scenario::fixed(128),
        Scenario::chunked(1024 * 1024, IO_BYTES),
    ] {
        let mut cc = client::<false, true>(&scenario);
        let mut cy = client::<true, true>(&scenario);
        let mut sc = server::<false, true>(&scenario);
        let mut sy = server::<true, true>(&scenario);
        for _ in 0..4 {
            let client_stats = cc.round_trip();
            let server_stats = sc.round_trip();
            assert_eq!(cy.round_trip(), client_stats);
            assert_eq!(sy.round_trip(), server_stats);
            assert_eq!(client_stats.sent, scenario.body_bytes());
            assert_eq!(server_stats.received, scenario.body_bytes());
            if scenario.body_bytes() > IO_BYTES {
                assert_eq!(client_stats.receipts, 64);
                assert_eq!(server_stats.receipts, 64);
                assert!(client_stats.reads > 64 && client_stats.body_deliveries >= 64);
                assert!(server_stats.reads > 64 && server_stats.body_deliveries >= 64);
                assert_eq!(client_stats.writes, 66);
                assert_eq!(server_stats.writes, 66);
            }
        }
        assert_eq!(
            client::<false, false>(&scenario).round_trip(),
            cc.round_trip()
        );
        assert_eq!(
            server::<false, false>(&scenario).round_trip(),
            sc.round_trip()
        );
        assert_eq!(
            client::<true, false>(&scenario).round_trip(),
            cy.round_trip()
        );
        assert_eq!(
            server::<true, false>(&scenario).round_trip(),
            sy.round_trip()
        );
    }
}

#[test]
fn driver_handles_fragmented_metadata_and_short_scatter_gather_writes() {
    for scenario in [Scenario::fixed(37), Scenario::chunked(79, 11)] {
        for (read, write) in [(1, 1), (7, 3), (IO_BYTES, 5)] {
            let mut cc = client::<false, true>(&scenario);
            let mut cy = client::<true, true>(&scenario);
            let mut sc = server::<false, true>(&scenario);
            let mut sy = server::<true, true>(&scenario);
            cc.fragment(read, write);
            cy.fragment(read, write);
            sc.fragment(read, write);
            sy.fragment(read, write);
            for _ in 0..2 {
                assert_eq!(cc.round_trip(), cy.round_trip());
                assert_eq!(sc.round_trip(), sy.round_trip());
            }
        }
    }
}

#[test]
fn reusable_sessions_outlive_the_default_request_limit() {
    let scenario = Scenario::fixed(128);
    let mut client = client::<false, true>(&scenario);
    let mut server = server::<true, true>(&scenario);
    for _ in 0..1024 {
        client.round_trip();
        server.round_trip();
    }
}
