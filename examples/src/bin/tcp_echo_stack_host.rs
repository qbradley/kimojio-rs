// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::num::NonZeroUsize;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use kimojio_stack::{Runtime, RuntimeContext, SocketIoRuntime};
use kimojio_stack_steal::{
    Ring, RingFd, Runtime as StealRuntime, RuntimeConfig as StealRuntimeConfig,
    RuntimeContext as StealContext, StealPolicy,
};
use kimojio_stack_tls::{RuntimeTlsStream, TlsContext as StackTlsContext};
use rustix::fd::OwnedFd;
use rustix::net::{
    self, AddressFamily, Shutdown, SocketType, ipproto,
    sockopt::{set_socket_reuseaddr, set_tcp_nodelay},
};

#[path = "../tls_support.rs"]
mod tls_support;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum RuntimeKind {
    /// Single-threaded kimojio-stack runtime with stackful local tasks.
    Pinned,
    /// kimojio-stack-steal runtime with connection handlers submitted as stealable work.
    Steal,
}

#[derive(Debug, Parser)]
#[command(version, about = "TCP echo server backed by kimojio stack runtimes")]
struct Args {
    /// Runtime family to use for connection handlers.
    #[arg(long, value_enum, default_value_t = RuntimeKind::Pinned)]
    runtime: RuntimeKind,

    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:9000")]
    addr: SocketAddr,

    /// Number of stack-steal worker threads. Ignored for --runtime pinned.
    #[arg(long, default_value_t = 4)]
    workers: usize,

    /// Listen backlog.
    #[arg(long, default_value_t = 1024)]
    backlog: i32,

    /// Per-connection read buffer size.
    #[arg(long, default_value_t = 16 * 1024)]
    buffer_size: usize,

    /// Stop after accepting this many connections.
    #[arg(long)]
    max_connections: Option<usize>,

    /// Enable TCP_NODELAY on accepted sockets.
    #[arg(long, default_value_t = true)]
    nodelay: bool,

    /// Serve TLS instead of plaintext TCP.
    #[arg(long)]
    tls: bool,

    /// PEM certificate chain for --tls.
    #[arg(long)]
    cert: Option<PathBuf>,

    /// PEM private key for --tls.
    #[arg(long)]
    key: Option<PathBuf>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    validate_args(&args)?;

    match args.runtime {
        RuntimeKind::Pinned => run_pinned(args),
        RuntimeKind::Steal => run_steal(args),
    }
}

fn validate_args(args: &Args) -> Result<()> {
    if args.buffer_size == 0 {
        bail!("--buffer-size must be greater than zero");
    }
    if args.workers == 0 {
        bail!("--workers must be greater than zero");
    }
    if args.backlog <= 0 {
        bail!("--backlog must be greater than zero");
    }
    Ok(())
}

fn run_pinned(args: Args) -> Result<()> {
    let tls_mode = tls_support::TlsMode::from_args(args.tls, &args.cert, &args.key)?;
    let mut runtime = Runtime::new();
    runtime.block_on(|cx| {
        let listener = create_stack_listener(cx, args.addr, args.backlog)?;
        let local_addr: SocketAddr = net::getsockname(&listener)
            .context("failed to read listener address")?
            .try_into()
            .context("listener address was not an IP socket address")?;
        println!(
            "tcp-echo-stack-host runtime=pinned listening={} buffer_size={} nodelay={} tls={}",
            local_addr,
            args.buffer_size,
            args.nodelay,
            tls_mode.is_tls()
        );

        cx.scope(|scope| {
            let mut accepted = 0_usize;
            let mut handlers = Vec::new();
            loop {
                if args.max_connections.is_some_and(|max| accepted >= max) {
                    break;
                }

                let connection = cx.accept(&listener).context("accept failed")?;
                if args.nodelay {
                    set_tcp_nodelay(&connection, true).context("failed to set TCP_NODELAY")?;
                }
                accepted += 1;
                let buffer_size = args.buffer_size;
                let tls_mode = tls_mode.clone();
                let handler = scope.spawn(move |cx| {
                    if let Err(error) =
                        handle_pinned_connection(cx, connection, buffer_size, tls_mode)
                    {
                        eprintln!("pinned echo handler failed: {error:#}");
                    }
                });
                if args.max_connections.is_some() {
                    handlers.push(handler);
                }
            }
            for handler in handlers {
                handler.join(cx);
            }
            Ok::<_, anyhow::Error>(())
        })?;

        cx.close(listener).context("listener close failed")?;
        Ok(())
    })
}

fn create_stack_listener(
    cx: &RuntimeContext<'_>,
    addr: SocketAddr,
    backlog: i32,
) -> Result<OwnedFd> {
    let domain = if addr.is_ipv4() {
        AddressFamily::INET
    } else {
        AddressFamily::INET6
    };
    let listener = cx
        .socket(domain, SocketType::STREAM, Some(ipproto::TCP))
        .context("socket creation failed")?;
    set_socket_reuseaddr(&listener, true).context("failed to set SO_REUSEADDR")?;
    cx.bind(&listener, &addr).context("bind failed")?;
    cx.listen(&listener, backlog).context("listen failed")?;
    Ok(listener)
}

fn handle_pinned_connection(
    cx: &RuntimeContext<'_>,
    connection: OwnedFd,
    buffer_size: usize,
    tls_mode: tls_support::TlsMode,
) -> Result<()> {
    match tls_mode {
        tls_support::TlsMode::Plain => {
            let echo_result = echo_pinned(cx, &connection, buffer_size);
            let shutdown_result = cx
                .shutdown(&connection, Shutdown::Write)
                .context("connection shutdown failed");
            let close_result = cx.close(connection).context("connection close failed");

            echo_result?;
            shutdown_result?;
            close_result?;
            Ok(())
        }
        tls_support::TlsMode::Tls { cert, key } => {
            let context = StackTlsContext::from_openssl(tls_support::server_context(&cert, &key)?);
            let mut stream = context
                .server(cx, buffer_size, connection)
                .context("TLS server handshake failed")?;
            let echo_result = echo_stack_tls(cx, &mut stream, buffer_size);
            let shutdown_result = stream.shutdown(cx).context("TLS shutdown failed");
            let close_result = stream.close(cx).context("connection close failed");

            echo_result?;
            shutdown_result?;
            close_result?;
            Ok(())
        }
    }
}

fn echo_pinned(cx: &RuntimeContext<'_>, connection: &OwnedFd, buffer_size: usize) -> Result<()> {
    let mut buffer = vec![0_u8; buffer_size];
    loop {
        let read = cx.read(connection, &mut buffer).context("read failed")?;
        if read == 0 {
            return Ok(());
        }
        write_all_pinned(cx, connection, &buffer[..read])?;
    }
}

fn write_all_pinned(cx: &RuntimeContext<'_>, connection: &OwnedFd, mut bytes: &[u8]) -> Result<()> {
    while !bytes.is_empty() {
        let written = cx.write(connection, bytes).context("write failed")?;
        if written == 0 {
            bail!("zero-length write on connected socket");
        }
        bytes = &bytes[written..];
    }
    Ok(())
}

fn run_steal(args: Args) -> Result<()> {
    let tls_mode = tls_support::TlsMode::from_args(args.tls, &args.cert, &args.key)?;
    let listener =
        TcpListener::bind(args.addr).with_context(|| format!("bind failed for {}", args.addr))?;
    let local_addr = listener
        .local_addr()
        .context("failed to read listener address")?;
    let workers = NonZeroUsize::new(args.workers).context("--workers must be non-zero")?;
    let mut runtime = StealRuntime::with_config(StealRuntimeConfig {
        workers,
        steal_policy: StealPolicy::steal_one(),
        ..StealRuntimeConfig::default()
    });

    println!(
        "tcp-echo-stack-host runtime=steal listening={} workers={} buffer_size={} nodelay={} tls={}",
        local_addr,
        args.workers,
        args.buffer_size,
        args.nodelay,
        tls_mode.is_tls()
    );
    println!(
        "note: stack-steal currently uses std blocking accept; connected socket I/O runs on worker rings"
    );

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let mut accepted = 0_usize;
            let mut handlers = Vec::new();
            loop {
                if args.max_connections.is_some_and(|max| accepted >= max) {
                    break;
                }

                let (stream, peer) = listener.accept().context("accept failed")?;
                if args.nodelay {
                    stream
                        .set_nodelay(true)
                        .with_context(|| format!("failed to set TCP_NODELAY for {peer}"))?;
                }
                accepted += 1;
                let buffer_size = args.buffer_size;
                let tls_mode = tls_mode.clone();
                let handler = scope.spawn_stealable(move |cx| {
                    if let Err(error) = handle_steal_connection(cx, stream, buffer_size, tls_mode) {
                        eprintln!("steal echo handler failed for {peer}: {error:#}");
                    }
                });
                if args.max_connections.is_some() {
                    handlers.push(handler);
                }
            }
            for handler in handlers {
                handler.join(cx);
            }
            Ok::<_, anyhow::Error>(())
        })
    })
}

fn handle_steal_connection(
    cx: &StealContext<'_>,
    stream: TcpStream,
    buffer_size: usize,
    tls_mode: tls_support::TlsMode,
) -> Result<()> {
    match tls_mode {
        tls_support::TlsMode::Plain => {
            let fd = RingFd::from_owned(OwnedFd::from(stream));
            let ring = cx.create_worker_ring();
            let echo_result = echo_steal(cx, &ring, &fd, buffer_size);
            let shutdown_result = ring
                .shutdown(cx, &fd, Shutdown::Write)
                .context("connection shutdown failed");
            let close_result = ring.close(cx, fd).context("connection close failed");

            echo_result?;
            shutdown_result?;
            close_result?;
            Ok(())
        }
        tls_support::TlsMode::Tls { cert, key } => {
            let context = StackTlsContext::from_openssl(tls_support::server_context(&cert, &key)?);
            let mut stream = context
                .server_with_runtime(cx, buffer_size, OwnedFd::from(stream))
                .context("TLS server handshake failed")?;
            let echo_result = echo_stack_tls(cx, &mut stream, buffer_size);
            let shutdown_result = stream.shutdown(cx).context("TLS shutdown failed");
            let close_result = stream.close(cx).context("connection close failed");

            echo_result?;
            shutdown_result?;
            close_result?;
            Ok(())
        }
    }
}

fn echo_steal(
    cx: &StealContext<'_>,
    ring: &Ring,
    connection: &RingFd,
    buffer_size: usize,
) -> Result<()> {
    let mut buffer = vec![0_u8; buffer_size];
    loop {
        let read = ring
            .read(cx, connection, &mut buffer)
            .context("read failed")?;
        if read == 0 {
            return Ok(());
        }
        write_all_steal(cx, ring, connection, &buffer[..read])?;
    }
}

fn write_all_steal(
    cx: &StealContext<'_>,
    ring: &Ring,
    connection: &RingFd,
    mut bytes: &[u8],
) -> Result<()> {
    while !bytes.is_empty() {
        let written = ring.write(cx, connection, bytes).context("write failed")?;
        if written == 0 {
            bail!("zero-length write on connected socket");
        }
        bytes = &bytes[written..];
    }
    Ok(())
}

fn echo_stack_tls<R, S>(cx: &R, stream: &mut RuntimeTlsStream<S>, buffer_size: usize) -> Result<()>
where
    R: SocketIoRuntime<Socket = S>,
{
    let mut buffer = vec![0_u8; buffer_size];
    loop {
        let read = stream.read(cx, &mut buffer).context("TLS read failed")?;
        if read == 0 {
            return Ok(());
        }
        stream
            .write(cx, &buffer[..read])
            .context("TLS write failed")?;
    }
}
