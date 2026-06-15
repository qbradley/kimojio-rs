// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::net::SocketAddr;

use anyhow::{Result, bail};
use clap::Parser;
use kimojio::{
    Errno, OwnedFd,
    configuration::Configuration,
    operations::{self, accept, close, read, spawn_task, write},
};
use rustix::net::{
    AddressFamily, Shutdown, SocketType, ipproto,
    sockopt::{set_socket_reuseaddr, set_tcp_nodelay},
};

#[derive(Debug, Parser)]
#[command(
    version,
    about = "TCP echo server backed by traditional kimojio async I/O"
)]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:9000")]
    addr: SocketAddr,

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
}

fn main() -> Result<()> {
    let args = Args::parse();
    validate_args(&args)?;

    let result = kimojio::run_with_configuration(0, run_server(args), Configuration::new());
    match result {
        Some(Ok(result)) => result.map_err(anyhow::Error::from),
        Some(Err(payload)) => std::panic::resume_unwind(payload),
        None => bail!("kimojio runtime shut down before the server completed"),
    }
}

fn validate_args(args: &Args) -> Result<()> {
    if args.buffer_size == 0 {
        bail!("--buffer-size must be greater than zero");
    }
    if args.backlog <= 0 {
        bail!("--backlog must be greater than zero");
    }
    Ok(())
}

async fn run_server(args: Args) -> Result<(), Errno> {
    let listener = create_listener(args.addr, args.backlog).await?;
    let local_addr: SocketAddr = rustix::net::getsockname(&listener)?.try_into().unwrap();
    println!(
        "tcp-echo-kimojio-host listening={} buffer_size={} nodelay={}",
        local_addr, args.buffer_size, args.nodelay
    );

    let mut accepted = 0_usize;
    loop {
        if args.max_connections.is_some_and(|max| accepted >= max) {
            break;
        }

        let connection = accept(&listener).await?;
        if args.nodelay {
            set_tcp_nodelay(&connection, true)?;
        }
        accepted += 1;
        let buffer_size = args.buffer_size;
        spawn_task(async move {
            if let Err(error) = handle_connection(connection, buffer_size).await {
                eprintln!("kimojio echo handler failed: {error}");
            }
        });
    }

    close(listener).await?;
    Ok(())
}

async fn create_listener(addr: SocketAddr, backlog: i32) -> Result<OwnedFd, Errno> {
    let domain = if addr.is_ipv4() {
        AddressFamily::INET
    } else {
        AddressFamily::INET6
    };
    let listener = operations::socket(domain, SocketType::STREAM, Some(ipproto::TCP)).await?;
    set_socket_reuseaddr(&listener, true)?;
    rustix::net::bind(&listener, &addr)?;
    operations::listen(&listener, backlog)?;
    Ok(listener)
}

async fn handle_connection(connection: OwnedFd, buffer_size: usize) -> Result<(), Errno> {
    let echo_result = echo_connection(&connection, buffer_size).await;
    let shutdown_result = operations::shutdown(&connection, Shutdown::Write as i32).await;
    let close_result = close(connection).await;

    echo_result?;
    shutdown_result?;
    close_result?;
    Ok(())
}

async fn echo_connection(connection: &OwnedFd, buffer_size: usize) -> Result<(), Errno> {
    let mut buffer = vec![0_u8; buffer_size];
    loop {
        let amount = read(connection, &mut buffer).await?;
        if amount == 0 {
            return Ok(());
        }
        write_all(connection, &buffer[..amount]).await?;
    }
}

async fn write_all(connection: &OwnedFd, mut bytes: &[u8]) -> Result<(), Errno> {
    while !bytes.is_empty() {
        let written = write(connection, bytes).await?;
        if written == 0 {
            return Err(Errno::IO);
        }
        bytes = &bytes[written..];
    }
    Ok(())
}
