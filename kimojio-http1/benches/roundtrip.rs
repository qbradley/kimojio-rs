mod support;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use support::{Backend, Fixture, IO_BYTES, Mode, Options, run};

fn case(c: &mut Criterion, group: &str, name: &str, fixture: &Fixture, options: Options) {
    let mut group = c.benchmark_group(group);
    group.throughput(Throughput::Elements(1));
    let mut qualified = false;
    group.bench_function(name, |b| {
        // Qualify the selected case once, before timing. Filtered-out cases do
        // not run I/O or contaminate the tail of an individual perf recording.
        if !qualified {
            black_box(run::<true>(fixture, options, 2));
            qualified = true;
        }
        b.iter_custom(|iterations| {
            let outcome = run::<false>(black_box(fixture), options, iterations);
            assert_eq!(outcome.measured, iterations);
            assert!(outcome.diagnostics.is_none());
            black_box((outcome.server, outcome.client));
            outcome.elapsed
        })
    });
    group.finish();
}

fn benchmark(c: &mut Criterion) {
    for (name, bytes, chunked, chunk_bytes) in [
        ("empty", 0, false, IO_BYTES),
        ("fixed_128b", 128, false, IO_BYTES),
        ("chunked_1mib", 1024 * 1024, true, IO_BYTES),
        ("fragmented_8k", 8192, true, 512),
    ] {
        let fixture = Fixture::new(bytes, chunked, chunk_bytes);
        let group = format!("http1_wrapper/{name}");
        for backend in [Backend::Native, Backend::Stream] {
            case(c, &group, backend.name(), &fixture, Options::new(backend));
        }
        if name == "fixed_128b" || name == "chunked_1mib" {
            case(
                c,
                &group,
                "native_shared",
                &fixture.clone().shared(),
                Options::new(Backend::Native),
            );
        }
        if name == "fixed_128b" {
            case(
                c,
                &group,
                "native_shared_coalesced",
                &fixture.clone().shared(),
                Options {
                    coalesce: true,
                    ..Options::new(Backend::Native)
                },
            );
            case(
                c,
                &group,
                "native_coalesced",
                &fixture,
                Options {
                    coalesce: true,
                    ..Options::new(Backend::Native)
                },
            );
        }
        if name == "fixed_128b" || name == "chunked_1mib" {
            case(
                c,
                &group,
                "native_no_deadlines",
                &fixture,
                Options {
                    deadlines: false,
                    ..Options::new(Backend::Native)
                },
            );
        }
        if name == "chunked_1mib" {
            for (name, mode) in [
                ("native_forward", Mode::Forward),
                ("native_copy_forward", Mode::CopyForward),
            ] {
                case(
                    c,
                    &group,
                    name,
                    &fixture,
                    Options {
                        mode,
                        ..Options::new(Backend::Native)
                    },
                );
            }
        }
    }
}

criterion_group!(benches, benchmark);
criterion_main!(benches);
