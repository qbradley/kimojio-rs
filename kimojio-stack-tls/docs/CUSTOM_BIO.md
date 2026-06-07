# Custom BIO experiment

`kimojio-stack-tls2` experimented with replacing the existing memory-BIO
integration with a custom OpenSSL BIO that called `kimojio-stack` I/O directly.
The goal was to avoid an apparent extra encrypted-buffer copy and make large
writes faster.

## What was tried

The experimental crate built a specialized `BIO_METHOD` with `openssl-sys`.
Each TLS stream stored the `RuntimeContext` and socket fd in BIO state, so
OpenSSL BIO callbacks could call `RuntimeContext::read`, `write`, and later
`writev` directly. This avoided the generic `openssl::ssl::SslStream<Read +
Write>` adapter and its Rust trait-object/error-storage path.

The experiment also added opt-in write corking. Small plaintext fragments were
encrypted immediately, but the BIO buffered the resulting ciphertext. A final
large `write_all` then combined the pending ciphertext with the direct encrypted
output using `writev`, so a small header plus large body did not require copying
the body into a caller-side staging buffer.

## What was measured

Benchmarks compared the existing `kimojio-stack-tls` memory-BIO path with the
custom-BIO path for echo and RPC-shaped workloads. The most representative
workload used a 64-byte length header, an 8/16/23/24 KiB body, and a 64-byte
response, with client and server running on separate OS threads and separate
`Runtime` instances.

The threaded `rpc_write` results showed the custom BIO was consistently slower:

| Body | Memory BIO | Custom BIO |
| ---: | ---: | ---: |
| 8 KiB | 44.579 us | 47.435 us |
| 16 KiB | 48.936 us | 51.093 us |
| 24 KiB | 55.338 us | 58.122 us |
| 23 KiB | 52.796 us | 57.901 us |

User-space `perf` sampling on the 8 KiB RPC workload showed the memory-BIO path
spent most time inside OpenSSL/SymCrypt crypto code. The custom-BIO path moved
more time into `libssl`, Rust runtime code, and the BIO shim itself:

| Area | Memory BIO | Custom BIO |
| --- | ---: | ---: |
| `libcrypto` | ~50.0% | ~37.6% |
| `libsymcrypt` | ~19.3% | ~20.8% |
| `libssl` | ~10.0% | ~14.6% |
| `libc` | ~11.4% | ~13.3% |
| Rust benchmark/runtime | ~7.0% | ~11.2% |

The custom-BIO profile showed higher runtime I/O overhead:

| Symbol | Memory BIO | Custom BIO |
| --- | ---: | ---: |
| `rustix_uring::submit::Submitter::submit_and_wait` | ~1.23% | ~2.31% |
| `kimojio_stack::run_completed_io` | ~0.80% | ~2.07% |

It also introduced custom-BIO-specific hot spots such as `stack_bio_write`,
`stack_bio_read`, and direct `TlsStream::write_all`/`read_exact_or_eof` frames.

## Why it was rejected

The original assumption was that the current memory-BIO path must be slower
because it copies encrypted bytes through an intermediate buffer. In practice,
`kimojio-stack-tls` uses the existing `kimojio-tls` OpenSSL BIO-pair helper and
exposes OpenSSL's BIO-pair buffers directly with `BIO_nwrite0` and `BIO_nread0`.
The stack runtime reads encrypted bytes directly into OpenSSL's input BIO buffer
and writes encrypted bytes directly from OpenSSL's output BIO buffer.

That means the memory-BIO implementation does not have a separate Rust staging
copy on the hot path. The remaining buffering is OpenSSL's own optimized BIO
pair machinery.

The custom BIO removed some OpenSSL BIO-pair work, but it put Rust callbacks on
OpenSSL's hot path. Each BIO read or write crossed from OpenSSL into Rust, then
into the stack runtime and io_uring scheduler. For the measured workloads, the
saved copy was smaller than the extra callback, scheduler, and completion
handling overhead.

The custom-BIO design also added more unsafe OpenSSL FFI surface area, more
state tracking, and more partial-write complexity. Since it was slower on the
target RPC workload and more complex to maintain, the experiment was removed.

## Future direction

Keep the memory-BIO implementation as the default TLS stack integration. Future
TLS performance work should focus on:

1. Reducing runtime/io_uring dispatch overhead for the existing memory-BIO path.
2. Preserving direct use of OpenSSL BIO-pair buffers.
3. Measuring with client and server on separate OS threads before drawing
   conclusions from same-runtime microbenchmarks.
4. Avoiding custom BIOs unless a future workload shows a clear, repeatable win
   that outweighs the added unsafe code and scheduler callback overhead.
