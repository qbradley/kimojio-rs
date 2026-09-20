# HTTP/2 clients and servers

`kimojio-http2` provides concurrent async requests over established native descriptors and `SplittableStream` transports.
The direct `kimojio-fsm-http2` engine owns HTTP/2 framing, HPACK, flow control, and protocol policy.
This crate does not instantiate an HTTP/1 parser.

Both transports use the same client, concurrent server, body, metadata, and connection driver.
This crate does not supply a DNS resolver, connection pool, TLS adapter, or ALPN policy.
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

## Established stream transports

`connect(stream, configuration)` accepts any established `kimojio::SplittableStream`.
It returns the same `Client` and a caller-polled `Connection<S>`.
Neither constructor performs I/O.
`Connection::run` splits the stream and drives the connection.
`serve_connection` and `serve_connection_with_shutdown` provide the corresponding concurrent server APIs.

```rust,no_run
use kimojio::SplittableStream;
use kimojio_http2::{
    Client, Config, Connection, Error, OutgoingBody, Shutdown, connect,
    serve_connection_with_shutdown, http::Response,
};

fn client<S: SplittableStream>(stream: S) -> (Client, Connection<S>) {
    connect(stream, Config::default())
}

async fn server<S: SplittableStream>(stream: S, shutdown: Shutdown) -> Result<(), Error> {
    serve_connection_with_shutdown(stream, Config::default(), shutdown, |request| async move {
        Ok(Response::new(OutgoingBody::from_incoming(request.into_body())))
    }).await
}
```

The caller establishes the transport and completes any TLS handshake or protocol negotiation before these calls.
The wrapper performs none of those operations.
An `https` URI does not enable TLS.
Generic and native connections have identical body, informational, observer, admission, and shutdown contracts.
Handlers and producers retain separate native tasks and cancellation scopes.

Generic reads use `AsyncStreamRead::try_read`.
Transport implementations own buffering, readiness, and retry policy.
Generic writes use `AsyncStreamWrite::writev`, which promises the entire offered range only on success.
A started write that fails reports `Progress::AtLeast(0)` for that operation, because the trait does not expose partial acceptance.

The core adds any previously confirmed progress for the same buffer to the final receipt.
Thus, a failed DATA receipt has a lower-bound `accepted` count and `exact == false`.
The wrapper does not replay that buffer.

Cancellation before the first transport call can report exact zero.
A late successful write still reports the entire offered range, even after a cancellation request.
The native adapter still reports the exact count from one native `writev` completion.
Neither adapter reports success when it queues an operation.

Generic cancellation requests cancellation of native I/O within that operation's scope.
The original future, operation, and buffers remain alive until the original settles.
The driver also cancels write-all continuations that start after a positive late completion.
It cannot forcibly complete an arbitrary non-native future that ignores cancellation.
Such a transport must eventually settle its original operation before the connection can close.

The driver retains the first unexpected generic read or write error as `Error::Transport`.
It also exposes that `Errno` through `std::error::Error::source`.
If transport I/O and close both fail, `Error::TransportAndClose` retains both errors.
Expected cancellation during shutdown does not replace the core connection result.
A split failure returns its original transport error.
The consumed stream implementation owns cleanup after a failed split.

## Server use

Call `serve_connection_native` with an established descriptor, configuration, and handler.
For external shutdown control, call `serve_connection_native_with_shutdown` with a `Shutdown::default()` handle.
Keep the server future polled until it returns.

```rust,no_run
use kimojio_http2::{
    Config, Error, OutgoingBody, Shutdown, serve_connection_native_with_shutdown,
    http::Response,
};

async fn serve(fd: kimojio::OwnedFd, shutdown: Shutdown) -> Result<(), Error> {
    serve_connection_native_with_shutdown(fd, Config::default(), shutdown, |request| async move {
        Ok(Response::new(OutgoingBody::from_incoming(request.into_body())))
    }).await
}
```

The handler accepts `http::Request<IncomingBody>` and returns a fallible future of `http::Response<OutgoingBody>`.
Each handler future runs in a separate native task with its own Kimojio I/O scope.
Native I/O belongs inside that future, not the synchronous handler factory.
The factory can borrow caller state, but each returned future must be `'static`.

Incoming requests preserve method, URI, HTTP/2 version, duplicate fields, and header sensitivity.
`HeaderMap` preserves value order within one field name, but not global occurrence order across different names.
This applies to regular headers and trailers.
Conventional-wrapper fixtures must compare field names and their ordered values, not a global wire-occurrence list.
Field-name comparison is case-insensitive.
Fixtures must preserve same-name values and their order.
Requests with an authority use absolute URIs.
Classic CONNECT uses an authority-form URI.
Requests without an authority use the received path.

Handlers return final responses.
An informational status from a handler returns a stream error, not a completed response.
The optional informational sender emits interim heads before the handler returns its final response.

A pending handler or response source does not stop sibling streams.
A failed or panicking handler resets only its stream.
The driver also cancels that stream's scoped tasks after a peer reset or connection shutdown.
An excess request receives `REFUSED_STREAM` when wrapper stream slots are full.

Dropping an unread server request body closes its application queue and releases queued leases.
The driver discards subsequent request data and returns its core credit until receive completion.
This policy does not reset the response or silently stop the peer upload.
An explicit `IncomingBody::cancel` still resets both stream halves.

Use `frame` to observe request EOF inside a handler.
Do not await request `completion` before you return the response.
Full retirement requires the response half to settle, so that wait can deadlock.

## Optional informational responses

`Client::send_with_informational(request, callback)` reports actual received 1xx heads through `http::Response<()>`.
The callback runs synchronously on the connection driver before the final response returns.
The callback must not block.
The callback owns its captures (`'static`).
A callback panic resets only its stream.
Ordinary `send` calls continue to consume informational responses internally.

`request.body().informational_sender()` provides optional server emission control.
Client response bodies return `None` from that method.
The control supports 1xx responses except 101.
The handler still returns its final response through the ordinary handler result.

```rust,no_run
use kimojio_http2::{
    Client, Error, IncomingBody, OutgoingBody,
    http::{Request, Response},
};

async fn observe(client: &Client) -> Result<Response<IncomingBody>, Error> {
    let request = Request::builder()
        .uri("https://example.com/resource")
        .body(OutgoingBody::empty())
        .unwrap();
    client.send_with_informational(request, |head| {
        eprintln!("received informational status {}", head.status());
    }).await
}

async fn handler(request: Request<IncomingBody>) -> Result<Response<OutgoingBody>, Error> {
    let sender = request.body().informational_sender().expect("server request");
    let hints = Response::builder()
        .status(103)
        .header("link", "</app.css>; rel=preload")
        .body(())
        .unwrap();
    sender.send(hints).await?;
    Ok(Response::new(OutgoingBody::empty()))
}
```

`InformationalSender::send` succeeds after the core accepts and encodes the metadata.
This result is not a socket write acknowledgement or proof of peer receipt.
The client callback observes decoded peer traffic, not local metadata acceptance.

One informational command per stream can await admission.
Cloned senders share that bound, and another concurrent command returns `Error::Limit`.
A blocked command retains its metadata until `admission_changed`.
A final response waits behind an earlier pending informational command.
Neither accepted informational heads nor accepted final heads enter the retry queue again.

Dropping an informational send future before admission prevents that head from committing.
Dropping it after admission cannot undo the committed head.
An invalid informational command returns an error without committing metadata.
The handler can handle that error and still return a valid final response.
New informational sends fail after the driver starts final-response admission.

Ordinary requests allocate no informational callback, sender state, or command queue.
The callback box exists only for `send_with_informational`.
Server control state appears only when the handler requests an informational sender.
Each optional send owns one metadata section and one local admission-result channel.
`max_response_storage` also bounds that optional pending section.

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

The API exposes separate observations:

| Observation | Meaning |
| --- | --- |
| `Client::send` returns | Final response headers are available. The upload can remain active. |
| `frame` returns `None` | The receive half completed successfully. `receive_outcome` exposes the core receive outcome. |
| `completion` returns | Both halves and all body leases settled. The stream retired. |
| `retirement` returns | The report contains the actual core outcome, receive outcome, failed-buffer receipt, and contextual error separately. |

Consume the receive body before you await `completion`.
Release all chunks from that body before you await `completion`.
Otherwise, the completion wait can wait for a lease that the application still holds.
Cancelling a completion wait does not consume its result.
`collect` copies payload bytes and discards trailers.

A final response status or receive END_STREAM does not cancel an upload.
Dropping an incomplete incoming client body requests stream-local cancellation.
The driver drains core completion notifications before it applies that abandonment.
This ordering preserves uploads when response completion follows headers, DATA, or trailers.

`IncomingBody::cancel` explicitly cancels the stream, including an upload after receive EOF.
A completed response remains valid if a later upload failure occurs.
That failure remains observable through `completion`.
Failed send receipts preserve the exact accepted count and the core exactness flag in `Error::Send`.
`completion` retains its conventional error precedence.
Thus, a failed DATA receipt can take precedence over a reset outcome in that method.
`retirement` returns a `StreamReport` whose `outcome` always comes directly from the core retirement event.
It does not infer that outcome from a send receipt.

For a complete response followed by `RST_STREAM(NO_ERROR)`, receive completion can remain `Complete` while retirement reports `Reset(0)`.
The report can also contain a failed-buffer receipt with reason `Reset(0)`.
The receipt's accepted count belongs to that buffer, not the complete upload.
`Error::Closed` from `retirement` means that no report arrived, not that the core reported a particular outcome.
Both completion methods share a cancellation-safe cached report.

## Retirement without response headers

`Client::send_with_observer(request, observer)` adds optional lifecycle observation.
`RequestObserver::admitted` receives the actual stream ID after the core accepts request metadata.
Admission is not a transport write acknowledgement.
`receive_end` and `retired` report their corresponding core events.
The observer remains attached after a pre-header `send` error, so no response body is necessary to observe retirement.
The observer can also receive informational heads through its `informational` method.

The following observer publishes an application-owned retirement handle:

```rust,no_run
use std::{cell::Cell, rc::Rc};
use kimojio::{SenderOneshot, oneshot};
use kimojio_http2::{
    Client, Error, OutgoingBody, RequestObserver, StreamId, StreamReport,
    http::Request,
};

struct Trace {
    id: Rc<Cell<Option<StreamId>>>,
    done: Option<SenderOneshot<StreamReport>>,
}
impl RequestObserver for Trace {
    fn admitted(&mut self, id: StreamId) {
        self.id.set(Some(id));
    }
    fn retired(&mut self, report: &StreamReport) {
        if let Some(done) = self.done.take() {
            let _ = done.send(report.clone());
        }
    }
}

async fn trace(client: &Client) -> Result<StreamReport, Error> {
    let id = Rc::new(Cell::new(None));
    let (done, retired) = oneshot();
    let request = Request::builder()
        .uri("https://example.com/resource")
        .body(OutgoingBody::empty())
        .unwrap();
    let observer = Trace { id: id.clone(), done: Some(done) };
    match client.send_with_observer(request, observer).await {
        Ok(mut response) => {
            let _ = response.body_mut().collect(1024 * 1024).await;
        }
        Err(error) if id.get().is_none() => return Err(error),
        Err(_) => {}
    }
    retired.recv().await.map_err(|_| Error::Closed)
}
```

The driver allocates an observer box only for the optional method.
It does not create a lifecycle event queue.
Ordinary body retirement uses the existing completion channel.
Observers must not block.
A callback panic disables that observer and becomes a contextual application error.
The driver resets a live stream after that failure, but never changes an already reported core outcome.

An unadmitted rejection or queued cancellation emits neither an admitted ID nor a retirement event.
Dropping the driver can also prevent retirement delivery.
The observer then drops, so an application-owned oneshot closes instead of delivering an invented outcome.
Consume or drop any returned response body before you await a separate retirement handle.

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
| Native handler tasks | 128 |
| One pending server response's owned fields and ready body | 256 KiB |
| One optional pending informational section | 256 KiB |
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
`max_response_storage` bounds each pending server response.
Stream slots remain reserved until the core record and its handler and producer tasks all settle.
These reservations also bound response queues and tasks after core retirement.

Native channels carry requests, producer output, incoming frames, and release receipts.
Core leases bound the number and retained pages of queued incoming data.
Source permits bound queued producer output.
The driver explicitly closes and drains body queues during abandonment and failed receive teardown.

Receive-capacity bounds count retained pages, not only visible bytes.
Small transport reads and fragmented forwarding can retain several pages for a small amount of body data.
Insufficient per-stream capacity causes the core's resource-limit reset, even when the peer stays within its flow-control window.
The fragmented duplex test uses a 2 MiB per-stream receive-capacity bound and unchanged receive windows.
Applications can choose that bound through `Config::protocol.max_stream_receive_capacity`.

These bounds exclude allocator overhead, caller-owned requests before submission, and storage inside opaque user streams, handler futures, callback captures, and application errors.
They also exclude application copies from `collect`, transport-owned buffers, and native runtime bookkeeping.
The wrapper cannot inspect memory that a user future owns.
Completed responses and application-retained metadata are outside the active-stream bound.

## Cancellation and closure

A dropped queued send future sets its cancellation marker before admission.
An admitted cancellation resets only its stream.
Read and write slots remain independent across sibling streams.
Source cancellation does not release storage from an original transport operation.

`client.control().graceful()` stops admission and asks the core to drain.
`client.control().abort()` supersedes graceful shutdown and requests hard cancellation.
Server functions accept the same control through an external `Shutdown` handle.
The policies never move from abort back to graceful.
Protocol, reset, GOAWAY, producer, and transport errors do not become success-shaped responses.

The driver calls native `close` after original descriptor references settle.
For generic streams, it drops the settled read half before it awaits `AsyncStreamWrite::close`.
It does not replace full close with write-side `shutdown`.
A close failure returns an error.
Read-only body chunks can remain valid after descriptor closure.
The driver continues to process their releases and stream retirements.
Thus, held chunks can delay `run` after the socket closes.

The driver uses `kimojio::clock_now` and supports the `virtual-clock` feature.
Cancelled alarms report cancellation after their native scope settles.
They do not report a fabricated deadline or a future timestamp.
The core's graceful deadline can terminate unfinished streams while the connection result remains `Graceful`.
Individual stream results remain separate and can report failure.

## Differences from the HTTP/1 wrapper

- HTTP/2 client handles are clonable and accept concurrent shared-reference sends.
- Configuration uses `Config::default` and has no external connection ID.
- Requests require an absolute URI, except classic CONNECT.
- The established transport selects HTTP/2, regardless of `Request::version`.
- Streaming constructors do not take a length argument. An optional `content-length` header remains subject to core enforcement.
- Full bodies do not split automatically. Larger messages use bounded stream frames.
- Receive EOF and full stream completion have separate methods.
- Client shutdown control comes from the client. Server shutdown control can come from the caller.
- Informational observation and emission are optional per-request capabilities.

## Shared implementation boundary

`body.rs` owns payload variants, read-only leases, incoming queues, and public body methods.
`metadata.rs` converts `http` fields and preserves duplicate values and sensitivity.
It retains outbound fields until a borrowed core command succeeds.

`io.rs` owns independent reusable pinned read and write slots.
Its private `Io` interface installs operations directly from core callbacks.
One native `writev` result supplies one exact progress receipt.
Original futures survive cancellation requests and late successful completions.
The shared run loop occupies one pinned box per connection to limit nested future stack size.

`io/stream.rs` implements the same private interface for generic stream halves.
Each half has one connection-lifetime box, which avoids repeated moves of large inline transport buffers.
Each reusable slot owns its half and core operation until completion.
Generic transport calls use static dispatch, without per-operation boxed futures.
Native operations do not pass through the generic adapter.
Both adapters share timers and monotonic clock handling.

`driver.rs` owns both core roles, stream records, admission retries, scoped tasks, cancellation, and channel scheduling.
Application commands execute after the core callback returns.
Only `CommandError::Blocked` queues a response or trailer retry after `admission_changed`.
Accepted metadata never returns to that retry queue.
`informational.rs` owns optional server controls, bounded pending commands, and their cancellation generations.
Producer notifications identify their stream without a scan of every stream after transport completion.
Runnable channel probes do not register an empty-channel wait.
Client and server use the same body ownership, transport adapters, event scheduling, producer scopes, and retirement code.

## Qualification boundary

The [wrapper contract matrix](tests/CONTRACTS.md) lists exact tests, observable sequences, and remaining qualification gaps.

The native suite uses real Kimojio socket operations between the public client and server APIs.
A separate client suite uses a small direct-core server fixture.
It covers repeated and concurrent requests, traffic beyond actual windows, trailers, classic CONNECT, forwarding, and early final responses.
It also covers cancellation, admission pressure, bounded queues, scoped producer I/O, GOAWAY, held chunks, actual closure, and virtual time.
Server tests cover unread requests, handler failures, scoped handler I/O, bodyless statuses, response admission, and shutdown deadlines.
Informational tests observe actual 100 and 103 heads, metadata rejection, cancellation, final-response ordering, and admission pressure.
Generic tests use real sockets through `OwnedFdStream` and controllable transport implementations.
They cover mixed native/generic roles, concurrent duplex forwarding, CONNECT, trailers, observers, paused consumers, and early responses.
They also cover scoped cancellation, partial-write failures without replay, source errors, failed close, and held chunks after close in both roles.
Cancellation tests retain late read/write successes and cancel write-all continuations without stopping unrelated native I/O.
The virtual-clock test expires a generic server's graceful deadline and observes actual close after handler settlement.

The late-success unit test checks original-future settlement without a kernel race prerequisite.
The suite does not inject every native cancellation race or a failed native close.
The [implementation assessment](../docs/http2-wrapper-report.md) records final-source independent peer results and measured runtime costs.
The [qualification ledger](../docs/http2-qualification.md) preserves exact sources, binary hashes, and exclusions.
Measurements use local UNIX socketpairs, not TLS or remote-network workloads.
They include both endpoints, application work, and payload assertions rather than isolated wrapper overhead.

A send permit reserves storage and is not a handshake barrier.
The wrapper neither parses startup frames nor infers readiness from admission notifications.
No readiness API is necessary for the diagnosed strict-peer startup failures.
A bodyless warmup can synchronize a later upload through ordinary response handling.
An independent peer can instead advertise default credit before a later window reduction and account for legal in-flight DATA.
Neither approach represents the unchanged cold-first-request case.

A pre-header `Error::Send` with reason `ConnectionFailed` does not establish the underlying connection cause.
Qualification must retain the actual retirement report and connection-driver result.

The startup diagnosis used wrapper `b9bd2445` and peer `9dfd57a8`.
The frozen-client startup trace recorded 49,152 DATA bytes before any server bytes, within the default 65,535-byte allowance.
Strict peers rejected that DATA against a 1,024-byte advertisement before the client received it.
Bodyless-warmup and public peer-configuration alternatives each echoed 131,087 bytes correctly.
Those peer-generated closures do not establish a wrapper flow-control defect.
This change does not alter startup behavior or add a readiness API.
