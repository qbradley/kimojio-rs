// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::{net::SocketAddr, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, ValueEnum};
use examples::object_gateway::{
    model::{NamespaceMapping, ObjectSizeLimits, StorageBackendMode, TelemetrySinkMode},
    tokio_impl::{
        TokioGatewayConfig, TokioGatewayState, TokioStorageClient, TokioTelemetryClient,
        serve_admin_listener, serve_grpc_listener, tokio_admin_get, tokio_grpc_health,
    },
};
use tokio::{net::TcpListener, sync::oneshot};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum StorageMode {
    Hermetic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum TelemetryMode {
    Hermetic,
}

#[derive(Debug, Parser)]
#[command(version, about = "Tokio Object Gateway comparison host")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:9200")]
    grpc_addr: SocketAddr,
    #[arg(long, default_value = "127.0.0.1:9201")]
    admin_addr: SocketAddr,
    #[arg(long, default_value_t = 4)]
    workers: usize,
    #[arg(long, value_enum, default_value_t = StorageMode::Hermetic)]
    storage: StorageMode,
    #[arg(long, value_enum, default_value_t = TelemetryMode::Hermetic)]
    telemetry: TelemetryMode,
    #[arg(long, default_value_t = 4 * 1024 * 1024)]
    max_object_bytes: u64,
    #[arg(long, default_value_t = 64 * 1024)]
    max_chunk_bytes: usize,
    #[arg(long, default_value_t = 30_000)]
    request_deadline_ms: u64,
    #[arg(long)]
    shutdown_after_ready: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    validate_args(&args)?;
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(args.workers)
        .enable_all()
        .build()
        .context("failed to build Tokio host runtime")?
        .block_on(run(args))
}

async fn run(args: Args) -> Result<()> {
    let config = TokioGatewayConfig {
        size_limits: ObjectSizeLimits::new(args.max_object_bytes, args.max_chunk_bytes),
        namespace_mapping: NamespaceMapping::HermeticContainerPerNamespace {
            account: "devstore".to_owned(),
        },
        version: env!("CARGO_PKG_VERSION").to_owned(),
        request_timeout: Some(Duration::from_millis(args.request_deadline_ms)),
        storage_client: TokioStorageClient::Hermetic,
        storage_backend: StorageBackendMode::Hermetic,
        telemetry_client: TokioTelemetryClient::Hermetic,
        telemetry_sink: TelemetrySinkMode::Hermetic,
    };
    let state = TokioGatewayState::new(config);
    let grpc_listener = TcpListener::bind(args.grpc_addr)
        .await
        .context("failed to bind Tokio gRPC listener")?;
    let admin_listener = TcpListener::bind(args.admin_addr)
        .await
        .context("failed to bind Tokio admin listener")?;
    let grpc_addr = grpc_listener
        .local_addr()
        .context("failed to read gRPC addr")?;
    let admin_addr = admin_listener
        .local_addr()
        .context("failed to read admin addr")?;
    print_startup(&args, state.as_ref(), grpc_addr, admin_addr);

    let (grpc_shutdown_tx, grpc_shutdown_rx) = oneshot::channel();
    let (admin_shutdown_tx, admin_shutdown_rx) = oneshot::channel();
    let grpc = tokio::spawn(serve_grpc_listener(
        grpc_listener,
        state.clone(),
        grpc_shutdown_rx,
    ));
    let admin = tokio::spawn(serve_admin_listener(
        admin_listener,
        state.clone(),
        admin_shutdown_rx,
    ));

    if args.shutdown_after_ready {
        run_self_check(state, grpc_addr, admin_addr).await?;
        let _ = grpc_shutdown_tx.send(());
        let _ = admin_shutdown_tx.send(());
        grpc.await.context("Tokio gRPC task panicked")??;
        admin.await.context("Tokio admin task panicked")??;
    } else {
        tokio::select! {
            result = grpc => result.context("Tokio gRPC task panicked")??,
            result = admin => result.context("Tokio admin task panicked")??,
        }
    }
    Ok(())
}

fn validate_args(args: &Args) -> Result<()> {
    if args.workers == 0 {
        bail!("--workers must be greater than zero");
    }
    if args.max_chunk_bytes == 0 {
        bail!("--max-chunk-bytes must be greater than zero");
    }
    if args.max_object_bytes == 0 {
        bail!("--max-object-bytes must be greater than zero");
    }
    if args.request_deadline_ms == 0 {
        bail!("--request-deadline-ms must be greater than zero");
    }
    Ok(())
}

async fn run_self_check(
    state: std::sync::Arc<TokioGatewayState>,
    grpc_addr: SocketAddr,
    admin_addr: SocketAddr,
) -> Result<()> {
    let health = tokio_grpc_health(grpc_addr).await?;
    ensure!(health.ready, "Tokio gRPC health did not report ready");
    let admin = tokio_admin_get(admin_addr, "/health").await?;
    ensure!(
        admin.contains("ready=true"),
        "Tokio admin health did not report ready: {admin}"
    );
    state.begin_shutdown();
    let status = tokio_admin_get(admin_addr, "/status").await?;
    ensure!(
        status.contains("shutting_down=true"),
        "Tokio admin status did not report shutdown: {status}"
    );
    Ok(())
}

fn print_startup(
    args: &Args,
    state: &TokioGatewayState,
    grpc_addr: SocketAddr,
    admin_addr: SocketAddr,
) {
    let config = state.config();
    println!(
        "object-gateway-tokio-host runtime=tokio runtime_mode=multi-thread grpc_addr={grpc_addr} admin_addr={admin_addr} workers={} storage_client={} storage_backend={} telemetry_client={} telemetry_sink={} ready={} deadline_ms={}",
        args.workers,
        config.storage_client.label(),
        config.storage_backend.as_str(),
        config.telemetry_client.label(),
        config.telemetry_sink.as_str(),
        state.admin_status().ready,
        Duration::from_millis(args.request_deadline_ms).as_millis()
    );
}
