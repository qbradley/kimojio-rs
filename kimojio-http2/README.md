# Native HTTP/2 client

`kimojio-http2` provides concurrent async requests over an established Kimojio descriptor.
The direct `kimojio-fsm-http2` engine owns HTTP/2 framing, HPACK, flow control, and protocol policy.
This crate does not instantiate an HTTP/1 parser.

This checkpoint contains a native client and shared body, metadata, and native I/O components.
It does not contain a server, generic transport, DNS resolver, connection pool, TLS adapter, or ALPN policy.
The caller establishes the connection and selects HTTP/2.

## Use

Keep `NativeConnection::run` polled alongside the application.
Use an absolute `http` or `https` request URI.
For classic CONNECT, use an authority-form URI, such as `example.com:443`.

```rust,no_run
use kimojio_http2::{Config, Error, OutgoingBody, connect_native, http::Request};

async fn exchange(fd: kimojio::OwnedFd) -> Result<Vec<u8>, Error> {
    let (client, connection) = connect_native(fd, Config::default());
    let application = async move {
        let request = Request::builder()
            .uri("https://example.com/resource")
            .body(OutgoingBody::empty())
            .unwrap();
        let mut response = client.send(request).await?;
        let bytes = response.body_mut().collect(1024 * 1024).await?;
        response.body_mut().completion().await?;
        client.control().graceful();
        Ok::<_, Error>(bytes)
    };
    let (response, closed) = futures::join!(application, connection.run());
    closed?;
    response
}
```

`Client` is clonable and supports concurrent `send(&self, request)` calls.
The driver owns one connection.
The runtime uses thread-local ownership, so these handles are not `Send`.
Dropping the last client handle requests graceful shutdown.

## Bodies and completion

`OutgoingBody::empty`, `full`, and `from_static` need no boxed producer stream or producer task.
`full` retains the original `Vec` allocation.
`from_static` retains no heap allocation for its payload.
Ready data must fit one send buffer.

`OutgoingBody::from_stream` accepts fallible data frames and a terminal trailer section.
Each admitted streaming body uses one native task with its own Kimojio I/O scope.
That task polls its source only after a core send permit.
Empty nonterminal data frames do not mean EOF.
A zero-byte permit still requires producer EOF, terminal empty DATA, or trailers.

`IncomingBody::frame` returns read-only `BodyChunk` leases and trailers.
Dropping a chunk returns its lease through a native channel.
`OutgoingFrame::Forward` and `OutgoingBody::from_incoming` transfer those leases without a payload copy.
The outgoing core retains a forwarded lease until the original write receipt, not queue insertion or cancellation acceptance.

The API exposes three separate observations:

| Observation | Meaning |
| --- | --- |
| `Client::send` returns | Final response headers are available. The upload can remain active. |
| `frame` returns `None` | The receive half completed successfully. `receive_outcome` exposes the core receive outcome. |
| `completion` returns | Both halves and all body leases settled. The stream retired. |

Consume the receive body before you await `completion`.
Release all chunks from that body before you await `completion`.
Otherwise, the completion wait can wait for a lease that the application still holds.
Cancelling a completion wait does not consume its result.
`collect` copies payload bytes and discards trailers.

A final response status or receive END_STREAM does not cancel an upload.
Dropping an incomplete incoming body requests stream-local cancellation.
The driver drains core completion notifications before it applies that abandonment.
This ordering preserves uploads when response completion follows headers, DATA, or trailers.

`IncomingBody::cancel` explicitly cancels the stream, including an upload after receive EOF.
A completed response remains valid if a later upload failure occurs.
That failure remains observable through `completion`.
Failed send receipts preserve the exact accepted count and the core exactness flag in `Error::Send`.

## Bounds

The wrapper rejects oversized caller buffers with `Error::BufferTooLarge`.
It does not clip buffers or report visible length as retained capacity.
Each data submission must fit both limits of the actual `SendPermit`.
The driver does not reconstruct wire credit.

Default bounds include:

| Resource | Default bound |
| --- | --- |
| Pending requests | 64 items |
| Pending owned fields and ready-body allocations | 8 MiB in aggregate |
| Wrapper stream records | 128 |
| Producer tasks, including tasks that still need settlement | 128 |
| One producer's pending owned trailers | 128 KiB |
| One send buffer | 64 KiB visible bytes and 64 KiB retained capacity |
| Receive allocations in the core | 8 MiB per connection |
| Receive retention per stream | 256 KiB and 128 fragments |
| Total HTTP message body | 8 MiB, except established CONNECT tunnels |

`Config` controls the wrapper bounds.
`Config::protocol` controls the core bounds, including total body size and receive windows.
Queue overflow returns `Error::Limit` rather than an unbounded enqueue.
Ready uploads retain at most one checked buffer per stream before core admission.
Each producer retains at most one checked data frame or trailer section outside the core.
The trailer bound applies per producer, not across all producers.

Native channels carry requests, producer output, incoming frames, and release receipts.
Core leases bound the number and retained pages of queued incoming data.
Source permits bound queued producer output.
The driver explicitly closes and drains body queues during abandonment and failed receive teardown.

These bounds exclude allocator overhead, caller-owned requests before submission, and storage inside opaque user streams or application errors.
They also exclude application copies from `collect` and native runtime bookkeeping.
The wrapper cannot inspect memory that a user future owns.
Completed responses and application-retained metadata are outside the active-stream bound.

## Cancellation and closure

A dropped queued send future sets its cancellation marker before admission.
An admitted cancellation resets only its stream.
Read and write slots remain independent across sibling streams.
Source cancellation does not release storage from an original transport operation.

`client.control().graceful()` stops admission and asks the core to drain.
`client.control().abort()` supersedes graceful shutdown and requests hard cancellation.
The policies never move from abort back to graceful.
Protocol, reset, GOAWAY, producer, and transport errors do not become success-shaped responses.

The driver calls native `close` after original descriptor references settle.
A close failure returns an error.
Read-only body chunks can remain valid after descriptor closure.
The driver continues to process their releases and stream retirements.
Thus, held chunks can delay `run` after the socket closes.

The driver uses `kimojio::clock_now` and supports the `virtual-clock` feature.
Cancelled alarms report cancellation after their native scope settles.
They do not report a fabricated deadline or a future timestamp.

## Differences from the HTTP/1 wrapper

- HTTP/2 client handles are clonable and accept concurrent shared-reference sends.
- Configuration uses `Config::default` and has no external connection ID.
- Requests require an absolute URI, except classic CONNECT.
- The established transport selects HTTP/2, regardless of `Request::version`.
- Streaming constructors do not take a length argument. An optional `content-length` header remains subject to core enforcement.
- Full bodies do not split automatically. Larger messages use bounded stream frames.
- Receive EOF and full stream completion have separate methods.
- Shutdown control comes from the client. The driver result reports connection closure.
- Informational responses are consumed internally. This checkpoint has no informational-response callback.

## Shared implementation boundary

`body.rs` owns payload variants, read-only leases, incoming queues, and public body methods.
`metadata.rs` converts `http` fields and preserves duplicate values and sensitivity.
It retains outbound fields until a borrowed core command succeeds.

`io.rs` owns independent reusable pinned read and write slots.
Its private `Io` interface installs operations directly from core callbacks.
One native `writev` result supplies one exact progress receipt.
Original futures survive cancellation requests and late successful completions.

`driver.rs` owns stream records, admission retries, producer tasks, cancellation, and channel scheduling.
Application commands execute after the core callback returns.
Producer notifications identify their stream without a scan of every stream after transport completion.
Runnable channel probes do not register an empty-channel wait.
The server phase can reuse these components under the same implementation owner.

## Qualification boundary

The native suite uses real Kimojio socket operations against a small direct-core server fixture.
It covers repeated and concurrent requests, traffic beyond actual windows, trailers, classic CONNECT, forwarding, and early final responses.
It also covers cancellation, admission pressure, bounded queues, scoped producer I/O, GOAWAY, held chunks, actual closure, and virtual time.

The late-success unit test checks original-future settlement without a kernel race prerequisite.
The suite does not inject every native cancellation race or a failed native close.
Independent peers, server support, generic transports, broader adversarial qualification, and performance measurements remain separate phases.
This checkpoint makes no performance claim.
