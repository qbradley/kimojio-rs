use std::num::NonZeroUsize;
use std::panic::{self, AssertUnwindSafe};

use kimojio_stack::channel::cross_thread;
use kimojio_stack_steal::{
    QueueSelectionPolicy, Runtime, RuntimeConfig, SchedulerConfig, SchedulerMode, StealPolicy,
    TenantReassignmentPolicy,
};

fn sfq_config() -> RuntimeConfig {
    RuntimeConfig {
        workers: NonZeroUsize::new(4).unwrap(),
        steal_policy: StealPolicy::Disabled,
        scheduler: SchedulerConfig::stochastic_fair(NonZeroUsize::new(8).unwrap()),
        ..RuntimeConfig::default()
    }
}

#[test]
fn sfq_scheduler_completes_stealable_work_and_records_metrics() {
    let mut runtime = Runtime::with_config(sfq_config());

    let sum = runtime.block_on(|cx| {
        cx.scope(|scope| {
            let tenant = cx.new_tenant_id();
            let handles: [_; 32] = std::array::from_fn(|index| {
                if index % 2 == 0 {
                    scope.spawn_stealable_with_tenant(tenant, move |cx| {
                        assert_eq!(cx.tenant_id(), tenant);
                        1_usize
                    })
                } else {
                    scope.spawn_stealable(move |cx| {
                        assert_ne!(cx.tenant_id(), tenant);
                        1_usize
                    })
                }
            });
            handles
                .into_iter()
                .map(|handle| handle.join(cx))
                .sum::<usize>()
        })
    });

    assert_eq!(sum, 32);
    let metrics = runtime.metrics();
    assert_eq!(metrics.scheduler.mode, SchedulerMode::StochasticFair);
    assert_eq!(metrics.completed_tasks, 32);
    assert!(metrics.max_sfq_partition_depth != 0);
    assert!(metrics.sfq_queue_polls != 0);
    assert!(metrics.last_tenant.is_some());
}

#[test]
fn sfq_scheduler_reassignment_epoch_advances() {
    let mut config = sfq_config();
    config.scheduler.reassignment =
        TenantReassignmentPolicy::EverySchedulingDecisions(NonZeroUsize::new(2).unwrap());
    let mut runtime = Runtime::with_config(config);

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let handles: [_; 16] =
                std::array::from_fn(|_| scope.spawn_stealable(|cx| cx.tenant_id().get()));
            for handle in handles {
                assert!(handle.join(cx) != 0);
            }
        });
    });

    assert!(runtime.metrics().sfq_epoch > 0);
}

#[test]
fn sfq_scheduler_honors_selection_policy_configuration() {
    let mut config = sfq_config();
    config.scheduler.selection = QueueSelectionPolicy::MovementCost {
        subset: NonZeroUsize::new(64).unwrap(),
        movement_penalty: 3,
    };
    let mut runtime = Runtime::with_config(config);

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let handle = scope.spawn_stealable(|_| 7_usize);
            assert_eq!(handle.join(cx), 7);
        });
    });

    assert_eq!(
        runtime.metrics().scheduler.selection,
        QueueSelectionPolicy::MovementCost {
            subset: NonZeroUsize::new(8).unwrap(),
            movement_penalty: 3,
        }
    );
}

#[test]
fn sfq_scheduler_all_selection_policies_complete_work() {
    let policies = [
        QueueSelectionPolicy::SingleChoice,
        QueueSelectionPolicy::RandomSubsetShortest {
            subset: NonZeroUsize::new(2).unwrap(),
        },
        QueueSelectionPolicy::Shortest,
        QueueSelectionPolicy::MovementCost {
            subset: NonZeroUsize::new(4).unwrap(),
            movement_penalty: 2,
        },
    ];

    for selection in policies {
        let mut config = sfq_config();
        config.scheduler.selection = selection;
        let mut runtime = Runtime::with_config(config);
        let sum = runtime.block_on(|cx| {
            cx.scope(|scope| {
                let handles: [_; 16] = std::array::from_fn(|_| scope.spawn_stealable(|_| 1_usize));
                handles
                    .into_iter()
                    .map(|handle| handle.join(cx))
                    .sum::<usize>()
            })
        });
        assert_eq!(sum, 16);
        assert!(runtime.metrics().sfq_queue_polls != 0);
    }
}

#[test]
fn sfq_scheduler_light_tenant_completes_with_hot_tenant_present() {
    let mut runtime = Runtime::with_config(sfq_config());

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let hot = cx.new_tenant_id();
            let hot_handles: [_; 64] = std::array::from_fn(|_| {
                scope.spawn_stealable_with_tenant(hot, |cx| {
                    for _ in 0..8 {
                        cx.yield_now();
                    }
                    1_usize
                })
            });
            let light = scope.spawn_stealable(|cx| {
                cx.yield_now();
                99_usize
            });

            assert_eq!(light.join(cx), 99);
            assert_eq!(
                hot_handles
                    .into_iter()
                    .map(|handle| handle.join(cx))
                    .sum::<usize>(),
                64
            );
        });
    });
}

#[test]
fn sfq_scheduler_parent_panic_cancels_blocked_tenant_work() {
    let mut runtime = Runtime::with_config(sfq_config());
    let (_release_tx, release_rx) = cross_thread::stackful::<()>(1);

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let tenant = cx.new_tenant_id();
                let _blocked = scope.spawn_stealable_with_tenant(tenant, move |cx| {
                    let _ = release_rx.recv_with(cx);
                });
                panic!("cancel sfq child");
            });
        });
    }));

    assert!(result.is_err());
}

#[test]
fn sfq_scheduler_queue_capacity_rejection_reaches_joiner() {
    let mut config = sfq_config();
    config.workers = NonZeroUsize::new(2).unwrap();
    config.max_worker_queue_len = 0;
    config.max_global_queue_len = 1;
    let mut runtime = Runtime::with_config(config);

    let (release_tx, release_rx) = cross_thread::stackful(1);
    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let blocked = scope.spawn_stealable(move |cx| {
                release_rx.recv_with(cx).unwrap();
                1_usize
            });
            let rejected = scope.spawn_stealable(|_| 2_usize);
            let rejected_panic = panic::catch_unwind(AssertUnwindSafe(|| rejected.join(cx)));
            assert!(rejected_panic.is_err());
            release_tx.send_with(cx, ()).unwrap();
            assert_eq!(blocked.join(cx), 1);
        });
    });

    assert!(runtime.metrics().rejected_tasks != 0);
}

#[test]
fn sfq_scheduler_high_partition_same_tenant_backlog_uses_global_active_capacity() {
    let mut config = sfq_config();
    config.workers = NonZeroUsize::new(1).unwrap();
    config.scheduler = SchedulerConfig::stochastic_fair(NonZeroUsize::new(64).unwrap());
    config.max_worker_queue_len = 8;
    config.max_global_queue_len = 120;
    let mut runtime = Runtime::with_config(config);

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let tenant = cx.new_tenant_id();
            let mut releases = Vec::new();
            let handles = (0..32)
                .map(|_| {
                    let (release_tx, release_rx) = cross_thread::stackful(1);
                    releases.push(release_tx);
                    scope.spawn_stealable_with_tenant(tenant, move |cx| {
                        release_rx.recv_with(cx).unwrap();
                        1_usize
                    })
                })
                .collect::<Vec<_>>();

            for release in releases {
                release.send_with(cx, ()).unwrap();
            }

            assert_eq!(
                handles
                    .into_iter()
                    .map(|handle| handle.join(cx))
                    .sum::<usize>(),
                32
            );
        });
    });

    assert_eq!(runtime.metrics().rejected_tasks, 0);
}
