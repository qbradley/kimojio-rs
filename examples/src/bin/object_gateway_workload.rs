// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::net::SocketAddr;

use anyhow::{Result, bail, ensure};
use clap::{Parser, ValueEnum};
use examples::object_gateway::{
    conformance::go_conformance_skip_reason,
    launch::{TcpGateway, launch_go_host},
    workload::{
        DEFAULT_WORKLOADS, WorkloadImplementation, WorkloadSummary, WorkloadTargetInfo,
        find_workload, format_summaries, go_workload_host_config, go_workload_target_info,
        run_implementation_profile, run_tcp_profile,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum Implementation {
    All,
    Stack,
    StackSteal,
    StackStealSfq,
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
    #[arg(long)]
    target_grpc_addr: Option<SocketAddr>,
    #[arg(long)]
    target_admin_addr: Option<SocketAddr>,
    #[arg(long, default_value = "target")]
    target_label: String,
    #[arg(long, default_value = "tcp")]
    target_runtime_mode: String,
    #[arg(long, default_value = "kimojio-stack-storage")]
    target_storage_client: String,
    #[arg(long, default_value = "hermetic")]
    target_backend_mode: String,
    #[arg(long, default_value = "kimojio-stack-opentelemetry-hermetic")]
    target_telemetry_sink_mode: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let profiles = selected_profiles(&args.workload)?;
    let mut summaries = Vec::new();
    let mut skip_lines = Vec::new();
    ensure!(
        args.target_grpc_addr.is_some() == args.target_admin_addr.is_some(),
        "--target-grpc-addr and --target-admin-addr must be provided together"
    );
    if let (Some(grpc_addr), Some(admin_addr)) = (args.target_grpc_addr, args.target_admin_addr) {
        run_target(&args, grpc_addr, admin_addr, &profiles, &mut summaries)?;
        let output = format_summaries(&summaries);
        if !output.is_empty() {
            println!("{output}");
        }
        return Ok(());
    }
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
        Implementation::StackStealSfq => {
            run_rust_implementation(
                WorkloadImplementation::StackStealSfq,
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

fn run_target(
    args: &Args,
    grpc_addr: SocketAddr,
    admin_addr: SocketAddr,
    profiles: &[examples::object_gateway::workload::WorkloadProfile],
    summaries: &mut Vec<WorkloadSummary>,
) -> Result<()> {
    let info = WorkloadTargetInfo {
        implementation: &args.target_label,
        runtime_mode: &args.target_runtime_mode,
        storage_client: &args.target_storage_client,
        backend_mode: &args.target_backend_mode,
        telemetry_sink_mode: &args.target_telemetry_sink_mode,
    };
    for profile in profiles {
        summaries.push(run_tcp_profile(
            info,
            TcpGateway::new(grpc_addr, admin_addr),
            *profile,
        )?);
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
