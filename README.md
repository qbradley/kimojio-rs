# kimojio

A thread-per-core Linux io_uring async runtime for Rust optimized for latency.

[Documentation](https://docs.rs/crate/kimojio/latest)

Kimojio uses a single-threaded, cooperatively scheduled runtime. Task scheduling is fast and consistent because tasks do not migrate between threads. This design works well for I/O-bound workloads with fine-grained tasks and minimal CPU-bound work.

Disk I/O in Kimojio is handled through io_uring, allowing asynchronous operations without relying on additional background threads. In some cases, the kernel may introduce a helper thread, but this is not part of the runtime itself.

Because Kimojio does not include automatic load balancing, developers have full control over concurrency and task distribution. This reduces implicit synchronization overhead but requires manual handling if multi-threaded execution is needed.

Key characteristics:

- Single-threaded, cooperative scheduling.
- Consistent task scheduling overhead.
- Asynchronous disk I/O via io_uring.
- Explicit control over concurrency and load balancing.
- No locks, atomics, or other thread synchronization

## Getting Started

### Prerequisites

Kimojio requires Linux Kernel of at least version 5.15 for sufficient I/O uring support.

### Installing

1. Add kimojio to your project:

    ```sh
    cargo add kimojio
    ```

### Hello World

```rust
use kimojio::{
    Errno,
    configuration::Configuration,
    operations::{self, OFlags},
};

#[kimojio::main]
async fn main() -> Result<(), Errno> {
    let flags = OFlags::CREATE | OFlags::RDWR;
    let fd = operations::open(c"/tmp/example.txt", flags, 0o644.into()).await?;
    let written = operations::write(&fd, b"hello world").await?;
    assert_eq!(written, 11);
    operations::close(fd).await?;
    Ok(())
}
```

## Optional HTTP client and server

Enable the `http` feature to use the HTTP/1.1 and HTTP/2 APIs:

```toml
[dependencies]
kimojio = { version = "0.17", features = ["http"] }
```

The client uses a request-builder API and standard `http` types:

```rust,no_run
use kimojio::http::{Client, Version};

# async fn example() -> kimojio::http::Result<()> {
let response = Client::new()
    .get("http://127.0.0.1:8080/health")
    .version(Version::HTTP_2)
    .send()
    .await?;
assert!(response.status().is_success());
# Ok(())
# }
```

`Server` accepts both protocols and calls a runtime-local async handler. With
the default `tls` feature, `TlsClientConfig` and `TlsServerConfig` add HTTPS and
select HTTP/1.1 or HTTP/2 through ALPN. Received bodies are buffered by default;
`Server::serve_streaming`, `Client::execute_streaming`, and
`RequestBuilder::send_streaming` opt into pull-based inbound chunks.
The server multiplexes concurrent HTTP/2 streams on a connection and serves
sequential HTTP/1 requests over persistent connections. Each client instance
pools idle HTTP/1 and HTTP/2 connections for sequential reuse; cloned clients
share that pool. Client request bodies and server response bodies may instead
use `Body::from_chunks` or `Body::from_stream` for incremental transmission;
inbound streaming uses `Body::next_chunk`. Buffered bodies retain a configured
total-size bound; streaming bodies are bounded in memory per chunk rather than
by their total transfer size. Protocol upgrades are not supported.
Host-name resolution is asynchronous and does not block the current runtime
thread. See [`kimojio/README.md`](kimojio/README.md) for server use, limits,
and configuration.

HTTP/1.1 clients honor `Expect: 100-continue` and use a bounded wait before
sending the body when an intermediary suppresses the interim response.
Servers continue expected requests by default and can install a request-head
hook to reject one before reading its body.

Protocol implementation references include the
[Stackful HTTP and gRPC Guide][stack-http-grpc-guide], the
[HPACK and Header Representation Reference][hpack-header-representation], and
the [FSM HTTP Server Guide][fsm-http-server-guide].

[stack-http-grpc-guide]: https://github.com/Azure/kimojio-rs/blob/main/docs/stack-http-grpc.md
[hpack-header-representation]: https://github.com/Azure/kimojio-rs/blob/main/docs/hpack-header-representation.md
[fsm-http-server-guide]: https://github.com/Azure/kimojio-rs/blob/main/docs/fsm-http-server.md

## Contributing

Please see [CONTRIBUTING.md](CONTRIBUTING.md) for more information on how to contribute to this project.

## Virtual Clock for Testing

Kimojio supports deterministic timing via a virtual clock, enabling tests
involving timeouts and sleeps to run instantly.

```rust
use kimojio::{Runtime, operations};
use kimojio::configuration::Configuration;
use std::time::Duration;

let mut runtime = Runtime::new(0, Configuration::new());
runtime.block_on(async {
    operations::virtual_clock_enable(true);
    operations::virtual_clock_set_idle_advance(|now, next| {
        next.map(|d| d.saturating_duration_since(now))
    });
    operations::sleep(Duration::from_secs(60)).await.unwrap(); // Instant!
});
```

Enable with `features = ["virtual-clock"]`. See the
[Virtual Clock Guide](docs/virtual-clock-guide.md) for detailed usage and the
[Design Document](docs/virtual-clock-design.md) for architecture details.

## License

This project is licensed under the [MIT License](LICENSE.txt).

## Trademarks

This project may contain trademarks or logos for projects, products, or services. Authorized use of Microsoft trademarks or logos is subject to and must follow [Microsoft’s Trademark & Brand Guidelines](https://www.microsoft.com/en-us/legal/intellectualproperty/trademarks). Use of Microsoft trademarks or logos in modified versions of this project must not cause confusion or imply Microsoft sponsorship. Any use of third-party trademarks or logos are subject to those third-party’s policies.
