# Examples

This crate contains small binaries and integration examples for exercising the
Kimojio runtimes. The Stackful Object Gateway is the canonical multi-runtime
example; see the [Stackful Object Gateway guide](../docs/stack-object-gateway.md)
for details.

## Stackful Object Gateway

Run stackful host self-checks:

```sh
cargo run -q -p examples --bin object-gateway-stack-host -- --runtime stack --grpc-addr 127.0.0.1:0 --admin-addr 127.0.0.1:0 --shutdown-after-ready
cargo run -q -p examples --bin object-gateway-stack-host -- --runtime steal --workers 2 --grpc-addr 127.0.0.1:0 --admin-addr 127.0.0.1:0 --shutdown-after-ready
```

Run comparison host self-checks:

```sh
cargo run -q -p examples --bin object-gateway-tokio-host -- --grpc-addr 127.0.0.1:0 --admin-addr 127.0.0.1:0 --workers 2 --shutdown-after-ready
(cd examples/go/object_gateway && go run . --grpc-addr 127.0.0.1:0 --admin-addr 127.0.0.1:0 --shutdown-after-ready)
```

Run conformance and workloads:

```sh
cargo run -q -p examples --bin object-gateway-conformance
cargo run -q -p examples --bin object-gateway-workload -- --implementation all --workload all
KIMOJIO_OBJECT_GATEWAY_GO=1 cargo test -p examples object_gateway_go_conformance --all-targets
KIMOJIO_OBJECT_GATEWAY_GO=1 cargo run -q -p examples --bin object-gateway-workload -- --implementation go --workload small-object
```

Default runs are hermetic. Optional live storage and OpenTelemetry receiver runs
are gated by environment variables documented in the guide. Check live-gate
configuration or skip reasons with:

```sh
cargo test -p examples object_gateway_workload_live_storage_gate --all-targets
cargo test -p examples object_gateway_workload_live_telemetry_gate --all-targets
```
