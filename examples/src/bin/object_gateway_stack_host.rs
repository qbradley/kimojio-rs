// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::{net::SocketAddr, num::NonZeroUsize, os::fd::OwnedFd, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, ValueEnum};
use examples::object_gateway::{
    model::{NamespaceMapping, ObjectSizeLimits, StorageBackendMode, TelemetrySinkMode},
    proto,
    stack_steal::{
        StackGrpcRuntime, StackStealGateway, StackStealGatewayConfig, StackStealGrpcRuntime,
    },
    stackful::{
        StackfulGatewayClient, StackfulGatewayConfig, StackfulGatewayState, serve_admin_once,
        serve_grpc_requests,
    },
};
use http::Request;
use kimojio_stack::{Runtime, RuntimeContext};
use kimojio_stack_grpc::{RuntimeUnaryClient, UnaryResponse};
use kimojio_stack_http::{Body, HttpConfig, RuntimeStackTransport, StackTransport, h2, http1};
use kimojio_stack_steal::{RingFd, Runtime as StealRuntime, RuntimeContext as StealContext};
use rustix::net::{
    self, AddressFamily, SocketType, ipproto,
    sockopt::{set_socket_reuseaddr, set_tcp_nodelay},
};

#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum RuntimeKind {
    Stack,
    Steal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum StorageMode {
    Hermetic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum TelemetryMode {
    Hermetic,
}

#[derive(Debug, Parser)]
#[command(version, about = "Stackful Object Gateway example host")]
struct Args {
    #[arg(long, value_enum, default_value_t = RuntimeKind::Stack)]
    runtime: RuntimeKind,
    #[arg(long, default_value = "127.0.0.1:9100")]
    grpc_addr: SocketAddr,
    #[arg(long, default_value = "127.0.0.1:9101")]
    admin_addr: SocketAddr,
    #[arg(long, default_value_t = 4)]
    workers: usize,
    #[arg(long, default_value_t = 1024)]
    backlog: i32,
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
    #[arg(long)]
    max_grpc_connections: Option<usize>,
    #[arg(long)]
    max_admin_connections: Option<usize>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    validate_args(&args)?;
    let gateway = StackfulGatewayConfig {
        size_limits: ObjectSizeLimits::new(args.max_object_bytes, args.max_chunk_bytes),
        namespace_mapping: NamespaceMapping::HermeticContainerPerNamespace {
            account: "devstore".to_owned(),
        },
        version: env!("CARGO_PKG_VERSION").to_owned(),
        request_timeout: (args.request_deadline_ms != 0)
            .then_some(Duration::from_millis(args.request_deadline_ms)),
    };
    match args.runtime {
        RuntimeKind::Stack => run_stack(args, gateway),
        RuntimeKind::Steal => run_steal(args, gateway),
    }
}

fn validate_args(args: &Args) -> Result<()> {
    if args.workers == 0 {
        bail!("--workers must be greater than zero");
    }
    if args.backlog <= 0 {
        bail!("--backlog must be greater than zero");
    }
    if args.max_chunk_bytes == 0 {
        bail!("--max-chunk-bytes must be greater than zero");
    }
    if args.max_object_bytes == 0 {
        bail!("--max-object-bytes must be greater than zero");
    }
    Ok(())
}

fn run_stack(args: Args, gateway: StackfulGatewayConfig) -> Result<()> {
    let mut runtime = Runtime::new();
    runtime.block_on(|cx| {
        let grpc_listener = create_stack_listener(cx, args.grpc_addr, args.backlog)
            .context("failed to create stack gRPC listener")?;
        let admin_listener = create_stack_listener(cx, args.admin_addr, args.backlog)
            .context("failed to create stack admin listener")?;
        let grpc_addr = listener_addr(&grpc_listener).context("failed to read gRPC listener")?;
        let admin_addr = listener_addr(&admin_listener).context("failed to read admin listener")?;
        let state = StackfulGatewayState::new(gateway);
        print_startup(
            &args,
            "stack",
            grpc_addr,
            admin_addr,
            state.admin_status().ready,
        );

        if args.shutdown_after_ready {
            run_stack_self_check(
                cx,
                grpc_listener,
                admin_listener,
                state,
                grpc_addr,
                admin_addr,
            )
        } else {
            run_stack_service(
                cx,
                grpc_listener,
                admin_listener,
                state,
                args.max_grpc_connections,
                args.max_admin_connections,
            )
        }
    })
}

fn run_steal(args: Args, gateway: StackfulGatewayConfig) -> Result<()> {
    let workers = NonZeroUsize::new(args.workers).context("--workers must be non-zero")?;
    let gateway = StackStealGateway::new(StackStealGatewayConfig { workers, gateway });
    let mut runtime = StealRuntime::with_config(gateway.runtime_config());
    runtime.block_on(|cx| {
        let grpc_listener = create_steal_listener(cx, args.grpc_addr, args.backlog)
            .context("failed to create stack-steal gRPC listener")?;
        let admin_listener = create_steal_listener(cx, args.admin_addr, args.backlog)
            .context("failed to create stack-steal admin listener")?;
        let grpc_addr = listener_addr(&grpc_listener).context("failed to read gRPC listener")?;
        let admin_addr = listener_addr(&admin_listener).context("failed to read admin listener")?;
        print_startup(
            &args,
            "steal",
            grpc_addr,
            admin_addr,
            gateway.admin_status().ready,
        );

        if args.shutdown_after_ready {
            run_steal_self_check(
                cx,
                grpc_listener,
                admin_listener,
                gateway.state(),
                grpc_addr,
                admin_addr,
            )
        } else {
            run_steal_service(
                cx,
                grpc_listener,
                admin_listener,
                gateway.state(),
                args.max_grpc_connections,
                args.max_admin_connections,
            )
        }
    })
}

fn create_stack_listener(
    cx: &RuntimeContext<'_>,
    addr: SocketAddr,
    backlog: i32,
) -> Result<OwnedFd> {
    let listener = cx
        .socket(address_family(addr), SocketType::STREAM, Some(ipproto::TCP))
        .context("socket creation failed")?;
    set_socket_reuseaddr(&listener, true).context("failed to set SO_REUSEADDR")?;
    cx.bind(&listener, &addr).context("bind failed")?;
    cx.listen(&listener, backlog).context("listen failed")?;
    Ok(listener)
}

fn create_steal_listener(cx: &StealContext<'_>, addr: SocketAddr, backlog: i32) -> Result<OwnedFd> {
    let listener = cx
        .socket(address_family(addr), SocketType::STREAM, Some(ipproto::TCP))
        .context("socket creation failed")?;
    set_socket_reuseaddr(&listener, true).context("failed to set SO_REUSEADDR")?;
    cx.bind(&listener, &addr).context("bind failed")?;
    cx.listen(&listener, backlog).context("listen failed")?;
    Ok(listener)
}

fn address_family(addr: SocketAddr) -> AddressFamily {
    if addr.is_ipv4() {
        AddressFamily::INET
    } else {
        AddressFamily::INET6
    }
}

fn listener_addr(listener: &OwnedFd) -> Result<SocketAddr> {
    net::getsockname(listener)?
        .try_into()
        .context("listener address was not an IP socket address")
}

fn run_stack_self_check(
    cx: &RuntimeContext<'_>,
    grpc_listener: OwnedFd,
    admin_listener: OwnedFd,
    state: Arc<StackfulGatewayState>,
    grpc_addr: SocketAddr,
    admin_addr: SocketAddr,
) -> Result<()> {
    cx.scope(|scope| {
        let grpc_state = state.clone();
        let admin_state = state.clone();
        let client_state = state.clone();
        let grpc = scope
            .spawn(move |cx| serve_stack_grpc_listener(cx, grpc_listener, grpc_state, Some(1)));
        let admin = scope
            .spawn(move |cx| serve_stack_admin_listener(cx, admin_listener, admin_state, Some(2)));
        let client =
            scope.spawn(move |cx| stack_self_check(cx, client_state, grpc_addr, admin_addr));

        let client_result = client.join(cx);
        let grpc_result = grpc.join(cx);
        let admin_result = admin.join(cx);
        client_result?;
        grpc_result?;
        admin_result
    })
}

fn run_stack_service(
    cx: &RuntimeContext<'_>,
    grpc_listener: OwnedFd,
    admin_listener: OwnedFd,
    state: Arc<StackfulGatewayState>,
    max_grpc_connections: Option<usize>,
    max_admin_connections: Option<usize>,
) -> Result<()> {
    cx.scope(|scope| {
        let grpc_state = state.clone();
        let grpc = scope.spawn(move |cx| {
            serve_stack_grpc_listener(cx, grpc_listener, grpc_state, max_grpc_connections)
        });
        let admin = scope.spawn(move |cx| {
            serve_stack_admin_listener(cx, admin_listener, state, max_admin_connections)
        });

        let grpc_result = grpc.join(cx);
        let admin_result = admin.join(cx);
        grpc_result?;
        admin_result
    })
}

fn serve_stack_grpc_listener(
    cx: &RuntimeContext<'_>,
    listener: OwnedFd,
    state: Arc<StackfulGatewayState>,
    max_connections: Option<usize>,
) -> Result<()> {
    cx.scope(|scope| {
        let mut accepted = 0_usize;
        let mut handlers = Vec::new();
        loop {
            if max_connections.is_some_and(|max| accepted >= max) {
                break;
            }
            let connection = cx.accept(&listener).context("gRPC accept failed")?;
            set_tcp_nodelay(&connection, true).context("failed to set TCP_NODELAY")?;
            accepted += 1;
            let state = state.clone();
            let handler = scope.spawn(move |cx| {
                let transport = StackTransport::plaintext(connection);
                serve_grpc_requests::<StackGrpcRuntime, _>(cx, transport, state, usize::MAX)
                    .context("gRPC connection failed")
            });
            if max_connections.is_some() {
                handlers.push(handler);
            }
        }
        for handler in handlers {
            handler.join(cx)?;
        }
        Ok::<_, anyhow::Error>(())
    })?;
    cx.close(listener).context("listener close failed")
}

fn serve_stack_admin_listener(
    cx: &RuntimeContext<'_>,
    listener: OwnedFd,
    state: Arc<StackfulGatewayState>,
    max_connections: Option<usize>,
) -> Result<()> {
    cx.scope(|scope| {
        let mut accepted = 0_usize;
        let mut handlers = Vec::new();
        loop {
            if max_connections.is_some_and(|max| accepted >= max) {
                break;
            }
            let connection = cx.accept(&listener).context("admin accept failed")?;
            accepted += 1;
            let state = state.clone();
            let handler = scope.spawn(move |cx| {
                serve_admin_once(cx, StackTransport::plaintext(connection), state)
                    .context("admin connection failed")
            });
            if max_connections.is_some() {
                handlers.push(handler);
            }
        }
        for handler in handlers {
            handler.join(cx)?;
        }
        Ok::<_, anyhow::Error>(())
    })?;
    cx.close(listener).context("listener close failed")
}

fn run_steal_self_check(
    cx: &StealContext<'_>,
    grpc_listener: OwnedFd,
    admin_listener: OwnedFd,
    state: Arc<StackfulGatewayState>,
    grpc_addr: SocketAddr,
    admin_addr: SocketAddr,
) -> Result<()> {
    cx.scope(|scope| {
        let grpc_state = state.clone();
        let admin_state = state.clone();
        let client_state = state.clone();
        let grpc = scope.spawn_local(move |cx| {
            serve_steal_grpc_listener(cx, grpc_listener, grpc_state, Some(1))
        });
        let admin = scope.spawn_local(move |cx| {
            serve_steal_admin_listener(cx, admin_listener, admin_state, Some(2))
        });
        let client =
            scope.spawn_local(move |cx| steal_self_check(cx, client_state, grpc_addr, admin_addr));

        let client_result = client.join(cx);
        let grpc_result = grpc.join(cx);
        let admin_result = admin.join(cx);
        client_result?;
        grpc_result?;
        admin_result
    })
}

fn run_steal_service(
    cx: &StealContext<'_>,
    grpc_listener: OwnedFd,
    admin_listener: OwnedFd,
    state: Arc<StackfulGatewayState>,
    max_grpc_connections: Option<usize>,
    max_admin_connections: Option<usize>,
) -> Result<()> {
    cx.scope(|scope| {
        let grpc_state = state.clone();
        let grpc = scope.spawn_local(move |cx| {
            serve_steal_grpc_listener(cx, grpc_listener, grpc_state, max_grpc_connections)
        });
        let admin = scope.spawn_local(move |cx| {
            serve_steal_admin_listener(cx, admin_listener, state, max_admin_connections)
        });

        let grpc_result = grpc.join(cx);
        let admin_result = admin.join(cx);
        grpc_result?;
        admin_result
    })
}

fn serve_steal_grpc_listener(
    cx: &StealContext<'_>,
    listener: OwnedFd,
    state: Arc<StackfulGatewayState>,
    max_connections: Option<usize>,
) -> Result<()> {
    cx.scope(|scope| {
        let mut accepted = 0_usize;
        let mut handlers = Vec::new();
        loop {
            if max_connections.is_some_and(|max| accepted >= max) {
                break;
            }
            let connection = cx.accept(&listener).context("gRPC accept failed")?;
            set_tcp_nodelay(&connection, true).context("failed to set TCP_NODELAY")?;
            accepted += 1;
            let state = state.clone();
            let handler = scope.spawn_stealable(move |cx| {
                let transport =
                    RuntimeStackTransport::plaintext_socket(RingFd::from_owned(connection));
                serve_grpc_requests::<StackStealGrpcRuntime, _>(cx, transport, state, usize::MAX)
                    .context("gRPC connection failed")
            });
            if max_connections.is_some() {
                handlers.push(handler);
            }
        }
        for handler in handlers {
            handler.join(cx)?;
        }
        Ok::<_, anyhow::Error>(())
    })?;
    cx.close(listener).context("listener close failed")
}

fn serve_steal_admin_listener(
    cx: &StealContext<'_>,
    listener: OwnedFd,
    state: Arc<StackfulGatewayState>,
    max_connections: Option<usize>,
) -> Result<()> {
    cx.scope(|scope| {
        let mut accepted = 0_usize;
        let mut handlers = Vec::new();
        loop {
            if max_connections.is_some_and(|max| accepted >= max) {
                break;
            }
            let connection = cx.accept(&listener).context("admin accept failed")?;
            accepted += 1;
            let state = state.clone();
            let handler = scope.spawn_stealable(move |cx| {
                let transport =
                    RuntimeStackTransport::plaintext_socket(RingFd::from_owned(connection));
                serve_admin_once(cx, transport, state).context("admin connection failed")
            });
            if max_connections.is_some() {
                handlers.push(handler);
            }
        }
        for handler in handlers {
            handler.join(cx)?;
        }
        Ok::<_, anyhow::Error>(())
    })?;
    cx.close(listener).context("listener close failed")
}

fn stack_self_check(
    cx: &RuntimeContext<'_>,
    state: Arc<StackfulGatewayState>,
    grpc_addr: SocketAddr,
    admin_addr: SocketAddr,
) -> Result<()> {
    let health = call_stack_health(cx, grpc_addr)?;
    ensure!(health.ready, "gRPC health did not report ready");
    let admin = call_stack_admin_health(cx, admin_addr)?;
    ensure!(
        admin.contains("ready=true"),
        "admin health did not report ready: {admin}"
    );
    state.begin_shutdown();
    let status = call_stack_admin_status(cx, admin_addr)?;
    ensure!(
        status.contains("shutting_down=true"),
        "admin status did not report shutdown: {status}"
    );
    Ok(())
}

fn steal_self_check(
    cx: &StealContext<'_>,
    state: Arc<StackfulGatewayState>,
    grpc_addr: SocketAddr,
    admin_addr: SocketAddr,
) -> Result<()> {
    let health = call_steal_health(cx, grpc_addr)?;
    ensure!(health.ready, "gRPC health did not report ready");
    let admin = call_steal_admin_health(cx, admin_addr)?;
    ensure!(
        admin.contains("ready=true"),
        "admin health did not report ready: {admin}"
    );
    state.begin_shutdown();
    let status = call_steal_admin_status(cx, admin_addr)?;
    ensure!(
        status.contains("shutting_down=true"),
        "admin status did not report shutdown: {status}"
    );
    Ok(())
}

fn call_stack_health(cx: &RuntimeContext<'_>, addr: SocketAddr) -> Result<proto::HealthResponse> {
    let transport = stack_connect(cx, addr)?;
    let http = h2::ClientConnection::new(transport, HttpConfig::default());
    let mut client = StackfulGatewayClient::new(RuntimeUnaryClient::new(
        http,
        kimojio_stack_grpc::ClientConfig::default(),
    ));
    let response: UnaryResponse<proto::HealthResponse> =
        client.unary(cx, proto::HEALTH_PATH, &proto::HealthRequest {})?;
    client.close(cx)?;
    Ok(response.message)
}

fn call_steal_health(cx: &StealContext<'_>, addr: SocketAddr) -> Result<proto::HealthResponse> {
    let transport = steal_connect(cx, addr)?;
    let http = h2::RuntimeClientConnection::new(transport, HttpConfig::default());
    let mut client = StackfulGatewayClient::new(RuntimeUnaryClient::new(
        http,
        kimojio_stack_grpc::ClientConfig::default(),
    ));
    let response: UnaryResponse<proto::HealthResponse> =
        client.unary(cx, proto::HEALTH_PATH, &proto::HealthRequest {})?;
    client.close(cx)?;
    Ok(response.message)
}

fn call_stack_admin_health(cx: &RuntimeContext<'_>, addr: SocketAddr) -> Result<String> {
    call_stack_admin(cx, addr, "/health")
}

fn call_stack_admin_status(cx: &RuntimeContext<'_>, addr: SocketAddr) -> Result<String> {
    call_stack_admin(cx, addr, "/status")
}

fn call_stack_admin(cx: &RuntimeContext<'_>, addr: SocketAddr, path: &str) -> Result<String> {
    let mut client = http1::ClientConnection::new(stack_connect(cx, addr)?, HttpConfig::default());
    let request = Request::builder()
        .method("GET")
        .uri(path)
        .header("host", "127.0.0.1")
        .body(Body::empty())
        .context("failed to build admin request")?;
    let response = client.send(cx, &request)?;
    client.close(cx)?;
    ensure!(
        response.status().is_success(),
        "admin request {path} failed"
    );
    Ok(String::from_utf8_lossy(response.body().as_bytes()).into_owned())
}

fn call_steal_admin_health(cx: &StealContext<'_>, addr: SocketAddr) -> Result<String> {
    call_steal_admin(cx, addr, "/health")
}

fn call_steal_admin_status(cx: &StealContext<'_>, addr: SocketAddr) -> Result<String> {
    call_steal_admin(cx, addr, "/status")
}

fn call_steal_admin(cx: &StealContext<'_>, addr: SocketAddr, path: &str) -> Result<String> {
    let transport = steal_connect(cx, addr)?;
    let mut client = http1::RuntimeClientConnection::new(transport, HttpConfig::default());
    let request = Request::builder()
        .method("GET")
        .uri(path)
        .header("host", "127.0.0.1")
        .body(Body::empty())
        .context("failed to build admin request")?;
    let response = client.send(cx, &request)?;
    client.close(cx)?;
    ensure!(
        response.status().is_success(),
        "admin request {path} failed"
    );
    Ok(String::from_utf8_lossy(response.body().as_bytes()).into_owned())
}

fn stack_connect(cx: &RuntimeContext<'_>, addr: SocketAddr) -> Result<StackTransport> {
    let socket = cx
        .socket(address_family(addr), SocketType::STREAM, Some(ipproto::TCP))
        .context("socket creation failed")?;
    cx.connect(&socket, &addr).context("connect failed")?;
    Ok(StackTransport::plaintext(socket))
}

fn steal_connect(cx: &StealContext<'_>, addr: SocketAddr) -> Result<RuntimeStackTransport<RingFd>> {
    let socket = cx
        .socket(address_family(addr), SocketType::STREAM, Some(ipproto::TCP))
        .context("socket creation failed")?;
    cx.connect(&socket, &addr).context("connect failed")?;
    Ok(RuntimeStackTransport::plaintext_socket(RingFd::from_owned(
        socket,
    )))
}

fn print_startup(
    args: &Args,
    runtime: &str,
    grpc_addr: SocketAddr,
    admin_addr: SocketAddr,
    ready: bool,
) {
    println!(
        "object-gateway-stack-host runtime={runtime} grpc_addr={grpc_addr} admin_addr={admin_addr} workers={} storage_client=kimojio-stack-storage storage_backend={} telemetry_sink={} ready={} deadline_ms={}",
        args.workers,
        StorageBackendMode::Hermetic.as_str(),
        TelemetrySinkMode::Hermetic.as_str(),
        ready,
        Duration::from_millis(args.request_deadline_ms).as_millis()
    );
}
