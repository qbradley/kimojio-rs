// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::hint::black_box;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use kimojio_stack::channel::cross_thread;
use kimojio_stack_steal::bench_support::{
    RawSchedulerQueues, RawSfqQueues, ReadyQueueCandidate, ReadyQueueCandidateKind,
    ReadyQueueConfig, ReadyQueueTask, build_ready_queue_candidate,
};
use kimojio_stack_steal::{
    QueueSelectionPolicy, RingFd, RingMode, Runtime, RuntimeConfig, SchedulerConfig, SchedulerMode,
    StealPolicy, TenantReassignmentPolicy,
};
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};
use rustix::pipe::pipe;

const RAW_SCHEDULER_OPS_PER_ITER: u64 = 256;
const RAW_SFQ_OPS_PER_ITER: u64 = 512;
const RAW_READY_QUEUE_OPS_PER_ITER: u64 = 256;
const RAW_READY_QUEUE_BACKLOG: usize = 2048;

fn steal_supports_link_timeout() -> bool {
    let mut runtime = Runtime::new();
    runtime.block_on(|cx| cx.supports_io_uring_opcode(rustix_uring::opcode::LinkTimeout::CODE))
}

fn ready_queue_worker_counts() -> Vec<usize> {
    let available = thread::available_parallelism()
        .map(NonZeroUsize::get)
        .unwrap_or(1)
        .min(32);
    let mut counts = [1_usize, 2, 4, 8, 16, available]
        .into_iter()
        .filter(|count| *count <= available)
        .collect::<Vec<_>>();
    counts.sort_unstable();
    counts.dedup();
    counts
}

fn ready_queue_config(workers: usize) -> ReadyQueueConfig {
    ReadyQueueConfig::new(workers, workers.next_power_of_two().clamp(8, 64), 4096)
}

fn ready_queue_candidate(
    kind: ReadyQueueCandidateKind,
    workers: usize,
) -> Arc<dyn ReadyQueueCandidate> {
    Arc::from(build_ready_queue_candidate(
        kind,
        ready_queue_config(workers),
    ))
}

fn run_ready_queue_parallel_roundtrips(
    kind: ReadyQueueCandidateKind,
    workers: usize,
    total_ops: usize,
) -> usize {
    let queue = ready_queue_candidate(kind, workers);
    let per_worker = total_ops.div_ceil(workers);
    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for worker in 0..workers {
            let queue = Arc::clone(&queue);
            handles.push(scope.spawn(move || {
                let mut sum = 0_usize;
                for value in 0..per_worker {
                    let task = ReadyQueueTask::new((worker + 1) as u64, value);
                    assert!(queue.submit_worker(worker, task));
                    loop {
                        if let Some(task) = queue.pop_worker(worker) {
                            sum = sum.wrapping_add(task.value);
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
                sum
            }));
        }
        handles
            .into_iter()
            .map(|handle| handle.join().expect("ready queue worker panicked"))
            .fold(0_usize, usize::wrapping_add)
    })
}

fn fill_ready_queue_backlog(queue: &dyn ReadyQueueCandidate, total: usize) {
    for value in 0..total {
        let tenant = (value % 257 + 1) as u64;
        assert!(queue.submit_external(ReadyQueueTask::new(tenant, value)));
    }
}

fn drain_ready_queue_backlog(queue: &dyn ReadyQueueCandidate, total: usize) -> usize {
    let mut remaining = total;
    let mut sum = 0_usize;
    while remaining != 0 {
        for worker in 0..queue.workers() {
            if let Some(task) = queue.pop_worker(worker) {
                sum = sum.wrapping_add(task.value);
                remaining -= 1;
                if remaining == 0 {
                    break;
                }
            }
        }
    }
    sum
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

    c.bench_function("scheduler/scope_empty", |b| {
        let mut runtime = Runtime::new();
        runtime.block_on(|cx| {
            b.iter(|| {
                cx.scope(|_| black_box(()));
                black_box(());
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

    for workers in [1_usize, 2, 4] {
        c.bench_function(
            &format!("scheduler/spawn_stealable_single_tenant_throughput_{workers}_workers"),
            |b| {
                let mut runtime = Runtime::with_config(RuntimeConfig {
                    workers: NonZeroUsize::new(workers).unwrap(),
                    steal_policy: if workers == 1 {
                        StealPolicy::Disabled
                    } else {
                        StealPolicy::steal_one()
                    },
                    ..RuntimeConfig::default()
                });
                runtime.block_on(|cx| {
                    b.iter(|| {
                        black_box(cx.scope(|scope| {
                            let handles: [_; 32] =
                                std::array::from_fn(|_| scope.spawn_stealable(|_| 1_usize));
                            handles
                                .into_iter()
                                .map(|handle| handle.join(cx))
                                .sum::<usize>()
                        }));
                    });
                });
                black_box(runtime.metrics());
            },
        );
    }

    c.bench_function("scheduler/io_readiness_timer_wakeup_batch_32", |b| {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(4).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });
        runtime.block_on(|cx| {
            b.iter(|| {
                black_box(cx.scope(|scope| {
                    let handles: [_; 32] = std::array::from_fn(|_| {
                        scope.spawn_stealable(|cx| {
                            cx.sleep(Duration::from_millis(0)).unwrap();
                            1_usize
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

    c.bench_function(
        "scheduler/sfq_spawn_stealable_single_tenant_4_workers",
        |b| {
            let mut runtime = Runtime::with_config(RuntimeConfig {
                workers: NonZeroUsize::new(4).unwrap(),
                steal_policy: StealPolicy::Disabled,
                scheduler: SchedulerConfig::stochastic_fair(NonZeroUsize::new(16).unwrap()),
                ..RuntimeConfig::default()
            });
            runtime.block_on(|cx| {
                b.iter(|| {
                    black_box(cx.scope(|scope| {
                        let tenant = cx.new_tenant_id();
                        let handles: [_; 32] = std::array::from_fn(|_| {
                            scope.spawn_stealable_with_tenant(tenant, |_| 1_usize)
                        });
                        handles
                            .into_iter()
                            .map(|handle| handle.join(cx))
                            .sum::<usize>()
                    }));
                });
            });
            black_box(runtime.metrics());
        },
    );

    for (label, selection) in [
        ("single_choice", QueueSelectionPolicy::SingleChoice),
        (
            "subset_shortest_2",
            QueueSelectionPolicy::RandomSubsetShortest {
                subset: NonZeroUsize::new(2).unwrap(),
            },
        ),
        ("shortest", QueueSelectionPolicy::Shortest),
        (
            "movement_cost_4",
            QueueSelectionPolicy::MovementCost {
                subset: NonZeroUsize::new(4).unwrap(),
                movement_penalty: 2,
            },
        ),
    ] {
        c.bench_function(
            &format!("scheduler/sfq_policy_{label}_single_tenant_4_workers"),
            |b| {
                let mut runtime = Runtime::with_config(RuntimeConfig {
                    workers: NonZeroUsize::new(4).unwrap(),
                    steal_policy: StealPolicy::Disabled,
                    scheduler: SchedulerConfig {
                        mode: SchedulerMode::StochasticFair,
                        ready_partitions: NonZeroUsize::new(16).unwrap(),
                        selection,
                        reassignment: TenantReassignmentPolicy::Stable,
                    },
                    ..RuntimeConfig::default()
                });
                runtime.block_on(|cx| {
                    b.iter(|| {
                        black_box(cx.scope(|scope| {
                            let tenant = cx.new_tenant_id();
                            let handles: [_; 32] = std::array::from_fn(|_| {
                                scope.spawn_stealable_with_tenant(tenant, |_| 1_usize)
                            });
                            handles
                                .into_iter()
                                .map(|handle| handle.join(cx))
                                .sum::<usize>()
                        }));
                    });
                });
                black_box(runtime.metrics());
            },
        );
    }

    c.bench_function("scheduler/sfq_skewed_heavy_light_tenants", |b| {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(4).unwrap(),
            steal_policy: StealPolicy::Disabled,
            scheduler: SchedulerConfig {
                mode: SchedulerMode::StochasticFair,
                ready_partitions: NonZeroUsize::new(16).unwrap(),
                selection: QueueSelectionPolicy::RandomSubsetShortest {
                    subset: NonZeroUsize::new(2).unwrap(),
                },
                reassignment: TenantReassignmentPolicy::EverySchedulingDecisions(
                    NonZeroUsize::new(10_000).unwrap(),
                ),
            },
            ..RuntimeConfig::default()
        });
        runtime.block_on(|cx| {
            b.iter(|| {
                black_box(cx.scope(|scope| {
                    let heavy = cx.new_tenant_id();
                    let light = cx.new_tenant_id();
                    let handles: [_; 64] = std::array::from_fn(|index| {
                        if index < 56 {
                            scope.spawn_stealable_with_tenant(heavy, |cx| {
                                cx.yield_now();
                                1_usize
                            })
                        } else {
                            scope.spawn_stealable_with_tenant(light, |_| 1_usize)
                        }
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

    for (label, reassignment) in [
        ("stable", TenantReassignmentPolicy::Stable),
        (
            "epoch_1024",
            TenantReassignmentPolicy::EverySchedulingDecisions(NonZeroUsize::new(1024).unwrap()),
        ),
        (
            "epoch_10000",
            TenantReassignmentPolicy::EverySchedulingDecisions(NonZeroUsize::new(10_000).unwrap()),
        ),
    ] {
        c.bench_function(
            &format!("scheduler/sfq_reassignment_{label}_collision_skew"),
            |b| {
                let mut runtime = Runtime::with_config(RuntimeConfig {
                    workers: NonZeroUsize::new(4).unwrap(),
                    steal_policy: StealPolicy::Disabled,
                    scheduler: SchedulerConfig {
                        mode: SchedulerMode::StochasticFair,
                        ready_partitions: NonZeroUsize::new(2).unwrap(),
                        selection: QueueSelectionPolicy::SingleChoice,
                        reassignment,
                    },
                    ..RuntimeConfig::default()
                });
                runtime.block_on(|cx| {
                    b.iter(|| {
                        black_box(cx.scope(|scope| {
                            let tenants: [_; 8] = std::array::from_fn(|_| cx.new_tenant_id());
                            let handles: [_; 64] = std::array::from_fn(|index| {
                                let tenant = tenants[index % tenants.len()];
                                scope.spawn_stealable_with_tenant(tenant, |cx| {
                                    cx.yield_now();
                                    1_usize
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
            },
        );
    }

    c.bench_function("scheduler/sfq_io_readiness_timer_wakeup_batch_32", |b| {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(4).unwrap(),
            steal_policy: StealPolicy::Disabled,
            scheduler: SchedulerConfig::stochastic_fair(NonZeroUsize::new(16).unwrap()),
            ..RuntimeConfig::default()
        });
        runtime.block_on(|cx| {
            b.iter(|| {
                black_box(cx.scope(|scope| {
                    let handles: [_; 32] = std::array::from_fn(|_| {
                        scope.spawn_stealable(|cx| {
                            cx.sleep(Duration::from_millis(0)).unwrap();
                            1_usize
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

    c.bench_function("scheduler/raw_sfq_single_choice_roundtrip", |b| {
        let mut queues = RawSfqQueues::new(16);
        b.iter_custom(|iters| {
            let start = Instant::now();
            for value in 0..(iters * RAW_SFQ_OPS_PER_ITER) as usize {
                black_box(
                    queues.single_choice_roundtrip(black_box(value as u64), black_box(value)),
                );
            }
            start.elapsed() / RAW_SFQ_OPS_PER_ITER as u32
        });
    });

    c.bench_function("scheduler/raw_sfq_random_subset_shortest_2", |b| {
        let mut queues = RawSfqQueues::new(16);
        b.iter_custom(|iters| {
            let start = Instant::now();
            for value in 0..(iters * RAW_SFQ_OPS_PER_ITER) as usize {
                black_box(queues.random_subset_shortest_roundtrip(
                    black_box(value as u64),
                    2,
                    black_box(value),
                ));
            }
            start.elapsed() / RAW_SFQ_OPS_PER_ITER as u32
        });
    });

    c.bench_function("scheduler/raw_sfq_global_shortest_16", |b| {
        let mut queues = RawSfqQueues::new(16);
        b.iter_custom(|iters| {
            let start = Instant::now();
            for value in 0..(iters * RAW_SFQ_OPS_PER_ITER) as usize {
                black_box(queues.global_shortest_roundtrip(black_box(value)));
            }
            start.elapsed() / RAW_SFQ_OPS_PER_ITER as u32
        });
    });

    c.bench_function("scheduler/raw_sfq_movement_cost_subset_4", |b| {
        let mut queues = RawSfqQueues::new(16);
        b.iter_custom(|iters| {
            let start = Instant::now();
            for value in 0..(iters * RAW_SFQ_OPS_PER_ITER) as usize {
                black_box(queues.movement_cost_roundtrip(
                    black_box(value as u64),
                    4,
                    black_box(value % 16),
                    2,
                    black_box(value),
                ));
            }
            start.elapsed() / RAW_SFQ_OPS_PER_ITER as u32
        });
    });

    c.bench_function("scheduler/raw_sfq_reassignment_hash", |b| {
        let queues = RawSfqQueues::new(16);
        b.iter_custom(|iters| {
            let start = Instant::now();
            for value in 0..(iters * RAW_SFQ_OPS_PER_ITER) {
                black_box(
                    queues.reassigned_partition(black_box(value), black_box(value.rotate_left(17))),
                );
            }
            start.elapsed() / RAW_SFQ_OPS_PER_ITER as u32
        });
    });

    for kind in ReadyQueueCandidateKind::all() {
        let label = kind.label();

        c.bench_function(
            &format!("scheduler/ready_queue/{label}/low_load_local_roundtrip"),
            |b| {
                let queue = ready_queue_candidate(kind, 1);
                b.iter_custom(|iters| {
                    let ops = (iters * RAW_READY_QUEUE_OPS_PER_ITER) as usize;
                    let start = Instant::now();
                    for value in 0..ops {
                        assert!(queue.submit_worker(0, ReadyQueueTask::new(1, value)));
                        black_box(queue.pop_worker(0).expect("ready queue task missing"));
                    }
                    start.elapsed() / RAW_READY_QUEUE_OPS_PER_ITER as u32
                });
            },
        );

        c.bench_function(
            &format!("scheduler/ready_queue/{label}/low_load_external_handoff"),
            |b| {
                let queue = ready_queue_candidate(kind, 2);
                b.iter_custom(|iters| {
                    let ops = (iters * RAW_READY_QUEUE_OPS_PER_ITER) as usize;
                    let start = Instant::now();
                    for value in 0..ops {
                        assert!(
                            queue.submit_external(ReadyQueueTask::new(
                                (value % 17 + 1) as u64,
                                value
                            ))
                        );
                        black_box(
                            queue
                                .pop_worker(value % queue.workers())
                                .expect("ready queue task missing"),
                        );
                    }
                    start.elapsed() / RAW_READY_QUEUE_OPS_PER_ITER as u32
                });
            },
        );

        c.bench_function(
            &format!("scheduler/ready_queue/{label}/high_load_backlog_drain"),
            |b| {
                let queue = ready_queue_candidate(kind, 4);
                b.iter_custom(|iters| {
                    let start = Instant::now();
                    for _ in 0..iters {
                        fill_ready_queue_backlog(queue.as_ref(), RAW_READY_QUEUE_BACKLOG);
                        black_box(drain_ready_queue_backlog(
                            queue.as_ref(),
                            RAW_READY_QUEUE_BACKLOG,
                        ));
                    }
                    start.elapsed() / RAW_READY_QUEUE_BACKLOG as u32
                });
            },
        );

        for workers in ready_queue_worker_counts() {
            c.bench_function(
                &format!("scheduler/ready_queue/{label}/throughput_{workers}_workers"),
                |b| {
                    b.iter_custom(|iters| {
                        let ops = (iters * RAW_READY_QUEUE_OPS_PER_ITER) as usize;
                        let start = Instant::now();
                        black_box(run_ready_queue_parallel_roundtrips(kind, workers, ops));
                        start.elapsed() / RAW_READY_QUEUE_OPS_PER_ITER as u32
                    });
                },
            );
        }
    }

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

    c.bench_function("ring/shared_worker_owned_pipe_write_read", |b| {
        let mut runtime = Runtime::with_config(RuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..RuntimeConfig::default()
        });
        runtime.block_on(|cx| {
            let ring = cx.create_shared_ring();
            let (read_fd, write_fd) = pipe().unwrap();
            let read_fd = RingFd::from_owned(read_fd);
            let write_fd = RingFd::from_owned(write_fd);
            let mut read_buffer = [0_u8; 1];
            cx.scope(|scope| {
                let bench = scope.spawn(|cx| {
                    ring.write(cx, &write_fd, &[1]).unwrap();
                    ring.read(cx, &read_fd, &mut read_buffer).unwrap();
                    b.iter(|| {
                        ring.write(cx, &write_fd, &[1]).unwrap();
                        ring.read(cx, &read_fd, &mut read_buffer).unwrap();
                        black_box(read_buffer[0]);
                    });
                });
                bench.join(cx);
            });
        });
    });

    for (label, payload_size) in [("8k", 8 * 1024_usize), ("64k", 64 * 1024_usize)] {
        let name = format!("ring/shared_worker_owned_socketpair_write_read_{label}");
        c.bench_function(&name, |b| {
            let mut runtime = Runtime::with_config(RuntimeConfig {
                workers: NonZeroUsize::new(2).unwrap(),
                steal_policy: StealPolicy::steal_one(),
                ..RuntimeConfig::default()
            });
            runtime.block_on(|cx| {
                let ring = cx.create_shared_ring();
                let (left_fd, right_fd) = socketpair(
                    AddressFamily::UNIX,
                    SocketType::STREAM,
                    SocketFlags::empty(),
                    None,
                )
                .unwrap();
                let left_fd = RingFd::from_owned(left_fd);
                let right_fd = RingFd::from_owned(right_fd);
                let write_buffer = vec![1_u8; payload_size];
                let mut read_buffer = vec![0_u8; payload_size];
                cx.scope(|scope| {
                    let bench = scope.spawn(|cx| {
                        assert_eq!(
                            ring.write(cx, &left_fd, write_buffer.as_slice()).unwrap(),
                            payload_size
                        );
                        assert_eq!(
                            ring.read(cx, &right_fd, read_buffer.as_mut_slice())
                                .unwrap(),
                            payload_size
                        );
                        b.iter(|| {
                            assert_eq!(
                                ring.write(cx, &left_fd, black_box(write_buffer.as_slice()))
                                    .unwrap(),
                                payload_size
                            );
                            assert_eq!(
                                ring.read(cx, &right_fd, read_buffer.as_mut_slice())
                                    .unwrap(),
                                payload_size
                            );
                            black_box(read_buffer[0]);
                        });
                    });
                    bench.join(cx);
                });
            });
        });
    }

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
