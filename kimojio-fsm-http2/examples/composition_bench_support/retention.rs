use super::*;

// Private protocol default, source-audited and debugger-checked; not a setting.
const SOURCE_AUDITED_TOMBSTONE_LIMIT: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
struct VectorStorage {
    len: usize,
    capacity: usize,
    requested_bytes: usize,
}

fn vector_storage<T>(vector: &Vec<T>) -> VectorStorage {
    VectorStorage {
        len: vector.len(),
        capacity: vector.capacity(),
        requested_bytes: vector.capacity() * std::mem::size_of::<T>(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
struct HarnessStorage {
    slots: VectorStorage,
    bodies: VectorStorage,
    permits: VectorStorage,
    requests: VectorStorage,
    alarms: VectorStorage,
    cancels: VectorStorage,
}

impl HarnessStorage {
    fn snapshot(ports: &Ports) -> Self {
        assert!(ports.diagnostics.is_none());
        Self {
            slots: vector_storage(&ports.slots),
            bodies: vector_storage(&ports.bodies),
            permits: vector_storage(&ports.permits),
            requests: vector_storage(&ports.requests),
            alarms: vector_storage(&ports.alarms),
            cancels: vector_storage(&ports.cancels),
        }
    }
}

#[derive(serde::Serialize)]
struct Boundary<S> {
    previous_cohort: usize,
    through_cohort: usize,
    retired_each_endpoint: usize,
    allocation_counts: S,
    client_harness: HarnessStorage,
    server_harness: HarnessStorage,
}

pub(super) fn execute<C: Client, S: Server>(
    make: impl FnOnce() -> (C, S),
    meter: &impl Meter,
    workload: Workload<'_>,
) -> serde_json::Value {
    let Workload {
        mode,
        case,
        concurrency,
        fragment,
        batches,
        ..
    } = workload;
    let tombstone_limit = SOURCE_AUDITED_TOMBSTONE_LIMIT;
    let bound_cohort = tombstone_limit.div_ceil(concurrency);
    let boundaries = [
        8,
        10,
        12,
        100,
        1000,
        2000,
        5000,
        batches,
        bound_cohort,
        bound_cohort + 1,
    ];
    // Metadata stays on the stack until shutdown, outside every allocator window.
    let mut samples: [Option<Boundary<_>>; 11] = std::array::from_fn(|_| None);
    let live_before_setup = meter.live_bytes();
    meter.begin();
    let (client, server) = make();
    let mut pair = Pair {
        client,
        server,
        cp: Ports::new(false, concurrency, case),
        sp: Ports::new(true, concurrency, case),
        case,
        fragment,
    };
    samples[0] = Some(Boundary {
        previous_cohort: 0,
        through_cohort: 0,
        retired_each_endpoint: 0,
        allocation_counts: meter.end(),
        client_harness: HarnessStorage::snapshot(&pair.cp),
        server_harness: HarnessStorage::snapshot(&pair.sp),
    });
    let mut used = 1;
    let mut previous_cohort = 0;
    meter.begin();
    for cohort in 1..=batches {
        pair.batch();
        if boundaries.contains(&cohort) {
            assert_eq!(pair.cp.retired, cohort * concurrency);
            assert_eq!(pair.sp.retired, pair.cp.retired);
            samples[used] = Some(Boundary {
                previous_cohort,
                through_cohort: cohort,
                retired_each_endpoint: pair.cp.retired,
                allocation_counts: meter.end(),
                client_harness: HarnessStorage::snapshot(&pair.cp),
                server_harness: HarnessStorage::snapshot(&pair.sp),
            });
            used += 1;
            previous_cohort = cohort;
            if cohort < batches {
                meter.begin();
            }
        }
    }
    let wire_bytes = pair.cp.wire_bytes + pair.sp.wire_bytes;
    pair.finish();
    drop(pair);
    let live_after_teardown = meter.live_bytes();
    serde_json::json!({
        "schema": 1, "phase": "retention", "mode": mode, "case": case.name,
        "concurrency": concurrency, "fragment": fragment, "batches": batches,
        "exchanges": batches * concurrency,
        "payload_bytes": batches * concurrency * (case.request + case.response),
        "wire_bytes": wire_bytes, "same_connection": true,
        "source_audited_tombstones_per_endpoint": tombstone_limit,
        "configured_active_streams": config().http.max_active_streams(),
        "samples": &samples[..used], "metadata_storage": "fixed-stack-array",
        "live_before_setup": live_before_setup,
        "live_after_teardown": live_after_teardown,
        "teardown_live_change": live_after_teardown as i128 - live_before_setup as i128,
        "all_payload_bytes_compared": true, "shutdown_settled": true,
        "payload_storage": "static", "transport": "direct-slices-to-read-page",
    })
}
