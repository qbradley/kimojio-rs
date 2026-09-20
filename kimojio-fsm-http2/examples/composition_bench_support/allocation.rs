use super::*;

#[path = "retention.rs"]
mod retention;

pub trait Meter {
    type Sample: serde::Serialize;

    fn begin(&self);
    fn end(&self) -> Self::Sample;
    fn live_bytes(&self) -> usize;
}

#[derive(Clone, Copy)]
pub struct Workload<'a> {
    pub mode: &'a str,
    pub case: Case,
    pub concurrency: usize,
    pub fragment: usize,
    pub batches: usize,
    pub phase: &'a str,
}

fn execute<C: Client, S: Server>(
    make: impl FnOnce() -> (C, S),
    meter: &impl Meter,
    workload: Workload<'_>,
) -> serde_json::Value {
    if workload.phase == "retention" {
        return retention::execute(make, meter, workload);
    }
    let Workload {
        mode,
        case,
        concurrency,
        fragment,
        batches,
        phase,
    } = workload;
    let live_before_setup = meter.live_bytes();
    if phase == "cold" {
        meter.begin();
    }
    let (client, server) = make();
    let mut pair = Pair {
        client,
        server,
        cp: Ports::new(false, concurrency, case),
        sp: Ports::new(true, concurrency, case),
        case,
        fragment,
    };
    let warmup_batches = if phase == "steady" { 8 } else { 0 };
    for _ in 0..warmup_batches {
        pair.batch();
    }
    let wire_before = pair.cp.wire_bytes + pair.sp.wire_bytes;
    if phase == "steady" {
        meter.begin();
    }
    for _ in 0..batches {
        pair.batch();
    }
    assert_eq!(pair.cp.retired, (warmup_batches + batches) * concurrency);
    assert_eq!(pair.sp.retired, pair.cp.retired);
    let counts = meter.end();
    let wire_bytes = pair.cp.wire_bytes + pair.sp.wire_bytes - wire_before;
    pair.finish();
    drop(pair);
    let live_after_teardown = meter.live_bytes();
    let exchanges = batches * concurrency;
    serde_json::json!({
        "schema": 1, "mode": mode, "case": case.name, "phase": phase,
        "concurrency": concurrency, "fragment": fragment, "batches": batches,
        "warmup_batches": warmup_batches, "exchanges": exchanges,
        "request_bytes": case.request, "response_bytes": case.response,
        "payload_bytes": exchanges * (case.request + case.response),
        "wire_bytes": wire_bytes, "allocation_counts": counts,
        "live_before_setup": live_before_setup,
        "live_after_teardown": live_after_teardown,
        "teardown_live_change": live_after_teardown as i128 - live_before_setup as i128,
        "payload_storage": "static", "transport": "direct-slices-to-read-page",
        "all_payload_bytes_compared": true, "shutdown_settled": true,
        "measurement_includes_shutdown": false,
    })
}

pub fn run(meter: &impl Meter, workload: Workload<'_>) -> serde_json::Value {
    assert!(workload.concurrency > 0 && workload.concurrency <= 128);
    assert!(workload.fragment > 0 && workload.batches > 0);
    assert!(
        workload.phase == "steady"
            || (workload.phase == "cold" && workload.batches == 1)
            || (workload.phase == "retention"
                && workload.case.name == "empty"
                && (10..=10000).contains(&workload.batches)),
        "cold requires one cohort; retention requires 10..=10000 empty cohorts"
    );
    assert!(
        std::env::var_os("BENCH_DIAGNOSTICS").is_none(),
        "diagnostic allocations must not contaminate the probe"
    );
    match workload.mode {
        "direct" => execute(
            || {
                (
                    h2::Client::new(config(), Duration::ZERO).unwrap(),
                    h2::Server::new(config(), Duration::ZERO).unwrap(),
                )
            },
            meter,
            workload,
        ),
        "selected" | "auto" => execute(
            || {
                let client =
                    http::Client::http2(h2::Client::new(config(), Duration::ZERO).unwrap());
                let server = if workload.mode == "selected" {
                    http::Server::http2(h2::Server::new(config(), Duration::ZERO).unwrap())
                } else {
                    http::Server::detect(
                        http::DetectionConfig {
                            http1_connection: h1::ConnectionId {
                                slot: 1,
                                generation: 1,
                            },
                            http1_config: h1::Config::default(),
                            http1_buffer: vec![0; 65536],
                            http2_config: config(),
                            timeout: Duration::from_secs(1),
                        },
                        Duration::ZERO,
                    )
                    .unwrap()
                };
                (client, server)
            },
            meter,
            workload,
        ),
        _ => panic!("unknown mode: {}", workload.mode),
    }
}
