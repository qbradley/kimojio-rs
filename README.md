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
- Core runtime paths avoid implicit cross-thread synchronization.

## Stackful HTTP and gRPC

The workspace includes low-level stackful networking crates for applications
that need explicit scheduling and predictable latency:

- `kimojio-stack-http` provides HTTP/1.1 and HTTP/2 client/server foundations.
- `kimojio-stack-grpc` provides unary gRPC client/server foundations over the
  HTTP/2 transport.
- `kimojio-stack-opentelemetry` provides low-level OpenTelemetry Protocol logs
  and metrics export over the stackful gRPC transport.

See the [Stackful HTTP and gRPC Guide][stack-http-grpc-guide] for supported
protocol features, transport setup, interoperability guarantees, and current
benchmark observations.
See the [Stack OpenTelemetry Guide][stack-opentelemetry-guide] for supported
telemetry signals, export behavior, and receiver interoperability testing.

[stack-http-grpc-guide]: https://github.com/Azure/kimojio-rs/blob/main/docs/stack-http-grpc.md
[stack-opentelemetry-guide]: https://github.com/Azure/kimojio-rs/blob/main/docs/stack-opentelemetry.md

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

## Cross-runtime Channels

`kimojio-stack` includes bounded in-memory channels for communication across
stackful runtimes, OS threads, and Tokio-compatible async tasks. See the
[Cross Runtime Channel Guide](docs/cross-runtime-channel.md) for endpoint
selection and usage.

## License

This project is licensed under the [MIT License](LICENSE.txt).

## Trademarks

This project may contain trademarks or logos for projects, products, or services. Authorized use of Microsoft trademarks or logos is subject to and must follow [Microsoft’s Trademark & Brand Guidelines](https://www.microsoft.com/en-us/legal/intellectualproperty/trademarks). Use of Microsoft trademarks or logos in modified versions of this project must not cause confusion or imply Microsoft sponsorship. Any use of third-party trademarks or logos are subject to those third-party’s policies.
