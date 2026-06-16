// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::Parser;

#[path = "../tls_support.rs"]
mod tls_support;

#[derive(Debug, Parser)]
#[command(version, about = "Blocking TCP/TLS echo latency client")]
struct Args {
    /// Echo server address.
    #[arg(long, default_value = "127.0.0.1:9000")]
    addr: SocketAddr,

    /// Bytes per echo request.
    #[arg(long, default_value_t = 64)]
    message_size: usize,

    /// Measured request/response iterations.
    #[arg(long, default_value_t = 10_000)]
    iterations: usize,

    /// Warmup iterations before measurement.
    #[arg(long, default_value_t = 100)]
    warmup: usize,

    /// Enable TCP_NODELAY on the client socket.
    #[arg(long, default_value_t = true)]
    nodelay: bool,

    /// Use TLS over the TCP connection.
    #[arg(long)]
    tls: bool,

    /// Server name used for TLS SNI/certificate validation.
    #[arg(long, default_value = "localhost")]
    tls_server_name: String,

    /// Disable TLS certificate verification. Intended for self-signed benchmark certificates.
    #[arg(long)]
    tls_insecure: bool,

    /// PEM CA certificate to trust for TLS verification.
    #[arg(long)]
    tls_ca_cert: Option<PathBuf>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    validate_args(&args)?;

    let payload = make_payload(args.message_size);
    let mut response = vec![0_u8; args.message_size];
    let mut stream = TcpStream::connect(args.addr)
        .with_context(|| format!("connect failed for {}", args.addr))?;
    stream
        .set_nodelay(args.nodelay)
        .context("failed to set TCP_NODELAY")?;

    let (mut samples, measured_elapsed) = if args.tls {
        let connector =
            tls_support::client_connector(args.tls_insecure, args.tls_ca_cert.as_deref())?;
        let mut stream = connector
            .connect(&args.tls_server_name, stream)
            .map_err(|error| anyhow!("TLS client handshake failed: {error}"))?;
        run_benchmark(&args, &mut stream, &payload, &mut response)?
    } else {
        run_benchmark(&args, &mut stream, &payload, &mut response)?
    };

    print_summary(&args, &mut samples, measured_elapsed);
    Ok(())
}

fn run_benchmark<S: Read + Write>(
    args: &Args,
    stream: &mut S,
    payload: &[u8],
    response: &mut [u8],
) -> Result<(Vec<Duration>, Duration)> {
    for _ in 0..args.warmup {
        round_trip(stream, payload, response)?;
    }

    let mut samples = Vec::with_capacity(args.iterations);
    let measured_start = Instant::now();
    for _ in 0..args.iterations {
        let start = Instant::now();
        round_trip(stream, payload, response)?;
        samples.push(start.elapsed());
    }
    let measured_elapsed = measured_start.elapsed();
    Ok((samples, measured_elapsed))
}

fn validate_args(args: &Args) -> Result<()> {
    if args.message_size == 0 {
        bail!("--message-size must be greater than zero");
    }
    if args.iterations == 0 {
        bail!("--iterations must be greater than zero");
    }
    Ok(())
}

fn make_payload(len: usize) -> Vec<u8> {
    let mut payload = vec![0_u8; len];
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte = (index % 251) as u8;
    }
    payload
}

fn round_trip<S: Read + Write>(stream: &mut S, payload: &[u8], response: &mut [u8]) -> Result<()> {
    stream.write_all(payload).context("write failed")?;
    stream.read_exact(response).context("read_exact failed")?;
    ensure!(
        response == payload,
        "echo response did not match request payload"
    );
    Ok(())
}

fn print_summary(args: &Args, samples: &mut [Duration], measured_elapsed: Duration) {
    samples.sort_unstable();
    let total_ns: u128 = samples.iter().map(Duration::as_nanos).sum();
    let count = samples.len() as u128;
    let average_ns = total_ns / count;
    let msgs_per_sec = samples.len() as f64 / measured_elapsed.as_secs_f64();
    let mib_per_sec = (samples.len() * args.message_size * 2) as f64
        / measured_elapsed.as_secs_f64()
        / (1024.0 * 1024.0);

    println!("addr={}", args.addr);
    println!("message_size_bytes={}", args.message_size);
    println!("warmup_iterations={}", args.warmup);
    println!("measured_iterations={}", args.iterations);
    println!("tcp_nodelay={}", args.nodelay);
    println!("tls={}", args.tls);
    println!("elapsed_ms={:.3}", measured_elapsed.as_secs_f64() * 1_000.0);
    println!("round_trips_per_sec={msgs_per_sec:.2}");
    println!("echo_payload_mib_per_sec={mib_per_sec:.2}");
    println!("min_us={:.3}", micros(samples[0].as_nanos()));
    println!("avg_us={:.3}", micros(average_ns));
    println!("p50_us={:.3}", micros(percentile(samples, 0.50).as_nanos()));
    println!("p90_us={:.3}", micros(percentile(samples, 0.90).as_nanos()));
    println!("p99_us={:.3}", micros(percentile(samples, 0.99).as_nanos()));
    println!(
        "max_us={:.3}",
        micros(samples[samples.len() - 1].as_nanos())
    );
}

fn percentile(samples: &[Duration], percentile: f64) -> Duration {
    let index = ((samples.len() - 1) as f64 * percentile).round() as usize;
    samples[index]
}

fn micros(nanos: u128) -> f64 {
    nanos as f64 / 1_000.0
}
