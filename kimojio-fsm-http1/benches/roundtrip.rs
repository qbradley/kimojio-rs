mod support;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use support::{Scenario, client, server};

fn benchmark(c: &mut Criterion) {
    for (name, scenario) in [
        ("fixed_128b", Scenario::fixed(128)),
        (
            "chunked_1mib",
            Scenario::chunked(1024 * 1024, support::IO_BYTES),
        ),
    ] {
        // The same driver performs full wire/payload validation outside timing.
        client::<false, true>(&scenario).round_trip();
        client::<true, true>(&scenario).round_trip();
        server::<false, true>(&scenario).round_trip();
        server::<true, true>(&scenario).round_trip();

        let mut group = c.benchmark_group(format!("http1/{name}"));
        group.throughput(Throughput::Bytes(2 * scenario.body_bytes() as u64));
        group.bench_function("client/continue", |b| {
            let mut session = client::<false, false>(black_box(&scenario));
            b.iter(|| black_box(session.round_trip()));
        });
        group.bench_function("client/yield", |b| {
            let mut session = client::<true, false>(black_box(&scenario));
            b.iter(|| black_box(session.round_trip()));
        });
        group.bench_function("server/continue", |b| {
            let mut session = server::<false, false>(black_box(&scenario));
            b.iter(|| black_box(session.round_trip()));
        });
        group.bench_function("server/yield", |b| {
            let mut session = server::<true, false>(black_box(&scenario));
            b.iter(|| black_box(session.round_trip()));
        });
        group.finish();

        #[cfg(feature = "bench-internals")]
        benchmark_replay(c, name, &scenario);
    }
}

#[cfg(feature = "bench-internals")]
fn benchmark_replay(c: &mut Criterion, name: &str, scenario: &Scenario) {
    use support::{replay_client, replay_server};

    assert_eq!(
        replay_client::<false, true>(scenario).round_trip(),
        client::<false, true>(scenario).round_trip()
    );
    assert_eq!(
        replay_client::<true, true>(scenario).round_trip(),
        client::<true, true>(scenario).round_trip()
    );
    assert_eq!(
        replay_server::<false, true>(scenario).round_trip(),
        server::<false, true>(scenario).round_trip()
    );
    assert_eq!(
        replay_server::<true, true>(scenario).round_trip(),
        server::<true, true>(scenario).round_trip()
    );

    let mut group = c.benchmark_group(format!("http1_replay/{name}"));
    group.throughput(Throughput::Bytes(2 * scenario.body_bytes() as u64));
    group.bench_function("client/continue", |b| {
        let mut session = replay_client::<false, false>(black_box(scenario));
        b.iter(|| black_box(session.round_trip()));
    });
    group.bench_function("client/yield", |b| {
        let mut session = replay_client::<true, false>(black_box(scenario));
        b.iter(|| black_box(session.round_trip()));
    });
    group.bench_function("server/continue", |b| {
        let mut session = replay_server::<false, false>(black_box(scenario));
        b.iter(|| black_box(session.round_trip()));
    });
    group.bench_function("server/yield", |b| {
        let mut session = replay_server::<true, false>(black_box(scenario));
        b.iter(|| black_box(session.round_trip()));
    });
    group.finish();
}

criterion_group!(benches, benchmark);
criterion_main!(benches);
