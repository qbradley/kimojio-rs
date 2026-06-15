// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::net::SocketAddr;

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum RuntimeKind {
    /// Tokio's multi-threaded work-stealing scheduler.
    MultiThread,
    /// Tokio's single-threaded current-thread scheduler.
    CurrentThread,
}

#[derive(Debug, Parser)]
#[command(version, about = "TCP echo server backed by tokio")]
struct Args {
    /// Tokio scheduler flavor.
    #[arg(long, value_enum, default_value_t = RuntimeKind::MultiThread)]
    runtime: RuntimeKind,

    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:9000")]
    addr: SocketAddr,

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
    if args.buffer_size == 0 {
        bail!("--buffer-size must be greater than zero");
    }

    let runtime = match args.runtime {
        RuntimeKind::MultiThread => tokio::runtime::Builder::new_multi_thread()
            .enable_io()
            .build()
            .context("failed to build Tokio multi-thread runtime")?,
        RuntimeKind::CurrentThread => tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .context("failed to build Tokio current-thread runtime")?,
    };
    runtime.block_on(run_server(args))
}

async fn run_server(args: Args) -> Result<()> {
    let listener = TcpListener::bind(args.addr)
        .await
        .with_context(|| format!("bind failed for {}", args.addr))?;
    let local_addr = listener
        .local_addr()
        .context("failed to read listener address")?;
    println!(
        "tcp-echo-tokio-host runtime={:?} listening={} buffer_size={} nodelay={}",
        args.runtime, local_addr, args.buffer_size, args.nodelay
    );

    let mut accepted = 0_usize;
    let mut handlers = Vec::new();
    loop {
        if args.max_connections.is_some_and(|max| accepted >= max) {
            break;
        }

        let (stream, peer) = listener.accept().await.context("accept failed")?;
        if args.nodelay {
            stream
                .set_nodelay(true)
                .with_context(|| format!("failed to set TCP_NODELAY for {peer}"))?;
        }
        accepted += 1;
        let buffer_size = args.buffer_size;
        let handler = tokio::spawn(async move {
            if let Err(error) = handle_connection(stream, buffer_size).await {
                eprintln!("tokio echo handler failed for {peer}: {error:#}");
            }
        });
        if args.max_connections.is_some() {
            handlers.push(handler);
        }
    }

    for handler in handlers {
        handler.await.context("tokio handler task panicked")?;
    }

    Ok(())
}

async fn handle_connection(mut stream: TcpStream, buffer_size: usize) -> Result<()> {
    let mut buffer = vec![0_u8; buffer_size];
    loop {
        let read = stream.read(&mut buffer).await.context("read failed")?;
        if read == 0 {
            break;
        }
        stream
            .write_all(&buffer[..read])
            .await
            .context("write failed")?;
    }
    stream.shutdown().await.context("shutdown failed")?;
    Ok(())
}
