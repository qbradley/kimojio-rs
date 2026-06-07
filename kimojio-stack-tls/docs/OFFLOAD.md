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
let offload = TlsOffloadConfig::new(pool)
    .with_read_threshold(32 * 1024)
    .with_write_threshold(32 * 1024);

let stream = tls_context.client_with_offload(cx, 16 * 1024, socket, offload)?;
```

`TlsOffloadConfig::always(pool)` is available for comparison experiments. It
offloads every non-empty TLS data read and write, including operations that are
likely to return `WantRead` or `WantWrite` immediately.

## Synchronization model

The stream temporarily moves its `TlsServer` handle to an offload worker and
waits for the handle to be returned. `TlsServer` is `Send` but not `Sync`, so the
OpenSSL object is never accessed concurrently. Completion is signaled through an
atomic futex word:

1. The stackful task sends a job to the offload pool and parks on
   `RuntimeContext::futex_wait` when the kernel supports io_uring futex waits.
2. The worker finishes `SSL_read` or `SSL_write`, stores the result, sets the
   futex word, and wakes the waiter with a userspace futex wake.
3. The stackful task resumes, restores the `TlsServer`, and handles
   `Success`, `WantRead`, `WantWrite`, `Eof`, or failure exactly as the inline
   path would.

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

## Open questions

The POC currently copies plaintext because the worker pool requires owned
`'static` jobs. A scoped or registered-buffer design could reduce copies, but it
would need to preserve the same rule that the OpenSSL handle is moved to exactly
one thread at a time. The offload pool is also intentionally simple; a production
version should measure whether per-runtime pools, per-core pools, bounded
queues, or direct handoff to dedicated crypto threads provide better tail
latency under overload.
