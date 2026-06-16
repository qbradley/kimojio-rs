// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Transport abstraction and stack HTTP adapter.
//!
//! Storage operation modules produce [`RequestParts`] and consume
//! [`ResponseParts`]. A [`Transport`] implementation owns the wire details for a
//! single attempt: converting request parts to HTTP, applying deadlines,
//! streaming success body chunks, and returning diagnostics on failure.
//!
//! # Runtime migration boundary
//!
//! [`Transport`] remains the storage-facing boundary. `StackHttpTransport` is the
//! stack-core alias for [`RuntimeStackHttpTransport`], which adapts generic
//! protocol-neutral HTTP clients for any runtime/socket family implementing the
//! shared HTTP socket contract without changing request construction, retry
//! classification, or diagnostics.
//!
//! ```
//! use kimojio_stack_storage::{OperationClass, RequestParts, ReplayBody};
//!
//! let mut request = RequestParts::new(OperationClass::Block, "PUT", "/acct/container/object");
//! request.body = ReplayBody::from_vec(b"payload".to_vec());
//! request.metadata.insert("content-length", "7");
//! assert!(request.body.as_bytes().is_some());
//! ```

use std::time::{Duration, Instant};
use std::{fmt, marker::PhantomData};

use bytes::Bytes;
use kimojio_stack::{IoFd, RuntimeSocket};
use kimojio_stack_http::HttpRuntime;

use crate::{
    AttemptDiagnostics, Diagnostics, Error, ErrorKind, MetadataMap, OperationClass, PoolConfig,
    ReplayBody,
};

/// Caller-visible request parts for a storage operation.
///
/// The request is deliberately decomposed into method, URI, metadata, replay
/// body, and deadline fields so retry and diagnostics code can inspect costs and
/// safety before a transport sends bytes.
#[derive(Clone, Eq, PartialEq)]
pub struct RequestParts {
    pub operation: OperationClass,
    pub method: String,
    pub uri: String,
    pub metadata: MetadataMap,
    pub body: ReplayBody,
    pub deadline: Option<Duration>,
    pub canceled: bool,
}

impl fmt::Debug for RequestParts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestParts")
            .field("operation", &self.operation)
            .field("method", &self.method)
            .field("uri", &redacted_uri(&self.uri))
            .field("metadata", &self.metadata)
            .field("body", &self.body)
            .field("deadline", &self.deadline)
            .field("canceled", &self.canceled)
            .finish()
    }
}

impl RequestParts {
    /// Creates request parts for one operation.
    pub fn new(
        operation: OperationClass,
        method: impl Into<String>,
        uri: impl Into<String>,
    ) -> Self {
        Self {
            operation,
            method: method.into(),
            uri: uri.into(),
            metadata: MetadataMap::new(),
            body: ReplayBody::Empty,
            deadline: None,
            canceled: false,
        }
    }
}

fn redacted_uri(uri: &str) -> String {
    if uri.contains("sig=") || uri.contains("sig%3d") {
        uri.split('?')
            .next()
            .map_or_else(|| "<redacted>".to_owned(), str::to_owned)
    } else {
        uri.to_owned()
    }
}

/// Caller-visible response parts for a storage operation.
///
/// `ResponseParts` contains response metadata and diagnostics. Success bodies are
/// delivered by the transport chunk callback instead of being stored here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponseParts {
    pub status: u16,
    pub metadata: MetadataMap,
    pub diagnostics: Diagnostics,
}

/// Failed request attempt with diagnostics captured before returning.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttemptError {
    pub error: Error,
    pub diagnostics: Diagnostics,
}

/// Storage transport abstraction used by retry/execution code.
///
/// Implement this trait for custom test doubles, pooled transports, or service
/// connectors. Implementations should report one attempted request and include
/// whatever diagnostics are available when an error occurs.
/// Runtime-family marker used by storage transports.
pub trait StorageRuntime {
    /// Context type supplied to storage client operations.
    type Context<'cx>;
}

/// Default storage runtime marker for the single-threaded `kimojio-stack` runtime.
pub struct StackStorageRuntime;

impl StorageRuntime for StackStorageRuntime {
    type Context<'cx> = kimojio_stack::RuntimeContext<'cx>;
}

pub trait Transport<R: StorageRuntime = StackStorageRuntime> {
    /// Executes one request attempt.
    fn execute<'cx>(
        &mut self,
        cx: &'cx R::Context<'cx>,
        request: &RequestParts,
    ) -> Result<ResponseParts, AttemptError>;

    /// Executes one request attempt and delivers response body chunks as they arrive.
    ///
    /// The default implementation fails explicitly. Transports that support
    /// downloads should call `on_chunk` for successful response body data only;
    /// error response bodies should be consumed for connection health and
    /// reported through diagnostics or [`Error`].
    fn execute_with_body_chunks<'cx>(
        &mut self,
        cx: &'cx R::Context<'cx>,
        request: &RequestParts,
        on_chunk: &mut dyn FnMut(Bytes) -> Result<(), Error>,
    ) -> Result<ResponseParts, AttemptError> {
        let _ = cx;
        let _ = on_chunk;
        Err(AttemptError {
            error: Error::new(
                ErrorKind::Transport,
                "transport does not support streaming response chunks",
            ),
            diagnostics: failure_diagnostics(request, None, None, false),
        })
    }
}

/// Runtime-generic adapter from storage request parts to the stack HTTP client.
///
/// The adapter owns one protocol-neutral HTTP connection. It applies
/// request-specific deadlines or a default total timeout from [`PoolConfig`], maps
/// HTTP status codes to storage errors, and streams success body chunks to the
/// caller sink.
pub struct RuntimeStackHttpTransport<R, S> {
    client: kimojio_stack_http::client::RuntimeClientConnection<S>,
    pool_config: Option<PoolConfig>,
    runtime: PhantomData<fn() -> R>,
}

/// Stack-core storage HTTP transport alias preserved for compatibility.
pub type StackHttpTransport = RuntimeStackHttpTransport<StackStorageRuntime, IoFd>;

impl<R, S> RuntimeStackHttpTransport<R, S> {
    /// Creates an adapter from an existing stack HTTP client connection.
    pub fn new(client: kimojio_stack_http::client::RuntimeClientConnection<S>) -> Self {
        Self {
            client,
            pool_config: None,
            runtime: PhantomData,
        }
    }

    /// Creates an adapter that applies request timeout defaults from pool configuration.
    pub fn new_with_pool_config(
        client: kimojio_stack_http::client::RuntimeClientConnection<S>,
        pool_config: PoolConfig,
    ) -> Self {
        Self {
            client,
            pool_config: Some(pool_config),
            runtime: PhantomData,
        }
    }

    fn effective_timeout(&self, request: &RequestParts) -> Option<Duration> {
        request
            .deadline
            .or_else(|| self.pool_config.map(|config| config.total_timeout))
    }
}

impl<R, S> Transport<R> for RuntimeStackHttpTransport<R, S>
where
    R: StorageRuntime,
    S: RuntimeSocket,
    for<'cx> R::Context<'cx>: HttpRuntime<S>,
{
    fn execute<'cx>(
        &mut self,
        cx: &'cx R::Context<'cx>,
        request: &RequestParts,
    ) -> Result<ResponseParts, AttemptError> {
        let mut discard = |_| Ok(());
        self.execute_with_body_chunks(cx, request, &mut discard)
    }

    fn execute_with_body_chunks<'cx>(
        &mut self,
        cx: &'cx R::Context<'cx>,
        request: &RequestParts,
        on_chunk: &mut dyn FnMut(Bytes) -> Result<(), Error>,
    ) -> Result<ResponseParts, AttemptError> {
        let timeout = self.effective_timeout(request);
        if request.canceled || timeout == Some(Duration::ZERO) {
            return Err(AttemptError {
                error: Error::new(ErrorKind::Timeout, "request deadline expired or canceled"),
                diagnostics: failure_diagnostics(request, None, None, false),
            });
        }
        let mut builder = http::Request::builder()
            .method(request.method.as_str())
            .uri(request.uri.as_str());
        for (name, value) in request.metadata.entries() {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let body = match &request.body {
            ReplayBody::Empty => Ok(kimojio_stack_http::Body::empty()),
            ReplayBody::BorrowedStatic(bytes) => kimojio_stack_http::Body::from_bytes(
                Bytes::from_static(bytes),
                kimojio_stack_http::BodyLimits::new(usize::MAX),
            ),
            ReplayBody::Shared(bytes) | ReplayBody::Owned(bytes) => {
                kimojio_stack_http::Body::from_bytes(
                    bytes.clone(),
                    kimojio_stack_http::BodyLimits::new(usize::MAX),
                )
            }
            ReplayBody::NonReplayable { .. } => Err(kimojio_stack_http::Error::Protocol(
                "non-replayable body cannot be adapted",
            )),
        }
        .map_err(|error| AttemptError {
            error: Error::new(ErrorKind::Transport, error.to_string()),
            diagnostics: failure_diagnostics(request, None, None, false),
        })?;
        let http_request = builder.body(body).map_err(|error| AttemptError {
            error: Error::new(ErrorKind::Transport, error.to_string()),
            diagnostics: failure_diagnostics(request, None, None, false),
        })?;
        let request_id = request
            .metadata
            .get("x-ms-client-request-id")
            .map(str::to_owned);
        let previous_deadline = self
            .client
            .set_io_deadline(timeout.map(|timeout| Instant::now() + timeout));
        let mut sink_error = None;
        let response_result = self.client.send_with_body_chunks_after_headers(
            cx,
            &http_request,
            |response| Ok(!is_error_status(response.status().as_u16())),
            |chunk| match on_chunk(chunk) {
                Ok(()) => Ok(()),
                Err(error) => {
                    sink_error = Some(error);
                    Err(kimojio_stack_http::Error::Protocol(
                        "storage response body sink failed",
                    ))
                }
            },
        );
        self.client.set_io_deadline(previous_deadline);
        let response = response_result.map_err(|error| AttemptError {
            error: sink_error.unwrap_or_else(|| http_error_to_storage(error)),
            diagnostics: failure_diagnostics(request, None, None, true),
        })?;
        let mut metadata = MetadataMap::new();
        for (name, value) in response.headers() {
            metadata.insert(name.as_str(), value.to_str().unwrap_or_default());
        }
        let mut diagnostics = Diagnostics::new(request.operation);
        diagnostics.push_attempt(AttemptDiagnostics {
            attempt: 1,
            status: Some(response.status().as_u16()),
            service_code: metadata.get("x-ms-error-code").map(str::to_owned),
            request_id: metadata
                .get("x-ms-request-id")
                .map(str::to_owned)
                .or(request_id),
            elapsed: None,
            retriable: status_is_retriable(
                response.status().as_u16(),
                metadata.get("x-ms-error-code"),
            ),
        });
        if is_error_status(response.status().as_u16()) {
            let status = response.status().as_u16();
            let service_code = metadata.get("x-ms-error-code").map(str::to_owned);
            let request_id = metadata.get("x-ms-request-id").map(str::to_owned);
            let mut error = Error::new(
                Error::classify_status(status, service_code.as_deref()),
                format!("storage service returned HTTP status {status}"),
            );
            if let Some(service_code) = service_code {
                error = error.with_service_code(service_code);
            }
            if let Some(request_id) = request_id {
                error = error.with_request_id(request_id);
            }
            return Err(AttemptError { error, diagnostics });
        }
        Ok(ResponseParts {
            status: response.status().as_u16(),
            metadata,
            diagnostics,
        })
    }
}

fn http_error_to_storage(error: kimojio_stack_http::Error) -> Error {
    match error {
        kimojio_stack_http::Error::Io(kimojio_stack::Errno::TIME) => {
            Error::new(ErrorKind::Timeout, "HTTP transport timed out")
        }
        error => Error::new(ErrorKind::Transport, error.to_string()),
    }
}

fn status_is_retriable(status: u16, service_code: Option<&str>) -> bool {
    matches!(
        Error::classify_status(status, service_code),
        ErrorKind::Timeout | ErrorKind::Unavailable | ErrorKind::Transport
    )
}

fn is_error_status(status: u16) -> bool {
    (400..=599).contains(&status)
}

fn failure_diagnostics(
    request: &RequestParts,
    status: Option<u16>,
    service_code: Option<String>,
    retriable: bool,
) -> Diagnostics {
    let mut diagnostics = Diagnostics::new(request.operation);
    diagnostics.push_attempt(AttemptDiagnostics {
        attempt: 1,
        status,
        service_code,
        request_id: request
            .metadata
            .get("x-ms-client-request-id")
            .map(str::to_owned),
        elapsed: Some(Duration::ZERO),
        retriable,
    });
    diagnostics
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroUsize, os::fd::OwnedFd, os::unix::net::UnixStream, time::Duration};

    use kimojio_stack::SocketIoRuntime;
    use kimojio_stack_steal::{
        Runtime as StealRuntime, RuntimeConfig as StealRuntimeConfig, StealPolicy,
    };

    use crate::{
        AccountId, AttemptDiagnostics, BlockClient, ContainerName, Error, ErrorKind, ObjectKind,
        ObjectName, ObjectRef,
    };

    use super::*;

    struct StealStorageRuntime;

    impl StorageRuntime for StealStorageRuntime {
        type Context<'cx> = kimojio_stack_steal::RuntimeContext<'cx>;
    }

    struct FakeTransport;

    impl Transport for FakeTransport {
        fn execute(
            &mut self,
            _cx: &kimojio_stack::RuntimeContext<'_>,
            request: &RequestParts,
        ) -> Result<ResponseParts, AttemptError> {
            let mut diagnostics = Diagnostics::new(request.operation);
            diagnostics.push_attempt(AttemptDiagnostics {
                attempt: 1,
                status: Some(200),
                service_code: None,
                request_id: Some("req".into()),
                elapsed: Some(Duration::from_millis(1)),
                retriable: false,
            });
            Ok(ResponseParts {
                status: 200,
                metadata: MetadataMap::new(),
                diagnostics,
            })
        }
    }

    #[test]
    fn fake_transport_executes_request_parts() {
        let mut transport = FakeTransport;
        let request = RequestParts::new(OperationClass::Metadata, "HEAD", "/object");
        let response = kimojio_stack::Runtime::new()
            .block_on(|cx| transport.execute(cx, &request))
            .unwrap();

        assert_eq!(response.status, 200);
        assert_eq!(response.diagnostics.operation(), OperationClass::Metadata);
    }

    #[test]
    fn runtime_migration_boundary_accepts_runtime_marker_transport() {
        fn accepts_storage_boundary<R, T>(_transport: &mut T)
        where
            R: StorageRuntime,
            T: Transport<R>,
        {
        }

        let mut transport = FakeTransport;
        accepts_storage_boundary::<StackStorageRuntime, _>(&mut transport);
    }

    #[test]
    fn request_parts_preserve_body_replay() {
        let mut request = RequestParts::new(OperationClass::PageWrite, "PUT", "/page");
        request.body = ReplayBody::NonReplayable { len: Some(512) };

        assert_eq!(request.body.replay(), crate::BodyReplay::NonReplayable);
        assert_eq!(
            Error::new(ErrorKind::Transport, "example").kind(),
            ErrorKind::Transport
        );
    }

    #[test]
    fn request_debug_redacts_signed_uri_and_body_bytes() {
        let mut request = RequestParts::new(OperationClass::Copy, "PUT", "/object?sig=secret");
        request.body = ReplayBody::BorrowedStatic(b"secret-payload");
        let debug = format!("{request:?}");

        assert!(!debug.contains("sig=secret"));
        assert!(!debug.contains("secret-payload"));
        assert!(debug.contains("len"));
    }

    struct FailingTransport;

    impl Transport for FailingTransport {
        fn execute(
            &mut self,
            _cx: &kimojio_stack::RuntimeContext<'_>,
            request: &RequestParts,
        ) -> Result<ResponseParts, AttemptError> {
            let mut diagnostics = Diagnostics::new(request.operation);
            diagnostics.push_attempt(AttemptDiagnostics {
                attempt: 1,
                status: Some(503),
                service_code: Some("Busy".into()),
                request_id: Some("req-fail".into()),
                elapsed: Some(Duration::from_millis(2)),
                retriable: true,
            });
            Err(AttemptError {
                error: Error::new(ErrorKind::Unavailable, "busy"),
                diagnostics,
            })
        }
    }

    #[test]
    fn failing_transport_preserves_attempt_diagnostics() {
        let mut transport = FailingTransport;
        let request = RequestParts::new(OperationClass::PageRead, "GET", "/object");
        let error = kimojio_stack::Runtime::new()
            .block_on(|cx| transport.execute(cx, &request))
            .unwrap_err();

        assert_eq!(error.error.kind(), ErrorKind::Unavailable);
        assert_eq!(
            error.diagnostics.attempts()[0].request_id.as_deref(),
            Some("req-fail")
        );
    }

    #[test]
    fn default_streaming_transport_fails_explicitly() {
        struct ExecuteOnlyTransport;

        impl Transport for ExecuteOnlyTransport {
            fn execute(
                &mut self,
                _cx: &kimojio_stack::RuntimeContext<'_>,
                request: &RequestParts,
            ) -> Result<ResponseParts, AttemptError> {
                Ok(ResponseParts {
                    status: 200,
                    metadata: MetadataMap::new(),
                    diagnostics: Diagnostics::new(request.operation),
                })
            }
        }

        let mut transport = ExecuteOnlyTransport;
        let request = RequestParts::new(OperationClass::PageRead, "GET", "/object");
        let error = kimojio_stack::Runtime::new()
            .block_on(|cx| transport.execute_with_body_chunks(cx, &request, &mut |_| Ok(())))
            .unwrap_err();

        assert_eq!(error.error.kind(), ErrorKind::Transport);
    }

    #[test]
    fn stack_http_transport_streams_response_chunks() {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let client_fd: OwnedFd = client_stream.into();
        let server_fd: OwnedFd = server_stream.into();
        let client = kimojio_stack_http::client::ClientConnection::http1(
            kimojio_stack_http::StackTransport::plaintext(client_fd),
            kimojio_stack_http::HttpConfig::default(),
        );
        let mut transport = StackHttpTransport::new(client);
        let request = RequestParts::new(OperationClass::PageRead, "GET", "/object");

        kimojio_stack::Runtime::new().block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut request = [0_u8; 512];
                    let _ = cx.read(&server_fd, &mut request).unwrap();
                    cx.write(
                        &server_fd,
                        b"HTTP/1.1 200 OK\r\ncontent-length: 11\r\nx-ms-request-id: req-ok\r\n\r\nhello world",
                    )
                    .unwrap();
                    cx.close(server_fd).unwrap();
                });

                let mut chunks = Vec::new();
                let response = transport
                    .execute_with_body_chunks(cx, &request, &mut |chunk| {
                        chunks.push(chunk);
                        Ok(())
                    })
                    .unwrap();
                server.join(cx);

                assert_eq!(response.status, 200);
                let body = chunks.concat();
                assert_eq!(body, b"hello world");
            });
        });
    }

    #[test]
    fn stack_http_transport_runs_storage_client_on_stealing_runtime() {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let client_fd: OwnedFd = client_stream.into();
        let server_fd: OwnedFd = server_stream.into();
        let object = ObjectRef {
            account: AccountId::new("acct"),
            container: ContainerName::new("container"),
            name: ObjectName::new("object"),
            kind: ObjectKind::Data,
        };
        let mut runtime = StealRuntime::with_config(StealRuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..StealRuntimeConfig::default()
        });

        let body = runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn_stealable(move |cx| {
                    let server_fd = cx.socket_from_owned_fd(server_fd).unwrap();
                    let mut server_transport =
                        kimojio_stack_http::RuntimeStackTransport::plaintext_socket(server_fd);
                    let mut request = [0_u8; 512];
                    let _ = server_transport.read(cx, &mut request).unwrap();
                    server_transport
                        .write_all(
                            cx,
                            b"HTTP/1.1 200 OK\r\ncontent-length: 11\r\nx-ms-request-id: req-steal\r\n\r\nhello steal",
                        )
                        .unwrap();
                    server_transport.close(cx).unwrap();
                });

                let client = scope.spawn_stealable(move |cx| {
                    let client_fd = cx.socket_from_owned_fd(client_fd).unwrap();
                    let client_transport =
                        kimojio_stack_http::RuntimeStackTransport::plaintext_socket(client_fd);
                    let client = kimojio_stack_http::client::RuntimeClientConnection::http1(
                        client_transport,
                        kimojio_stack_http::HttpConfig::default(),
                    );
                    let mut transport =
                        RuntimeStackHttpTransport::<StealStorageRuntime, _>::new(client);
                    BlockClient.collect_small(cx, &mut transport, &object, 1024)
                });

                server.join(cx);
                client.join(cx).unwrap()
            })
        });

        assert_eq!(body.as_ref(), b"hello steal");
    }

    #[test]
    fn stack_http_transport_preserves_streaming_chunk_order() {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let client_fd: OwnedFd = client_stream.into();
        let server_fd: OwnedFd = server_stream.into();
        let client = kimojio_stack_http::client::ClientConnection::http1(
            kimojio_stack_http::StackTransport::plaintext(client_fd),
            kimojio_stack_http::HttpConfig::default(),
        );
        let mut transport = StackHttpTransport::new(client);
        let request = RequestParts::new(OperationClass::PageRead, "GET", "/object");

        kimojio_stack::Runtime::new().block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut server_transport =
                        kimojio_stack_http::StackTransport::plaintext(server_fd);
                    let mut request = [0_u8; 512];
                    let _ = server_transport.read(cx, &mut request).unwrap();
                    server_transport
                        .write_all(
                            cx,
                            b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n3\r\nabc\r\n3\r\ndef\r\n0\r\n\r\n",
                        )
                        .unwrap();
                    server_transport.close(cx).unwrap();
                });

                let mut chunks = Vec::new();
                transport
                    .execute_with_body_chunks(cx, &request, &mut |chunk| {
                        chunks.push(String::from_utf8(chunk.to_vec()).unwrap());
                        Ok(())
                    })
                    .unwrap();
                server.join(cx);

                assert_eq!(chunks, ["abc", "def"]);
            });
        });
    }

    #[test]
    fn stack_http_transport_classifies_error_statuses() {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let client_fd: OwnedFd = client_stream.into();
        let server_fd: OwnedFd = server_stream.into();
        let client = kimojio_stack_http::client::ClientConnection::http1(
            kimojio_stack_http::StackTransport::plaintext(client_fd),
            kimojio_stack_http::HttpConfig {
                max_body_bytes: 128 * 1024,
                ..kimojio_stack_http::HttpConfig::default()
            },
        );
        let mut transport = StackHttpTransport::new(client);
        let request = RequestParts::new(OperationClass::PageRead, "GET", "/object");

        let (error, chunks) = kimojio_stack::Runtime::new().block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut server_transport =
                        kimojio_stack_http::StackTransport::plaintext(server_fd);
                    let mut request = [0_u8; 512];
                    let _ = server_transport.read(cx, &mut request).unwrap();
                    server_transport
                        .write_all(
                            cx,
                        b"HTTP/1.1 503 Service Unavailable\r\ntransfer-encoding: chunked\r\nx-ms-error-code: ServerBusy\r\nx-ms-request-id: req-busy\r\n\r\n",
                    )
                    .unwrap();
                    let body = vec![b'x'; 64 * 1024];
                    server_transport
                        .write_all(cx, format!("{:x}\r\n", body.len()).as_bytes())
                        .unwrap();
                    server_transport.write_all(cx, &body).unwrap();
                    server_transport.write_all(cx, b"\r\n0\r\n\r\n").unwrap();
                    server_transport.close(cx).unwrap();
                });

                let mut chunks = Vec::new();
                let error = transport
                    .execute_with_body_chunks(cx, &request, &mut |chunk| {
                        chunks.push(chunk);
                        Ok(())
                    })
                    .unwrap_err();
                server.join(cx);
                (error, chunks)
            })
        });

        assert_eq!(error.error.kind(), ErrorKind::Unavailable);
        assert_eq!(error.error.service_code(), Some("ServerBusy"));
        assert_eq!(error.error.request_id(), Some("req-busy"));
        assert!(error.diagnostics.attempts()[0].retriable);
        assert!(chunks.is_empty());
    }

    #[test]
    fn stack_http_transport_rejects_oversized_suppressed_error_body() {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let client_fd: OwnedFd = client_stream.into();
        let server_fd: OwnedFd = server_stream.into();
        let client = kimojio_stack_http::client::ClientConnection::http1(
            kimojio_stack_http::StackTransport::plaintext(client_fd),
            kimojio_stack_http::HttpConfig {
                max_body_bytes: 1,
                ..kimojio_stack_http::HttpConfig::default()
            },
        );
        let mut transport = StackHttpTransport::new(client);
        let request = RequestParts::new(OperationClass::PageRead, "GET", "/object");

        let error = kimojio_stack::Runtime::new().block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut server_transport =
                        kimojio_stack_http::StackTransport::plaintext(server_fd);
                    let mut request = [0_u8; 512];
                    let _ = server_transport.read(cx, &mut request).unwrap();
                    server_transport
                        .write_all(
                            cx,
                            b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 4\r\n\r\nbusy",
                        )
                        .unwrap();
                    server_transport.close(cx).unwrap();
                });

                let error = transport.execute(cx, &request).unwrap_err();
                server.join(cx);
                error
            })
        });

        assert_eq!(error.error.kind(), ErrorKind::Transport);
    }

    #[test]
    fn stack_http_transport_honors_zero_pool_total_timeout() {
        let (client_stream, _server_stream) = UnixStream::pair().unwrap();
        let client_fd: OwnedFd = client_stream.into();
        let client = kimojio_stack_http::client::ClientConnection::http1(
            kimojio_stack_http::StackTransport::plaintext(client_fd),
            kimojio_stack_http::HttpConfig::default(),
        );
        let mut transport = StackHttpTransport::new_with_pool_config(
            client,
            PoolConfig {
                total_timeout: Duration::ZERO,
                ..PoolConfig::default()
            },
        );
        let request = RequestParts::new(OperationClass::PageRead, "GET", "/object");
        let error = kimojio_stack::Runtime::new()
            .block_on(|cx| transport.execute(cx, &request))
            .unwrap_err();

        assert_eq!(error.error.kind(), ErrorKind::Timeout);
    }

    #[test]
    fn stack_http_transport_rejects_invalid_request_parts_without_panic() {
        let (client_stream, _server_stream) = UnixStream::pair().unwrap();
        let client_fd: OwnedFd = client_stream.into();
        let client = kimojio_stack_http::client::ClientConnection::http1(
            kimojio_stack_http::StackTransport::plaintext(client_fd),
            kimojio_stack_http::HttpConfig::default(),
        );
        let mut transport = StackHttpTransport::new(client);
        let request = RequestParts::new(OperationClass::Metadata, "bad method", "/object");
        let error = kimojio_stack::Runtime::new()
            .block_on(|cx| transport.execute(cx, &request))
            .unwrap_err();

        assert_eq!(error.error.kind(), ErrorKind::Transport);
    }
}
