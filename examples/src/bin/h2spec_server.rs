// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A minimal HTTP/2 server used to run the `h2spec` conformance suite.
//!
//! `h2spec` (<https://github.com/summerwind/h2spec>) drives an implementation
//! through the RFC 7540 and RFC 7541 test cases. It only needs a peer that
//! answers every well-formed request with a successful response, so this
//! server replies `200 OK` to anything it receives and reports the number of
//! request body bytes it consumed.
//!
//! Run the cleartext suite with:
//!
//! ```text
//! h2spec-server --listen 127.0.0.1:8888
//! h2spec -h 127.0.0.1 -p 8888
//! ```
//!
//! Pass `--tls-listen`, `--cert`, and `--key` to expose an ALPN-negotiated
//! listener for `h2spec -t -k`.

use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use kimojio::http::{
    AlpnProtocol, Body, HeaderValue, Limits, Request, Response, Server, StatusCode, TlsServerConfig,
};
use kimojio::{CancellationToken, operations};
use openssl::ssl::{SslContextBuilder, SslFiletype, SslMethod};

/// h2spec exercises frame sizes well beyond a default body limit, so accept a
/// generous request body rather than rejecting the test case with a 413.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
const SIGNAL_POLL_INTERVAL: Duration = Duration::from_millis(100);
const TEXT_CONTENT_TYPE: &str = "text/plain; charset=utf-8";

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Serve HTTP/2 requests for the h2spec conformance suite"
)]
struct Args {
    /// Cleartext listen address
    #[arg(long, value_name = "ADDR")]
    listen: Option<SocketAddr>,

    /// TLS listen address
    #[arg(long, value_name = "ADDR")]
    tls_listen: Option<SocketAddr>,

    /// PEM certificate chain for the TLS listener
    #[arg(long, value_name = "FILE")]
    cert: Option<PathBuf>,

    /// PEM private key for the TLS listener
    #[arg(long, value_name = "FILE")]
    key: Option<PathBuf>,
}

extern "C" fn shutdown_signal_handler(_signal: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
}

fn install_signal_handlers() -> Result<()> {
    let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
    action.sa_sigaction = shutdown_signal_handler as usize;
    if unsafe { libc::sigemptyset(&mut action.sa_mask) } != 0 {
        return Err(io::Error::last_os_error()).context("failed to initialize signal mask");
    }
    for signal in [libc::SIGINT, libc::SIGTERM] {
        if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } != 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("failed to install handler for signal {signal}"));
        }
    }
    Ok(())
}

async fn cancel_on_signal(cancellation: Rc<CancellationToken>) {
    while !SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
        if operations::sleep(SIGNAL_POLL_INTERVAL).await.is_err() {
            return;
        }
    }
    cancellation.cancel();
}

fn validate_args(args: &Args) -> Result<()> {
    if args.listen.is_none() && args.tls_listen.is_none() {
        bail!("at least one of --listen or --tls-listen is required");
    }
    if args.tls_listen.is_some() && (args.cert.is_none() || args.key.is_none()) {
        bail!("--tls-listen requires both --cert and --key");
    }
    Ok(())
}

fn tls_config(cert: &Path, key: &Path) -> Result<TlsServerConfig> {
    let mut builder = SslContextBuilder::new(SslMethod::tls_server())
        .context("failed to create the TLS context builder")?;
    builder
        .set_certificate_chain_file(cert)
        .with_context(|| format!("failed to load certificate chain {}", cert.display()))?;
    builder
        .set_private_key_file(key, SslFiletype::PEM)
        .with_context(|| format!("failed to load private key {}", key.display()))?;
    builder
        .check_private_key()
        .context("certificate and private key do not match")?;
    TlsServerConfig::new(builder, &[AlpnProtocol::H2, AlpnProtocol::Http11])
        .context("failed to configure the TLS server")
}

/// Answers every request with `200 OK`.
///
/// h2spec only inspects the framing and header fields of the response, so the
/// body simply confirms how many request bytes the server accepted.
async fn handle_request(request: Request<Body>) -> Response<Body> {
    let body = format!(
        "kimojio h2spec server\nmethod: {}\npath: {}\nrequest-body-bytes: {}\n",
        request.method(),
        request.uri().path(),
        request.body().len()
    );
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::OK;
    response
        .headers_mut()
        .insert("content-type", HeaderValue::from_static(TEXT_CONTENT_TYPE));
    response
}

async fn serve_listener(
    server: Server,
    cancellation: Rc<CancellationToken>,
) -> Result<(), kimojio::http::Error> {
    let cancellation_for_serve = Rc::clone(&cancellation);
    let result = server
        .serve(
            move |request| async move { handle_request(request).await },
            cancellation_for_serve,
        )
        .await;
    cancellation.cancel();
    result
}

#[kimojio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    validate_args(&args)?;
    install_signal_handlers()?;

    let limits = Limits::new().set_max_body_bytes(MAX_BODY_BYTES);

    let plain_server = match args.listen {
        Some(address) => Some(
            Server::builder(address)
                .limits(limits)
                .bind()
                .await
                .with_context(|| format!("failed to bind cleartext listener {address}"))?,
        ),
        None => None,
    };
    let tls_server = match args.tls_listen {
        Some(address) => {
            let tls = tls_config(
                args.cert.as_deref().expect("validated certificate path"),
                args.key.as_deref().expect("validated private key path"),
            )?;
            Some(
                Server::builder(address)
                    .limits(limits)
                    .tls(tls)
                    .bind()
                    .await
                    .with_context(|| format!("failed to bind TLS listener {address}"))?,
            )
        }
        None => None,
    };

    if let Some(server) = &plain_server {
        println!("listening plain http://{}", server.local_addr());
    }
    if let Some(server) = &tls_server {
        println!("listening tls https://{}", server.local_addr());
    }
    io::stdout()
        .flush()
        .context("failed to flush readiness lines")?;

    let cancellation = Rc::new(CancellationToken::new());
    let _signal_task = operations::spawn_task(cancel_on_signal(Rc::clone(&cancellation)));

    match (plain_server, tls_server) {
        (Some(plain), Some(tls)) => {
            let (plain_result, tls_result) = futures::join!(
                serve_listener(plain, Rc::clone(&cancellation)),
                serve_listener(tls, cancellation)
            );
            plain_result?;
            tls_result?;
        }
        (Some(plain), None) => serve_listener(plain, cancellation).await?,
        (None, Some(tls)) => serve_listener(tls, cancellation).await?,
        (None, None) => unreachable!("listener presence validated"),
    }
    Ok(())
}
