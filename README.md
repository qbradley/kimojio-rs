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

## Contributing

Please see [CONTRIBUTING.md](CONTRIBUTING.md) for more information on how to contribute to this project.

## FSM Architecture

The [FSM composition pattern](docs/fsm-composition.md) records the family-wide
design for synchronous state machines, layered protocols, and async adapters.
The [HTTP/1 implementation plan](docs/http1-fsm-plan.md) applies that pattern
through independent native-FSM and Kimojio consumers.
The [standalone HTTP/1 crate](kimojio-fsm-http1/README.md) contains synchronous
client and server machines with caller-owned I/O execution.

| Component | Purpose |
| --- | --- |
| [Kimojio HTTP/1 wrapper](kimojio-http1/README.md) | Conventional async client, handlers, and body streams |
| [HTTP/2 FSM](kimojio-fsm-http2/README.md) | Sans-I/O HTTP/2 and HTTP/1 plus HTTP/2 composition |
| [Kimojio HTTP/2 wrapper](kimojio-http2/README.md) | Concurrent native and generic clients, handlers, and streaming bodies |
| [Static-file server](examples/http1-static/README.md) | HTTP and file FSMs with a direct `rustix-uring` driver |
| [WebSocket FSM](kimojio-fsm-websocket/README.md) | Runtime-neutral RFC 6455 server and HTTP upgrade |
| [Broadcast chat](examples/websocket-chat/README.md) | Bounded application FSM with a Kimojio raw-I/O executor |

The [HTTP harness](interop/http1/README.md) and [WebSocket harness](interop/websocket/README.md)
use independent protocol peers.
The [composition assessment](docs/http1-fsm-report.md) records the change topology,
correctness evidence, performance results, and remaining limitations.
The [HTTP/2 assessment](docs/http2-wrapper-report.md) records the completed scope,
runtime retention repair, measured wrapper costs, and qualification limits.

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
