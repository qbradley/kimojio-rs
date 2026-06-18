// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::hint::black_box;
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use kimojio_stack::channel::cross_thread;
use kimojio_stack_steal::bench_support::RawSchedulerQueues;
use kimojio_stack_steal::{RingMode, Runtime, RuntimeConfig, StealPolicy};

const RAW_SCHEDULER_OPS_PER_ITER: u64 = 256;

fn steal_supports_link_timeout() -> bool {
    let mut runtime = Runtime::new();
    runtime.block_on(|cx| cx.supports_io_uring_opcode(rustix_uring::opcode::LinkTimeout::CODE))
}

fn runtime_baseline(c: &mut Criterion) {
    c.bench_function("runtime/block_on_empty", |b| {
        b.iter(|| {
            let mut runtime = Runtime::new();
            runtime.block_on(|_| ());
        });
    });

    c.bench_function("scheduler/yield_now_root", |b| {
        let mut runtime = Runtime::new();
        runtime.block_on(|cx| {
            b.iter(|| cx.yield_now());
        });
    });

    c.bench_function("scheduler/spawn_join_local_cached_stack", |b| {
        let mut runtime = Runtime::new();
        runtime.block_on(|cx| {
            b.iter(|| {
                black_box(cx.scope(|scope| {
                    let handle = scope.spawn(|_| 1_u64);
                    handle.join(cx)
                }));
            });
        });
    });

    c.bench_function("scheduler/spawn_yield_join_local_cached_stack", |b| {
        let mut runtime = Runtime::new();
        runtime.block_on(|cx| {
            b.iter(|| {
                black_box(cx.scope(|scope| {
                    let handle = scope.spawn(|cx| {
                        cx.yield_now();
                        1_u64
                    });
                    handle.join(cx)
                }));
            });
        });
    });

    c.bench_function("scheduler/raw_global_submit_poll", |b| {
        let queues = RawSchedulerQueues::new();
        b.iter_custom(|iters| {
            let start = Instant::now();
            for value in 0..(iters * RAW_SCHEDULER_OPS_PER_ITER) as usize {
                black_box(queues.global_push_poll(black_box(value)));
            }
            start.elapsed() / RAW_SCHEDULER_OPS_PER_ITER as u32
        });
    });

    c.bench_function("scheduler/raw_steal_one", |b| {
        let queues = RawSchedulerQueues::new();
        b.iter_custom(|iters| {
            let start = Instant::now();
            for value in 0..(iters * RAW_SCHEDULER_OPS_PER_ITER) as usize {
                black_box(queues.steal_one(black_box(value)));
            }
            start.elapsed() / RAW_SCHEDULER_OPS_PER_ITER as u32
        });
    });

    c.bench_function("scheduler/raw_steal_batch_transfer_drain_8", |b| {
        let queues = RawSchedulerQueues::new();
        b.iter_custom(|iters| {
            let start = Instant::now();
            for value in 0..(iters * RAW_SCHEDULER_OPS_PER_ITER) as usize {
                black_box(queues.steal_batch_transfer_and_drain(black_box(value * 8), 8));
            }
            start.elapsed() / RAW_SCHEDULER_OPS_PER_ITER as u32
        });
    });

    c.bench_function("scheduler/spawn_stealable_global_submit_join", |b| {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });
        runtime.block_on(|cx| {
            b.iter(|| {
                black_box(cx.scope(|scope| {
                    let handle = scope.spawn_stealable(|cx| cx.worker_id().index());
                    handle.join(cx)
                }));
            });
        });
        black_box(runtime.metrics());
    });

    c.bench_function("scheduler/spawn_stealable_nested_batch_join_8", |b| {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(4).unwrap(),
            steal_policy: StealPolicy::steal_half(),
            ..RuntimeConfig::default()
        });
        runtime.block_on(|cx| {
            b.iter(|| {
                black_box(cx.scope(|scope| {
                    let handle = scope.spawn_stealable(|cx| {
                        cx.scope(|scope| {
                            let handles: [_; 8] = std::array::from_fn(|_| {
                                scope.spawn_stealable(|cx| cx.worker_id().index())
                            });
                            handles
                                .into_iter()
                                .map(|handle| handle.join(cx))
                                .sum::<usize>()
                        })
                    });
                    handle.join(cx)
                }));
            });
        });
        black_box(runtime.metrics());
    });

    c.bench_function("scheduler/skewed_workload_scaling", |b| {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(4).unwrap(),
            steal_policy: StealPolicy::StealBatch {
                batch: NonZeroUsize::new(4).unwrap(),
                global_queue_interval: NonZeroUsize::new(1).unwrap(),
            },
            ..RuntimeConfig::default()
        });
        runtime.block_on(|cx| {
            b.iter(|| {
                black_box(cx.scope(|scope| {
                    let handles: [_; 32] = std::array::from_fn(|i| {
                        scope.spawn_stealable(move |cx| {
                            if i % 4 == 0 {
                                cx.yield_now();
                            }
                            i
                        })
                    });
                    handles
                        .into_iter()
                        .map(|handle| handle.join(cx))
                        .sum::<usize>()
                }));
            });
        });
        black_box(runtime.metrics());
    });

    c.bench_function("ring/owned_worker_local_nop", |b| {
        let mut runtime = Runtime::new();
        runtime.block_on(|cx| {
            let ring = cx.create_ring(RingMode::WorkerLocal).unwrap();
            b.iter(|| ring.nop(cx).unwrap());
        });
    });

    c.bench_function("ring/owned_worker_local_timeout_zero", |b| {
        let mut runtime = Runtime::new();
        runtime.block_on(|cx| {
            let ring = cx.create_ring(RingMode::WorkerLocal).unwrap();
            b.iter(|| ring.sleep(cx, Duration::from_millis(0)).unwrap());
        });
    });

    c.bench_function("ring/embedded_nop_select_timeout_not_fired", |b| {
        let mut runtime = Runtime::new();
        runtime.block_on(|cx| {
            cx.nop_with_select_timeout(Duration::from_secs(1)).unwrap();
            b.iter(|| cx.nop_with_select_timeout(Duration::from_secs(1)).unwrap());
        });
    });

    if steal_supports_link_timeout() {
        c.bench_function("ring/embedded_nop_link_timeout_not_fired", |b| {
            let mut runtime = Runtime::new();
            runtime.block_on(|cx| {
                cx.nop_with_link_timeout(Duration::from_secs(1)).unwrap();
                b.iter(|| cx.nop_with_link_timeout(Duration::from_secs(1)).unwrap());
            });
        });
    }

    c.bench_function("ring/shared_worker_owned_nop", |b| {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });
        runtime.block_on(|cx| {
            let ring = cx.create_shared_ring();
            cx.scope(|scope| {
                let bench = scope.spawn(|cx| b.iter(|| ring.nop(cx).unwrap()));
                bench.join(cx);
            });
        });
    });

    c.bench_function("ring/shared_worker_owned_timeout_zero", |b| {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });
        runtime.block_on(|cx| {
            let ring = cx.create_shared_ring();
            cx.scope(|scope| {
                let bench =
                    scope.spawn(|cx| b.iter(|| ring.sleep(cx, Duration::from_millis(0)).unwrap()));
                bench.join(cx);
            });
        });
    });

    c.bench_function("metrics/steal_counters_snapshot", |b| {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });
        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let handles = (0..16)
                    .map(|_| scope.spawn_stealable(|cx| cx.worker_id().index()))
                    .collect::<Vec<_>>();
                for handle in handles {
                    black_box(handle.join(cx));
                }
            });
        });
        b.iter(|| {
            let metrics = runtime.metrics();
            black_box((
                metrics.worker_count,
                metrics.steal_policy,
                metrics.steal_attempts,
                metrics.successful_steals,
                metrics.failed_steals,
                metrics.max_local_queue_depth,
                metrics.max_global_queue_depth,
                metrics.worker_completed_tasks,
            ));
        });
    });

    c.bench_function("channel/thread_ready_try_send_recv", |b| {
        let (tx, rx) = cross_thread::bounded(1).thread();
        b.iter(|| {
            tx.try_send(1_u64).unwrap();
            black_box(rx.try_recv().unwrap());
        });
    });

    c.bench_function("channel/steal_stackful_ready_send_recv", |b| {
        let mut runtime = Runtime::new();
        runtime.block_on(|cx| {
            let (tx, rx) = cross_thread::bounded(1).stackful();
            b.iter(|| {
                tx.send_with(cx, 1_u64).unwrap();
                black_box(rx.recv_with(cx).unwrap());
            });
        });
    });

    c.bench_function("channel/thread_to_steal_ready_ping_pong", |b| {
        let mut runtime = Runtime::new();
        runtime.block_on(|cx| {
            let (tx, rx) = cross_thread::bounded(1).thread_to_stackful();
            b.iter(|| {
                tx.send_blocking(1_u64).unwrap();
                black_box(rx.recv_with(cx).unwrap());
            });
        });
    });

    c.bench_function("channel/steal_to_thread_ready_ping_pong", |b| {
        let mut runtime = Runtime::new();
        runtime.block_on(|cx| {
            let (tx, rx) = cross_thread::bounded(1).stackful_to_thread();
            b.iter(|| {
                tx.send_with(cx, 1_u64).unwrap();
                black_box(rx.recv_blocking().unwrap());
            });
        });
    });

    #[cfg(feature = "tokio")]
    tokio_channel_baselines(c);
}

#[cfg(feature = "tokio")]
fn tokio_channel_baselines(c: &mut Criterion) {
    let tokio = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    c.bench_function("channel/tokio_ready_send_recv", |b| {
        let (tx, rx) = cross_thread::bounded(1).tokio();
        b.iter(|| {
            tokio.block_on(async {
                tx.send(1_u64).await.unwrap();
                black_box(rx.recv().await.unwrap());
            });
        });
    });

    c.bench_function("channel/tokio_to_steal_ready_ping_pong", |b| {
        let mut runtime = Runtime::new();
        runtime.block_on(|cx| {
            let (tx, rx) = cross_thread::bounded(1).tokio_to_stackful();
            b.iter(|| {
                tokio.block_on(tx.send(1_u64)).unwrap();
                black_box(rx.recv_with(cx).unwrap());
            });
        });
    });

    c.bench_function("channel/steal_to_tokio_ready_ping_pong", |b| {
        let mut runtime = Runtime::new();
        runtime.block_on(|cx| {
            let (tx, rx) = cross_thread::bounded(1).stackful_to_tokio();
            b.iter(|| {
                tx.send_with(cx, 1_u64).unwrap();
                black_box(tokio.block_on(rx.recv()).unwrap());
            });
        });
    });
}

criterion_group!(benches, runtime_baseline);
criterion_main!(benches);
