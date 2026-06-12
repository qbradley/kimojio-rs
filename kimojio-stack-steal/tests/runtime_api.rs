// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::num::NonZeroUsize;
use std::time::Duration;

use kimojio_stack::{
    IoRuntime, Runtime as StackCoreRuntime, RuntimeCapabilities, RuntimeCapability,
    StackRuntime as StackRuntimeTrait, StackRuntimeContext,
};
use kimojio_stack_steal::{Runtime as StealRuntime, RuntimeConfig, StealPolicy};

fn runtime_agnostic_component<R>(runtime: &mut R, explicit_ring_io_supported: bool)
where
    R: StackRuntimeTrait,
{
    runtime.block_on(|cx| {
        assert!(cx.supports(RuntimeCapability::StackfulWait));
        cx.yield_now();
        assert_eq!(
            cx.spawn_scoped(|child| {
                child.yield_now();
                child.sleep_for(Duration::from_millis(0)).unwrap();
                42
            }),
            42
        );
        cx.sleep_for(Duration::from_millis(0)).unwrap();

        let ring_capability = cx.require_explicit_ring_io();
        assert_eq!(ring_capability.is_ok(), explicit_ring_io_supported);
        if !explicit_ring_io_supported {
            let error = ring_capability.unwrap_err();
            assert_eq!(error.capability(), "explicit-ring-io");
        }
    });
}

#[test]
fn runtime_agnostic_component_runs_on_stack_runtime() {
    let mut runtime = StackCoreRuntime::new();

    runtime_agnostic_component(&mut runtime, false);
}

#[test]
fn runtime_agnostic_component_runs_on_stealing_runtime() {
    let mut runtime = StealRuntime::with_config(RuntimeConfig {
        workers: NonZeroUsize::new(2).unwrap(),
        steal_policy: StealPolicy::steal_one(),
        ..RuntimeConfig::default()
    });

    runtime_agnostic_component(&mut runtime, true);
}
