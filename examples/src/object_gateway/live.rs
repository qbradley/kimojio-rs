// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Environment gates for optional live Object Gateway integrations.
//!
//! Default gateway tests and workloads are hermetic. These helpers centralize
//! the opt-in variables that enable live Azure-compatible storage and live
//! OpenTelemetry collector runs so binaries and tests can report consistent skip
//! reasons when external services are not configured.

use std::env;

pub const LIVE_STORAGE_ENV: &str = "KIMOJIO_OBJECT_GATEWAY_LIVE_STORAGE";
pub const LIVE_STORAGE_ENDPOINT_ENV: &str = "KIMOJIO_OBJECT_GATEWAY_STORAGE_ENDPOINT";
pub const LIVE_STORAGE_CONTAINER_ENV: &str = "KIMOJIO_OBJECT_GATEWAY_STORAGE_CONTAINER";
pub const LIVE_TELEMETRY_ENV: &str = "KIMOJIO_OBJECT_GATEWAY_LIVE_OTEL";
pub const LIVE_TELEMETRY_ENDPOINT_ENV: &str = "KIMOJIO_OBJECT_GATEWAY_OTEL_ENDPOINT";

pub fn live_storage_skip_reason() -> Option<String> {
    live_storage_skip_reason_from(|name| env::var(name).ok())
}

pub fn live_storage_skip_reason_from(
    mut get_var: impl FnMut(&str) -> Option<String>,
) -> Option<String> {
    if get_var(LIVE_STORAGE_ENV).as_deref() != Some("1") {
        return Some(format!(
            "set {LIVE_STORAGE_ENV}=1, {LIVE_STORAGE_ENDPOINT_ENV}, and {LIVE_STORAGE_CONTAINER_ENV} to enable live Azure-compatible storage"
        ));
    }
    let missing = [LIVE_STORAGE_ENDPOINT_ENV, LIVE_STORAGE_CONTAINER_ENV]
        .into_iter()
        .filter(|name| get_var(name).map(|value| value.is_empty()).unwrap_or(true))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        None
    } else {
        Some(format!(
            "missing live storage configuration: {}",
            missing.join(",")
        ))
    }
}

pub fn live_telemetry_skip_reason() -> Option<String> {
    live_telemetry_skip_reason_from(|name| env::var(name).ok())
}

pub fn live_telemetry_skip_reason_from(
    mut get_var: impl FnMut(&str) -> Option<String>,
) -> Option<String> {
    if get_var(LIVE_TELEMETRY_ENV).as_deref() != Some("1") {
        return Some(format!(
            "set {LIVE_TELEMETRY_ENV}=1 and {LIVE_TELEMETRY_ENDPOINT_ENV} to enable live OpenTelemetry export"
        ));
    }
    if get_var(LIVE_TELEMETRY_ENDPOINT_ENV)
        .map(|value| value.is_empty())
        .unwrap_or(true)
    {
        Some(format!(
            "missing live telemetry configuration: {LIVE_TELEMETRY_ENDPOINT_ENV}"
        ))
    } else {
        None
    }
}
