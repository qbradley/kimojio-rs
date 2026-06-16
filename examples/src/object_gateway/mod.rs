// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared contract and hermetic fixtures for the Stackful Object Gateway example.
//!
//! This module is intentionally runtime-neutral. Later host phases adapt these
//! types to stackful, stack-steal, Tokio, and Go services while keeping the
//! object-visible contract, storage/error mapping, telemetry assertions, and
//! workload labels comparable.

pub mod admin;
pub mod conformance;
pub mod launch;
pub mod live;
pub mod model;
pub mod proto;
pub mod stack_steal;
pub mod stackful;
pub mod storage;
pub mod telemetry;
pub mod tokio_impl;
pub mod workload;
