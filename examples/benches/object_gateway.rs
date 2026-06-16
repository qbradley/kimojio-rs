// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use examples::object_gateway::workload::{
    WorkloadImplementation, parse_workload, run_implementation_profile,
};

fn bench_object_gateway_workloads(c: &mut Criterion) {
    let profile = parse_workload("small-object").expect("small-object workload profile exists");
    let mut group = c.benchmark_group("object_gateway_workload");
    group.sample_size(10);
    for implementation in WorkloadImplementation::ALL {
        group.bench_function(implementation.label(), |b| {
            b.iter(|| {
                let summary = run_implementation_profile(implementation, black_box(profile))
                    .expect("object gateway workload benchmark failed");
                black_box(summary);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_object_gateway_workloads);
criterion_main!(benches);
