// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::net::SocketAddr;

use anyhow::{Result, ensure};
use clap::{Parser, ValueEnum};
use examples::object_gateway::{
    conformance::{
        DEFAULT_SCENARIOS, format_reports, run_default_implementations, run_implementation,
        run_implementation_scenarios, run_target_implementation,
        run_target_implementation_scenarios, run_tokio_implementation,
        run_tokio_implementation_scenarios,
    },
    launch::LocalRuntime,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum Implementation {
    All,
    Stack,
    StackSteal,
    Tokio,
}

#[derive(Debug, Parser)]
#[command(version, about = "Run hermetic Object Gateway conformance scenarios")]
struct Args {
    #[arg(long, value_enum, default_value_t = Implementation::All)]
    implementation: Implementation,
    #[arg(long)]
    scenario: Option<String>,
    #[arg(long)]
    target_grpc_addr: Option<SocketAddr>,
    #[arg(long)]
    target_admin_addr: Option<SocketAddr>,
    #[arg(long, default_value = "target")]
    target_label: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let selected = args.scenario.as_deref().map(select_scenario);
    ensure!(
        args.target_grpc_addr.is_some() == args.target_admin_addr.is_some(),
        "--target-grpc-addr and --target-admin-addr must be provided together"
    );
    let reports = if let (Some(grpc_addr), Some(admin_addr)) =
        (args.target_grpc_addr, args.target_admin_addr)
    {
        let report = if let Some(scenario) = selected {
            run_target_implementation_scenarios(args.target_label, grpc_addr, admin_addr, scenario)
        } else {
            run_target_implementation(args.target_label, grpc_addr, admin_addr)
        };
        vec![report]
    } else {
        match args.implementation {
            Implementation::All => selected.map_or_else(run_default_implementations, |scenario| {
                vec![
                    run_implementation_scenarios(LocalRuntime::Stack, scenario),
                    run_implementation_scenarios(LocalRuntime::StackSteal, scenario),
                    run_tokio_implementation_scenarios(scenario),
                ]
            }),
            Implementation::Stack => vec![selected.map_or_else(
                || run_implementation(LocalRuntime::Stack),
                |scenario| run_implementation_scenarios(LocalRuntime::Stack, scenario),
            )],
            Implementation::StackSteal => vec![selected.map_or_else(
                || run_implementation(LocalRuntime::StackSteal),
                |scenario| run_implementation_scenarios(LocalRuntime::StackSteal, scenario),
            )],
            Implementation::Tokio => vec![
                selected.map_or_else(run_tokio_implementation, run_tokio_implementation_scenarios),
            ],
        }
    };
    print!("{}", format_reports(&reports));
    ensure!(
        reports.iter().all(|report| report.passed()),
        "one or more conformance scenarios failed"
    );
    Ok(())
}

fn select_scenario(label: &str) -> &'static [examples::object_gateway::conformance::ScenarioSpec] {
    let scenario = DEFAULT_SCENARIOS
        .iter()
        .find(|scenario| scenario.label == label)
        .unwrap_or_else(|| panic!("unknown scenario label: {label}"));
    std::slice::from_ref(scenario)
}
