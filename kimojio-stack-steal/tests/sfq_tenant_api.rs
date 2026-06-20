use std::num::NonZeroUsize;

use kimojio_stack_steal::{
    QueueSelectionPolicy, Runtime, RuntimeConfig, SchedulerConfig, SchedulerMode, StealPolicy,
    TenantReassignmentPolicy,
};

#[test]
fn tenant_api_reports_current_and_allocates_fresh_identifiers() {
    let mut runtime = Runtime::new();

    runtime.block_on(|cx| {
        let root = cx.tenant_id();
        let first = cx.new_tenant_id();
        let second = cx.new_tenant_id();

        assert_ne!(root, first);
        assert_ne!(first, second);
        assert_eq!(first.get() + 1, second.get());
    });
}

#[test]
fn stealable_spawn_defaults_to_distinct_tenants_and_groups_explicit_tenant() {
    let mut runtime = Runtime::with_config(RuntimeConfig {
        workers: NonZeroUsize::new(2).unwrap(),
        steal_policy: StealPolicy::steal_one(),
        ..RuntimeConfig::default()
    });

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let first = scope.spawn_stealable(|cx| cx.tenant_id()).join(cx);
            let second = scope.spawn_stealable(|cx| cx.tenant_id()).join(cx);
            assert_ne!(first, second);

            let tenant = cx.new_tenant_id();
            let left = scope
                .spawn_stealable_with_tenant(tenant, |cx| cx.tenant_id())
                .join(cx);
            let right = scope
                .spawn_stealable_with_tenant(tenant, |cx| cx.tenant_id())
                .join(cx);
            assert_eq!(left, tenant);
            assert_eq!(right, tenant);
        });
    });

    let metrics = runtime.metrics();
    assert!(metrics.allocated_tenants >= 4);
    assert_eq!(metrics.last_tenant.map(|tenant| tenant.get()), Some(4));
}

#[test]
fn local_and_pinned_work_can_propagate_tenant_to_child_stealable_work() {
    let mut runtime = Runtime::with_config(RuntimeConfig {
        workers: NonZeroUsize::new(2).unwrap(),
        steal_policy: StealPolicy::steal_one(),
        ..RuntimeConfig::default()
    });

    runtime.block_on(|cx| {
        let tenant = cx.new_tenant_id();
        cx.scope(|scope| {
            let propagated_from_local = scope
                .spawn_local_with_tenant(tenant, |cx| {
                    cx.scope(|scope| {
                        scope
                            .spawn_stealable_with_tenant(cx.tenant_id(), |cx| cx.tenant_id())
                            .join(cx)
                    })
                })
                .join(cx);
            let propagated_from_pinned = scope
                .spawn_pinned_with_tenant(tenant, |cx| {
                    cx.scope(|scope| {
                        scope
                            .spawn_stealable_with_tenant(cx.tenant_id(), |cx| cx.tenant_id())
                            .join(cx)
                    })
                })
                .join(cx);

            assert_eq!(propagated_from_local, tenant);
            assert_eq!(propagated_from_pinned, tenant);
        });
    });
}

#[test]
fn custom_stack_size_spawn_preserves_tenant_metadata_for_local_work() {
    let mut runtime = Runtime::new();

    runtime.block_on(|cx| {
        let tenant = cx.new_tenant_id();
        cx.scope(|scope| {
            let observed = scope
                .spawn_with_stack_size_and_tenant(64 * 1024, tenant, |cx| cx.tenant_id())
                .join(cx);
            assert_eq!(observed, tenant);
        });
    });
}

#[test]
fn scheduler_config_normalizes_subset_sizes_to_ready_partitions() {
    let config = RuntimeConfig {
        workers: NonZeroUsize::new(4).unwrap(),
        scheduler: SchedulerConfig {
            mode: SchedulerMode::StochasticFair,
            ready_partitions: NonZeroUsize::new(4).unwrap(),
            selection: QueueSelectionPolicy::RandomSubsetShortest {
                subset: NonZeroUsize::new(64).unwrap(),
            },
            reassignment: TenantReassignmentPolicy::EverySchedulingDecisions(
                NonZeroUsize::new(1024).unwrap(),
            ),
        },
        ..RuntimeConfig::default()
    };

    let runtime = Runtime::with_config(config);
    assert_eq!(
        runtime.config().scheduler.selection,
        QueueSelectionPolicy::RandomSubsetShortest {
            subset: NonZeroUsize::new(4).unwrap()
        }
    );
}
