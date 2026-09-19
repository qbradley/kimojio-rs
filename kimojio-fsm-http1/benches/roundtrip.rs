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
    }
}

criterion_group!(benches, benchmark);
criterion_main!(benches);
