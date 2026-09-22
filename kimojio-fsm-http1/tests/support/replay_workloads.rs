use super::support::{IO_BYTES, Scenario, client, replay_client, replay_server, server};

fn compare<const YIELD: bool, const CHECK: bool>(
    scenario: &Scenario,
    fragments: &[(usize, usize)],
    rounds: usize,
) {
    let mut copy_client = client::<YIELD, CHECK>(scenario);
    let mut copy_server = server::<YIELD, CHECK>(scenario);
    let mut replay_client = replay_client::<YIELD, CHECK>(scenario);
    let mut replay_server = replay_server::<YIELD, CHECK>(scenario);
    for &(read, write) in fragments {
        copy_client.fragment(read, write);
        copy_server.fragment(read, write);
        replay_client.fragment(read, write);
        replay_server.fragment(read, write);
        for _ in 0..rounds {
            assert_eq!(replay_client.round_trip(), copy_client.round_trip());
            assert_eq!(replay_server.round_trip(), copy_server.round_trip());
            assert_eq!(replay_client.trace(), copy_client.trace());
            assert_eq!(replay_server.trace(), copy_server.trace());
        }
    }
}

#[test]
fn replay_matches_copy_for_all_timed_workloads_and_callback_modes() {
    for scenario in [
        Scenario::fixed(128),
        Scenario::fixed(1024 * 1024),
        Scenario::chunked(1024 * 1024, IO_BYTES),
    ] {
        compare::<false, true>(&scenario, &[(IO_BYTES, usize::MAX)], 4);
        compare::<true, true>(&scenario, &[(IO_BYTES, usize::MAX)], 4);
        compare::<false, false>(&scenario, &[(IO_BYTES, usize::MAX)], 4);
        compare::<true, false>(&scenario, &[(IO_BYTES, usize::MAX)], 4);
    }
}

#[test]
fn replay_preserves_fragmentation_and_can_rebuild_with_a_pending_read() {
    for scenario in [Scenario::fixed(37), Scenario::chunked(79, 11)] {
        // Reconfigure the same sessions between exchanges, including a server
        // that has already issued its next read after retiring an exchange.
        let fragments = [(1, 1), (7, 3), (IO_BYTES, 5), (3, usize::MAX)];
        compare::<false, true>(&scenario, &fragments, 3);
        compare::<true, true>(&scenario, &fragments, 3);
    }
}

#[test]
fn replay_reuses_single_and_multiple_fixture_slots_without_replenishing_bytes() {
    compare::<false, true>(&Scenario::fixed(128), &[(IO_BYTES, usize::MAX)], 1024);
    compare::<true, true>(&Scenario::fixed(128), &[(IO_BYTES, usize::MAX)], 1024);
    compare::<false, true>(&Scenario::chunked(79, 11), &[(7, usize::MAX)], 1024);
    compare::<true, true>(&Scenario::chunked(79, 11), &[(7, usize::MAX)], 1024);
}
