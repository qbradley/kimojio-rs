# TLS CPU Offload

`kimojio-stack-tls` can optionally move large OpenSSL `SSL_read` and
`SSL_write` calls to a small worker pool. The stackful task parks while the
worker performs encryption or decryption, so the runtime thread can continue
running other stackful tasks instead of spending that interval in CPU-bound
crypto.

The offload path is deliberately above the socket I/O layer. Encrypted bytes
still move directly between the socket and OpenSSL's memory BIO buffers through
`RuntimeContext::read` and `RuntimeContext::write`; only the OpenSSL operation
that transforms plaintext to ciphertext, or ciphertext to plaintext, is moved to
another thread. This preserves the current memory-BIO hot path described in
`CUSTOM_BIO.md` and avoids reintroducing an extra encrypted-byte staging copy.

## API shape

Offload is opt-in per stream:

```rust
let pool = TlsOffloadPool::new(2)?;
let offload = TlsOffloadConfig::large_records(pool);

let stream = tls_context.client_with_offload(cx, 32 * 1024, socket, offload)?;
```

`TlsOffloadConfig::large_records(pool)` uses 24 KiB read/write thresholds. That
keeps small replies and mid-sized RPC bodies inline while still exercising
offload for near-maximum 32 KiB writes and reads.

`TlsOffloadConfig::always(pool)` is available for comparison experiments. It
offloads every non-empty TLS data write and every non-empty TLS data read once a
complete encrypted input record is buffered.

## Synchronization model

The stream temporarily moves its `TlsServer` handle to an offload worker and
waits for the handle to be returned. `TlsServer` is `Send` but not `Sync`, so the
OpenSSL object is never accessed concurrently. Completion is signaled through an
atomic futex word:

1. Each stream is assigned to one worker queue when offload is enabled. This
   preserves per-stream ordering and avoids a single mutex-protected global
   receiver.
2. The stackful task sends a job to its assigned offload worker and parks on
   `RuntimeContext::futex_wait` when the kernel supports io_uring futex waits.
3. The worker finishes `SSL_read` or `SSL_write`, stores the result, sets the
   futex word, and wakes the waiter with a userspace futex wake.
4. The stackful task resumes, restores the `TlsServer`, and handles
   `Success`, `WantRead`, `WantWrite`, `Eof`, or failure exactly as the inline
   path would.

Read/decrypt offload is additionally gated on OpenSSL BIO-pair readiness: the
stream only offloads `SSL_read` after `kimojio-tls` can see that a complete
encrypted TLS record is buffered. This avoids paying a worker handoff just to get
`WantRead`. The BIO pair must be large enough to hold a full encrypted record;
use at least 32 KiB for this POC.

If io_uring futex wait support is unavailable, the POC falls back to cooperative
yielding until the worker completes. That keeps the OS thread unblocked, but it
is less efficient than a true kernel wait and should be treated as a portability
fallback rather than the target path.

## Expected performance tradeoff

Offload has fixed costs:

- one job enqueue and worker wakeup,
- one futex wait and wake,
- moving cache ownership of the TLS object,
- copying plaintext into a worker-owned buffer on write,
- copying decrypted plaintext back into the caller buffer on read.

Those costs mean always-offload should usually lose for small records, handshake
progress calls, and I/O-bound connections. It is useful as an upper-bound
experiment that shows the cost of maximum scheduler relief.

Threshold offload is the intended mode. It should become profitable only when
the crypto work is large enough that freeing the stack runtime thread is worth
the queueing, synchronization, and copy costs. A good threshold depends on CPU,
cipher suite, record size, runtime load, and the amount of unrelated work ready
to run on the stack scheduler. The initial measurements to collect are:

- inline baseline throughput and tail latency,
- always-offload throughput and tail latency,
- threshold sweeps for read and write independently,
- scheduler availability while a large TLS operation is in progress,
- offload pool queue depth and worker utilization.

The expected best result is not maximum raw TLS throughput for a single
connection. It is lower tail latency for mixed workloads where the runtime
thread has useful non-crypto work to schedule while large TLS records are being
encrypted or decrypted elsewhere.

## RPC write benchmark

The `rpc_write` Criterion benchmark exercises the prior custom-BIO workload
shape with a 64-byte request header, 8/16/23/24/32 KiB body, and 64-byte
response:

```sh
cargo bench -p kimojio-stack-tls --bench rpc_write
```

It has two three-connection topologies:

- `rpc_write/client_fanout`: one client-side `Runtime` creates three stackful
  tasks that write to three server runtimes on separate OS threads. Client-side
  offload is enabled in the offload variants.
- `rpc_write/server_fanin`: three client runtimes on separate OS threads write
  to one server-side `Runtime` with three stackful tasks. Server-side offload is
  enabled in the offload variants.

Each topology compares inline TLS, threshold offload with 8 KiB and 24 KiB
thresholds, and always-offload. The benchmark asserts that the configured side
actually uses the offload path when the body reaches the selected threshold, so a
successful run validates both the topology and the offload routing.

On the current POC, offload remains slower than inline whenever it actually runs:

| Topology | Body | Inline | 8 KiB threshold | 24 KiB threshold | Always |
| --- | ---: | ---: | ---: | ---: | ---: |
| client fanout | 8 KiB | 22.7 us | 31.9 us | 22.4 us | 42.9 us |
| client fanout | 16 KiB | 26.1 us | 34.2 us | 26.0 us | 43.5 us |
| client fanout | 23 KiB | 30.0 us | 36.3 us | 31.0 us | 52.9 us |
| client fanout | 24 KiB | 29.9 us | 42.7 us | 36.9 us | 54.7 us |
| client fanout | 32 KiB | 32.1 us | 56.0 us | 45.6 us | 69.3 us |
| server fanin | 8 KiB | 16.0 us | 23.6 us | 17.5 us | 35.4 us |
| server fanin | 16 KiB | 18.6 us | 24.6 us | 18.6 us | 35.9 us |
| server fanin | 23 KiB | 20.9 us | 26.2 us | 20.5 us | 45.4 us |
| server fanin | 24 KiB | 21.0 us | 34.3 us | 27.0 us | 46.6 us |
| server fanin | 32 KiB | 25.8 us | 32.3 us | 31.6 us | 47.6 us |

The 24 KiB threshold is useful as a guardrail because it keeps small replies and
sub-threshold bodies near inline performance. It does not make the current
offload handoff profitable. The remaining gap is the fundamental difference from
FreeBSD kTLS: this POC still waits for the offloaded OpenSSL call before the
`read` or `write` returns, while kTLS queues records into the socket layer and
lets workers make those records ready later.

## Open questions

The POC currently copies plaintext because the worker pool requires owned
`'static` jobs. A scoped or registered-buffer design could reduce copies, but it
would need to preserve the same rule that the OpenSSL handle is moved to exactly
one thread at a time.

The next design step is a true record queue: application writes enqueue plaintext
records, crypto workers produce encrypted records, and stackful tasks only wait
when the queue is full or a read has no decrypted record ready. That would match
FreeBSD kTLS more closely than the current synchronous offload call.
