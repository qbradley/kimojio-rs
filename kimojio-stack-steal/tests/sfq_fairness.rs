use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use kimojio_stack::channel::cross_thread;
use kimojio_stack_steal::{
    QueueSelectionPolicy, Runtime, RuntimeConfig, SchedulerConfig, SchedulerMode, StealPolicy,
    TenantReassignmentPolicy,
};
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

fn sfq_config() -> RuntimeConfig {
    RuntimeConfig {
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
    }
}

#[test]
fn sfq_fairness_light_tenants_progress_while_heavy_tenant_is_ready() {
    const LIGHT_TENANTS: usize = 8;
    const LIGHT_WORK: usize = 32;
    const HEAVY_WORK: usize = 512;

    let mut runtime = Runtime::with_config(sfq_config());
    let light_progress = Arc::new(
        (0..LIGHT_TENANTS)
            .map(|_| AtomicUsize::new(0))
            .collect::<Vec<_>>(),
    );
    let heavy_progress = Arc::new(AtomicUsize::new(0));

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let heavy_tenant = cx.new_tenant_id();
            let heavy_handles = (0..HEAVY_WORK)
                .map(|_| {
                    let heavy_progress = Arc::clone(&heavy_progress);
                    scope.spawn_stealable_with_tenant(heavy_tenant, move |cx| {
                        for _ in 0..4 {
                            cx.yield_now();
                        }
                        heavy_progress.fetch_add(1, Ordering::Relaxed);
                    })
                })
                .collect::<Vec<_>>();

            let mut light_handles = Vec::new();
            for tenant_index in 0..LIGHT_TENANTS {
                let tenant = cx.new_tenant_id();
                for _ in 0..LIGHT_WORK {
                    let light_progress = Arc::clone(&light_progress);
                    light_handles.push(scope.spawn_stealable_with_tenant(tenant, move |cx| {
                        cx.yield_now();
                        light_progress[tenant_index].fetch_add(1, Ordering::Relaxed);
                    }));
                }
            }

            for handle in light_handles {
                handle.join(cx);
            }
            for count in light_progress.iter() {
                assert_eq!(count.load(Ordering::Relaxed), LIGHT_WORK);
            }

            for handle in heavy_handles {
                handle.join(cx);
            }
        });
    });

    assert_eq!(heavy_progress.load(Ordering::Relaxed), HEAVY_WORK);
}

#[test]
fn sfq_fairness_light_tenants_complete_within_large_heavy_decision_window() {
    const LIGHT_TENANTS: usize = 8;
    const LIGHT_WORK: usize = 95;
    const HEAVY_WORK: usize = 10_000;

    let mut config = sfq_config();
    config.max_worker_queue_len = 4096;
    config.max_global_queue_len = 4096;
    config.scheduler.ready_partitions = NonZeroUsize::new(4).unwrap();
    config.scheduler.selection = QueueSelectionPolicy::Shortest;
    let mut runtime = Runtime::with_config(config);
    let light_progress = Arc::new(
        (0..LIGHT_TENANTS)
            .map(|_| AtomicUsize::new(0))
            .collect::<Vec<_>>(),
    );
    let first_light_sequence = Arc::new(
        (0..LIGHT_TENANTS)
            .map(|_| AtomicUsize::new(usize::MAX))
            .collect::<Vec<_>>(),
    );
    let schedule_sequence = Arc::new(AtomicUsize::new(0));

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let heavy_tenant = cx.new_tenant_id();
            let heavy_handles = (0..HEAVY_WORK)
                .map(|_| {
                    let schedule_sequence = Arc::clone(&schedule_sequence);
                    scope.spawn_stealable_with_tenant(heavy_tenant, move |cx| {
                        schedule_sequence.fetch_add(1, Ordering::Relaxed);
                        cx.yield_now();
                    })
                })
                .collect::<Vec<_>>();

            let mut light_handles = Vec::new();
            for tenant_index in 0..LIGHT_TENANTS {
                let tenant = cx.new_tenant_id();
                for _ in 0..LIGHT_WORK {
                    let light_progress = Arc::clone(&light_progress);
                    let first_light_sequence = Arc::clone(&first_light_sequence);
                    let schedule_sequence = Arc::clone(&schedule_sequence);
                    light_handles.push(scope.spawn_stealable_with_tenant(tenant, move |_| {
                        let sequence = schedule_sequence.fetch_add(1, Ordering::Relaxed);
                        first_light_sequence[tenant_index].fetch_min(sequence, Ordering::Relaxed);
                        light_progress[tenant_index].fetch_add(1, Ordering::Relaxed);
                    }));
                }
            }

            for handle in light_handles {
                handle.join(cx);
            }
            for count in light_progress.iter() {
                assert_eq!(count.load(Ordering::Relaxed), LIGHT_WORK);
            }
            for first_sequence in first_light_sequence.iter() {
                assert!(
                    first_sequence.load(Ordering::Relaxed) < 10_000,
                    "light tenant missed the first 10,000 scheduling-decision window"
                );
            }

            for handle in heavy_handles {
                handle.join(cx);
            }
        });
    });

    let metrics = runtime.metrics();
    assert!(metrics.sfq_queue_polls >= 10_000);
}

#[test]
fn sfq_fairness_many_tenants_complete_mixed_cpu_and_parking_work() {
    const TENANTS: usize = 32;
    let mut runtime = Runtime::with_config(sfq_config());

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let mut handles = Vec::new();
            for tenant_index in 0..TENANTS {
                let tenant = cx.new_tenant_id();
                handles.push(scope.spawn_stealable_with_tenant(tenant, move |cx| {
                    if tenant_index % 2 == 0 {
                        for _ in 0..8 {
                            std::hint::spin_loop();
                        }
                    } else {
                        cx.sleep(Duration::from_millis(0)).unwrap();
                    }
                    tenant_index
                }));
            }

            let mut seen = handles
                .into_iter()
                .map(|handle| handle.join(cx))
                .collect::<Vec<_>>();
            seen.sort_unstable();
            assert_eq!(seen, (0..TENANTS).collect::<Vec<_>>());
        });
    });
}

#[test]
fn sfq_fairness_tenant_attribution_survives_parking_and_timer_readiness() {
    let mut runtime = Runtime::with_config(sfq_config());

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let tenant = cx.new_tenant_id();
            let (tx, rx) = cross_thread::stackful(1);
            let parked = scope.spawn_stealable_with_tenant(tenant, move |cx| {
                assert_eq!(cx.tenant_id(), tenant);
                rx.recv_with(cx).unwrap();
                assert_eq!(cx.tenant_id(), tenant);
                cx.sleep(Duration::from_millis(0)).unwrap();
                assert_eq!(cx.tenant_id(), tenant);
                tenant
            });

            tx.send_with(cx, ()).unwrap();
            assert_eq!(parked.join(cx), tenant);
        });
    });
}

#[test]
fn sfq_fairness_tenant_attribution_survives_yield_socket_and_shared_ring_readiness() {
    let mut runtime = Runtime::with_config(sfq_config());
    let (left, right) = socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )
    .unwrap();

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let tenant = cx.new_tenant_id();
            let task = scope.spawn_stealable_with_tenant(tenant, move |cx| {
                assert_eq!(cx.tenant_id(), tenant);
                cx.yield_now();
                assert_eq!(cx.tenant_id(), tenant);

                cx.send(&left, b"x").unwrap();
                let mut buf = [0_u8; 1];
                assert_eq!(cx.recv(&right, &mut buf).unwrap(), 1);
                assert_eq!(&buf, b"x");
                assert_eq!(cx.tenant_id(), tenant);

                let ring = cx.create_shared_ring();
                ring.nop(cx).unwrap();
                assert_eq!(cx.tenant_id(), tenant);
                tenant
            });

            assert_eq!(task.join(cx), tenant);
        });
    });
}
