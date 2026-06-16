// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::{Result, bail};
use clap::{Parser, ValueEnum};
use examples::object_gateway::{
    conformance::go_conformance_skip_reason,
    launch::launch_go_host,
    workload::{
        DEFAULT_WORKLOADS, WorkloadImplementation, WorkloadSummary, find_workload,
        format_summaries, go_workload_host_config, go_workload_target_info,
        run_implementation_profile, run_tcp_profile,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum Implementation {
    All,
    Stack,
    StackSteal,
    Tokio,
    Go,
}

#[derive(Debug, Parser)]
#[command(version, about = "Run normalized Object Gateway workloads")]
struct Args {
    #[arg(long, value_enum, default_value_t = Implementation::All)]
    implementation: Implementation,
    #[arg(long, default_value = "all")]
    workload: String,
    #[arg(long)]
    include_go: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let profiles = selected_profiles(&args.workload)?;
    let mut summaries = Vec::new();
    let mut skip_lines = Vec::new();
    match args.implementation {
        Implementation::All => {
            for implementation in WorkloadImplementation::ALL {
                run_rust_implementation(implementation, &profiles, &mut summaries)?;
            }
            if args.include_go {
                run_go(&profiles, &mut summaries, &mut skip_lines)?;
            }
        }
        Implementation::Stack => {
            run_rust_implementation(WorkloadImplementation::Stack, &profiles, &mut summaries)?;
        }
        Implementation::StackSteal => {
            run_rust_implementation(
                WorkloadImplementation::StackSteal,
                &profiles,
                &mut summaries,
            )?;
        }
        Implementation::Tokio => {
            run_rust_implementation(WorkloadImplementation::Tokio, &profiles, &mut summaries)?;
        }
        Implementation::Go => run_go(&profiles, &mut summaries, &mut skip_lines)?,
    }
    let output = format_summaries(&summaries);
    if !output.is_empty() {
        println!("{output}");
    }
    for line in skip_lines {
        println!("{line}");
    }
    Ok(())
}

fn selected_profiles(
    label: &str,
) -> Result<Vec<examples::object_gateway::workload::WorkloadProfile>> {
    if label == "all" {
        return Ok(DEFAULT_WORKLOADS.to_vec());
    }
    match find_workload(label) {
        Some(profile) => Ok(vec![profile]),
        None => bail!("unknown workload profile: {label}"),
    }
}

fn run_rust_implementation(
    implementation: WorkloadImplementation,
    profiles: &[examples::object_gateway::workload::WorkloadProfile],
    summaries: &mut Vec<WorkloadSummary>,
) -> Result<()> {
    for profile in profiles {
        summaries.push(run_implementation_profile(implementation, *profile)?);
    }
    Ok(())
}

fn run_go(
    profiles: &[examples::object_gateway::workload::WorkloadProfile],
    summaries: &mut Vec<WorkloadSummary>,
    skip_lines: &mut Vec<String>,
) -> Result<()> {
    if let Some(reason) = go_conformance_skip_reason() {
        skip_lines.push(format!(
            "object-gateway-workload implementation=go skipped=true reason={:?}",
            reason
        ));
        return Ok(());
    }
    for profile in profiles {
        let host = launch_go_host(go_workload_host_config(*profile))?;
        summaries.push(run_tcp_profile(
            go_workload_target_info(),
            host.gateway(),
            *profile,
        )?);
    }
    Ok(())
}
