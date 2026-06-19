// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::{num::NonZeroUsize, sync::Arc};

use kimojio_stack_grpc::GrpcRuntime;
use kimojio_stack_steal::{
    Runtime as StealRuntime, RuntimeConfig as StealRuntimeConfig, StealPolicy,
};

use super::{
    admin::AdminStatus,
    stackful::{StackfulGatewayConfig, StackfulGatewayState},
};

/// gRPC runtime marker for stack-steal gateway handlers.
pub struct StackStealGrpcRuntime;

impl GrpcRuntime for StackStealGrpcRuntime {
    type Context<'cx> = kimojio_stack_steal::RuntimeContext<'cx>;
}

/// gRPC runtime marker for single-runtime stack gateway handlers.
pub struct StackGrpcRuntime;

impl GrpcRuntime for StackGrpcRuntime {
    type Context<'cx> = kimojio_stack::RuntimeContext<'cx>;
}

/// Configuration for the stack-steal object gateway mode.
#[derive(Clone, Debug)]
pub struct StackStealGatewayConfig {
    pub workers: NonZeroUsize,
    pub gateway: StackfulGatewayConfig,
    pub stack_size: usize,
}

impl StackStealGatewayConfig {
    pub fn runtime_config(&self) -> StealRuntimeConfig {
        StealRuntimeConfig {
            workers: self.workers,
            stack_size: self.stack_size,
            steal_policy: StealPolicy::steal_one(),
            ..StealRuntimeConfig::default()
        }
    }
}

/// Stack-steal object gateway container using the shared stackful handlers.
#[derive(Debug)]
pub struct StackStealGateway {
    config: StackStealGatewayConfig,
    state: Arc<StackfulGatewayState>,
}

impl StackStealGateway {
    pub fn new(config: StackStealGatewayConfig) -> Self {
        let state = StackfulGatewayState::new(config.gateway.clone());
        Self { config, state }
    }

    pub fn runtime_config(&self) -> StealRuntimeConfig {
        self.config.runtime_config()
    }

    pub fn state(&self) -> Arc<StackfulGatewayState> {
        self.state.clone()
    }

    pub fn admin_status(&self) -> AdminStatus {
        self.state.admin_status()
    }
}

/// Starts a minimal stack-steal runtime and returns its shared health model.
pub fn stack_steal_startup_status(config: StackStealGatewayConfig) -> AdminStatus {
    let gateway = StackStealGateway::new(config);
    let mut runtime = StealRuntime::with_config(gateway.runtime_config());
    runtime.block_on(|_cx| gateway.admin_status())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_gateway_stack_steal_runtime_config_uses_requested_workers() {
        let workers = NonZeroUsize::new(2).unwrap();
        let config = StackStealGatewayConfig {
            workers,
            gateway: StackfulGatewayConfig::default(),
            stack_size: StealRuntimeConfig::default().stack_size,
        };

        assert_eq!(config.runtime_config().workers, workers);
        assert_eq!(
            config.runtime_config().stack_size,
            StealRuntimeConfig::default().stack_size
        );
    }

    #[test]
    fn object_gateway_stack_steal_startup_reports_ready_health() {
        let status = stack_steal_startup_status(StackStealGatewayConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            gateway: StackfulGatewayConfig::default(),
            stack_size: StealRuntimeConfig::default().stack_size,
        });

        assert!(status.is_ready());
    }
}
