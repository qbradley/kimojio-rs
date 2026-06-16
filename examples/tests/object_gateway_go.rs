// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use examples::object_gateway::conformance::{
    GO_CONFORMANCE_ENV, GoConformanceOutcome, go_conformance_skip_reason, go_supported_scenarios,
    run_go_conformance_if_enabled,
};

#[test]
fn object_gateway_go_default_gate_skips_without_env() {
    if std::env::var(GO_CONFORMANCE_ENV).as_deref() == Ok("1") {
        return;
    }
    let reason = go_conformance_skip_reason().expect("go conformance should be gated by default");
    assert!(reason.contains(GO_CONFORMANCE_ENV));
}

#[test]
fn object_gateway_go_conformance() {
    match run_go_conformance_if_enabled().expect("go conformance gate should be evaluable") {
        GoConformanceOutcome::Skipped(reason) => {
            assert!(
                reason.contains(GO_CONFORMANCE_ENV) || reason.contains("go toolchain"),
                "unexpected skip reason: {reason}"
            );
        }
        GoConformanceOutcome::Ran(report) => {
            assert_eq!(
                report.results.len(),
                go_supported_scenarios().len(),
                "go protocol matrix coverage changed unexpectedly"
            );
            assert!(
                report.passed(),
                "go conformance failed: {:#?}",
                report.results
            );
        }
    }
}
