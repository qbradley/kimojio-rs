// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Low-level OTLP logs export.
//!
//! Logs are grouped by one [`Resource`] and one [`InstrumentationScope`] per
//! [`LogBatch`]. Empty batches complete locally without sending a request. A
//! receiver may accept the request but report rejected records through
//! [`LogsExportResult`]; callers decide whether that partial success is fatal.
//! Export runtime behavior is delegated to the shared unary export client; this
//! module only builds OTLP log messages and applies local size limits.
//!
//! ```no_run
//! use kimojio_stack::RuntimeContext;
//! use kimojio_stack_http::{HttpConfig, StackTransport};
//! use kimojio_stack_opentelemetry::{
//!     AnyValue, ExportClientConfig, InstrumentationScope, LogBatch, LogRecord,
//!     LogsClient, Resource, SeverityNumber,
//! };
//!
//! # fn transport() -> StackTransport { unimplemented!() }
//! # fn example(cx: &RuntimeContext<'_>) -> Result<(), kimojio_stack_opentelemetry::Error> {
//! let batch = LogBatch::new(Resource::new(), InstrumentationScope::new("worker"))
//!     .with_record(LogRecord::new(1, SeverityNumber::Info, AnyValue::String("started".into())));
//! let mut client = LogsClient::from_transport(
//!     transport(),
//!     HttpConfig::default(),
//!     ExportClientConfig::default(),
//! );
//! let result = client.export(cx, batch)?;
//! if result.rejected_log_records() != 0 {
//!     eprintln!("receiver rejected logs: {}", result.error_message());
//! }
//! client.finish(cx)?;
//! # Ok(())
//! # }
//! ```

use kimojio_stack::{IoFd, RuntimeSocket};
use kimojio_stack_http::{HttpConfig, HttpRuntime, RuntimeStackTransport, h2};
use prost::Message;

use crate::{
    AnyValue, Error, ExportClientConfig, ExportLimits, InstrumentationScope, KeyValue, Resource,
    check_encoded_len, client::RuntimeUnaryExportClient, proto,
};

/// OTLP logs unary export path.
pub const LOGS_SERVICE_PATH: &str = "/opentelemetry.proto.collector.logs.v1.LogsService/Export";

/// OpenTelemetry log severity number.
///
/// Values use the standard OTLP numeric severity groups. Intermediate numeric
/// severities are not represented yet; choose the nearest group boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum SeverityNumber {
    Unspecified = 0,
    Trace = 1,
    Debug = 5,
    Info = 9,
    Warn = 13,
    Error = 17,
    Fatal = 21,
}

/// A low-level log record.
///
/// The record stores exactly the fields this crate emits today. It does not
/// capture source location, trace/span IDs, flags, or dropped-attribute counts;
/// higher-level instrumentation can layer those in once their costs are explicit.
#[derive(Clone, Debug, PartialEq)]
pub struct LogRecord {
    time_unix_nano: u64,
    observed_time_unix_nano: u64,
    severity_number: SeverityNumber,
    severity_text: String,
    body: AnyValue,
    attributes: Vec<KeyValue>,
}

impl LogRecord {
    /// Creates a log record with nanosecond timestamps and a body.
    ///
    /// `observed_time_unix_nano` defaults to `time_unix_nano`.
    pub fn new(time_unix_nano: u64, severity_number: SeverityNumber, body: AnyValue) -> Self {
        Self {
            time_unix_nano,
            observed_time_unix_nano: time_unix_nano,
            severity_number,
            severity_text: String::new(),
            body,
            attributes: Vec::new(),
        }
    }

    /// Sets the observed timestamp.
    pub fn with_observed_time_unix_nano(mut self, observed_time_unix_nano: u64) -> Self {
        self.observed_time_unix_nano = observed_time_unix_nano;
        self
    }

    /// Sets the severity text.
    pub fn with_severity_text(mut self, severity_text: impl Into<String>) -> Self {
        self.severity_text = severity_text.into();
        self
    }

    /// Adds a log attribute.
    pub fn with_attribute(mut self, attribute: KeyValue) -> Self {
        self.attributes.push(attribute);
        self
    }

    fn into_proto(self) -> proto::LogRecord {
        proto::LogRecord {
            time_unix_nano: self.time_unix_nano,
            observed_time_unix_nano: self.observed_time_unix_nano,
            severity_number: self.severity_number as i32,
            severity_text: self.severity_text,
            body: Some(self.body.into_proto()),
            attributes: self
                .attributes
                .into_iter()
                .map(KeyValue::into_proto)
                .collect(),
        }
    }
}

/// Caller-owned batch of log records.
///
/// The batch consumes records by value so callers can prebuild telemetry without
/// hidden global state or background queues.
#[derive(Clone, Debug, PartialEq)]
pub struct LogBatch {
    resource: Resource,
    scope: InstrumentationScope,
    records: Vec<LogRecord>,
}

impl LogBatch {
    /// Creates a log batch.
    pub fn new(resource: Resource, scope: InstrumentationScope) -> Self {
        Self {
            resource,
            scope,
            records: Vec::new(),
        }
    }

    /// Adds a log record.
    pub fn with_record(mut self, record: LogRecord) -> Self {
        self.records.push(record);
        self
    }

    /// Returns the log record count.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Returns true if the batch contains no records.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Builds an OTLP export request.
    ///
    /// Use [`encode_request`](Self::encode_request) when you want local size
    /// validation and a serialized request for tests or fixtures.
    pub fn into_request(self) -> proto::ExportLogsServiceRequest {
        proto::ExportLogsServiceRequest {
            resource_logs: vec![proto::ResourceLogs {
                resource: Some(self.resource.into_proto()),
                scope_logs: vec![proto::ScopeLogs {
                    scope: Some(self.scope.into_proto()),
                    log_records: self
                        .records
                        .into_iter()
                        .map(LogRecord::into_proto)
                        .collect(),
                }],
            }],
        }
    }

    fn into_checked_request(
        self,
        limits: ExportLimits,
    ) -> Result<proto::ExportLogsServiceRequest, Error> {
        let request = self.into_request();
        check_encoded_len(request.encoded_len(), limits)?;
        Ok(request)
    }

    /// Encodes an OTLP export request after applying size limits.
    pub fn encode_request(self, limits: ExportLimits) -> Result<Vec<u8>, Error> {
        let request = self.into_request();
        let len = request.encoded_len();
        check_encoded_len(len, limits)?;
        Ok(request.encode_to_vec())
    }
}

/// Low-level logs export result.
///
/// An all-zero/default result means the receiver did not report partial failure.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LogsExportResult {
    rejected_log_records: i64,
    error_message: String,
}

impl LogsExportResult {
    /// Converts an OTLP logs export response into a low-level result.
    pub fn from_response(response: proto::ExportLogsServiceResponse) -> Self {
        response
            .partial_success
            .map_or_else(Self::default, |partial| Self {
                rejected_log_records: partial.rejected_log_records,
                error_message: partial.error_message,
            })
    }

    /// Decodes an OTLP logs export response.
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let response = proto::ExportLogsServiceResponse::decode(bytes)
            .map_err(|_| Error::Validation("logs export response"))?;
        Ok(Self::from_response(response))
    }

    /// Number of rejected log records reported by the receiver.
    pub fn rejected_log_records(&self) -> i64 {
        self.rejected_log_records
    }

    /// Receiver-provided rejection message.
    pub fn error_message(&self) -> &str {
        &self.error_message
    }
}

/// Low-level OTLP logs export client.
///
/// The client owns one gRPC connection. It performs one unary export per
/// [`export`](Self::export) call and does not retry or batch in the background.
pub struct RuntimeLogsClient<S> {
    inner: RuntimeUnaryExportClient<S>,
}

/// Stack-core OTLP logs export client compatibility alias.
pub type LogsClient = RuntimeLogsClient<IoFd>;

impl<S> RuntimeLogsClient<S> {
    /// Creates a logs client from a caller-created HTTP/2 connection.
    pub fn new(http: h2::RuntimeClientConnection<S>, config: ExportClientConfig) -> Self {
        Self {
            inner: RuntimeUnaryExportClient::new(http, config),
        }
    }

    /// Creates a logs client from a caller-created stack transport.
    pub fn from_transport(
        transport: RuntimeStackTransport<S>,
        http_config: HttpConfig,
        config: ExportClientConfig,
    ) -> Self {
        Self {
            inner: RuntimeUnaryExportClient::from_transport(transport, http_config, config),
        }
    }

    /// Exports one caller-owned batch of log records.
    ///
    /// Empty batches return `Ok(LogsExportResult::default())` without sending a
    /// gRPC request.
    pub fn export(
        &mut self,
        cx: &impl HttpRuntime<S>,
        batch: LogBatch,
    ) -> Result<LogsExportResult, Error>
    where
        S: RuntimeSocket,
    {
        if batch.is_empty() {
            return Ok(LogsExportResult::default());
        }
        let request = batch.into_checked_request(self.inner.export_limits())?;
        let response = self.inner.export::<_, proto::ExportLogsServiceResponse>(
            cx,
            LOGS_SERVICE_PATH,
            &request,
        )?;
        Ok(LogsExportResult::from_response(response))
    }

    /// Finishes the client by closing the underlying gRPC connection.
    pub fn finish(self, cx: &impl HttpRuntime<S>) -> Result<(), Error>
    where
        S: RuntimeSocket,
    {
        self.inner.finish(cx)
    }
}

impl LogsClient {
    /// Creates a plaintext logs client from a connected socket.
    pub fn plaintext(
        socket: kimojio_stack::OwnedFd,
        http_config: HttpConfig,
        config: ExportClientConfig,
    ) -> Self {
        Self {
            inner: RuntimeUnaryExportClient::plaintext(socket, http_config, config),
        }
    }

    /// Creates a TLS logs client from an established stack TLS stream.
    #[cfg(feature = "tls")]
    pub fn tls(
        stream: kimojio_stack_tls::TlsStream,
        http_config: HttpConfig,
        config: ExportClientConfig,
    ) -> Self {
        Self {
            inner: RuntimeUnaryExportClient::tls(stream, http_config, config),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{hint::black_box, num::NonZeroUsize};

    use kimojio_stack::{Runtime, SocketIoRuntime, once};
    use kimojio_stack_grpc::{
        GrpcRuntime, RuntimeUnaryServer, ServerConfig, Status, StatusCode, UnaryReply, UnaryServer,
    };
    use kimojio_stack_http::{HttpConfig, RuntimeStackTransport, StackTransport, h2};
    use kimojio_stack_steal::{
        Runtime as StealRuntime, RuntimeConfig as StealRuntimeConfig, StealPolicy,
    };
    use rustix::net::{AddressFamily, RecvFlags, SocketFlags, SocketType, recv, socketpair};

    use super::*;
    use crate::allocation_tracking;

    struct StealGrpcRuntime;

    impl GrpcRuntime for StealGrpcRuntime {
        type Context<'cx> = kimojio_stack_steal::RuntimeContext<'cx>;
    }

    fn batch() -> LogBatch {
        LogBatch::new(
            Resource::new().with_attribute(KeyValue::new(
                "service.name",
                AnyValue::String("test-service".into()),
            )),
            InstrumentationScope::new("test-scope"),
        )
        .with_record(
            LogRecord::new(10, SeverityNumber::Info, AnyValue::String("hello".into()))
                .with_severity_text("INFO")
                .with_attribute(KeyValue::new("answer", AnyValue::I64(42))),
        )
    }

    #[test]
    fn log_batch_encodes_resource_scope_and_record() {
        let request = batch().into_request();
        let resource_logs = &request.resource_logs[0];
        let scope_logs = &resource_logs.scope_logs[0];

        assert_eq!(
            resource_logs.resource.as_ref().unwrap().attributes[0].key,
            "service.name"
        );
        assert_eq!(scope_logs.scope.as_ref().unwrap().name, "test-scope");
        assert_eq!(scope_logs.log_records.len(), 1);
        assert_eq!(scope_logs.log_records[0].severity_number, 9);
    }

    #[test]
    fn empty_log_batch_is_deterministic_noop_request() {
        let request =
            LogBatch::new(Resource::new(), InstrumentationScope::new("scope")).into_request();

        assert_eq!(request.resource_logs.len(), 1);
        assert!(
            request.resource_logs[0].scope_logs[0]
                .log_records
                .is_empty()
        );
    }

    #[test]
    fn oversized_log_request_returns_size_limit_error() {
        let error = batch()
            .encode_request(ExportLimits { max_encoded_len: 1 })
            .unwrap_err();

        assert_eq!(error.kind(), crate::ErrorKind::SizeLimit);
    }

    #[test]
    fn logs_partial_success_decodes_rejections() {
        let response = proto::ExportLogsServiceResponse {
            partial_success: Some(proto::ExportLogsPartialSuccess {
                rejected_log_records: 3,
                error_message: "bad logs".into(),
            }),
        };

        let result = LogsExportResult::decode(&response.encode_to_vec()).unwrap();

        assert_eq!(result.rejected_log_records(), 3);
        assert_eq!(result.error_message(), "bad logs");
    }

    #[test]
    fn empty_log_export_does_not_write_to_transport() {
        let (client_fd, server_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let mut client = LogsClient::plaintext(
                client_fd,
                HttpConfig::default(),
                ExportClientConfig::default(),
            );
            let result = client
                .export(
                    cx,
                    LogBatch::new(Resource::new(), InstrumentationScope::new("scope")),
                )
                .unwrap();
            let mut probe = [0_u8; 1];
            assert_eq!(
                recv(&server_fd, &mut probe, RecvFlags::DONTWAIT).unwrap_err(),
                rustix::io::Errno::AGAIN
            );
            assert_eq!(result.rejected_log_records(), 0);
        });
    }

    #[test]
    fn grpc_message_size_limit_maps_to_size_limit_kind() {
        let (client_fd, _server_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        let mut runtime = Runtime::new();

        let error = runtime.block_on(|cx| {
            let mut client = LogsClient::plaintext(
                client_fd,
                HttpConfig::default(),
                ExportClientConfig {
                    max_message_len: 1,
                    export_limits: ExportLimits::default(),
                },
            );
            client.export(cx, batch()).unwrap_err()
        });

        assert_eq!(error.kind(), crate::ErrorKind::SizeLimit);
    }

    #[test]
    fn logs_client_exports_multi_record_batch_and_partial_success() {
        let (client_fd, server_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let mut client = LogsClient::plaintext(
                client_fd,
                HttpConfig::default(),
                ExportClientConfig::default(),
            );
            let mut probe = [0_u8; 1];
            assert_eq!(
                recv(&server_fd, &mut probe, RecvFlags::DONTWAIT).unwrap_err(),
                rustix::io::Errno::AGAIN
            );

            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut http = h2::ServerConnection::new(
                        StackTransport::plaintext(server_fd),
                        HttpConfig::default(),
                    );
                    let mut grpc = UnaryServer::new(ServerConfig::default());
                    grpc.add_unary::<
                        proto::ExportLogsServiceRequest,
                        proto::ExportLogsServiceResponse,
                        _,
                    >(LOGS_SERVICE_PATH, |_cx, _metadata, request| {
                        let records = &request.resource_logs[0].scope_logs[0].log_records;
                        assert_eq!(records.len(), 2);
                        assert_eq!(records[0].severity_text, "INFO");
                        assert_eq!(records[1].severity_text, "WARN");
                        Ok(UnaryReply::new(proto::ExportLogsServiceResponse {
                            partial_success: Some(proto::ExportLogsPartialSuccess {
                                rejected_log_records: 1,
                                error_message: "one rejected".into(),
                            }),
                        }))
                    });
                    grpc.serve_one(cx, &mut http).unwrap();
                    http.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                let batch = batch().with_record(
                    LogRecord::new(11, SeverityNumber::Warn, AnyValue::String("second".into()))
                        .with_severity_text("WARN"),
                );
                let result = client.export(cx, batch).unwrap();
                client.finish(cx).unwrap();
                server.join(cx);

                assert_eq!(result.rejected_log_records(), 1);
                assert_eq!(result.error_message(), "one rejected");
            });
        });
    }

    #[test]
    fn logs_client_exports_on_stealing_runtime() {
        let (client_fd, server_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        let mut runtime = StealRuntime::with_config(StealRuntimeConfig {
            workers: NonZeroUsize::new(2).unwrap(),
            steal_policy: StealPolicy::steal_one(),
            ..StealRuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn_stealable(move |cx| {
                    let server_fd = cx.socket_from_owned_fd(server_fd).unwrap();
                    let server_transport = RuntimeStackTransport::plaintext_socket(server_fd);
                    let mut http =
                        h2::RuntimeServerConnection::new(server_transport, HttpConfig::default());
                    let mut grpc =
                        RuntimeUnaryServer::<StealGrpcRuntime>::new(ServerConfig::default());
                    grpc.add_unary::<
                        proto::ExportLogsServiceRequest,
                        proto::ExportLogsServiceResponse,
                        _,
                    >(LOGS_SERVICE_PATH, |_cx, _metadata, request| {
                        let records = &request.resource_logs[0].scope_logs[0].log_records;
                        assert_eq!(records.len(), 1);
                        assert_eq!(records[0].severity_text, "INFO");
                        Ok(UnaryReply::new(proto::ExportLogsServiceResponse {
                            partial_success: None,
                        }))
                    });
                    grpc.serve_one(cx, &mut http).unwrap();
                    http.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                let client = scope.spawn_stealable(move |cx| {
                    let client_fd = cx.socket_from_owned_fd(client_fd).unwrap();
                    let client_transport = RuntimeStackTransport::plaintext_socket(client_fd);
                    let http =
                        h2::RuntimeClientConnection::new(client_transport, HttpConfig::default());
                    let mut client = RuntimeLogsClient::new(http, ExportClientConfig::default());
                    let result = client.export(cx, batch()).unwrap();
                    client.finish(cx).unwrap();
                    result.rejected_log_records()
                });

                server.join(cx);
                assert_eq!(client.join(cx), 0);
            });
        });
    }

    #[test]
    fn logs_client_preserves_receiver_status_error_kind() {
        let (client_fd, server_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        let mut runtime = Runtime::new();

        let error = runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut http = h2::ServerConnection::new(
                        StackTransport::plaintext(server_fd),
                        HttpConfig::default(),
                    );
                    let mut grpc = UnaryServer::new(ServerConfig::default());
                    grpc.add_unary::<
                        proto::ExportLogsServiceRequest,
                        proto::ExportLogsServiceResponse,
                        _,
                    >(LOGS_SERVICE_PATH, |_cx, _metadata, _request| {
                        Err(Status::new(
                            StatusCode::Unavailable,
                            "collector unavailable",
                        ))
                    });
                    grpc.serve_one(cx, &mut http).unwrap();
                    http.shutdown_write_and_close_after_peer(cx).unwrap();
                });
                let mut client = LogsClient::plaintext(
                    client_fd,
                    HttpConfig::default(),
                    ExportClientConfig::default(),
                );
                let error = client.export(cx, batch()).unwrap_err();
                client.finish(cx).unwrap();
                server.join(cx);
                error
            })
        });

        assert_eq!(error.kind(), crate::ErrorKind::Status);
        assert!(error.to_string().contains("collector unavailable"));
    }

    #[test]
    fn allocation_logs_export_warmed_local_loop_records_current_hot_path_allocations() {
        const WARMUP_ROUNDS: usize = 2;
        const MEASURED_ROUNDS: usize = 4;

        let counts = Runtime::new().block_on(|cx| {
            let (client_fd, server_fd) = socketpair(
                AddressFamily::UNIX,
                SocketType::STREAM,
                SocketFlags::CLOEXEC,
                None,
            )
            .unwrap();

            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    let mut http = h2::ServerConnection::new(
                        StackTransport::plaintext(server_fd),
                        HttpConfig::default(),
                    );
                    let mut grpc = UnaryServer::new(ServerConfig::default());
                    grpc.add_unary::<
                        proto::ExportLogsServiceRequest,
                        proto::ExportLogsServiceResponse,
                        _,
                    >(LOGS_SERVICE_PATH, |_cx, _metadata, _request| {
                        Ok(UnaryReply::new(proto::ExportLogsServiceResponse {
                            partial_success: None,
                        }))
                    });
                    for _ in 0..WARMUP_ROUNDS + MEASURED_ROUNDS {
                        grpc.serve_one(cx, &mut http).unwrap();
                    }
                    http.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                let mut client = LogsClient::plaintext(
                    client_fd,
                    HttpConfig::default(),
                    ExportClientConfig::default(),
                );
                let batch = batch();
                for _ in 0..WARMUP_ROUNDS {
                    black_box(client.export(cx, batch.clone()).unwrap());
                }

                let (_, counts) = allocation_tracking::measure(|| {
                    for _ in 0..MEASURED_ROUNDS {
                        black_box(client.export(cx, batch.clone()).unwrap());
                    }
                });

                client.finish(cx).unwrap();
                server.join(cx);
                counts
            })
        });

        assert!(counts.allocating_operations() <= 1024, "{counts:?}");
        assert!(
            counts.allocated_or_reallocated_bytes() <= 1024 * 1024,
            "{counts:?}"
        );
    }

    #[test]
    fn blocked_logs_export_allows_unrelated_stackful_work_to_continue() {
        let (client_fd, server_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        let mut runtime = Runtime::new();

        let output = runtime.block_on(|cx| {
            cx.scope(|scope| {
                let (tx, rx) = once::channel();
                let client = scope.spawn(move |cx| {
                    let mut client = LogsClient::plaintext(
                        client_fd,
                        HttpConfig::default(),
                        ExportClientConfig::default(),
                    );
                    let result = client.export(cx, batch()).unwrap();
                    client.finish(cx).unwrap();
                    result
                });
                let unrelated = scope.spawn(move |_| {
                    tx.send(42).unwrap();
                });
                let server = scope.spawn(move |cx| {
                    assert_eq!(rx.recv(cx).unwrap(), 42);
                    let mut http = h2::ServerConnection::new(
                        StackTransport::plaintext(server_fd),
                        HttpConfig::default(),
                    );
                    let mut grpc = UnaryServer::new(ServerConfig::default());
                    grpc.add_unary::<
                        proto::ExportLogsServiceRequest,
                        proto::ExportLogsServiceResponse,
                        _,
                    >(LOGS_SERVICE_PATH, |_cx, _metadata, _request| {
                        Ok(UnaryReply::new(proto::ExportLogsServiceResponse {
                            partial_success: None,
                        }))
                    });
                    grpc.serve_one(cx, &mut http).unwrap();
                    http.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                unrelated.join(cx);
                server.join(cx);
                client.join(cx)
            })
        });

        assert_eq!(output.rejected_log_records(), 0);
    }
}
