// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use openssl::ssl::Ssl;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_openssl::SslStream;

#[path = "../tls_support.rs"]
mod tls_support;

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
    if args.buffer_size == 0 {
        bail!("--buffer-size must be greater than zero");
    }
    tls_support::TlsMode::from_args(args.tls, &args.cert, &args.key)?;

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
    let tls_mode = tls_support::TlsMode::from_args(args.tls, &args.cert, &args.key)?;
    let listener = TcpListener::bind(args.addr)
        .await
        .with_context(|| format!("bind failed for {}", args.addr))?;
    let local_addr = listener
        .local_addr()
        .context("failed to read listener address")?;
    println!(
        "tcp-echo-tokio-host runtime={:?} listening={} buffer_size={} nodelay={} tls={}",
        args.runtime,
        local_addr,
        args.buffer_size,
        args.nodelay,
        tls_mode.is_tls()
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
        let tls_mode = tls_mode.clone();
        let handler = tokio::spawn(async move {
            if let Err(error) = handle_connection(stream, buffer_size, tls_mode).await {
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

async fn handle_connection(
    mut stream: TcpStream,
    buffer_size: usize,
    tls_mode: tls_support::TlsMode,
) -> Result<()> {
    match tls_mode {
        tls_support::TlsMode::Plain => handle_plain_connection(&mut stream, buffer_size).await,
        tls_support::TlsMode::Tls { cert, key } => {
            let acceptor = tls_support::server_acceptor(&cert, &key)?;
            let ssl = Ssl::new(acceptor.context()).context("create server SSL object")?;
            let mut stream = SslStream::new(ssl, stream).context("create TLS stream")?;
            Pin::new(&mut stream)
                .accept()
                .await
                .context("TLS server handshake failed")?;
            echo_tls_connection(&mut stream, buffer_size).await?;
            Pin::new(&mut stream)
                .shutdown()
                .await
                .context("TLS shutdown failed")?;
            Ok(())
        }
    }
}

async fn handle_plain_connection(stream: &mut TcpStream, buffer_size: usize) -> Result<()> {
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

async fn echo_tls_connection(stream: &mut SslStream<TcpStream>, buffer_size: usize) -> Result<()> {
    let mut buffer = vec![0_u8; buffer_size];
    loop {
        let read = stream.read(&mut buffer).await.context("TLS read failed")?;
        if read == 0 {
            return Ok(());
        }
        stream
            .write_all(&buffer[..read])
            .await
            .context("TLS write failed")?;
    }
}
